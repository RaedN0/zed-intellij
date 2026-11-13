//! Side-channel that lets an out-of-band process (`intellij-native dap <worktree>`,
//! the mode Zed spawns as its debug adapter) ask the language server to start its
//! DAP TCP endpoint.
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
//! Launch configs are not resolved here. An earlier version forwarded them to
//! `intellij.java.resolveLaunch` first, mirroring the JetBrains VS Code client;
//! that command does not exist in 271.2085.0 (the only java command ids there are
//! buildTool, codeserver, compiler, debugger, execution, frontback, psi and
//! resolveClassDocument) and the server resolves run steps itself inside the DAP
//! launch handler (`JvmDebuggerLaunchSupport.resolveRunSteps`). The bridge simply
//! forwards `launch` arguments.
//!
//! Injected `executeCommand` requests use ids in a range Zed never produces
//! (>= 1<<40); their responses are consumed here and never forwarded to Zed.

use serde_json::{json, Value};

/// Answer for the bridge. START replies `PORT <n>`; failures reply `ERR <message>`.
enum BridgeReply {
    Port(Value),
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

/// fnv-1a over the root URI. Both ends live in this crate: the proxy mode binds
/// this path after `initialize`, the DAP mode connects to it.
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
                    "control: failed to bind {}: {e} — debugging (`intellij-native dap`) will not \
                     work",
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
        } else {
            BridgeReply::Err("expected START".into())
        };
        match reply {
            BridgeReply::Err(msg) => {
                let _ = writeln!(stream, "ERR {msg}");
            }
            BridgeReply::Port(number) => {
                let _ = writeln!(stream, "PORT {number}");
            }
        };
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
            Some(other) => BridgeReply::Err(format!("start_debug_server returned {other}")),
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
                        params
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or("command"),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Both ends of the control socket are in this crate (the proxy binds it, the
    // `dap` mode connects), so these pin the wire contract itself rather than a
    // mirror copy: a change to the name or the hash still breaks every running
    // session, and `dap::tests` asserts the same literal.

    #[test]
    fn fnv64_matches_the_fnv1a_64_reference_vectors() {
        assert_eq!(fnv64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv64("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn socket_name_for_a_root_uri_is_pinned() {
        // Literal mirrored in dap.rs's tests.
        let name = socket_path_for("file:///repo");
        assert_eq!(
            name.file_name().unwrap().to_str().unwrap(),
            "intellij-dap-df01bf954840a316.sock"
        );
        assert_eq!(name.parent().unwrap(), env::temp_dir());
    }
}
