//! DAP bridge between Zed's debugger and intellij-server's embedded DAP endpoint.
//!
//! Zed spawns this process as its "debug adapter" and speaks DAP over stdio. The
//! real adapter runs *inside* the JetBrains language server: an LSP
//! `workspace/executeCommand` with command `start_debug_server` makes the server
//! bind a DAP-speaking TCP endpoint on 127.0.0.1 and answer with the port. That
//! request must travel on the live LSP connection, which is owned by
//! kotlin-lsp-proxy — so the proxy exposes a Unix control socket
//! (`$TMPDIR/intellij-dap-<fnv64(root_uri)>.sock`, see proxy/src/control.rs).
//!
//! This bridge:
//!   1. derives the socket path from the worktree path given as argv[1] —
//!      uri `file://<abs path>`, hashed with the same fnv-1a as the proxy,
//!   2. sends `START`, reads `PORT <n>` (or `ERR …`),
//!   3. rewrites the first DAP `initialize` request's adapterID to
//!      `intellij_jvm` — the id intellij-server's debugger lookup expects —
//!   4. and from then on pumps bytes Zed<->TCP completely unchanged.
//!
//! All heavy lifting (breakpoints incl. Kotlin sources, variables, evaluation)
//! happens in the IntelliJ engine behind the TCP endpoint.

use serde_json::{json, Value};
use std::{
    env,
    io::{Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    path::Path,
    process,
    thread,
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The adapter id intellij-server registers its debugger under; VS Code uses the
/// same string as the debug configuration type.
const ADAPTER_ID: &str = "intellij_jvm";

/// Same fnv-1a as proxy/src/control.rs.
fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn trace(msg: &str) {
    // stderr is swallowed when Zed spawns us; keep an append-only trail in /tmp
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/intellij-dap-bridge.log")
    {
        let _ = f.write_all(format!("{msg}\n").as_bytes());
    }
    eprintln!("intellij-dap-bridge: {msg}");
}

fn die(msg: &str) -> ! {
    trace(msg);
    process::exit(1);
}

fn main() {
    // argv[1] = worktree absolute path (from zed-intellij's debug_adapter_binary_for)
    let worktree = env::args()
        .nth(1)
        .unwrap_or_else(|| die("missing worktree path argument"));
    trace(&format!("start: worktree={worktree}"));
    let root_uri = format!("file://{}", Path::new(&worktree).display());
    let sock_path = env::temp_dir().join(format!("intellij-dap-{:016x}.sock", fnv64(&root_uri)));

    // 1. control socket (the proxy creates it once its LSP session is initialized)
    let mut control = connect_with_retry(&sock_path);
    if writeln!(control, "START").is_err() {
        die("failed to send START to the proxy control socket");
    }
    let mut reply = String::new();
    if control.read_to_string(&mut reply).map(|n| n == 0).unwrap_or(true) {
        die("the proxy closed the control socket without a reply");
    }
    let reply = reply.trim();
    trace(&format!("control reply: {reply}"));
    let port: u16 = match reply.strip_prefix("PORT ") {
        Some(p) => p.parse().unwrap_or_else(|_| die(&format!("malformed proxy reply: {reply:?}"))),
        None => die(&format!("proxy refused to start the debug server: {reply}")),
    };
    trace(&format!("IntelliJ debug endpoint at 127.0.0.1:{port}"));

    // 2. the real DAP endpoint
    let mut dap = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| die(&format!(
        "cannot connect to intellij-server's debug endpoint at port {port}: {e}"
    )));
    dap.set_nodelay(true).ok();

    // 3. rewrite the first DAP message (initialize) to the expected adapter id
    let first = read_message(std::io::stdin()).unwrap_or_else(|e| die(&format!("Zed sent no DAP message: {e}")));
    let rewritten = rewrite_initialize(&first);
    dap.write_all(&rewritten).unwrap_or_else(|e| die(&format!("DAP endpoint died on initialize: {e}")));

    // 4. pumps. Zed -> server is frame-aware: DAP `launch` requests that lack
    //    `javaExec` (Zed/the server never resolve those themselves) get resolved
    //    via the proxy control socket first — replicating what JetBrains' own
    //    VS Code client does (`intellij.java.resolveClassDocument` +
    //    `.resolveLaunch`) before sending the launch. Everything else forwards
    //    verbatim; server -> Zed is a raw byte pump.
    let mut tcp_in = dap.try_clone().unwrap_or_else(|e| die(&format!("tcp clone failed: {e}")));
    let pump_sock = sock_path.clone();
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        loop {
            let frame = match read_message_frame(&mut stdin) {
                Ok(f) => f,
                Err(_) => {
                    let _ = tcp_in.shutdown(std::net::Shutdown::Both);
                    return;
                }
            };
            let out = match intercept_launch(&frame, &pump_sock) {
                Some(resolved) => resolved,
                None => frame,
            };
            if tcp_in.write_all(&out).is_err() || tcp_in.flush().is_err() {
                return;
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 32 * 1024];
    loop {
        match dap.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                    return;
                }
            }
        }
    }
}

