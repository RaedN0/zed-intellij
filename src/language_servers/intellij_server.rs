use std::fs;

use zed_extension_api::{self as zed, make_file_executable, Result};

use crate::language_servers::util;

pub struct IntelliJServer {
    cached_binary_path: Option<String>,
}

impl IntelliJServer {
    pub const LANGUAGE_SERVER_ID: &'static str = "intellij-server";

    // Server build 1.0 of "Java and Kotlin by IntelliJ IDEA"
    // (VS Code marketplace id: JetBrains.intellij-server, first stable release
    // 2026-10-15; product version 2026.3). Still the same build family as
    // kotlin-lsp 263.6379.0: 1.0 ships a byte-identical
    // workspace-import-maven-plugin.jar, so the v3 source-roots patch carries
    // over unchanged (verified: new jar == stored 263.6379.0 .orig).
    //
    // NOTE (local machine): the OpenAPI kotlin generated-source-roots patch must
    // be applied to plugins/kotlin.lsp/lib/workspace-import-maven-plugin.jar in
    // the extracted distro (see ~/.local/share/zed-kotlin-patches and the
    // `patch-kotlin-lsp-sourceDirs` script). When bumping SERVER_VERSION:
    // re-download, re-apply the patch, and recompute EULA_SHA256_16 as
    // sha256(EULA.txt)[:16] from the new distro root. Pre-seeding the work
    // directory below makes the extension skip the (unpatched) download.
    //
    // NOTE (1.0 layout change): the archive's top-level directory is now the
    // PRODUCT version (`intellij-server-2026.3`) instead of the build number —
    // `locate_server_binary` scans for it instead of hardcoding the name.
    // License: EAP expiry is gone; the server needs an IDEA Ultimate license
    // after the 30-day trial (launcher enforces it like the EULA).
    pub const SERVER_VERSION: &'static str = "263.7084.0";
    // First 16 hex chars of sha256(EULA.txt) of build 263.7084.0 (1.0). The
    // launcher enforces the EULA before the LSP handshake; passing the hash via
    // `--eula=<hash>` records acceptance for this user+machine.
    pub const EULA_SHA256_16: &'static str = "cd82e6cdaf2790f1";

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

    let exe_suffix = match os {
        zed::Os::Windows => ".exe",
        _ => "",
    };

    if !fs::metadata(&extension_dir).is_ok_and(|metadata| metadata.is_dir()) {
        let downloaded_file_type = match os {
            // `sit` == zip, per the kotlin-server packaging
            zed::Os::Windows | zed::Os::Mac => zed_extension_api::DownloadedFileType::Zip,
            zed::Os::Linux => zed_extension_api::DownloadedFileType::GzipTar,
        };
        zed::download_file(&url, &extension_dir, downloaded_file_type)?;
        util::remove_outdated_versions(IntelliJServer::LANGUAGE_SERVER_ID, &extension_dir)?;
    }

    let binary_path = locate_server_binary(&extension_dir, exe_suffix)?;

    if !fs::metadata(&binary_path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(format!(
            "IntelliJ server download succeeded but binary is missing at {binary_path}"
        ));
    }
    make_file_executable(&binary_path)?;

    Ok(binary_path)
}

/// Find `bin/intellij-server<suffix>` inside the extracted distribution.
///
/// The archive's top-level directory name is NOT stable across builds: through
/// the 263.6379.0 RC it was the build number (`intellij-server-263.6379.0`),
/// from 1.0 on it is the product version (`intellij-server-2026.3`), and the
/// Windows zip may extract flat. So scan one level deep for the binary instead
/// of hardcoding the inner directory name.
fn locate_server_binary(extension_dir: &str, exe_suffix: &str) -> Result<String> {
    let binary_name = format!("intellij-server{exe_suffix}");

    let entries = fs::read_dir(extension_dir)
        .map_err(|e| format!("failed to list {extension_dir}: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read {extension_dir} entry: {e}"))?;
        let candidate = entry.path().join("bin").join(&binary_name);
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }

    // Flat extraction fallback (no top-level directory).
    let flat = format!("{extension_dir}/bin/{binary_name}");
    if fs::metadata(&flat).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(flat);
    }

    Err(format!(
        "intellij-server binary not found under {extension_dir} \
         (expected <top-level>/bin/{binary_name})"
    ))
}
