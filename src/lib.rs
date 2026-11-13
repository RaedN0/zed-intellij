use zed::serde_json;
use zed::LanguageServerId;
use zed_extension_api::{self as zed, settings::LspSettings, Result};

mod language_servers;

use language_servers::{util, IntelliJServer, Proxy};

/// Resolve the JDK home for this worktree, in priority order:
///
/// 1. `lsp.intellij-server.settings.java_home` — per-project override,
/// 2. `$JAVA_HOME` from the worktree's shell environment (set by login shells,
///    sdkman, IDE wrappers, …),
/// 3. the JDK the server distribution ships (`jbr/`, present since
///    271.2085.0), so a machine without `JAVA_HOME` still starts.
///
/// For sources 1 and 2 the extension sandbox can't stat absolute paths (WASI
/// preopens only the work dir), so existence is not verified here — the
/// launcher resolves the path in the real filesystem and reports errors itself.
/// Source 3 is inside the work dir and therefore checked.
///
/// Errors when nothing is configured, rather than silently letting the server's
/// Maven import forks fall back to `/usr/bin/java`.
fn resolve_java_home(
    configuration: &Option<serde_json::Value>,
    worktree: &zed::Worktree,
    bundled: Option<String>,
) -> Result<String> {
    if let Some(home) = configuration
        .as_ref()
        .and_then(|settings| settings.pointer("/java_home"))
        .and_then(|value| value.as_str())
        .filter(|home| !home.is_empty())
    {
        return Ok(home.to_string());
    }
    if let Some((_, home)) = worktree
        .shell_env()
        .into_iter()
        .find(|(key, _)| key == "JAVA_HOME")
        .filter(|(_, home)| !home.is_empty())
    {
        return Ok(home);
    }
    if let Some(home) = bundled {
        return Ok(home);
    }
    Err(
        "JAVA_HOME must be configured via lsp.intellij-server.settings.java_home, the \
         environment, or the server distribution's bundled jbr"
            .to_string(),
    )
}

/// Shown when the user has not accepted the JetBrains EULA.
///
/// The server distribution is proprietary JetBrains software: it must not be
/// downloaded (it is ~370 MB) or executed before the user has read and accepted
/// the JetBrains User Agreement. Consent is opt-in, once, in settings.json.
const EULA_GATE_MESSAGE: &str = concat!(
    "The intellij-server distribution is proprietary software by JetBrains.\n",
    "It is downloaded from JetBrains' CDN only after you have read and accepted\n",
    "their license, the JetBrains User Agreement (v2.0) — the exact text ships\n",
    "as EULA.txt inside the server bundle, and is published under\n",
    "https://www.jetbrains.com/legal/.\n",
    "\n",
    "To accept, add this to your Zed settings.json and reload the window:\n",
    "\n",
    "{\n",
    "  \"lsp\": {\n",
    "    \"intellij-server\": {\n",
    "      \"settings\": {\n",
    "        \"accept_jetbrains_eula\": true\n",
    "      }\n",
    "    }\n",
    "  }\n",
    "}",
);

