// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use std::{error::Error, fmt};

#[cfg(any(target_os = "macos", test))]
use std::{fs, path::Path};

/// The official macFUSE download and installation website.
pub const MACFUSE_INSTALL_URL: &str = "https://macfuse.io/";

#[cfg(target_os = "macos")]
const MACFUSE_BUNDLE_PATH: &str = "/Library/Filesystems/macfuse.fs";

/// Returns whether the macFUSE runtime required by the macOS clients is installed.
///
/// The check is deliberately a no-op on other operating systems. On macOS it
/// verifies the standard filesystem bundle and its Info.plist without launching
/// a subprocess or probing for development-only pkg-config metadata.
#[must_use]
pub fn macfuse_is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        macfuse_bundle_is_installed(Path::new(MACFUSE_BUNDLE_PATH))
    }

    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Stops a macOS client command when the macFUSE runtime is not installed.
///
/// This always succeeds on non-macOS platforms.
pub fn require_macfuse() -> Result<(), MacFuseRequired> {
    if macfuse_is_installed() {
        Ok(())
    } else {
        Err(MacFuseRequired)
    }
}

/// The actionable error returned when a macOS client starts without macFUSE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacFuseRequired;

impl fmt::Display for MacFuseRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "macFUSE is required to use quicKFS on macOS. Install it from {MACFUSE_INSTALL_URL}, then run this command again"
        )
    }
}

impl Error for MacFuseRequired {}

#[cfg(any(target_os = "macos", test))]
fn macfuse_bundle_is_installed(bundle_path: &Path) -> bool {
    let Ok(bundle_metadata) = fs::symlink_metadata(bundle_path) else {
        return false;
    };
    if !bundle_metadata.file_type().is_dir() {
        return false;
    }

    let Ok(info_metadata) = fs::symlink_metadata(bundle_path.join("Contents/Info.plist")) else {
        return false;
    };
    info_metadata.file_type().is_file()
}

#[cfg(test)]
mod tests {
    use super::macfuse_bundle_is_installed;
    use std::{fs, path::Path};

    #[test]
    fn recognizes_a_complete_filesystem_bundle() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let bundle = temporary.path().join("macfuse.fs");
        fs::create_dir_all(bundle.join("Contents"))?;
        fs::write(bundle.join("Contents/Info.plist"), b"test")?;

        assert!(macfuse_bundle_is_installed(&bundle));
        Ok(())
    }

    #[test]
    fn rejects_missing_or_incomplete_bundles() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let bundle = temporary.path().join("macfuse.fs");
        assert!(!macfuse_bundle_is_installed(&bundle));

        fs::create_dir_all(&bundle)?;
        assert!(!macfuse_bundle_is_installed(&bundle));

        fs::create_dir_all(bundle.join("Contents/Info.plist"))?;
        assert!(!macfuse_bundle_is_installed(&bundle));
        assert!(!macfuse_bundle_is_installed(Path::new(
            "/path/that/does/not/exist"
        )));
        Ok(())
    }
}
