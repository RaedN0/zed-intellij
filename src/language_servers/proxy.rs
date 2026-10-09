use std::path::PathBuf;

use zed_extension_api::{self as zed, serde_json::Value, Worktree};

use crate::language_servers::util;

const PROXY_BINARY: &str = "kotlin-lsp-proxy";
/// Local override location: `bin/kotlin-lsp-proxy` inside this extension's
/// work directory.
const INSTALL_DIR: &str = util::NATIVE_BIN_DIR;

/// Resolves the `kotlin-lsp-proxy` binary that rewrites archive-internal
/// (`jar!/`, `zip!/`) source URIs returned intellij-server into real files so
/// Go-to-Definition into library/JDK sources works in Zed
/// (see zed-extensions/kotlin#106 for the original kotlin-lsp wiring).
///
/// Resolution order:
/// 1. `lsp.intellij-server.settings.proxy_path` in settings.json (escape hatch),
/// 2. `bin/kotlin-lsp-proxy` in the extension work dir (local copy — on this
///    machine kept in sync with the one used by the kotlin extension),
/// 3. nothing -> callers fall back to launching the server directly (only
///    library source navigation is affected).
///
/// Unlike the kotlin extension there is no release download: this extension is
/// machine-local, the proxy binary ships alongside it in the work dir.
pub struct Proxy {
    cached_binary_path: Option<PathBuf>,
}

impl Proxy {
    pub fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    pub fn language_server_binary_path(
        &mut self,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> Result<Option<PathBuf>, String> {
        if let Some(path) = self.cached_binary_path.as_ref() {
            return Ok(Some(path.clone()));
        }

        let path = if let Some(path) = user_configured_proxy_path(configuration, worktree) {
            Some(PathBuf::from(path))
        } else {
            util::find_native_binary(&proxy_exec())?
        };

        match path {
            Some(path) => {
                self.cached_binary_path = Some(path.clone());
                Ok(Some(path))
            }
            None => Ok(None),
        }
    }
}

fn proxy_exec() -> String {
    match zed::current_platform().0 {
        zed::Os::Windows => format!("{PROXY_BINARY}.exe"),
        _ => PROXY_BINARY.to_string(),
    }
}

fn user_configured_proxy_path(
    configuration: &Option<Value>,
    worktree: &Worktree,
) -> Option<String> {
    let path = configuration
        .as_ref()?
        .pointer("/proxy_path")
        .and_then(|v| v.as_str())?;
    expand_home_path(worktree, path.to_string()).ok()
}

/// Expand a leading `~` to the worktree's `$HOME`. On Windows the path is returned
/// unchanged (mirrors the equivalent helper in the Zed Java extension).
fn expand_home_path(worktree: &Worktree, path: String) -> Result<String, String> {
    match zed::current_platform().0 {
        zed::Os::Windows => Ok(path),
        _ => worktree
            .shell_env()
            .into_iter()
            .find(|(key, _)| key == "HOME")
            .map(|(_, home)| path.replace('~', &home))
            .ok_or_else(|| {
                "Failed to expand '~': $HOME not found in shell environment".to_string()
            }),
    }
}
