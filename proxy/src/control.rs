//! Side-channel that lets an out-of-band process (intellij-dap-bridge) ask the
//! language server to start its DAP TCP endpoint.
//!
//! intellij-server (and kotlin-lsp) embed a full DAP server: an LSP
//! `workspace/executeCommand` with the command `start_debug_server` (see
//! `LSStartDebugCommandDescriptorProvider`) makes the server bind a DAP-speaking
//! TCP endpoint on 127.0.0.1 and answer with the port. That request must travel on
//! the SAME LSP connection that owns the project session — which is this proxy's
//! pipe — so the bridge cannot issue it itself.
//!
//! After `initialize` the proxy knows the workspace root and opens a Unix domain
//! socket at `$TMPDIR/intellij-dap-<fnv64(root_uri)>.sock`. Line protocol:
//!   bridge -> `START\n`
//!   proxy  -> `PORT <n>\n` | `ERR <message>\n`
//!
//! Injected `executeCommand` requests use ids in a range Zed never produces
//! (>= 1<<40); their responses are consumed here and never forwarded to Zed.

use serde_json::{json, Value};

/// Answer for the bridge. START replies `PORT <n>` (the bridge's original
/// protocol); RESOLVE replies `OK <json>`; failures reply `ERR <message>`.
enum BridgeReply {
    Port(Value),
    Ok(Value),
    Err(String),
}
use std::{
    collections::HashMap,
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::ChildStdin,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use crate::log::{error as log_error, info as log_info};

/// First id the control channel may use; Zed's own ids are small sequential ints.
pub const CTRL_ID_BASE: i64 = 1 << 40;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// fnv-1a over the root URI — shared formula with intellij-dap-bridge.
pub fn socket_path_for(root_uri: &str) -> PathBuf {
    env::temp_dir().join(format!("intellij-dap-{:016x}.sock", fnv64(root_uri)))
}

pub fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub struct ControlHub {
    next_id: AtomicI64,
    /// id -> sender for the response of that injected request
    pending: Arc<Mutex<HashMap<i64, std::sync::mpsc::Sender<Value>>>>,
    child_stdin: Arc<Mutex<ChildStdin>>,
    root_uri: String,
}

impl ControlHub {
    pub fn new(child_stdin: Arc<Mutex<ChildStdin>>, root_uri: String) -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicI64::new(CTRL_ID_BASE),
            pending: Arc::new(Mutex::new(HashMap::new())),
            child_stdin,
            root_uri,
        })
    }

    /// Bind the socket and serve bridges until the process dies.
    /// Must be called once, after `initialize` revealed the root.
    pub fn listen(self: &Arc<Self>) {
        let path = socket_path_for(&self.root_uri);
        let _ = std::fs::remove_file(&path); // stale socket from a previous run
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                log_error(&format!(
                    "control: failed to bind {}: {e} — intellij-dap-bridge will not work",
                    path.display()
                ));
                return;
            }
        };
        log_info(&format!("control: listening at {}", path.display()));
        let hub = Arc::clone(self);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let hub = Arc::clone(&hub);
                thread::spawn(move || hub.serve(stream));
            }
        });
    }

    fn serve(&self, mut stream: UnixStream) {
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let line = line.trim();
        let reply = if line.eq_ignore_ascii_case("START") {
            self.start_debug_server()
        } else if let Some(config) = line.strip_prefix("RESOLVE ") {
            self.resolve_launch(config)
        } else {
            BridgeReply::Err("expected START or RESOLVE <json>".into())
        };
        match reply {
            BridgeReply::Err(msg) => {
                let _ = writeln!(stream, "ERR {msg}");
            }
            BridgeReply::Ok(value) => {
                let _ = writeln!(stream, "OK {value}");
            }
            BridgeReply::Port(number) => {
                let _ = writeln!(stream, "PORT {number}");
            }
        };
    }

    /// The JetBrains client (dap.ts) always resolves a JVM launch before the DAP
    /// request: mainClass -> source uri (`intellij.java.resolveClassDocument`),
    /// then uri -> runtime paths (`intellij.java.resolveLaunch`) against the
    /// imported project model, merging javaExec/classpath/modulePath/vmArgs/
    /// workingDirectory into the launch config. The server's DAP `launch`
    /// handler requires that pre-work — replying exactly what its VS Code
    /// client would have done.
    fn resolve_launch(&self, config_json: &str) -> BridgeReply {
        let Ok(cfg) = serde_json::from_str::<Value>(config_json) else {
            return BridgeReply::Err("RESOLVE payload is not JSON".into());
        };
        if cfg.get("mainClass").and_then(|v| v.as_str()).is_none() {
            return BridgeReply::Err("launch config has no mainClass to resolve".into());
        }
        log_info("control: resolving JVM launch via project model");

        // 1. If the config carries no `file`, find the declaration's document.
        let uri = match cfg.get("file").and_then(|v| v.as_str()) {
            Some(file) => Value::String(format!("file://{file}")),
            None => {
                let resp = match self.request(json!({
                    "command": "intellij.java.resolveClassDocument",
                    "arguments": [{"fqn": cfg["mainClass"]}],
                })) {
                    Ok(r) => r,
                    Err(e) => return BridgeReply::Err(e),
                };
                // `request` returns the whole response envelope; the payload
                // (the class document lookup) is under "result".
                match resp.get("result").and_then(|r| r.get("uri")).cloned() {
                    Some(uri) => uri,
                    None => return BridgeReply::Err(format!(
                        "resolveClassDocument returned no uri: {}",
                        serde_json::to_string(&resp.get("result")).unwrap_or_default()
                    )),
                }
            }
        };

        // 2. Resolve runtime paths. The wire shape mirrors what the VS Code
        //    client produces: fields are DROPPED when unset, never sent as
        //    JSON `null` — explicit nulls poison the server's Kotlin
        //    deserializer, which answers with an EMPTY payload instead of an
        //    error (verified with harness_resolve_probe.py).
        let mut launch_arg = json!({ "uri": uri });
        if let Some(cwd) = cfg.get("cwd").filter(|v| !v.is_null()) {
            launch_arg["cwd"] = cwd.clone();
        }
        let mut overrides = serde_json::Map::new();
        for key in ["classPaths", "modulePaths", "moduleName", "javaExec", "vmArgs"] {
            if let Some(v) = cfg.get(key).filter(|v| !v.is_null()) {
                overrides.insert(key.to_string(), v.clone());
            }
        }
        if !overrides.is_empty() {
            launch_arg["overrides"] = Value::Object(overrides);
        }
        let resp = match self.request(json!({
            "command": "intellij.java.resolveLaunch",
            "arguments": [launch_arg],
        })) {
            Ok(r) => r,
            Err(e) => return BridgeReply::Err(e),
        };

        // 3. Merge: resolved values fill gaps; config's own win where present.
        //    The payload again lives under "result".
        let resolved = resp.get("result").cloned().unwrap_or(Value::Null);
        let mut merged = cfg;
        let merge = |cfg: &mut Value, key: &str, resolved: Option<&Value>| {
            if let Some(v) = resolved {
                if !v.is_null() && (v.is_string() && v.as_str() != Some("") || !v.is_string()) {
                    cfg[key] = v.clone();
                } else if cfg.get(key).is_none() {
                    cfg[key] = json!([]);
                }
            } else if cfg.get(key).is_none() {
                cfg[key] = json!([]);
            }
        };
        merge(&mut merged, "javaExec", resolved.get("javaExec"));
        merge(&mut merged, "classPaths", resolved.get("classpath"));
        merge(&mut merged, "modulePaths", resolved.get("modulePath"));
        merge(&mut merged, "moduleContentPaths", resolved.get("moduleContentPaths"));
        merge(&mut merged, "vmArgs", resolved.get("vmArgs"));
        if let Some(name) = resolved.get("moduleName") {
            if name.is_string() {
                merged["moduleName"] = name.clone();
            }
        }
        if merged.get("cwd").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
            if let Some(wd) = resolved.get("workingDirectory") {
                merged["cwd"] = wd.clone();
            }
        }
        if merged.get("javaExec").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
            return BridgeReply::Err("resolveLaunch produced no javaExec".into());
        }
        BridgeReply::Ok(merged)
    }

    fn start_debug_server(&self) -> BridgeReply {
        let resp = match self.request(json!({
            "command": "start_debug_server",
            "arguments": [self.root_uri],
        })) {
            Ok(r) => r,
            Err(e) => return BridgeReply::Err(e),
        };
        match resp.get("result") {
            Some(Value::Number(n)) => BridgeReply::Port(Value::Number(n.clone())),
            Some(other) => {
                BridgeReply::Err(format!("start_debug_server returned {other}"))
            }
            None => BridgeReply::Err("start_debug_server returned no result".into()),
        }
    }

    /// Inject one `workspace/executeCommand` and wait for its response.
    fn request(&self, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        self.pending.lock().unwrap().insert(id, tx);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "workspace/executeCommand",
            "params": params,
        });
        if self.write_message(&request).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("could not write to language server".into());
        }

        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(response) => {
                if let Some(err) = response.get("error") {
                    return Err(format!(
                        "{} failed: {}",
                        params.get("command").and_then(|c| c.as_str()).unwrap_or("command"),
                        err
                    ));
                }
                Ok(response)
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err("language server did not answer in time".into())
            }
        }
    }

    fn write_message(&self, msg: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(msg)?;
        let frame = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut w = self.child_stdin.lock().unwrap();
        w.write_all(frame.as_bytes())?;
        w.write_all(&body)?;
        w.flush()
    }

    /// Consume a server->proxy response that belongs to an injected request.
    /// Returns true when the message was one of ours and must not reach Zed.
    pub fn intercept_response(&self, msg: &Value) -> bool {
        let Some(id) = msg.get("id").and_then(|v| v.as_i64()) else {
            return false;
        };
        if id < CTRL_ID_BASE {
            return false;
        }
        let tx = self.pending.lock().unwrap().remove(&id);
        match tx {
            Some(tx) => tx.send(msg.clone()).is_ok(),
            None => true, // already timed out; swallow anyway
        }
    }
}