fn connect_with_retry(sock_path: &Path) -> UnixStream {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match UnixStream::connect(sock_path) {
            Ok(s) => return s,
            Err(e) => {
                if Instant::now() >= deadline {
                    die(&format!(
                        "no proxy control socket at {} (is the intellij-server language server \
                         running? original error: {e})",
                        sock_path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Structured result of resolving a launch via the proxy.
fn intercept_launch(frame: &[u8], sock_path: &Path) -> Option<Vec<u8>> {
    let header_end = frame
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(frame.len());
    let Ok(mut value) = serde_json::from_slice::<Value>(&frame[header_end..]) else {
        return None;
    };
    if value.get("command").and_then(|v| v.as_str()) != Some("launch") {
        return None;
    }
    let args = value.get("arguments").cloned().unwrap_or(Value::Null);
    let has_main = args.get("mainClass").and_then(|v| v.as_str()).is_some();
    let has_exec = args
        .get("javaExec")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_main || has_exec {
        return None; // attach, pre-resolved, or not a JVM launch: pass through
    }

    trace("launch: resolving via project model");
    let mut control = UnixStream::connect(sock_path).ok()?;
    let body = serde_json::to_vec(&args).ok()?;
    control
        .write_all(format!("RESOLVE ", ).as_bytes())
        .and_then(|_| control.write_all(&body))
        .and_then(|_| control.write_all(b"\n"))
        .and_then(|_| control.flush())
        .ok()?;
    let mut reply = String::new();
    use std::io::BufRead;
    std::io::BufReader::new(control).read_line(&mut reply).ok()?;
    let reply = reply.trim();
    if let Some(err) = reply.strip_prefix("ERR ") {
        trace(&format!("resolve FAILED: {err} (forwarding unresolved)"));
        return None;
    }
    let merged = reply.strip_prefix("OK ")?;
    let Ok(merged) = serde_json::from_str::<Value>(merged) else {
        return None;
    };
    value["arguments"] = merged;
    trace("launch resolved, forwarding");
    let new_body = serde_json::to_vec(&value).ok()?;
    let mut out = format!("Content-Length: {}\r\n\r\n", new_body.len()).into_bytes();
    out.extend_from_slice(&new_body);
    Some(out)
}

/// Read one complete framed DAP message (headers + body) from a reader.
fn read_message_frame(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut headers: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    loop {
        r.read_exact(&mut one)?;
        headers.push(one[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 16 * 1024 {
            return Err(std::io::Error::other("header too large"));
        }
    }
    let len = headers
        .split(|&b| b == b'\n')
        .find_map(|line| {
            let line = String::from_utf8_lossy(line);
            let line = line.trim_end_matches('\r');
            line.strip_prefix("Content-Length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .ok_or_else(|| std::io::Error::other("missing Content-Length header"))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let mut out = headers.clone();
    out.extend_from_slice(&body);
    Ok(out)
}

/// Read one complete DAP/JSON-RPC message (headers + body) from a reader.
fn read_message(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut headers: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    // read until \r\n\r\n
    loop {
        r.read_exact(&mut one)?;
        headers.push(one[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 16 * 1024 {
            return Err(std::io::Error::other("header too large"));
        }
    }
    let len = headers
        .split(|&b| b == b'\n')
        .find_map(|line| {
            let line = String::from_utf8_lossy(line);
            let line = line.trim_end_matches('\r');
            line.strip_prefix("Content-Length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .ok_or_else(|| std::io::Error::other("missing Content-Length header"))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let mut out = headers.clone();
    out.extend_from_slice(&body);
    Ok(out)
}

/// If `msg` is a DAP `initialize` request, force its adapterID to
/// `intellij_jvm` so the server's debugger lookup matches; re-frame the body
/// with a fresh Content-Length. Anything else passes through untouched.
fn rewrite_initialize(msg: &[u8]) -> Vec<u8> {
    let header_end = msg
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(msg.len());
    let body = &msg[header_end..];
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return msg.to_vec();
    };
    let is_initialize = value.get("command").and_then(|c| c.as_str()) == Some("initialize")
        || value.get("method").and_then(|m| m.as_str()) == Some("initialize");
    if !is_initialize {
        return msg.to_vec();
    }
    if !value.get("arguments").map(Value::is_object).unwrap_or(false) {
        value["arguments"] = json!({});
    }
    value["arguments"]["adapterID"] = json!(ADAPTER_ID);
    let new_body = serde_json::to_vec(&value).expect("re-serializing initialize cannot fail");
    let mut out = format!("Content-Length: {}\r\n\r\n", new_body.len()).into_bytes();
    out.extend_from_slice(&new_body);
    out
}
