//! Supply `textDocument/codeAction` requests with the diagnostics that gate
//! intellij-server's quickfixes.
//!
//! intellij-server builds its unresolved-symbol quickfixes ("Import class …",
//! "Create class …") *only* from the diagnostics the client echoes back in
//! `context.diagnostics`: the Java/Kotlin code-action providers deserialize each
//! incoming diagnostic's `data`, recompute the file's diagnostics, match the two
//! (`KotlinCompilerDiagnosticData.matches`) and then list the fixes stored in
//! that `data`. With an empty `context.diagnostics` they return **no quickfixes
//! at all** — the user sees only unrelated actions (typically "Organize
//! imports") even though the completions path inserts the very same import.
//!
//! Zed builds that list in `GetCodeActions::to_lsp` with
//! `entries_in_range(self.range, buffer, inclusive = false, reversed = false)`,
//! whose overlap test is *strict*: `start < entry.max_end && end > entry.min_start`.
//! The request range is the selection — normally a zero-width caret — so a caret
//! that sits exactly on a diagnostic's first (or last) character, or that is not
//! strictly inside the published span, drops that diagnostic from the request.
//! The server is then handed `diagnostics: []` and answers with nothing. This is
//! why the very same import works from completion (which does not use this path)
//! and why only servers that derive fixes from `context.diagnostics` are hit.
//!
//! (Zed does round-trip each sent diagnostic's opaque `data` faithfully, so the
//! request only fails when the diagnostic itself is left out — verified against
//! captures of real Zed requests.)
//!
//! This module remembers the diagnostics the server last published for each
//! document — push (`textDocument/publishDiagnostics`) and pull
//! (`textDocument/diagnostic`) alike — and repairs outgoing code-action requests:
//!
//! * [`DiagnosticCache::supplement`] adds published diagnostics that cover the
//!   request's range but are missing from `context.diagnostics` (the bug above);
//! * [`DiagnosticCache::reinject`] fills a *missing* `data` field on a diagnostic
//!   the client did send, for clients that do not round-trip it.
//!
//! Both are additive and conservative: they never remove or overwrite what the
//! client sent, and matching requires the same code and range (or the same code
//! and message when unique).

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

/// Upper bound on remembered documents; diagnostics can be large (an unresolved
/// symbol's `data` embeds the whole fix payload), so the cache is a small LRU
/// rather than an unbounded map. Code actions are requested for a handful of
/// open buffers at a time, so this never evicts a document that is in use.
const MAX_DOCUMENTS: usize = 32;

/// Upper bound on correlated-but-unanswered pull requests. A pull the server
/// never answers (cancelled mid-flight) would otherwise leak its id; hitting
/// this bound just drops the correlation, so the affected response is not
/// cached — code actions fall back to the previous publish, never to a wrong
/// `data`.
const MAX_PULLS: usize = 256;

/// Identity of a diagnostic within a document, used to match a request's stub
/// against the published original. Ranges are the LSP `line`/`character` pair;
/// `code` is normalised to a string so `NumberOrString` compares uniformly.
#[derive(PartialEq, Eq, Hash)]
struct Key {
    code: String,
    start: (i64, i64),
    end: (i64, i64),
}

#[derive(Default)]
pub struct DiagnosticCache {
    /// Last diagnostics published/returned per document uri, oldest first.
    documents: HashMap<String, Vec<Value>>,
    /// Insertion order of `documents` keys, for LRU eviction.
    order: VecDeque<String>,
    /// In-flight `textDocument/diagnostic` request ids -> their document uri.
    pulls: HashMap<i64, String>,
}

impl DiagnosticCache {
    /// Remember the diagnostics of a parsed `textDocument/publishDiagnostics`
    /// notification. The diagnostics array is *moved* out of `message` rather
    /// than cloned — the caller still forwards the original bytes, and the
    /// array can be large (each unresolved symbol's `data` embeds its fixes).
    pub fn record_published(&mut self, message: &mut Value) {
        let Some(uri) = message
            .pointer("/params/uri")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let Some(Value::Array(diagnostics)) =
            message.pointer_mut("/params/diagnostics").map(Value::take)
        else {
            return;
        };
        self.store(&uri, diagnostics);
    }

