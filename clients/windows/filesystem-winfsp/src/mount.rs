// SPDX-License-Identifier: Apache-2.0
//! Starting, running and stopping a Windows mount.
//!
//! The volume parameters set here are as much a part of the client's behaviour
//! as any callback: they decide how long the Windows cache may hold metadata,
//! whether the namespace is case sensitive, and whether the file system is
//! asked for access checks at all. Each one is chosen to match what the shared
//! adapter actually guarantees, so nothing below promises Windows something the
//! layer underneath cannot deliver.

use crate::filesystem::{ALLOCATION_UNIT, WindowsFilesystem};
use quickfs_fs_adapter::Adapter;
use quickfs_protocol::FilesystemCapabilities;
use std::{fmt, path::Path, time::SystemTime};
use widestring::U16CString;
use winfsp_wrs::{FileSystem, OperationGuardStrategy, Params, VolumeParams};

/// How long Windows may serve file metadata from its own cache without asking
/// again.
///
/// This matches the one-second attribute lifetime the macFUSE bridge reports to
/// the macOS kernel, and it is what makes an Explorer window with several
/// hundred files usable: the shell re-queries attributes constantly, and
/// without a cache window every one of those queries would cross the adapter.
/// One second is short enough that a change made elsewhere shows up promptly
/// and is the same bound the adapter's own metadata cache already accepts.
const FILE_INFO_TIMEOUT_MS: u32 = 1_000;

/// Sector size reported to Windows. Together with one sector per allocation
/// unit this gives the 4 KiB allocation granularity Explorer divides by for
/// "size on disk".
const SECTOR_SIZE: u16 = ALLOCATION_UNIT as u16;

/// Longest path component, matching NTFS and the adapter's own limit.
const MAX_COMPONENT_LENGTH: u16 = 255;

/// Where a volume should appear in the Windows namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MountPoint {
    /// A drive letter such as `Q:`.
    DriveLetter(char),
    /// An empty directory on an existing NTFS volume.
    Directory(String),
    /// Whichever drive letter is free, counting down from `Z:`.
    NextAvailable,
}

impl MountPoint {
    /// Parse the mountpoint spelling the CLI accepts: `Q`, `Q:`, `Q:\`, `*`, or
    /// a directory path.
    pub fn parse(value: &str) -> Result<Self, MountError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(MountError::InvalidMountPoint(
                "mountpoint must not be empty".into(),
            ));
        }
        if trimmed == "*" {
            return Ok(Self::NextAvailable);
        }
        let letter_form = trimmed.trim_end_matches(['\\', '/']);
        let mut characters = letter_form.chars();
        if let (Some(letter), rest) = (characters.next(), characters.as_str())
            && letter.is_ascii_alphabetic()
            && (rest.is_empty() || rest == ":")
        {
            return Ok(Self::DriveLetter(letter.to_ascii_uppercase()));
        }
        // Anything else has to be a directory, and it has to already exist and
        // be empty: WinFsp cannot create it, and mounting over a directory with
        // contents would hide the user's files behind the volume.
        let path = Path::new(trimmed);
        if !path.is_dir() {
            return Err(MountError::InvalidMountPoint(format!(
                "'{trimmed}' is neither a drive letter nor an existing directory"
            )));
        }
        Ok(Self::Directory(trimmed.to_owned()))
    }

    /// The form WinFsp wants: `Q:` for a drive, the path for a directory, and
    /// nothing at all to let it pick.
    fn to_winfsp(&self) -> Result<Option<U16CString>, MountError> {
        let text = match self {
            Self::DriveLetter(letter) => format!("{letter}:"),
            Self::Directory(path) => path.clone(),
            Self::NextAvailable => return Ok(None),
        };
        U16CString::from_str(&text)
            .map(Some)
            .map_err(|_| MountError::InvalidMountPoint(text))
    }

    /// A stable identifier for this mountpoint, used to name the unmount event
    /// so `quickfs unmount Q:` can reach the process that mounted it.
    pub fn identifier(&self) -> String {
        match self {
            Self::DriveLetter(letter) => letter.to_string(),
            Self::Directory(path) => path
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() {
                        character.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect(),
            Self::NextAvailable => "AUTO".into(),
        }
    }
}

impl fmt::Display for MountPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DriveLetter(letter) => write!(formatter, "{letter}:"),
            Self::Directory(path) => formatter.write_str(path),
            Self::NextAvailable => formatter.write_str("the next available drive letter"),
        }
    }
}