/// Whether the user has explicitly accepted the JetBrains EULA via
/// `lsp.intellij-server.settings.accept_jetbrains_eula`.
///
/// False unless the setting is a literal JSON `true` — a string `"true"` does
/// not count, so a typo cannot silently imply consent to a license.
fn accepts_jetbrains_eula(configuration: &Option<serde_json::Value>) -> bool {
    configuration
        .as_ref()
        .and_then(|settings| settings.pointer("/accept_jetbrains_eula"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Fields beyond `defaultSdk` that the server reads out of the `initialize`
/// payload — the same shape JetBrains' own VS Code client sends in
/// `buildInitializationOptions`.
///
/// - `intellij.projects` (array of `{ type, path }`) tells the server which
///   sub-projects to import. Without it a monorepo's nested build files are
///   invisible: the server imports the worktree root as a single project and
///   every module below it resolves against the wrong model.
/// - `intellij.buildTool` (`"gradle"`, `"maven"`, `"bazel"`, or `""` to
///   disable detection) is sent as a per-folder map. It only matters when
///   detection picks the wrong one or finds none. An empty string is forwarded
///   deliberately: it is the documented way to turn detection off, so it cannot
///   be treated as "unset".
///
/// Both are forwarded verbatim when present; the server validates the shape.
/// Pure so the mapping is testable without a `Worktree`.
fn workspace_init_options(
    configuration: &Option<serde_json::Value>,
    root_uri: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let setting = |name: &str| {
        configuration
            .as_ref()
            .and_then(|settings| settings.pointer(name))
    };

    let mut init = serde_json::Map::new();

    if let Some(projects) = setting("/intellij.projects").filter(|value| !value.is_null()) {
        init.insert("projects".to_string(), projects.clone());
    }

    // `buildTool` is accepted as an alias, matching the upstream setting that
    // predates the `intellij.` prefix.
    let build_tool = setting("/intellij.buildTool")
        .or_else(|| setting("/buildTool"))
        .and_then(|value| value.as_str());
    if let Some(build_tool) = build_tool {
        // Keyed by the worktree folder URI the server saw in `initialize` —
        // it matches this map against that folder, not against a path.
        let mut folders = serde_json::Map::new();
        folders.insert(
            root_uri.to_string(),
            serde_json::Value::String(build_tool.to_string()),
        );
        init.insert("buildTools".to_string(), serde_json::Value::Object(folders));
    }

    init
}

/// `file://` URI for an absolute path, matching what Zed sends as `rootUri`.
///
/// Paths are percent-encoded apart from RFC 3986's unreserved set plus `/` and
/// `:`, so a worktree with a space or `#` in it produces the same URI Zed sent.
/// That matters because the server compares this string against its own folder
/// URI: `file:///a b` would never match `file:///a%20b`, and `buildTools` would
/// be silently ignored. (The DAP mode of the native binary derives its own URI
/// without encoding — it only feeds a local socket key, never a server-side
/// comparison.)
fn path_to_file_uri(path: &str) -> String {
    let normalised = path.replace('\\', "/");
    let normalised = normalised
        .strip_prefix("file://")
        .unwrap_or(normalised.as_str());

    let mut uri = String::from("file://");
    if !normalised.starts_with('/') {
        // Windows drive letter: `C:/dir` → `file:///C:/dir`.
        uri.push('/');
    }
    for byte in normalised.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                uri.push(byte as char);
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The adapter id intellij-server's debugger lookup expects — also the name
/// registered under [debug_adapters] in extension.toml and used as the
/// `adapter` field of debug configurations in .zed/debug.json.
pub const DAP_ADAPTER_ID: &str = "intellij_jvm";

pub struct IntelliJServerExtension {
    intellij_server: Option<IntelliJServer>,
    proxy: Option<Proxy>,
}

impl zed::Extension for IntelliJServerExtension {
    fn new() -> Self {
        Self {
            intellij_server: None,
            proxy: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        match language_server_id.as_ref() {
            IntelliJServer::LANGUAGE_SERVER_ID => {
                let intellij_server = self.intellij_server.get_or_insert_with(IntelliJServer::new);

                // Per-project settings from `lsp.intellij-server` in settings.json:
                // `accept_jetbrains_eula` (consent gate, evaluated first),
                // `proxy_path` (proxy binary), `vm_options` (JVM flags for the
                // server, see `java_options`) and `eula_hash` (acceptance-hash
                // override, see `IntelliJServer::eula_hash`).
                let configuration =
                    LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                        .ok()
                        .and_then(|lsp_settings| lsp_settings.settings);

                // EULA gate: nothing is downloaded or executed without explicit
                // consent. This has to stay *ahead* of
                // `language_server_binary_path`, because that call is what
                // triggers the proprietary download.
                if !accepts_jetbrains_eula(&configuration) {
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &zed::LanguageServerInstallationStatus::Failed(
                            EULA_GATE_MESSAGE.to_string(),
                        ),
                    );
                    return Err(EULA_GATE_MESSAGE.to_string());
                }

                let binary_path =
                    intellij_server.language_server_binary_path(language_server_id)?;

                // JetBrains intellij-server ("Java and Kotlin by IntelliJ IDEA"):
                // serves Java AND Kotlin buffers from the same engine and project
                // model. The launcher enforces the bundled EULA before the LSP
                // handshake; `--eula=<sha256(EULA.txt)[:16]>` records acceptance
                // for this user+machine. Telemetry stays off.
                let java_home = resolve_java_home(
                    &configuration,
                    worktree,
                    intellij_server.bundled_java_home(),
                )?;

                // The accepted hash is derived from the EULA.txt of the
                // *installed* distribution (see `IntelliJServer::eula_hash`), so
                // it follows a version bump instead of being pinned here.
                let eula_arg = format!("--eula={}", intellij_server.eula_hash(&configuration));

                let env = vec![
                    ("JAVA_HOME".to_string(), java_home.clone()),
                    ("INTELLIJ_DATA_SHARING".to_string(), "none".to_string()),
                    // The server's Maven import forks do NOT reliably inherit this
                    // process's env (GUI-launched Zed has no JAVA_HOME anyway) —
                    // they resolve their JVM from the *server JVM's*
                    // JB_MAVEN_JAVA_HOME system property first (same mechanism as
                    // kotlin-lsp; IJ_JAVA_OPTIONS is honored by the JetBrains
                    // launcher and passed as a JVM arg).
                    (
                        "IJ_JAVA_OPTIONS".to_string(),
                        java_options(&configuration, &java_home),
                    ),
                ];

                // Run intellij-server behind the native proxy, which rewrites
                // archive-internal (`jar!/`) source URIs into real files so
                // Go-to-Definition into library sources works (see
                // zed-extensions/kotlin#106). If the proxy isn't available,
                // fall back to launching intellij-server directly so core
                // language features keep working (only library source navigation
                // is affected).
                let proxy = self.proxy.get_or_insert_with(Proxy::new);
                match proxy.language_server_binary_path(&configuration, worktree) {
                    Ok(Some(proxy_path)) => {
                        // `binary_path` is relative to the extension's work directory.
                        // Zed resolves a relative `command` against that directory
                        // automatically, but does NOT resolve `args` — so the server
                        // path must be made absolute before being passed as an
                        // argument to the proxy, otherwise the proxy inherits the
                        // *worktree's* cwd and fails to find it.
                        let absolute_binary_path = std::env::current_dir()
                            .map(|dir| dir.join(&binary_path))
                            .map(|path| path.to_string_lossy().into_owned())
                            .unwrap_or(binary_path);

                        Ok(zed::Command {
                            command: proxy_path.to_string_lossy().into_owned(),
                            args: vec![
                                absolute_binary_path,
                                "--stdio".to_string(),
                                eula_arg.clone(),
                            ],
                            env,
                        })
                    }
                    Ok(None) => {
                        eprintln!(
                            "intellij-native not found in bin/; launching intellij-server directly"
                        );
                        Ok(zed::Command {
                            command: binary_path,
                            args: vec!["--stdio".to_string(), eula_arg.clone()],
                            env,
                        })
                    }
                    Err(err) => Err(err),
                }
            }
            _ => Err(format!(
                "Unrecognized language server: {language_server_id}"
            )),
        }
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        match language_server_id.as_ref() {
            IntelliJServer::LANGUAGE_SERVER_ID => {
                // Without an SDK the analyzer can't resolve JDK symbols. Same
                // resolution order as the launcher env (per-project `java_home`,
                // shell env); `lsp.intellij-server.initialization_options` still
                // overrides wholesale.
                let configuration =
                    LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                        .ok()
                        .and_then(|lsp_settings| lsp_settings.settings);
                // Same gate as `language_server_command`: never hand the server
                // an SDK until the user has accepted the license.
                if !accepts_jetbrains_eula(&configuration) {
                    return Err(EULA_GATE_MESSAGE.to_string());
                }
                let bundled = self
                    .intellij_server
                    .get_or_insert_with(IntelliJServer::new)
                    .bundled_java_home();

                let mut init = serde_json::Map::new();
                init.insert(
                    "defaultSdk".to_string(),
                    serde_json::Value::String(resolve_java_home(
                        &configuration,
                        worktree,
                        bundled,
                    )?),
                );
                init.extend(workspace_init_options(
                    &configuration,
                    &path_to_file_uri(&worktree.root_path()),
                ));
                Ok(Some(serde_json::Value::Object(init)))
            }
            _ => Ok(None),
        }
    }

    // --- Debug adapter: IntelliJ's engine, streamed live over the LSP session ---
    // Zed spawns the native binary in `dap` mode as the "adapter"; that process
    // asks the proxy-mode process (which owns the live LSP connection) to run the
    // server-side `start_debug_server` LSP command, and then pumps DAP between
    // Zed's stdio and the TCP endpoint the server opened. All resolution
    // (mainClass → classpath,
    // java binary, module paths) happens inside intellij-server against ITS
    // imported project model — which is what makes Kotlin + Java breakpoints, and
    // mixed-source stepping "just work".

    fn dap_request_kind(
        &mut self,
        adapter_name: String,
        config: serde_json::Value,
    ) -> Result<zed::StartDebuggingRequestArgumentsRequest> {
        if adapter_name != DAP_ADAPTER_ID {
            return Err(format!("Unrecognized debug adapter: {adapter_name}"));
        }
        let is_attach = config
            .get("request")
            .and_then(|v| v.as_str())
            .map(|r| r.eq_ignore_ascii_case("attach"))
            .unwrap_or(false);
        Ok(if is_attach {
            zed::StartDebuggingRequestArgumentsRequest::Attach
        } else {
            zed::StartDebuggingRequestArgumentsRequest::Launch
        })
    }

    fn get_dap_binary(
        &mut self,
        adapter_name: String,
        config: zed::DebugTaskDefinition,
        user_installed_path: Option<String>,
        worktree: &zed::Worktree,
    ) -> Result<zed::DebugAdapterBinary> {
        if adapter_name != DAP_ADAPTER_ID {
            return Err(format!("Unrecognized debug adapter: {adapter_name}"));
        }

        // The user's debug.json body for this scenario (a JSON string).
        let mut cfg: serde_json::Value =
            serde_json::from_str(&config.config).unwrap_or_else(|_| serde_json::json!({}));
        if !cfg.is_object() {
            cfg = serde_json::json!({});
        }
        let obj = cfg.as_object_mut().expect("just checked");
        // Same vocabulary the JetBrains VS Code extension uses; with the default
        // console the server spawns the process itself and streams stdout/stderr
        // as DAP output events (Zed shows them in the Debug console).
        obj.entry("console")
            .or_insert(serde_json::json!("internalConsole"));
        // The server's DAP implementation looks these up for launch configs.
        obj.entry("request").or_insert(serde_json::json!("launch"));

        let request = if obj.get("request").and_then(|v| v.as_str()) == Some("attach") {
            zed::StartDebuggingRequestArgumentsRequest::Attach
        } else {
            zed::StartDebuggingRequestArgumentsRequest::Launch
        };

        // The same executable the language server pipe runs (see
        // `util::ensure_native_binary`): bin/ holds a local dev override or the
        // extracted release download. A dev reinstall wipes bin/, but the
        // extension re-downloads from the pinned GitHub release on demand — no
        // manual reseeding.
        // Must be absolute: Zed spawns a debug adapter with the worktree as cwd
        // and does not resolve relative paths against the extension work dir the
        // way it does for language servers (see `util::ensure_absolute_native_binary`).
        let native = match user_installed_path {
            Some(path) => std::path::PathBuf::from(path),
            None => util::ensure_absolute_native_binary(util::NATIVE_BINARY)?.ok_or_else(|| {
                format!(
                    "{} missing after release download; build native/ in the zed-intellij \
                         repo and attach a fixed archive to the release, then retry",
                    util::NATIVE_BINARY
                )
            })?,
        };
        // `user_installed_path` is not validated by Zed — make sure it exists.
        if !native.is_file() {
            return Err(format!(
                "{} not found at {} (settings override); fix the path and retry",
                util::NATIVE_BINARY,
                native.display()
            ));
        }

        Ok(zed::DebugAdapterBinary {
            command: Some(native.to_string_lossy().into_owned()),
            // `dap` selects the debug-adapter mode; without it the binary would
            // treat the worktree path as a server binary and proxy it.
            arguments: vec!["dap".to_string(), worktree.root_path().to_string()],
            envs: vec![],
            cwd: None,
            connection: None,
            request_args: zed::StartDebuggingRequestArguments {
                configuration: serde_json::to_string(&cfg).map_err(|e| e.to_string())?,
                request,
            },
        })
    }
}

/// `IJ_JAVA_OPTIONS` for the intellij-server launcher.
///
/// Always pins `JB_MAVEN_JAVA_HOME` (the resolved `java_home`, see
/// [`resolve_java_home`]) so the server's Maven import forks resolve the same
/// JDK, then appends
/// `lsp.intellij-server.settings.vm_options` from settings.json — a list of JVM
/// flags used to tune the server without editing the extracted distribution's
/// `bin/intellij-server.vmoptions`. That file pins `-Xmx2048m` and
/// `-XX:CICompilerCount=2`, which is modest for large projects and machines with
/// many cores, e.g.:
///
/// ```json
/// { "lsp": { "intellij-server": { "settings": { "vm_options": [
///     "-Xmx4g", "-XX:CICompilerCount=4"
/// ] } } } }
/// ```
///
/// Leaving `vm_options` unset keeps the upstream defaults untouched — any
/// change to heap/GC/JIT settings should follow measurement, not guesswork.
///
/// The launcher passes these as JVM arguments. Whether a flag that also appears
/// in the vmoptions file overrides it or not has not been verified here — confirm
/// with `jcmd <pid> VM.flags` after changing one that matters.
fn java_options(configuration: &Option<serde_json::Value>, java_home: &str) -> String {
    let mut options = format!("-DJB_MAVEN_JAVA_HOME={java_home}");
    let vm_options = configuration
        .as_ref()
        .and_then(|settings| settings.pointer("/vm_options"))
        .and_then(|value| value.as_array());
    for option in vm_options.into_iter().flatten().filter_map(|v| v.as_str()) {
        options.push(' ');
        options.push_str(option);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_matches_what_zed_sends_as_root_uri() {
        assert_eq!(path_to_file_uri("/Users/me/proj"), "file:///Users/me/proj");
        // Encoded, because the server compares this against its own folder URI.
        assert_eq!(
            path_to_file_uri("/Users/me/my project"),
            "file:///Users/me/my%20project"
        );
        assert_eq!(path_to_file_uri("/tmp/a#b"), "file:///tmp/a%23b");
        // Already a URI: normalised, not prefixed twice.
        assert_eq!(
            path_to_file_uri("file:///Users/me/my project"),
            "file:///Users/me/my%20project"
        );
        // Windows drive letters keep the colon and gain the third slash.
        assert_eq!(path_to_file_uri("C:\\src\\proj"), "file:///C:/src/proj");
    }

    #[test]
    fn monorepo_projects_are_forwarded_verbatim() {
        let projects = serde_json::json!([
            { "type": "gradle", "path": "." },
            { "type": "maven", "path": "services/api" }
        ]);
        let settings = Some(serde_json::json!({ "intellij.projects": projects }));
        let init = workspace_init_options(&settings, "file:///repo");
        assert_eq!(init.get("projects"), Some(&projects));
        assert_eq!(init.get("buildTools"), None);
    }

    #[test]
    fn build_tool_is_keyed_by_worktree_uri() {
        let settings = Some(serde_json::json!({ "intellij.buildTool": "gradle" }));
        assert_eq!(
            workspace_init_options(&settings, "file:///repo").get("buildTools"),
            Some(&serde_json::json!({ "file:///repo": "gradle" }))
        );
    }

    #[test]
    fn build_tool_accepts_unprefixed_alias_and_empty_means_disabled() {
        let alias = Some(serde_json::json!({ "buildTool": "maven" }));
        assert_eq!(
            workspace_init_options(&alias, "file:///r").get("buildTools"),
            Some(&serde_json::json!({ "file:///r": "maven" }))
        );
        // "" disables detection, so it has to reach the server as a mapping.
        let disabled = Some(serde_json::json!({ "intellij.buildTool": "" }));
        assert_eq!(
            workspace_init_options(&disabled, "file:///r").get("buildTools"),
            Some(&serde_json::json!({ "file:///r": "" }))
        );
    }

    #[test]
    fn no_monorepo_settings_present_adds_nothing() {
        assert!(workspace_init_options(&None, "file:///repo").is_empty());
        assert!(workspace_init_options(&Some(serde_json::json!({})), "file:///repo").is_empty());
        // null means "not set", not "forward null to the server".
        assert!(workspace_init_options(
            &Some(serde_json::json!({ "intellij.projects": null })),
            "file:///repo"
        )
        .is_empty());
    }

    #[test]
    fn eula_gate_requires_a_literal_true() {
        let accept = serde_json::json!({ "accept_jetbrains_eula": true });
        let reject_bool = serde_json::json!({ "accept_jetbrains_eula": false });
        let reject_string = serde_json::json!({ "accept_jetbrains_eula": "true" });

        assert!(accepts_jetbrains_eula(&Some(accept)));
        assert!(!accepts_jetbrains_eula(&Some(reject_bool)));
        // A string is not consent: a typo must not imply accepting a license.
        assert!(!accepts_jetbrains_eula(&Some(reject_string)));
        assert!(!accepts_jetbrains_eula(&Some(serde_json::json!({}))));
        assert!(!accepts_jetbrains_eula(&None));
    }
}

zed::register_extension!(IntelliJServerExtension);
