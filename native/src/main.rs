//! intellij-native — the extension's single native helper. Two modes, one
//! executable:
//!
//! ```text
//! intellij-native <server-bin> [args...]   # LSP proxy mode (stdin/stdout pipe)
//! intellij-native dap <worktree>           # DAP bridge mode (Zed's debug adapter)
//! ```
//!
//! They are the same file but never the same process: [`dap`] is spawned by Zed
//! with the worktree as cwd and asks the *proxy* process (which owns the live
//! LSP connection) over a Unix control socket to start the server-side debug
//! endpoint — see [`control`].
//!
//! The modes must not share a log sink: proxy mode owns stdout as an LSP pipe
//! (`lsp_info!` emits `window/logMessage` frames), while DAP mode owns stdout as
//! a DAP pipe, so [`dap`] logs to stderr and a file instead.
//!
//! ## Proxy mode
//!
//! intellij-server returns navigation targets that live inside source archives
//! (`…/foo-sources.jar!/pkg/Foo.kt`, `…/lib/src.zip!/…/Instant.java`). Zed cannot open
//! those archive-internal paths, so "Go to Definition" into any library/JDK class shows
//! an empty buffer (zed-extensions/kotlin#106).
//!
//! This proxy sits between Zed and intellij-server, forwards every message untouched, and
//! only rewrites the location URIs in definition-family responses: it extracts the
//! referenced entry from the jar/zip to a real temp file and points Zed at that instead.
//! It also repairs outgoing `textDocument/codeAction` requests (see [`diagnostics`]).

#[macro_use]
mod log;
mod control;
/// Debug adapter mode. Unix-socket only (see the module docs), so Windows builds
/// keep the proxy mode and answer `dap` with an explicit error.
#[cfg(unix)]
mod dap;
mod diagnostics;
mod lsp;
mod platform;
mod resolve;

