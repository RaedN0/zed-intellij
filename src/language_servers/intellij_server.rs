use std::fs;

use zed_extension_api::{self as zed, make_file_executable, Result};

use crate::language_servers::util;

pub struct IntelliJServer {
    cached_binary_path: Option<String>,
}

impl IntelliJServer {
    pub const LANGUAGE_SERVER_ID: &'static str = "intellij-server";

    // Server build 271.2085.0 of "Java and Kotlin by IntelliJ IDEA"
    // (VS Code marketplace id: JetBrains.intellij-server, wrapper 1.0.86,
    // published 2026-11-03; product version 2027.1 — a full platform branch on
    // from 1.0's 2026.3).
    //
    // NOTE (no more source-roots patch): 1.0 (263.7084.0) needed a local rebuild
    // of plugins/kotlin.lsp/lib/workspace-import-maven-plugin.jar because
    // `kotlinSourceDirs` only read `<sourceDir>` children and dropped the
    // OpenAPI generated-source roots. 271.2085.0 fixes this upstream:
    // `kotlinSourceDirs(plugin, basedir, test)` resolves `<sourceDirs>` children
    // against the module basedir, and `SourceRootData.generated` is now a
    // serialized field populated by `markRootsUnderBuildDirectoryGenerated`.
    // Do NOT re-apply the old patched jar — it is built against the 263-era
    // sources and would revert exactly that.
    //
    // NOTE (layout): the archive's top-level directory is the PRODUCT version
    // (`intellij-server-2027.1`) since 1.0 — `locate_server_binary` scans for it
    // instead of hardcoding the name, and `bundled_java_home` walks into it.
    //
    // NOTE (launcher): 271 ships `bin/lsp-router` + `bin/restarter` and routes a
    // stdio LSP client to a per-workspace server that outlives the editor
    // session (`--socket`, `--multi-client`, `--idle-timeout`, state under
    // $INTELLIJ_LSP_PORT_DIR). `bin/intellij-server` stays the entry point and
    // still accepts `--stdio` / `--eula`, so the proxy keeps its place on the
    // pipe — but diagnostics cached by the proxy may predate a server restart.
    //
    // License: the server needs an IDEA Ultimate license after the 30-day trial
    // (launcher enforces it like the EULA).
    pub const SERVER_VERSION: &'static str = "271.2085.0";
    // First 16 hex chars of sha256(EULA.txt) of build 271.2085.0. The launcher
    // enforces the EULA before the LSP handshake; passing the hash via
    // `--eula=<hash>` records acceptance for this user+machine. Unchanged from
    // 263.7084.0 (verified against the extracted 271.2085.0 distro).
    pub const EULA_SHA256_16: &'static str = "cd82e6cdaf2790f1";

    pub fn new() -> Self {
        IntelliJServer {
            cached_binary_path: None,
        }
    }

    /// The JDK the distribution ships (`jbr/`), for machines that have no
    /// `JAVA_HOME` of their own. Returns `None` until the distro is extracted or
    /// when the build has no bundled runtime.
    ///
    /// The path is relative to the extension's work directory, like
    /// [`language_server_binary_path`](Self::language_server_binary_path) — the
    /// sandbox can only stat paths inside it.
    pub fn bundled_java_home(&self) -> Option<String> {
        let extension_dir = format!(
            "{}-{}",
            Self::LANGUAGE_SERVER_ID,
            Self::SERVER_VERSION
        );
        let distro = locate_server_home(&extension_dir, exe_suffix())?;
        // macOS keeps the JDK under Contents/Home, Linux/Windows use jbr itself.
        let candidates = [
            format!("{distro}/jbr/Contents/Home"),
            format!("{distro}/jbr"),
        ];
        candidates
            .into_iter()
            .find(|home| {
                let java = format!("{home}/bin/java{}", exe_suffix());
                fs::metadata(&java).is_ok_and(|metadata| metadata.is_file())
            })
    }