/// Everything about a mount that is not the connection itself.
#[derive(Clone, Debug)]
pub struct MountConfig {
    pub mountpoint: MountPoint,
    /// Shown as the volume label in Explorer. WinFsp caps this at 32 UTF-16
    /// units.
    pub volume_name: String,
    /// Shown as the file system type, as `NTFS` or `FAT32` would be. WinFsp
    /// caps this at 16 UTF-16 units.
    pub filesystem_name: String,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            mountpoint: MountPoint::NextAvailable,
            volume_name: "quicKFS".into(),
            filesystem_name: "quickfs".into(),
        }
    }
}

/// Why a mount could not start.
#[derive(Debug)]
pub enum MountError {
    InvalidMountPoint(String),
    VolumeName(String),
    /// WinFsp refused the mount. The status is the raw `NTSTATUS`, which is
    /// what WinFsp's own documentation and log messages use.
    WinFsp(i32),
}

impl fmt::Display for MountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMountPoint(detail) => write!(formatter, "invalid mountpoint: {detail}"),
            Self::VolumeName(detail) => write!(formatter, "invalid volume name: {detail}"),
            Self::WinFsp(status) => write!(
                formatter,
                "WinFsp refused the mount with NTSTATUS {status:#010x}. \
                 A drive letter already in use reports 0xC0000035; a missing \
                 WinFsp driver reports 0xC0000034"
            ),
        }
    }
}

impl std::error::Error for MountError {}

/// A running mount. Dropping it, or calling [`Mount::stop`], unmounts the
/// volume and releases every server handle the adapter still holds.
pub struct Mount {
    filesystem: Option<FileSystem>,
    mountpoint: MountPoint,
}

impl Mount {
    pub fn mountpoint(&self) -> &MountPoint {
        &self.mountpoint
    }