    /// Remember the document uri of an outgoing `textDocument/diagnostic` pull,
    /// so the matching response can be attributed to it.
    pub fn track_pull(&mut self, id: i64, uri: String) {
        if self.pulls.len() >= MAX_PULLS {
            self.pulls.clear();
        }
        self.pulls.insert(id, uri);
    }

    /// Whether `id` belongs to an in-flight `textDocument/diagnostic` request.
    /// Checked before parsing a response body, so the (large, frequent)
    /// completion payloads are never parsed just to rule this out.
    pub fn is_tracked_pull(&self, id: i64) -> bool {
        self.pulls.contains_key(&id)
    }

    /// Remember the diagnostics of a parsed `textDocument/diagnostic` response.
    /// The request must have been [tracked](Self::track_pull); returns `false`
    /// for any other response. Like [`record_published`](Self::record_published),
    /// the items are moved out instead of cloned.
    pub fn record_pull_response(&mut self, id: i64, response: &mut Value) -> bool {
        let Some(uri) = self.pulls.remove(&id) else {
            return false;
        };
        if let Some(Value::Array(items)) = response.pointer_mut("/result/items").map(Value::take) {
            self.store(&uri, items);
        }
        true
    }

    /// Add published diagnostics that cover a `textDocument/codeAction`
    /// request's range but are missing from `context.diagnostics`, using an
    /// *inclusive* overlap test (touching counts). Returns `true` when the
    /// message changed.
    ///
    /// This is the fix for the caret-on-the-boundary case: Zed's own selection
    /// uses a strict test, so a zero-width range at the start of a diagnostic is
    /// dropped and the server — which needs the diagnostic's `data` to offer a
    /// quickfix — returns none.
    pub fn supplement(&self, message: &mut Value) -> bool {
        if message.get("method").and_then(Value::as_str) != Some("textDocument/codeAction") {
            return false;
        }
        let Some(published) = message
            .pointer("/params/textDocument/uri")
            .and_then(Value::as_str)
            .and_then(|uri| self.documents.get(uri))
        else {
            return false;
        };
        let Some(range) = message.pointer("/params/range").cloned() else {
            return false;
        };
        let covering: Vec<Value> = published
            .iter()
            .filter(|diagnostic| overlaps(diagnostic, &range))
            .cloned()
            .collect();
        if covering.is_empty() {
            return false;
        }

        // Any client-supplied diagnostic is left untouched; only missing ones
        // (by code+range) are appended, in the order the server published them.
        let Some(diagnostics) = message
            .pointer_mut("/params/context/diagnostics")
            .and_then(Value::as_array_mut)
        else {
            return false;
        };
        let present: HashSet<Key> = diagnostics.iter().filter_map(key_of).collect();

        let mut changed = false;
        for diagnostic in covering {
            match key_of(&diagnostic) {
                Some(key) if present.contains(&key) => continue,
                _ => {}
            }
            diagnostics.push(diagnostic);
            changed = true;
        }
        changed
    }