    pub fn language_server_binary_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
    ) -> Result<String> {
        if let Some(path) = self.cached_binary_path.as_ref() {
            return Ok(path.clone());
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let version = Self::SERVER_VERSION.to_string();

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );

        let binary_path = download_from_jetbrains(version)?;

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

fn download_from_jetbrains(version: String) -> Result<String> {
    let (os, arch) = zed_extension_api::current_platform();

    // macOS arm: intellij-server-<v>-aarch64.sit, x64: intellij-server-<v>.sit
    // Linux x64: .tar.gz, arm: -aarch64.tar.gz; Windows: .win.zip / -aarch64.win.zip
    let arch_suffix = match arch {
        zed::Architecture::X8664 => "",
        zed::Architecture::Aarch64 => "-aarch64",
        _ => return Err("Platform X86 is not supported by the IntelliJ server.".to_owned()),
    };

    let asset_name = match os {
        zed::Os::Windows => format!("intellij-server-{version}{arch_suffix}.win.zip"),
        zed::Os::Mac => format!("intellij-server-{version}{arch_suffix}.sit"),
        zed::Os::Linux => format!("intellij-server-{version}{arch_suffix}.tar.gz"),
    };

    // NOTE: unlike kotlin-server, this product is served from
    // download.jetbrains.com (language-server/intellij-server/), not from the
    // kotlin-lsp RELEASES CDN path.
    let url = format!(
        "https://download.jetbrains.com/language-server/intellij-server/{version}/{asset_name}"
    );

    let extension_dir = format!(
        "{server_id}-{version}",
        server_id = IntelliJServer::LANGUAGE_SERVER_ID
    );

    if !fs::metadata(&extension_dir).is_ok_and(|metadata| metadata.is_dir()) {
        let downloaded_file_type = match os {
            // `sit` == zip, per the kotlin-server packaging
            zed::Os::Windows | zed::Os::Mac => zed_extension_api::DownloadedFileType::Zip,
            zed::Os::Linux => zed_extension_api::DownloadedFileType::GzipTar,
        };
        zed::download_file(&url, &extension_dir, downloaded_file_type)?;
        util::remove_outdated_versions(IntelliJServer::LANGUAGE_SERVER_ID, &extension_dir)?;
    }

    let binary_path = locate_server_binary(&extension_dir, exe_suffix())?;

    if !fs::metadata(&binary_path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(format!(
            "IntelliJ server download succeeded but binary is missing at {binary_path}"
        ));
    }
    make_file_executable(&binary_path)?;

    Ok(binary_path)
}

/// Find the extracted distribution root inside `extension_dir` — the directory
/// holding `bin/intellij-server<suffix>`.
///
/// The archive's top-level directory name is NOT stable across builds: through
/// the 263.6379.0 RC it was the build number (`intellij-server-263.6379.0`),
/// from 1.0 on it is the product version (`intellij-server-2026.3`, then
/// `intellij-server-2027.1`), and the Windows zip may extract flat. So scan one
/// level deep instead of hardcoding the inner directory name.
fn locate_server_home(extension_dir: &str, exe_suffix: &str) -> Option<String> {
    let binary_name = format!("intellij-server{exe_suffix}");

    let entries = fs::read_dir(extension_dir).ok()?;
    for entry in entries.flatten() {
        if entry.path().join("bin").join(&binary_name).is_file() {
            return Some(entry.path().to_string_lossy().into_owned());
        }
    }

    // Flat extraction fallback (no top-level directory).
    let flat = format!("{extension_dir}/bin/{binary_name}");
    if fs::metadata(&flat).is_ok_and(|metadata| metadata.is_file()) {
        return Some(extension_dir.to_owned());
    }
    None
}

/// Find `bin/intellij-server<suffix>` inside the extracted distribution.
fn locate_server_binary(extension_dir: &str, exe_suffix: &str) -> Result<String> {
    let binary_name = format!("intellij-server{exe_suffix}");
    locate_server_home(extension_dir, exe_suffix)
        .map(|home| format!("{home}/bin/{binary_name}"))
        .ok_or_else(|| {
            format!(
                "intellij-server binary not found under {extension_dir} \
                 (expected <top-level>/bin/{binary_name})"
            )
        })
}

/// The executable suffix for the current platform (`""` outside Windows).
fn exe_suffix() -> &'static str {
    match zed_extension_api::current_platform().0 {
        zed::Os::Windows => ".exe",
        _ => "",
    }
}
