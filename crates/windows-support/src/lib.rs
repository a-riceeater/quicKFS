// SPDX-License-Identifier: Apache-2.0
//! Windows platform support for the quicKFS client.
//!
//! Four things live here, and nothing else:
//!
//! * [`require_winfsp`] — the actionable startup check for the WinFsp runtime,
//!   mirroring `quickfs-macos-support`'s macFUSE check.
//! * [`current_user_sid`] — the mounting account's SID, used to build the
//!   security descriptors the WinFsp bridge presents for every node.
//! * [`UnmountSignal`] and [`signal_unmount`], the named event that lets
//!   `quickfs unmount Q:` reach the process holding the mount.
//! * The private-state primitives ([`create_private_directory`],
//!   [`harden_private_path`], [`validate_private_owner`], [`lock_file`]) that
//!   give the trust database and the offline cache the same protection on
//!   Windows that owner-only `0700`/`0600` modes and `flock` give them on Unix.
//!
//! This is the only crate in the workspace allowed to write `unsafe`. Keeping
//! every Win32 call in one small, reviewed module is what lets the workspace
//! keep `unsafe_code = "forbid"` everywhere else.

use std::{error::Error, fmt};

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    UnmountSignal, create_private_directory, current_user_sid, harden_private_path, lock_file,
    signal_unmount, validate_private_owner, winfsp_install_directory, winfsp_is_installed,
};

/// The official WinFsp download and installation website.
pub const WINFSP_INSTALL_URL: &str = "https://winfsp.dev/";

/// Whether the WinFsp runtime required by the Windows client is installed.
///
/// Deliberately a no-op returning `true` on other operating systems, matching
/// how `quickfs-macos-support` treats the macFUSE check off macOS.
#[must_use]
pub fn winfsp_available() -> bool {
    #[cfg(windows)]
    {
        windows::winfsp_is_installed()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Stops a Windows client command when the WinFsp runtime is not installed.
///
/// Always succeeds on non-Windows platforms.
pub fn require_winfsp() -> Result<(), WinFspRequired> {
    if winfsp_available() {
        Ok(())
    } else {
        Err(WinFspRequired)
    }
}

/// The actionable error returned when a Windows client starts without WinFsp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WinFspRequired;

impl fmt::Display for WinFspRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "WinFsp is required to use quicKFS on Windows. Install it from \
             {WINFSP_INSTALL_URL}, then run this command again"
        )
    }
}

impl Error for WinFspRequired {}

/// A Win32 operation that failed, carrying the code so callers can report
/// something a user can act on rather than a bare "permission denied".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsError {
    pub operation: &'static str,
    pub code: u32,
}

impl fmt::Display for WindowsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with Windows error {}",
            self.operation, self.code
        )
    }
}

impl Error for WindowsError {}

impl From<WindowsError> for std::io::Error {
    fn from(value: WindowsError) -> Self {
        std::io::Error::from_raw_os_error(value.code as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_missing_runtime_error_names_the_install_site() {
        let message = WinFspRequired.to_string();
        assert!(message.contains(WINFSP_INSTALL_URL));
        assert!(message.contains("WinFsp"));
    }

    #[cfg(not(windows))]
    #[test]
    fn the_runtime_check_is_inert_off_windows() {
        assert!(winfsp_available());
        assert!(require_winfsp().is_ok());
    }
}