    /// Copy published `data` back onto a `textDocument/codeAction` request whose
    /// diagnostics arrived without it. Returns `true` when the message changed.
    pub fn reinject(&self, message: &mut Value) -> bool {
        if message.get("method").and_then(Value::as_str) != Some("textDocument/codeAction") {
            return false;
        }
        let Some(published) = message
            .pointer("/params/textDocument/uri")
            .and_then(Value::as_str)
            .and_then(|uri| self.documents.get(uri))
        else {
            return false;
        };
        let Some(diagnostics) = message
            .pointer_mut("/params/context/diagnostics")
            .and_then(Value::as_array_mut)
        else {
            return false;
        };

        // Index the published diagnostics that carry `data`, by exact key. A
        // code+message index is built as well so a stub whose range drifted
        // (buffer edited between publish and request) can still be matched when
        // exactly one published diagnostic carries that code and message.
        let mut by_key: HashMap<Key, &Value> = HashMap::new();
        let mut by_message: HashMap<(String, String), Vec<&Value>> = HashMap::new();
        for diagnostic in published {
            if diagnostic.get("data").is_none() {
                continue;
            }
            if let Some(key) = key_of(diagnostic) {
                by_key.entry(key).or_insert(diagnostic);
            }
            if let Some((code, message)) = code_and_message(diagnostic) {
                by_message.entry((code, message)).or_default().push(diagnostic);
            }
        }
        if by_key.is_empty() {
            return false;
        }

        let mut changed = false;
        for diagnostic in diagnostics.iter_mut() {
            if diagnostic.get("data").is_some() {
                continue;
            }
            let source = key_of(diagnostic)
                .and_then(|key| by_key.get(&key).copied())
                .or_else(|| {
                    let (code, message) = code_and_message(diagnostic)?;
                    match by_message.get(&(code, message))?.as_slice() {
                        [only] => Some(*only),
                        _ => None,
                    }
                });
            let Some(data) = source.and_then(|source| source.get("data")) else {
                continue;
            };
            diagnostic["data"] = data.clone();
            changed = true;
        }
        changed
    }

    fn store(&mut self, uri: &str, diagnostics: Vec<Value>) {
        if !self.documents.contains_key(uri) {
            if self.order.len() >= MAX_DOCUMENTS {
                if let Some(evicted) = self.order.pop_front() {
                    self.documents.remove(&evicted);
                }
            }
            self.order.push_back(uri.to_owned());
        }
        self.documents.insert(uri.to_owned(), diagnostics);
    }
}

/// The match key of a diagnostic: code plus its range, or `None` when the
/// message is not shaped like an LSP diagnostic.
fn key_of(diagnostic: &Value) -> Option<Key> {
    let range = diagnostic.get("range")?;
    let point = |which: &str| -> Option<(i64, i64)> {
        Some((
            range.pointer(&format!("/{which}/line"))?.as_i64()?,
            range.pointer(&format!("/{which}/character"))?.as_i64()?,
        ))
    };
    Some(Key {
        code: code_of(diagnostic),
        start: point("start")?,
        end: point("end")?,
    })
}

/// Normalised `code` plus `message`, the fallback identity for a drifted range.
fn code_and_message(diagnostic: &Value) -> Option<(String, String)> {
    let message = diagnostic.get("message")?;
    // A `DiagnosticMessage` may be a plain string or a `{ kind, value }` markup
    // object; the server publishes the plain form, but accept both.
    let message = match message {
        Value::String(text) => text.clone(),
        Value::Object(object) => object.get("value")?.as_str()?.to_owned(),
        _ => return None,
    };
    Some((code_of(diagnostic), message))
}

/// Whether a published diagnostic overlaps the request range, *inclusive* of a
/// shared boundary. Deliberately looser than Zed's own selection (which is
/// strict and can drop a zero-width caret sitting on a diagnostic's first
/// character): here touching is enough, because the server re-checks the match
/// against its own recomputed diagnostics and simply ignores anything unrelated.
fn overlaps(diagnostic: &Value, range: &Value) -> bool {
    let (Some(ds), Some(de)) = (
        diagnostic.pointer("/range/start"),
        diagnostic.pointer("/range/end"),
    ) else {
        return false;
    };
    let (Some(rs), Some(re)) = (range.get("start"), range.get("end")) else {
        return false;
    };
    let (Some((rs, rc)), Some((re, ec))) = (line_col(rs), line_col(re)) else {
        return false;
    };
    let (Some((dsl, dsc)), Some((del, dec))) = (line_col(ds), line_col(de)) else {
        return false;
    };
    (dsl, dsc) <= (re, ec) && (rs, rc) <= (del, dec)
}

/// `(line, character)` of an LSP position object.
fn line_col(point: &Value) -> Option<(i64, i64)> {
    Some((point.get("line")?.as_i64()?, point.get("character")?.as_i64()?))
}

