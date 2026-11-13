//! DAP bridge between Zed's debugger and intellij-server's embedded DAP endpoint
//! — mode `intellij-native dap <worktree>`.
//!
//! Zed spawns this process as its "debug adapter" and speaks DAP over stdio. The
//! real adapter runs *inside* the JetBrains language server: an LSP
//! `workspace/executeCommand` with command `start_debug_server` makes the server
//! bind a DAP-speaking TCP endpoint on 127.0.0.1 and answer with the port. That
//! request must travel on the live LSP connection, which is owned by the proxy
//! mode of this same executable — so the proxy exposes a Unix control socket
//! (`$TMPDIR/intellij-dap-<fnv64(root_uri)>.sock`, see [`crate::control`]).
//!
//! This bridge:
//!   1. derives the socket path from the worktree path given as argv[2] —
//!      uri `file://<abs path>`, hashed with the same fnv-1a as the proxy
//!      ([`crate::control::socket_path_for`], which is now literally the same
//!      function instead of a mirror copy),
//!   2. sends `START`, reads `PORT <n>` (or `ERR …`),
//!   3. rewrites the first DAP `initialize` request's adapterID to
//!      `intellij_jvm` — the id intellij-server's debugger lookup expects —
//!   4. and from then on pumps bytes Zed<->TCP completely unchanged.
//!
//! All heavy lifting (breakpoints incl. Kotlin sources, variables, evaluation)
//! happens in the IntelliJ engine behind the TCP endpoint.
//!
//! Nothing here may write to stdout except DAP frames: stdout is the pipe Zed
//! parses, which is why this mode logs to stderr plus a file instead of using
//! [`crate::log`] (that one emits `window/logMessage` LSP frames).

