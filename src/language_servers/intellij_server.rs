use std::fs;

use zed_extension_api::{self as zed, make_file_executable, Result};

use crate::language_servers::util;

pub struct IntelliJServer {
    cached_binary_path: Option<String>,
}

impl IntelliJServer {
    pub const LANGUAGE_SERVER_ID: &'static str = "intellij-server";

    // Server build of the "Java and Kotlin by IntelliJ IDEA" RC
    // (VS Code marketplace id: JetBrains.intellij-server). Same build family as
    // kotlin-lsp 263.6379.0: this build ships the byte-identical
    // workspace-import-maven-plugin.jar and the same strict workspace model.
    //
    // NOTE (local machine): the OpenAPI kotlin generated-source-roots patch must
    // be applied to plugins/kotlin.lsp/lib/workspace-import-maven-plugin.jar in
    // the extracted distro (see ~/.local/share/zed-kotlin-patches and the
    // `patch-kotlin-lsp-sourceDirs` script). When bumping SERVER_VERSION:
    // re-download, re-apply the patch, and recompute EULA_SHA256_16 as
    // sha256(EULA.txt)[:16] from the new distro root. Pre-seeding the work
    // directory below makes the extension skip the (unpatched) download.
    pub const SERVER_VERSION: &'static str = "263.6379.0";
    // First 16 hex chars of sha256(EULA.txt) of build 263.6379.0. The launcher
    // enforces the EULA before the LSP handshake; passing the hash via
    // `--eula=<hash>` records acceptance for this user+machine.
    pub const EULA_SHA256_16: &'static str = "34d850193ee04897";

    pub fn new() -> Self {
        IntelliJServer {
            cached_binary_path: None,
        }
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

    let target_dir = match os {
        zed::Os::Windows => extension_dir.clone(),
        _ => format!("{extension_dir}/intellij-server-{version}"),
    };

    let binary_path = format!(
        "{target_dir}/bin/intellij-server{exe_suffix}",
        exe_suffix = match os {
            zed::Os::Windows => ".exe",
            _ => "",
        }
    );

    if !fs::metadata(&extension_dir).is_ok_and(|metadata| metadata.is_dir()) {
        let downloaded_file_type = match os {
            // `sit` == zip, per the kotlin-server packaging
            zed::Os::Windows | zed::Os::Mac => zed_extension_api::DownloadedFileType::Zip,
            zed::Os::Linux => zed_extension_api::DownloadedFileType::GzipTar,
        };
        zed::download_file(&url, &extension_dir, downloaded_file_type)?;
        make_file_executable(&binary_path)?;
        util::remove_outdated_versions(IntelliJServer::LANGUAGE_SERVER_ID, &extension_dir)?;
    }

    if !fs::metadata(&binary_path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(format!(
            "IntelliJ server download succeeded but binary is missing at {binary_path}"
        ));
    }

    Ok(binary_path)
}