use diagnostics::DiagnosticCache;
use lsp::{
    parse_lsp_content, parse_lsp_id, raw_body_contains, raw_has_id, write_raw, write_to_stdout,
    LspReader,
};
use platform::spawn_parent_monitor;
use resolve::rewrite_archive_locations;
use serde_json::Value;
use std::{
    collections::HashSet,
    env,
    io::{self, BufReader, Write},
    process::{self, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    // `dap` is the only reserved first argument, and the placeholder captures
    // it so `run_proxy` sees exactly what it saw before the merge. Every other
    // invocation is proxy mode, which keeps `intellij-native <server> …`
    // compatible with the old `kotlin-lsp-proxy <server> …`.
    match args.first().map(String::as_str) {
        #[cfg(unix)]
        Some("dap") => dap::run(args.get(1).map(String::as_str)),
        #[cfg(not(unix))]
        Some("dap") => {
            eprintln!(
                "intellij-native: debug adapter mode is not supported on Windows (the control \
                 channel and the DAP bridge speak over Unix sockets)"
            );
            process::exit(1);
        }
        _ => run_proxy(&args),
    }
}

/// LSP proxy mode: `intellij-native <server-bin> [args...]`.
fn run_proxy(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: intellij-native <bin> [args...]");
        eprintln!("       intellij-native dap <worktree>");
        process::exit(1);
    }

    let bin = &args[0];
    let child_args = &args[1..];

    lsp_info!("intellij-native (proxy) starting: bin={bin}");

    let mut child = Command::new(bin)
        .args(child_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("Failed to spawn {bin}: {e}");
            lsp_error!("Failed to spawn {bin}: {e}");
            process::exit(1);
        });

    lsp_info!("kotlin-lsp process spawned (pid={})", child.id());

    let child_stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
    let child_stdout = child.stdout.take().unwrap();
    let alive = Arc::new(AtomicBool::new(true));

    // Request ids whose responses may carry archive (`jar!/`) location URIs.
    let tracked: Arc<Mutex<HashSet<Value>>> = Arc::new(Mutex::new(HashSet::new()));

    // Control channel for the DAP bridge; created once `initialize` reveals the root.
    let hub_slot: Arc<Mutex<Option<Arc<control::ControlHub>>>> = Arc::new(Mutex::new(None));

    // Diagnostics the server published, replayed onto outgoing code-action
    // requests so the server can offer its quickfixes (see `diagnostics`); also
    // remembers in-flight `textDocument/diagnostic` pulls.
    let cache = Arc::new(Mutex::new(DiagnosticCache::default()));

    // --- Thread 1: Zed stdin -> kotlin-lsp stdin (record definition-family ids) ---
    let stdin_writer = Arc::clone(&child_stdin);
    let alive_in = Arc::clone(&alive);
    let tracked_in = Arc::clone(&tracked);
    let hub_slot_in = Arc::clone(&hub_slot);
    let cache_in = Arc::clone(&cache);
    thread::spawn(move || {
        let stdin = io::stdin().lock();
        let mut reader = LspReader::new(BufReader::new(stdin));
        while alive_in.load(Ordering::Relaxed) {
            match reader.read_message() {
                Ok(Some(mut raw)) => {
                    // Only requests carry an `id`; skip parsing high-volume notifications.
                    if raw_has_id(&raw) {
                        if let Some(mut msg) = parse_lsp_content(&raw) {
                            if is_location_request(&msg) {
                                if let Some(id) = msg.get("id").cloned() {
                                    tracked_in.lock().unwrap().insert(id);
                                }
                            }
                            // Repair the diagnostics a `textDocument/codeAction`
                            // request carries: add back any the client left out
                            // (Zed drops those its strict range test excludes), then
                            // fill in a missing `data` on what is now present. Both
                            // steps are additive and no-ops when nothing is missing,
                            // so a correct request is forwarded untouched.
                            if msg.get("method").and_then(Value::as_str)
                                == Some("textDocument/codeAction")
                            {
                                let cache = cache_in.lock().unwrap();
                                let supplemented = cache.supplement(&mut msg);
                                let reinjected = cache.reinject(&mut msg);
                                drop(cache);
                                if supplemented || reinjected {
                                    raw = lsp::encode_lsp(&msg).into_bytes();
                                }
                            }
                            // Correlate pull-diagnostic requests with their responses,
                            // so the diagnostics can be cached the same way push ones are.
                            if msg.get("method").and_then(|m| m.as_str())
                                == Some("textDocument/diagnostic")
                            {
                                if let (Some(id), Some(uri)) = (
                                    msg.get("id").and_then(Value::as_i64),
                                    msg.pointer("/params/textDocument/uri")
                                        .and_then(Value::as_str),
                                ) {
                                    cache_in.lock().unwrap().track_pull(id, uri.to_owned());
                                }
                            }
                            if msg.get("method").and_then(|m| m.as_str()) == Some("initialize")
                                && hub_slot_in.lock().unwrap().is_none()
                            {
                                let p = &msg["params"];
                                let root_uri = p
                                    .get("rootUri")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned)
                                    .or_else(|| {
                                        p.pointer("/workspaceFolders/0/uri")
                                            .and_then(|v| v.as_str())
                                            .map(str::to_owned)
                                    })
                                    .or_else(|| {
                                        p.get("rootPath")
                                            .and_then(|v| v.as_str())
                                            .map(|path| format!("file://{path}"))
                                    });
                                if let Some(root_uri) = root_uri {
                                    let hub = control::ControlHub::new(
                                        Arc::clone(&stdin_writer),
                                        root_uri,
                                    );
                                    hub.listen();
                                    hub_slot_in.lock().unwrap().replace(hub);
                                    lsp_info!("control channel up for workspace");
                                } else {
                                    lsp_error!(
                                        "initialize had no root to hang the control channel from"
                                    );
                                }
                            }
                        }
                    }
                    let mut w = stdin_writer.lock().unwrap();
                    if w.write_all(&raw).is_err() || w.flush().is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        alive_in.store(false, Ordering::Relaxed);
    });

    // --- Thread 2: server stdout -> rewrite archive URIs -> Zed stdout ---
    let alive_out = Arc::clone(&alive);
    let tracked_out = Arc::clone(&tracked);
    let hub_slot_out = Arc::clone(&hub_slot);
    let cache_out = Arc::clone(&cache);
    thread::spawn(move || {
        let mut reader = LspReader::new(BufReader::new(child_stdout));
        while alive_out.load(Ordering::Relaxed) {
            match reader.read_message() {
                Ok(Some(raw)) => {
                    // A notification has no response id, but publishDiagnostics
                    // carries the diagnostics (and their `data`) that code actions
                    // need later; a cheap body scan avoids parsing the high-volume
                    // ones (telemetry, import logs, logMessage).
                    if !raw_has_id(&raw) {
                        if raw_body_contains(&raw, b"Diagnostics") {
                            if let Some(mut msg) = parse_lsp_content(&raw).filter(|msg| {
                                msg.get("method").and_then(Value::as_str)
                                    == Some("textDocument/publishDiagnostics")
                            }) {
                                cache_out.lock().unwrap().record_published(&mut msg);
                            }
                        }
                        write_raw(&mut io::stdout().lock(), &raw);
                        continue;
                    }

                    // The id alone decides what to do with a response; only
                    // parse the (potentially huge) body in the rare cases that
                    // actually need it, so completion payloads are forwarded
                    // byte-for-byte.
                    let Some(id) = parse_lsp_id(&raw) else {
                        write_raw(&mut io::stdout().lock(), &raw);
                        continue;
                    };

                    // Responses to client pull-diagnostic requests carry the same
                    // diagnostics as push notifications; remember them too. The id
                    // is checked first so completion payloads are never parsed.
                    if let Some(request_id) = id.as_i64() {
                        if cache_out.lock().unwrap().is_tracked_pull(request_id) {
                            if let Some(mut msg) = parse_lsp_content(&raw) {
                                cache_out
                                    .lock()
                                    .unwrap()
                                    .record_pull_response(request_id, &mut msg);
                            }
                            write_raw(&mut io::stdout().lock(), &raw);
                            continue;
                        }
                    }

                    // Responses to control-channel requests never reach Zed.
                    if id.as_i64().is_some_and(|n| n >= control::CTRL_ID_BASE) {
                        if let Some(hub) = hub_slot_out.lock().unwrap().as_ref() {
                            if let Some(msg) = parse_lsp_content(&raw) {
                                if hub.intercept_response(&msg) {
                                    continue;
                                }
                            }
                        }
                        write_raw(&mut io::stdout().lock(), &raw);
                        continue;
                    }

                    // Only definition-family responses may carry archive
                    // (`jar!/`) location URIs; everything else is passed through.
                    if !tracked_out.lock().unwrap().remove(&id) {
                        write_raw(&mut io::stdout().lock(), &raw);
                        continue;
                    }

                    let Some(mut msg) = parse_lsp_content(&raw) else {
                        write_raw(&mut io::stdout().lock(), &raw);
                        continue;
                    };
                    if rewrite_archive_locations(&mut msg) {
                        write_to_stdout(&msg);
                    } else {
                        write_raw(&mut io::stdout().lock(), &raw);
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        alive_out.store(false, Ordering::Relaxed);
    });

    // --- Thread 3: terminate kotlin-lsp if Zed dies ---
    spawn_parent_monitor(Arc::clone(&alive), child.id());

    let status = child.wait();
    lsp_info!("intellij-server exited: {status:?}");
    alive.store(false, Ordering::Relaxed);
}

fn is_location_request(msg: &Value) -> bool {
    matches!(
        msg.get("method").and_then(|m| m.as_str()),
        Some(
            "textDocument/definition"
                | "textDocument/typeDefinition"
                | "textDocument/implementation"
                | "textDocument/declaration"
                | "textDocument/references"
        )
    )
}