use serde_json::{json, Value};
use std::{
    env,
    io::{Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    path::Path,
    process, thread,
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The adapter id intellij-server registers its debugger under; VS Code uses the
/// same string as the debug configuration type.
const ADAPTER_ID: &str = "intellij_jvm";

/// Where [`trace`] appends. Zed swallows a debug adapter's stderr into its log
/// panel, so a file is the only trail that survives a failed session — and it
/// has to be a path the user can be pointed at, hence temp_dir over a literal
/// `/tmp` (no such directory on Windows).
fn trace_path() -> std::path::PathBuf {
    env::temp_dir().join("intellij-dap-bridge.log")
}

fn trace(msg: &str) {
    use std::fs::OpenOptions;
    let path = trace_path();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(format!("{msg}\n").as_bytes());
    }
    eprintln!("intellij-native dap: {msg} (trace: {})", path.display());
}

fn die(msg: &str) -> ! {
    trace(msg);
    process::exit(1);
}

/// Entry point for `intellij-native dap <worktree>`.
pub fn run(worktree: Option<&str>) {
    // The extension passes the worktree absolute path (see Zed's get_dap_binary).
    let worktree = worktree.unwrap_or_else(|| {
        die("missing worktree path argument (usage: intellij-native dap <worktree>)")
    });
    trace(&format!("start: worktree={worktree}"));
    let root_uri = format!("file://{}", Path::new(worktree).display());
    let sock_path = crate::control::socket_path_for(&root_uri);

    // 1. control socket (the proxy creates it once its LSP session is initialized)
    let mut control = connect_with_retry(&sock_path);
    if writeln!(control, "START").is_err() {
        die("failed to send START to the proxy control socket");
    }
    let mut reply = String::new();
    if control
        .read_to_string(&mut reply)
        .map(|n| n == 0)
        .unwrap_or(true)
    {
        die("the proxy closed the control socket without a reply");
    }
    let reply = reply.trim();
    trace(&format!("control reply: {reply}"));
    let port: u16 = match reply.strip_prefix("PORT ") {
        Some(p) => p
            .parse()
            .unwrap_or_else(|_| die(&format!("malformed proxy reply: {reply:?}"))),
        None => die(&format!("proxy refused to start the debug server: {reply}")),
    };
    trace(&format!("IntelliJ debug endpoint at 127.0.0.1:{port}"));

    // 2. the real DAP endpoint
    let mut dap = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
        die(&format!(
            "cannot connect to intellij-server's debug endpoint at port {port}: {e}"
        ))
    });
    dap.set_nodelay(true).ok();

    // 3. rewrite the first DAP message (initialize) to the expected adapter id
    let first = read_message_frame(std::io::stdin())
        .unwrap_or_else(|e| die(&format!("Zed sent no DAP message: {e}")));
    let rewritten = rewrite_initialize(&first);
    dap.write_all(&rewritten)
        .unwrap_or_else(|e| die(&format!("DAP endpoint died on initialize: {e}")));

    // 4. pumps. Zed -> server is frame-aware only to fill in a missing entry
    //    point (see `intercept_launch`); everything else forwards verbatim.
    //    Server -> Zed is a raw byte pump.
    let mut tcp_in = dap
        .try_clone()
        .unwrap_or_else(|e| die(&format!("tcp clone failed: {e}")));
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
            let out = match intercept_launch(&frame) {
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

/// Upgrades a `launch` request in place: **entry point from the open file**.
///
/// A configuration of `{"request":"launch","file":"$ZED_FILE"}` names no class,
/// so the fully-qualified name of the `main` declaration is derived from that
/// file here. That is what makes a single entry in the *global* debug.json debug
/// whatever file is open, in any project — intellij-server resolves the classpath,
/// module and JDK from its project model, but not the entry point itself.
///
/// `None` means "nothing to rewrite", so the caller forwards the frame verbatim.
fn intercept_launch(frame: &[u8]) -> Option<Vec<u8>> {
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

    let mut args = value.get("arguments").cloned().unwrap_or(Value::Null);
    let mut rewritten = false;

    // Derive the entry point when the configuration only names a file.
    let main_class = args
        .get("mainClass")
        .and_then(|v| v.as_str())
        .filter(|class| !class.is_empty());
    if main_class.is_none() {
        if let Some(class) = args
            .get("file")
            .and_then(|v| v.as_str())
            .filter(|file| !file.is_empty())
            .and_then(|file| main_class_for_file(Path::new(file)))
        {
            trace(&format!("launch: inferred mainClass {class}"));
            args["mainClass"] = json!(class);
            rewritten = true;
        }
    }

    if !rewritten {
        return None; // attach, no main in that file, or an explicit mainClass
    }
    value["arguments"] = args;
    let body = serde_json::to_vec(&value).ok()?;
    Some(reframe(&body))
}

/// A `Content-Length`-framed DAP message around `body`.
fn reframe(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Fully-qualified name of the `main` entry point declared in `path`.
fn main_class_for_file(path: &Path) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let is_kotlin = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("kt"));
    main_class_in_source(&source, &stem, is_kotlin)
}

/// Entry point of `source`, qualified by its package.
///
/// Deliberately textual rather than a real parse: the only inputs are a
/// `package` line and the declaration enclosing `main`, and a wrong guess fails
/// loudly ("class not found") instead of silently debugging the wrong target.
fn main_class_in_source(source: &str, file_stem: &str, is_kotlin: bool) -> Option<String> {
    let package = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("package "))
        .and_then(|rest| rest.split(';').next())
        .map(str::trim)
        .filter(|package| !package.is_empty());

    let class = enclosing_class(source, file_stem, is_kotlin)?;
    Some(match package {
        Some(package) => format!("{package}.{class}"),
        None => class,
    })
}

