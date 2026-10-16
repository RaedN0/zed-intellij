use std::fs;
use std::path::{Path, PathBuf};

use zed_extension_api::{self as zed, DownloadedFileType, Result};

/// Root-level binaries (`bin/<exec>`) are local development overrides; versioned
/// installs live under `bin/<version>/<exec>`.
pub(super) const NATIVE_BIN_DIR: &str = "bin";

/// GitHub repository the native binaries (proxy + bridge) are downloaded from.
/// `.github/workflows/release.yml` attaches one `.tar.gz` per target triple to
/// the release tagged `v<CARGO_PKG_VERSION>` — keep the tag, the asset names
/// and this constant in lockstep when bumping the extension version.
const RELEASES_REPO: &str = "RaedN0/zed-intellij";

/// The binaries inside the release archive. `kotlin-lsp-proxy` keeps the
/// upstream kotlin extension's binary name (see `proxy.rs`).
const RELEASE_BINARIES: [&str; 2] = ["kotlin-lsp-proxy", "intellij-dap-bridge"];

/// The release tag corresponding to this extension package version.
pub(super) fn extension_release_version() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// Find a local development override or the binary matching this extension release.
/// Returns Ok(None) when no proxy binary is installed (callers then launch the
/// language server directly).
pub(super) fn find_native_binary(executable: &str) -> Result<Option<PathBuf>, String> {
    let install_dir = Path::new(NATIVE_BIN_DIR);
    let preferred_paths = [
        install_dir.join(executable),
        install_dir
            .join(extension_release_version())
            .join(executable),
    ];
    Ok(preferred_paths
        .into_iter()
        .find(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file())))
}

/// Find a native binary, downloading the platform archive from the pinned
/// GitHub release if the versioned install doesn't have it. Resolution order:
///
/// 1. `bin/<exec>` — local development override (this repo's build output,
///    wins over any downloaded copy),
/// 2. `bin/<version>/<exec>` — extracted from the release archive,
/// 3. download `zed-intellij-<version>-<triple>.tar.gz` from the GitHub
///    release tagged `<version>` and extract it into `bin/<version>/`, then
///    re-resolve.
///
/// The download self-heals the work dir after a dev extension reinstall, which
/// wipes `bin/` entirely. Callers decide how to treat failure: the proxy
/// degrades to launching intellij-server directly, the bridge (required for
/// debugging) surfaces the error to the user.
pub(crate) fn ensure_native_binary(executable: &str) -> Result<Option<PathBuf>, String> {
    if let Some(path) = find_native_binary(executable)? {
        return Ok(Some(path));
    }
    download_release_binaries()?;
    find_native_binary(executable)
}

/// Download the platform archive for this extension's exact release tag and
/// extract it into `bin/<version>/`. Archives contain the binaries at their
/// root (see the workflow's packaging step); Zed's host extracts tar.gz
/// contents into the destination directory and preserves paths.
fn download_release_binaries() -> Result<(), String> {
    let tag = extension_release_version();
    let asset_name = format!("zed-intellij-{tag}-{}.tar.gz", target_triple());
    let release = zed::github_release_by_tag_name(RELEASES_REPO, &tag).map_err(|e| {
        format!(
            "failed to look up release {tag} of {RELEASES_REPO} (create it and attach the \
             release.yml archives, or seed bin/ manually): {e}"
        )
    })?;
    let url = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .map(|asset| &asset.download_url)
        .ok_or_else(|| {
            format!("release {tag} of {RELEASES_REPO} has no asset named {asset_name}")
        })?;

    let install_dir = Path::new(NATIVE_BIN_DIR).join(&tag);
    fs::create_dir_all(&install_dir)
        .map_err(|e| format!("failed to create {}: {e}", install_dir.display()))?;
    zed::download_file(
        url,
        &install_dir.to_string_lossy(),
        DownloadedFileType::GzipTar,
    )
    .map_err(|e| format!("failed to download {url}: {e}"))?;
    for name in RELEASE_BINARIES {
        let path = install_dir.join(name);
        if path.is_file() {
            zed::make_file_executable(&path.to_string_lossy())
                .map_err(|e| format!("failed to make {} executable: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// Rust target triple for the running platform — release assets are named
/// `zed-intellij-<tag>-<triple>.tar.gz`.
fn target_triple() -> &'static str {
    match zed::current_platform() {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
        (zed::Os::Mac, _) => "x86_64-apple-darwin",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
        (zed::Os::Linux, _) => "x86_64-unknown-linux-gnu",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
        (zed::Os::Windows, zed::Architecture::X86) => "i686-pc-windows-msvc",
        (zed::Os::Windows, _) => "x86_64-pc-windows-msvc",
    }
}

/// Delete other downloaded versions of the given server distribution so the
/// work directory only ever holds the version the extension pins.
/// (Not used for proxy/bridge — those share one `bin/<version>/` dir keyed by
/// the extension version itself.)
pub(super) fn remove_outdated_versions(
    language_server_id: &'static str,
    version_dir: &str,
) -> Result<()> {
    let entries = fs::read_dir(".").map_err(|e| format!("failed to list working directory {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to load directory entry {e}"))?;
        if entry.file_name().to_str().is_none_or(|file_name| {
            file_name.starts_with(language_server_id) && file_name != version_dir
        }) {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
    Ok(())
}
