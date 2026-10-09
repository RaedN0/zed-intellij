use zed::serde_json;
use zed::LanguageServerId;
use zed_extension_api::{self as zed, settings::LspSettings, Result};

mod language_servers;

use language_servers::{IntelliJServer, Proxy};

// NOTE (personal machine): baked so that intellij-server works with zero
// per-project configuration — GUI-launched Zed has no JAVA_HOME, and without
// it the server's Maven import forks fall back to /usr/bin/java (Java 1.8!).
// A project's `.zed/settings.json` (`lsp.intellij-server.binary.env`) still
// overrides this if present.
pub const DEFAULT_JAVA_HOME: &str = "/Users/mschoerghuber/.sdkman/candidates/java/current";

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
                let intellij_server = self
                    .intellij_server
                    .get_or_insert_with(IntelliJServer::new);
                let binary_path = intellij_server.language_server_binary_path(language_server_id)?;

                // JetBrains intellij-server ("Java and Kotlin by IntelliJ IDEA"):
                // serves Java AND Kotlin buffers from the same engine and project
                // model. The launcher enforces the bundled EULA before the LSP
                // handshake; `--eula=<sha256(EULA.txt)[:16]>` records acceptance
                // for this user+machine. Telemetry stays off.
                let env = vec![
                    ("JAVA_HOME".to_string(), DEFAULT_JAVA_HOME.to_string()),
                    ("INTELLIJ_DATA_SHARING".to_string(), "none".to_string()),
                    // The server's Maven import forks do NOT reliably inherit this
                    // process's env (GUI-launched Zed has no JAVA_HOME anyway) —
                    // they resolve their JVM from the *server JVM's*
                    // JB_MAVEN_JAVA_HOME system property first (same mechanism as
                    // kotlin-lsp; IJ_JAVA_OPTIONS is honored by the JetBrains
                    // launcher and passed as a JVM arg).
                    (
                        "IJ_JAVA_OPTIONS".to_string(),
                        format!("-DJB_MAVEN_JAVA_HOME={DEFAULT_JAVA_HOME}"),
                    ),
                ];

                // Run intellij-server behind kotlin-lsp-proxy, which rewrites
                // archive-internal (`jar!/`) source URIs into real files so
                // Go-to-Definition into library sources works (see
                // zed-extensions/kotlin#106). If the proxy isn't available,
                // fall back to launching intellij-server directly so core
                // language features keep working (only library source navigation
                // is affected).
                let configuration =
                    LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                        .ok()
                        .and_then(|lsp_settings| lsp_settings.settings);

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
                                format!("--eula={}", IntelliJServer::EULA_SHA256_16),
                            ],
                            env,
                        })
                    }
                    Ok(None) => {
                        eprintln!(
                            "kotlin-lsp-proxy not found in {}/; launching intellij-server directly",
                            "bin"
                        );
                        Ok(zed::Command {
                            command: binary_path,
                            args: vec![
                                "--stdio".to_string(),
                                format!("--eula={}", IntelliJServer::EULA_SHA256_16),
                            ],
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
        _worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        match language_server_id.as_ref() {
            IntelliJServer::LANGUAGE_SERVER_ID => Ok(Some(serde_json::json!({
                // Same motivation as DEFAULT_JAVA_HOME: without an SDK the
                // analyzer can't resolve JDK symbols. Per-project settings
                // (`lsp.intellij-server.initialization_options`) override this.
                "defaultSdk": DEFAULT_JAVA_HOME,
            }))),
            _ => Ok(None),
        }
    }

    // --- Debug adapter: IntelliJ's engine, streamed live over the LSP session ---
    // Zed spawns `bin/intellij-dap-bridge` as the "adapter"; that process asks
    // kotlin-lsp-proxy (which owns the live LSP connection) to run the server-side
    // `start_debug_server` LSP command, and then pumps DAP between Zed's stdio and
    // the TCP endpoint the server opened. All resolution (mainClass → classpath,
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
        let mut cfg: serde_json::Value = serde_json::from_str(&config.config)
            .unwrap_or_else(|_| serde_json::json!({}));
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
        obj.entry("request")
            .or_insert(serde_json::json!("launch"));

        let request = if obj.get("request").and_then(|v| v.as_str()) == Some("attach") {
            zed::StartDebuggingRequestArgumentsRequest::Attach
        } else {
            zed::StartDebuggingRequestArgumentsRequest::Launch
        };

        // The bridge binary is seeded next to the proxy in this extension's work
        // dir (dev reinstalls wipe it — reseed after reinstalling the dev ext!)
        let (bridge, bridge_source) = match user_installed_path {
            Some(path) => (std::path::PathBuf::from(path), "settings override"),
            None => (
                std::env::current_dir()
                    .map_err(|e| format!("no extension work dir: {e}"))?
                    .join("bin")
                    .join("intellij-dap-bridge"),
                "work dir bin/",
            ),
        };
        if !bridge.is_file() {
            return Err(format!(
                "intellij-dap-bridge not found at {} ({bridge_source}); rebuild bridge/ in the \
                 zed-intellij repo and reseed bin/, then retry",
                bridge.display()
            ));
        }

        Ok(zed::DebugAdapterBinary {
            command: Some(bridge.to_string_lossy().into_owned()),
            arguments: vec![worktree.root_path().to_string()],
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

zed::register_extension!(IntelliJServerExtension);