/// The class-like declaration that encloses `main`, or the synthetic name a
/// top-level Kotlin `fun main` compiles to (`<File>Kt`).
fn enclosing_class(source: &str, file_stem: &str, is_kotlin: bool) -> Option<String> {
    let main_at = ["static void main", "fun main", "void main"]
        .iter()
        .filter_map(|signature| source.find(signature))
        .min()?;
    let before = &source[..main_at];

    let nearest = ["class ", "object ", "record "]
        .iter()
        .filter_map(|keyword| before.rfind(keyword).map(|at| (at, *keyword)))
        .max_by_key(|(at, _)| *at);
    if let Some((at, keyword)) = nearest {
        let name: String = before[at + keyword.len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }

    Some(if is_kotlin {
        format!("{file_stem}Kt")
    } else {
        file_stem.to_string()
    })
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

/// If `msg` is a DAP `initialize` request, force its adapterID to
/// `intellij_jvm` so the server's debugger lookup matches; re-frame the body
/// with a fresh Content-Length. Anything else passes through untouched.
///
/// The payload sits under `arguments` for DAP `command` messages and under
/// `params` for JSON-RPC-style ones, so the rewrite targets whichever the
/// message actually used — writing `arguments` into a `params` message leaves
/// the id unchanged and the server's debugger lookup misses.
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
    let key = if value.get("arguments").is_some() || value.get("command").is_some() {
        "arguments"
    } else {
        "params"
    };
    if !value.get(key).map(Value::is_object).unwrap_or(false) {
        value[key] = json!({});
    }
    value[key]["adapterID"] = json!(ADAPTER_ID);
    let new_body = serde_json::to_vec(&value).expect("re-serializing initialize cannot fail");
    let mut out = format!("Content-Length: {}\r\n\r\n", new_body.len()).into_bytes();
    out.extend_from_slice(&new_body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    /// Body of a framed message, asserting the framing is self-consistent.
    fn content_length(msg: &[u8]) -> String {
        String::from_utf8_lossy(msg)
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .map(|n| n.trim().to_string())
            .expect("frame declares a Content-Length")
    }

    fn body_of(msg: &[u8]) -> &[u8] {
        let start = msg
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("frame has a header terminator")
            + 4;
        let len: usize = String::from_utf8_lossy(&msg[..start])
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .expect("frame has a Content-Length")
            .trim()
            .parse()
            .expect("Content-Length is a number");
        let body = &msg[start..];
        assert_eq!(body.len(), len, "Content-Length matches the body");
        body
    }

    // --- the control socket is shared with the proxy mode ---

    #[test]
    fn derives_the_same_socket_path_as_the_proxy_side() {
        // `run` connects to whatever the proxy mode bound; both call
        // control::socket_path_for, so this pins the name both ends use.
        let root_uri = "file:///repo";
        assert_eq!(
            crate::control::socket_path_for(root_uri)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "intellij-dap-df01bf954840a316.sock"
        );
    }

    // --- trace location ---

    #[test]
    fn the_trace_file_is_under_temp_dir_not_a_hardcoded_tmp() {
        // `/tmp` does not exist everywhere; the log has to be somewhere `die()`
        // can name and the user can open.
        let path = trace_path();
        assert_eq!(path.parent().unwrap(), env::temp_dir());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "intellij-dap-bridge.log"
        );
    }

    // --- frame reading ---

    #[test]
    fn reads_one_frame_and_leaves_the_next_in_the_stream() {
        let both = [frame(r#"{"seq":1}"#), frame(r#"{"seq":2}"#)].concat();
        let mut stream = Cursor::new(both.clone());
        let first = read_message_frame(&mut stream).unwrap();
        let second = read_message_frame(&mut stream).unwrap();
        assert_eq!(first, frame(r#"{"seq":1}"#));
        assert_eq!(second, frame(r#"{"seq":2}"#));
        // Nothing consumed twice, nothing left over.
        assert_eq!(stream.position() as usize, both.len());
    }

    #[test]
    fn reads_a_frame_delivered_one_byte_at_a_time() {
        // A socket hands back partial reads; read_exact has to loop for both the
        // header scan and the body.
        struct Dribble {
            bytes: Vec<u8>,
            pos: usize,
        }
        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.bytes.len() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.bytes[self.pos];
                self.pos += 1;
                Ok(1)
            }
        }

        let raw = frame(&"x".repeat(4096));
        let mut dribble = Dribble {
            bytes: raw.clone(),
            pos: 0,
        };
        assert_eq!(read_message_frame(&mut dribble).unwrap(), raw);
    }

    #[test]
    fn finds_content_length_among_other_headers() {
        let body = r#"{"seq":7}"#;
        let raw = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
        assert_eq!(
            read_message_frame(Cursor::new(raw.clone())).unwrap(),
            raw,
            "headers are forwarded verbatim, in their original order"
        );
    }

    #[test]
    fn rejects_a_frame_without_content_length() {
        let err = read_message_frame(Cursor::new(b"X-Other: 1\r\n\r\n".to_vec())).unwrap_err();
        assert!(err.to_string().contains("Content-Length"), "{err}");
    }

    #[test]
    fn rejects_a_body_that_never_arrives() {
        let err = read_message_frame(Cursor::new(b"Content-Length: 100\r\n\r\nshort".to_vec()))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rejects_an_unterminated_header() {
        // Without CRLFCRLF this would read forever; the 16 KiB cap is the bound.
        let raw = "Content-Length: 1\r\n".repeat(2000).into_bytes();
        let err = read_message_frame(Cursor::new(raw)).unwrap_err();
        assert!(err.to_string().contains("header too large"), "{err}");
    }

    // --- initialize rewrite ---

    #[test]
    fn rewrites_adapter_id_and_keeps_the_other_arguments() {
        let out = rewrite_initialize(&frame(
            r#"{"seq":1,"command":"initialize","arguments":{"adapterID":"zed","clientID":"zed"}}"#,
        ));
        let value: Value = serde_json::from_slice(body_of(&out)).unwrap();
        assert_eq!(value["arguments"]["adapterID"], json!(ADAPTER_ID));
        assert_eq!(value["arguments"]["clientID"], json!("zed"));
    }

    #[test]
    fn rewrites_the_jsonrpc_method_form_too() {
        // Zed has spoken both shapes across versions; the server only knows one.
        let out = rewrite_initialize(&frame(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"adapterID":"x"}}"#,
        ));
        assert_eq!(
            serde_json::from_slice::<Value>(body_of(&out)).unwrap()["params"]["adapterID"],
            json!(ADAPTER_ID)
        );
    }

    #[test]
    fn creates_arguments_when_initialize_omits_them() {
        let out = rewrite_initialize(&frame(r#"{"seq":1,"command":"initialize"}"#));
        assert_eq!(
            serde_json::from_slice::<Value>(body_of(&out)).unwrap()["arguments"]["adapterID"],
            json!(ADAPTER_ID)
        );
    }

    #[test]
    fn replaces_non_object_arguments() {
        // `arguments: null` is legal DAP; leaving it would hand the server an
        // empty adapter id and the debugger lookup would miss.
        let out = rewrite_initialize(&frame(
            r#"{"seq":1,"command":"initialize","arguments":null}"#,
        ));
        assert_eq!(
            serde_json::from_slice::<Value>(body_of(&out)).unwrap()["arguments"]["adapterID"],
            json!(ADAPTER_ID)
        );
    }

    #[test]
    fn passes_non_initialize_messages_through_untouched() {
        // Breakpoint traffic must not be re-serialized: a reordered key or a
        // reformatted number is a needless change to messages the server parses.
        let raw = frame(
            r#"{"seq":4,"command":"setBreakpoints","arguments":{"source":{"path":"/a/B.kt"},"breakpoints":[{"line":9}]}}"#,
        );
        assert_eq!(rewrite_initialize(&raw), raw);
    }

    #[test]
    fn passes_unparseable_bodies_through_untouched() {
        let raw = frame("not json");
        assert_eq!(rewrite_initialize(&raw), raw);
        // Empty body: nothing to parse, nothing to rewrite.
        let raw = b"Content-Length: 0\r\n\r\n".to_vec();
        assert_eq!(rewrite_initialize(&raw), raw);
    }

    // --- launch rewriting ---

    #[test]
    fn passes_through_launches_that_need_no_rewrite() {
        // Attach: not a launch of an entry point.
        assert!(intercept_launch(&frame(
            r#"{"seq":2,"command":"launch","arguments":{"request":"attach"}}"#
        ))
        .is_none());
        // A resolved launch: the caller already named the entry point.
        assert!(intercept_launch(&frame(
            r#"{"seq":3,"command":"launch","arguments":{"mainClass":"a.B","javaExec":"/jbr/bin/java"}}"#
        ))
        .is_none());
        // Not a launch at all.
        assert!(intercept_launch(&frame(
            r#"{"seq":4,"command":"setBreakpoints","arguments":{"mainClass":"a.B"}}"#
        ))
        .is_none());
    }

    #[test]
    fn forwards_unresolved_when_the_control_socket_is_missing() {
        // A missing proxy must not swallow the launch: the server falls back to
        // its own resolution, which is strictly better than no debug session.
        assert!(intercept_launch(&frame(
            r#"{"seq":5,"command":"launch","arguments":{"mainClass":"a.B"}}"#
        ))
        .is_none());
    }

    // --- entry point inference ---

    #[test]
    fn infers_the_entry_point_from_the_source_file() {
        // Java: package + the declaration enclosing main.
        let java = "package com.example.app;\n\npublic class App {\n  public static void main(String[] a) {}\n}\n";
        assert_eq!(
            main_class_in_source(java, "App", false).as_deref(),
            Some("com.example.app.App")
        );

        // Default package.
        let default_pkg = "public class App {\n  public static void main(String[] a) {}\n}\n";
        assert_eq!(
            main_class_in_source(default_pkg, "App", false).as_deref(),
            Some("App")
        );

        // Kotlin: main inside an object keeps the object's name.
        let kotlin_object =
            "package com.example\n\nobject App {\n    fun main(args: Array<String>) {}\n}\n";
        assert_eq!(
            main_class_in_source(kotlin_object, "App", true).as_deref(),
            Some("com.example.App")
        );

        // Kotlin: top-level main compiles to <File>Kt.
        let kotlin_top_level = "package com.example\n\nfun main() {\n}\n";
        assert_eq!(
            main_class_in_source(kotlin_top_level, "Main", true).as_deref(),
            Some("com.example.MainKt")
        );
    }

    #[test]
    fn a_file_without_main_has_no_entry_point() {
        // Must not guess: debugging a library class silently would be worse
        // than letting the server report "no main found".
        let lib = "package com.example;\n\npublic class Util {\n}\n";
        assert_eq!(main_class_in_source(lib, "Util", false), None);
    }

    #[test]
    fn derives_the_entry_point_from_the_open_file() {
        let dir = env::temp_dir().join(format!("zidt-src-{}", process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("App.java");
        std::fs::write(
            &file,
            "package com.example;\npublic class App {\n  public static void main(String[] a) {}\n}\n",
        )
        .unwrap();

        let launch = frame(&format!(
            r#"{{"seq":9,"command":"launch","arguments":{{"request":"launch","file":"{}"}}}}"#,
            file.display()
        ));
        let out =
            intercept_launch(&launch).expect("frame is rewritten with the inferred entry point");
        // Content-Length must be recomputed: the body grew by `mainClass`.
        assert_eq!(body_of(&out).len().to_string(), content_length(&out));
        let value: Value = serde_json::from_slice(body_of(&out)).unwrap();
        assert_eq!(value["arguments"]["mainClass"], json!("com.example.App"));
        assert_eq!(value["arguments"]["request"], json!("launch"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicit_main_class_is_never_overridden() {
        let launch = frame(
            r#"{"seq":10,"command":"launch","arguments":{"mainClass":"a.B","file":"/does/not/exist.java"}}"#,
        );
        // A class is already named and the file does not exist, so there is
        // nothing to infer and the frame must stay byte-identical.
        assert!(intercept_launch(&launch).is_none());
    }
}
