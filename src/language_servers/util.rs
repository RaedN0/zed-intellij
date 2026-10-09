use std::fs;
use std::path::{Path, PathBuf};

use zed_extension_api::Result;

/// Root-level binaries (`bin/<exec>`) are local development overrides; versioned
/// installs live under `bin/<version>/<exec>`.
pub(super) const NATIVE_BIN_DIR: &str = "bin";

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

/// Delete other downloaded versions of the given language server so the work
/// directory only ever holds the version the extension pins.
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
