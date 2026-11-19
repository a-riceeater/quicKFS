// SPDX-License-Identifier: Apache-2.0
//! The few places where moving files between the server and the local disk
//! depends on the local operating system.
//!
//! quicKFS names are byte strings. On Unix a local name is a byte string too,
//! so it maps across unchanged. On Windows a local name is UTF-16, so a remote
//! name must be valid UTF-8 — and must also be a name Win32 can reach — before
//! the bridge creates a file under it; a name that is neither is refused rather
//! than mangled into something the user did not choose.

use crate::model::{BridgeError, BridgeResult, validate_name};
use quickfs_protocol::Name;
use std::{ffi::OsStr, path::Path};

/// Whether a symbolic link that cannot be created locally should be skipped
/// with a warning instead of failing the whole download. On Windows creating a
/// link needs Developer Mode or an administrator, and an absolute POSIX target
/// has no Windows meaning, so a folder holding one link must still download.
pub const SKIP_UNCREATABLE_SYMLINKS: bool = cfg!(windows);

/// The local spelling of a remote entry name.
pub fn local_name(name: &Name) -> BridgeResult<&OsStr> {
    validate_name(name.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(OsStr::from_bytes(name.as_bytes()))
    }
    #[cfg(not(unix))]
    {
        let text = std::str::from_utf8(name.as_bytes()).map_err(|_| {
            BridgeError::invalid(format!(
                "'{}' is not valid UTF-8, so Windows cannot store it",
                String::from_utf8_lossy(name.as_bytes())
            ))
        })?;
        #[cfg(windows)]
        quickfs_common::windows_names::check_creatable(text)
            .map_err(|problem| BridgeError::invalid(format!("'{text}' {problem}")))?;
        Ok(OsStr::new(text))
    }
}

/// The remote spelling of a local file name.
pub fn remote_name(name: &OsStr) -> BridgeResult<Name> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = name
        .to_str()
        .ok_or_else(|| {
            BridgeError::invalid(format!(
                "'{}' is not valid Unicode, so it has no quicKFS spelling",
                name.to_string_lossy()
            ))
        })?
        .as_bytes()
        .to_vec();
    validate_name(&bytes)?;
    Ok(Name::new(bytes))
}

/// The POSIX permission bits to give a remote copy of a local file.
pub fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        // Windows has only the read-only attribute, which maps onto the owner
        // write bit exactly as the WinFsp bridge maps it the other way.
        match (metadata.is_dir(), metadata.permissions().readonly()) {
            (true, _) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}

/// Create a local symbolic link at `path` pointing at the remote `target`.
pub fn create_local_symlink(target: &[u8], path: &Path) -> BridgeResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::os::unix::fs::symlink(OsStr::from_bytes(target), path)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let target = std::str::from_utf8(target)
            .map_err(|_| BridgeError::invalid("the link target is not valid UTF-8"))?;
        if target.starts_with('/') {
            return Err(BridgeError::invalid(format!(
                "the link points at the absolute server path '{target}', which has no meaning on Windows"
            )));
        }
        let translated = target.replace('/', "\\");
        // Windows records whether a link names a directory. Decide from what
        // the target is now; a target downloaded later defaults to a file.
        let resolved = path
            .parent()
            .map(|parent| parent.join(&translated))
            .unwrap_or_else(|| Path::new(&translated).to_path_buf());
        let result = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&translated, path)
        } else {
            std::os::windows::fs::symlink_file(&translated, path)
        };
        result.map_err(|error| {
            // ERROR_PRIVILEGE_NOT_HELD
            if error.raw_os_error() == Some(1314) {
                BridgeError::invalid(
                    "Windows only lets administrators, or accounts with Developer Mode turned on, create symbolic links",
                )
            } else {
                error.into()
            }
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, path);
        Err(BridgeError::invalid(
            "symbolic links are not supported on this platform",
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_round_trip() {
        let name = Name::new("photo café.jpg".as_bytes().to_vec());
        let local = local_name(&name).unwrap();
        assert_eq!(remote_name(local).unwrap(), name);
    }

    #[test]
    fn names_the_platform_cannot_hold_are_refused_on_download() {
        assert!(local_name(&Name::new(b"a/b".to_vec())).is_err());
        assert!(local_name(&Name::new(b"..".to_vec())).is_err());
        #[cfg(windows)]
        {
            assert!(local_name(&Name::new(b"bad:name".to_vec())).is_err());
            assert!(local_name(&Name::new(b"CON".to_vec())).is_err());
            assert!(local_name(&Name::new(b"trailing.".to_vec())).is_err());
            assert!(local_name(&Name::new(vec![0xff, 0xfe])).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn absolute_server_link_targets_are_refused_on_windows() {
        let directory = tempfile::tempdir().unwrap();
        let error = create_local_symlink(b"/etc/passwd", &directory.path().join("link"));
        assert!(error.is_err());
        assert!(!directory.path().join("link").exists());
    }
}