/// `Diagnostic.code` is `NumberOrString`; normalise it so a client that
/// re-encodes a numeric code as a string still matches the server's original.
fn code_of(diagnostic: &Value) -> String {
    match diagnostic.get("code") {
        Some(Value::String(code)) => code.clone(),
        Some(Value::Number(code)) => code.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn published_params(uri: &str, code: &str, line: i64, message: &str) -> Value {
        json!({
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": { "line": line, "character": 8 },
                        "end": { "line": line, "character": 14 }
                    },
                    "code": code,
                    "message": message,
                    "data": { "fixes": [{ "name": "Import class → com.example.Widget" }] }
                }]
            }
        })
    }

    fn code_action(uri: &str, code: &str, line: i64, with_data: bool) -> Value {
        let mut diagnostic = json!({
            "range": {
                "start": { "line": line, "character": 8 },
                "end": { "line": line, "character": 14 }
            },
            "code": code,
            "severity": 1,
            "message": "Cannot resolve symbol 'Widget'"
        });
        if with_data {
            diagnostic["data"] = json!({ "stale": true });
        }
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": line, "character": 11 },
                    "end": { "line": line, "character": 11 }
                },
                "context": { "diagnostics": [diagnostic] }
            }
        })
    }

    #[test]
    fn fills_missing_data_from_publish() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_params("file:///a/Bar.java", "reference.unresolved", 5, "Cannot resolve symbol 'Widget'"));
        let mut request = code_action("file:///a/Bar.java", "reference.unresolved", 5, false);
        assert!(cache.reinject(&mut request));
        assert_eq!(
            request.pointer("/params/context/diagnostics/0/data/fixes/0/name"),
            Some(&json!("Import class → com.example.Widget"))
        );
    }

    #[test]
    fn leaves_client_supplied_data_alone() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_params("file:///a/Bar.java", "reference.unresolved", 5, "Cannot resolve symbol 'Widget'"));
        let mut request = code_action("file:///a/Bar.java", "reference.unresolved", 5, true);
        assert!(!cache.reinject(&mut request));
        assert_eq!(
            request.pointer("/params/context/diagnostics/0/data"),
            Some(&json!({ "stale": true }))
        );
    }

    #[test]
    fn matches_other_documents_and_unknown_codes_by_nothing() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_params("file:///a/Bar.java", "reference.unresolved", 5, "x"));
        let mut other_document = code_action("file:///a/Other.java", "reference.unresolved", 5, false);
        assert!(!cache.reinject(&mut other_document));
        let mut other_code = code_action("file:///a/Bar.java", "call.unresolved.name", 5, false);
        assert!(!cache.reinject(&mut other_code));
    }

    #[test]
    fn matches_drifted_range_by_unique_code_and_message() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_params("file:///a/Bar.java", "reference.unresolved", 5, "Cannot resolve symbol 'Widget'"));
        let mut request = code_action("file:///a/Bar.java", "reference.unresolved", 9, false);
        assert!(cache.reinject(&mut request));
    }

    #[test]
    fn ignores_non_code_action_messages() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_params("file:///a/Bar.java", "reference.unresolved", 5, "x"));
        let mut definition = json!({
            "jsonrpc": "2.0", "id": 2, "method": "textDocument/definition",
            "params": { "textDocument": { "uri": "file:///a/Bar.java" } }
        });
        assert!(!cache.reinject(&mut definition));
    }

    /// A code-action request with an explicit caret (zero-width range) and an
    /// arbitrary set of client-supplied diagnostics.
    fn code_action_at(uri: &str, line: i64, character: i64, diagnostics: Value) -> Value {
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character }
                },
                "context": { "diagnostics": diagnostics }
            }
        })
    }

    /// A published diagnostic spanning characters `start..end` on `line`.
    fn published_at(uri: &str, code: &str, line: i64, start: i64, end: i64) -> Value {
        json!({
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": { "line": line, "character": start },
                        "end": { "line": line, "character": end }
                    },
                    "code": code,
                    "message": "Cannot resolve symbol 'Widget'",
                    "data": { "fixes": [{ "name": "Import class → com.example.Widget" }] }
                }]
            }
        })
    }

    #[test]
    fn supplement_adds_diagnostic_omitted_by_the_client() {
        // The captured bug: caret exactly on the diagnostic's first character,
        // so Zed's strict range test drops it and sends `diagnostics: []`.
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_at("file:///a/Bar.java", "reference.unresolved", 40, 87, 98));
        let mut request = code_action_at("file:///a/Bar.java", 40, 87, json!([]));
        assert!(cache.supplement(&mut request));
        assert_eq!(
            request.pointer("/params/context/diagnostics/0/data/fixes/0/name"),
            Some(&json!("Import class → com.example.Widget"))
        );
    }

    #[test]
    fn supplement_leaves_client_supplied_diagnostics_alone() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_at("file:///a/Bar.java", "reference.unresolved", 40, 87, 98));
        let sent = json!([{
            "range": {
                "start": { "line": 40, "character": 87 },
                "end": { "line": 40, "character": 98 }
            },
            "code": "reference.unresolved",
            "message": "Cannot resolve symbol 'Widget'",
            "data": { "stale": true }
        }]);
        let mut request = code_action_at("file:///a/Bar.java", 40, 88, sent);
        assert!(!cache.supplement(&mut request));
        // The client's own `data` is untouched.
        assert_eq!(
            request.pointer("/params/context/diagnostics/0/data"),
            Some(&json!({ "stale": true }))
        );
    }

    #[test]
    fn supplement_ignores_diagnostics_away_from_the_caret() {
        let mut cache = DiagnosticCache::default();
        cache.record_published(&mut published_at("file:///a/Bar.java", "reference.unresolved", 40, 87, 98));
        let mut request = code_action_at("file:///a/Bar.java", 12, 4, json!([]));
        assert!(!cache.supplement(&mut request));
    }

    #[test]
    fn supplement_adds_only_covering_diagnostics() {
        let mut cache = DiagnosticCache::default();
        let mut published = published_at("file:///a/Bar.java", "reference.unresolved", 40, 87, 98);
        published["params"]["diagnostics"].as_array_mut().unwrap().push(json!({
            "range": {
                "start": { "line": 12, "character": 0 },
                "end": { "line": 12, "character": 10 }
            },
            "code": "call.unresolved.name",
            "message": "Cannot resolve method 'spin()'",
            "data": { "fixes": [] }
        }));
        cache.record_published(&mut published);
        let mut request = code_action_at("file:///a/Bar.java", 40, 90, json!([]));
        assert!(cache.supplement(&mut request));
        let added = request.pointer("/params/context/diagnostics").unwrap().as_array().unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["code"], json!("reference.unresolved"));
    }

    #[test]
    fn supplement_needs_cached_diagnostics() {
        let cache = DiagnosticCache::default();
        let mut request = code_action_at("file:///a/Bar.java", 40, 87, json!([]));
        assert!(!cache.supplement(&mut request));
    }

    #[test]
    fn only_tracked_ids_are_parsed() {
        let mut cache = DiagnosticCache::default();
        assert!(!cache.is_tracked_pull(7));
        cache.track_pull(7, "file:///a/Bar.java".into());
        assert!(cache.is_tracked_pull(7));
    }

    #[test]
    fn pull_responses_are_recorded_under_their_request_uri() {
        let mut cache = DiagnosticCache::default();
        cache.track_pull(7, "file:///a/Bar.java".into());
        let mut response = json!({
            "jsonrpc": "2.0", "id": 7,
            "result": { "kind": "full",
                        "items": published_params("file:///a/Bar.java", "reference.unresolved", 5, "x")["params"]["diagnostics"] }
        });
        assert!(cache.record_pull_response(7, &mut response));
        let mut request = code_action("file:///a/Bar.java", "reference.unresolved", 5, false);
        assert!(cache.reinject(&mut request));
        // The pull request id is consumed, so a second response is ignored.
        assert!(!cache.record_pull_response(7, &mut response));
    }
}