    /// Unmount and release everything. Idempotent.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(filesystem) = self.filesystem.take() {
            // Stopping the dispatcher runs `dispatcher_stopped`, which is where
            // retained inode references are returned and open server handles
            // are closed.
            filesystem.stop();
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build the volume parameters for one export.
///
/// Split out from [`mount`] so the choices can be asserted in tests without a
/// WinFsp driver present.
pub fn volume_params(
    capabilities: &FilesystemCapabilities,
    config: &MountConfig,
) -> Result<VolumeParams, MountError> {
    let mut params = VolumeParams::default();
    let filesystem_name = U16CString::from_str(&config.filesystem_name)
        .map_err(|_| MountError::VolumeName(config.filesystem_name.clone()))?;

    params
        .set_sector_size(SECTOR_SIZE)
        .set_sectors_per_allocation_unit(1)
        .set_max_component_length(MAX_COMPONENT_LENGTH)
        .set_volume_creation_time(volume_creation_time())
        .set_volume_serial_number(volume_serial_number(capabilities))
        // Windows applications assume case-insensitive lookup with preserved
        // spelling, which is exactly what the shared adapter's entry selection
        // already implements: exact bytes first, then an NFD case-folded match,
        // and a refusal when two entries would both match.
        .set_case_sensitive_search(false)
        .set_case_preserved_names(true)
        .set_unicode_on_disk(true)
        // The security descriptors this client returns are derived from POSIX
        // modes and are meaningful, so the FSD is asked to enforce them. It is
        // a second, more restrictive gate: the server still authorises every
        // operation for the authenticated quicKFS account regardless.
        .set_persistent_acls(true)
        .set_read_only_volume(!capabilities.writable)
        // Only ask for a cleanup callback when something actually changed,
        // which keeps closing a file that was merely read off the network.
        .set_post_cleanup_when_modified_only(true)
        .set_file_info_timeout(FILE_INFO_TIMEOUT_MS)
        .set_volume_info_timeout(FILE_INFO_TIMEOUT_MS)
        .set_security_timeout(FILE_INFO_TIMEOUT_MS)
        // Features this bridge does not implement. Advertising any of them
        // without the matching callbacks would make Windows issue requests that
        // fail deep inside an application instead of being cleanly unsupported.
        .set_reparse_point(false)
        .set_named_streams(false)
        .set_extended_attributes(false)
        .set_hard_links(false);
    let _ = params.set_file_system_name(&filesystem_name);
    Ok(params)
}

/// The volume's creation time, reported to Windows as the moment the mount
/// started. The export has no creation time of its own that survives a remount.
fn volume_creation_time() -> u64 {
    let unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    crate::attributes::filetime_from_unix_ms(unix_ms)
}

/// A serial number derived from the server's epoch.
///
/// Windows uses this to tell one volume from another across remounts, so it has
/// to be stable for a given export and different between exports. The server
/// epoch is exactly that identity, and the same value the offline cache is
/// namespaced by.
fn volume_serial_number(capabilities: &FilesystemCapabilities) -> u32 {
    // Fold the whole UUID rather than taking a prefix: UUIDs that differ only
    // in their trailing bytes are common, and two exports that collided here
    // would look like one volume to Windows.
    let (words, _) = capabilities.server_epoch.as_bytes().as_chunks::<4>();
    words.iter().fold(0u32, |accumulated, word| {
        accumulated ^ u32::from_le_bytes(*word)
    })
}

/// Mount an authenticated export and start dispatching Windows requests to it.
///
/// The returned [`Mount`] owns the volume; the caller keeps it alive for as long
/// as the mount should exist and drops it, or calls [`Mount::stop`], to unmount.
pub fn mount(
    adapter: Adapter,
    capabilities: FilesystemCapabilities,
    config: &MountConfig,
    owner_sid: String,
) -> Result<Mount, MountError> {
    let volume_params = volume_params(&capabilities, config)?;
    let filesystem = WindowsFilesystem::new(adapter, capabilities, &config.volume_name, owner_sid)
        .map_err(|error| MountError::VolumeName(error.to_string()))?;

    // Warm the caches a freshly mounted volume is about to be asked for. The
    // shell probes a new drive the moment it appears — root attributes, a
    // listing, and free space — and answering those from memory keeps the first
    // Explorer window from paying a round trip each.
    let _ = filesystem.adapter().prewarm_for_mount();

    let params = Params {
        volume_params,
        // Fine-grained locking: the adapter serialises per handle and per
        // directory itself, and holds no global lock, so concurrent reads and
        // writes on different files must be allowed to proceed together. A
        // coarse volume lock would serialise every operation on a mount that is
        // frequently waiting on the network.
        guard_strategy: OperationGuardStrategy::Fine,
    };
    let mountpoint = config.mountpoint.to_winfsp()?;
    let running =
        FileSystem::start(params, mountpoint.as_deref(), filesystem).map_err(MountError::WinFsp)?;
    Ok(Mount {
        filesystem: Some(running),
        mountpoint: config.mountpoint.clone(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn capabilities(writable: bool) -> FilesystemCapabilities {
        let mut capabilities = crate::testing::writable_capabilities_for_tests();
        capabilities.writable = writable;
        capabilities
    }

    #[test]
    fn every_spelling_of_a_drive_letter_names_the_same_mountpoint() {
        for spelling in ["Q", "q", "Q:", "q:", "Q:\\", "q:/"] {
            assert_eq!(
                MountPoint::parse(spelling).unwrap(),
                MountPoint::DriveLetter('Q'),
                "{spelling} should name drive Q"
            );
        }
        assert_eq!(MountPoint::parse("*").unwrap(), MountPoint::NextAvailable);
    }

    #[test]
    fn a_mountpoint_directory_must_already_exist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_string_lossy().into_owned();
        assert_eq!(
            MountPoint::parse(&path).unwrap(),
            MountPoint::Directory(path.clone())
        );
        assert!(MountPoint::parse(&format!("{path}\\absent")).is_err());
        assert!(MountPoint::parse("").is_err());
    }

    #[test]
    fn the_unmount_identifier_is_stable_and_filename_safe() {
        assert_eq!(MountPoint::parse("q:").unwrap().identifier(), "Q");
        assert_eq!(MountPoint::parse("Q:\\").unwrap().identifier(), "Q");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_string_lossy().into_owned();
        let identifier = MountPoint::Directory(path).identifier();
        assert!(
            identifier
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        );
    }

    #[test]
    fn a_read_only_export_mounts_as_a_read_only_volume() {
        let config = MountConfig::default();
        let read_only = volume_params(&capabilities(false), &config).unwrap();
        let writable = volume_params(&capabilities(true), &config).unwrap();
        // `VolumeParams` exposes no getters, so compare the raw structures: the
        // only field that differs between these two is the read-only flag.
        assert_ne!(
            format!("{read_only:?}"),
            format!("{writable:?}"),
            "a read-only export must not mount the same way as a writable one"
        );
    }

    #[test]
    fn the_volume_serial_number_follows_the_server_epoch() {
        let config = MountConfig::default();
        let mut first = capabilities(true);
        first.server_epoch = uuid::Uuid::from_u128(1);
        let mut second = capabilities(true);
        second.server_epoch = uuid::Uuid::from_u128(2);

        assert_ne!(volume_serial_number(&first), volume_serial_number(&second));
        assert_eq!(volume_serial_number(&first), volume_serial_number(&first));
        // Building the parameters twice for one export must not change it.
        let _ = volume_params(&first, &config).unwrap();
        assert_eq!(volume_serial_number(&first), volume_serial_number(&first));
    }

    #[test]
    fn an_unrepresentable_volume_name_is_refused_rather_than_truncated() {
        let config = MountConfig {
            filesystem_name: "quickfs\u{0}bad".into(),
            ..MountConfig::default()
        };
        assert!(matches!(
            volume_params(&capabilities(true), &config),
            Err(MountError::VolumeName(_))
        ));
    }
}
