// SPDX-License-Identifier: Apache-2.0
//! quicKFS and adapter errors to `NTSTATUS`.
//!
//! This is the Windows counterpart of the macFUSE bridge's `errno` mapping, and
//! it is deliberately written against the same source errors so the two clients
//! cannot drift apart on what a given server error means. Where the macFUSE
//! bridge answers `ENOENT`, this answers `STATUS_OBJECT_NAME_NOT_FOUND`; where
//! it answers `ENOTEMPTY`, this answers `STATUS_DIRECTORY_NOT_EMPTY`; and so on.
//!
//! Getting this right is not cosmetic. Windows applications branch hard on
//! these codes: a `CopyFile` retries on `STATUS_SHARING_VIOLATION` but gives up
//! on `STATUS_ACCESS_DENIED`, and Explorer reports "the network name is no
//! longer available" — the right thing to show a user whose link dropped — only
//! for the specific device-not-connected codes. Collapsing everything to
//! `STATUS_UNSUCCESSFUL` would make every transient failure look permanent.
//!
//! The constants are spelled out here rather than imported so this module, and
//! the table it defines, compiles and is tested on every platform.

use quickfs_client_core::ClientError;
use quickfs_fs_adapter::AdapterError;
use quickfs_protocol::ErrorCode;

pub type NtStatus = i32;

pub const STATUS_SUCCESS: NtStatus = 0x0000_0000;
pub const STATUS_PENDING: NtStatus = 0x0000_0103;
pub const STATUS_OBJECT_NAME_NOT_FOUND: NtStatus = 0xC000_0034u32 as NtStatus;
pub const STATUS_OBJECT_NAME_COLLISION: NtStatus = 0xC000_0035u32 as NtStatus;
pub const STATUS_OBJECT_NAME_INVALID: NtStatus = 0xC000_0033u32 as NtStatus;
pub const STATUS_OBJECT_PATH_NOT_FOUND: NtStatus = 0xC000_003Au32 as NtStatus;
pub const STATUS_NOT_A_DIRECTORY: NtStatus = 0xC000_0103u32 as NtStatus;
pub const STATUS_FILE_IS_A_DIRECTORY: NtStatus = 0xC000_00BAu32 as NtStatus;
pub const STATUS_DIRECTORY_NOT_EMPTY: NtStatus = 0xC000_0101u32 as NtStatus;
pub const STATUS_ACCESS_DENIED: NtStatus = 0xC000_0022u32 as NtStatus;
pub const STATUS_MEDIA_WRITE_PROTECTED: NtStatus = 0xC000_00A2u32 as NtStatus;
pub const STATUS_DISK_FULL: NtStatus = 0xC000_007Fu32 as NtStatus;
pub const STATUS_FILE_TOO_LARGE: NtStatus = 0xC000_00A8u32 as NtStatus;
pub const STATUS_INVALID_PARAMETER: NtStatus = 0xC000_000Du32 as NtStatus;
pub const STATUS_INVALID_HANDLE: NtStatus = 0xC000_0008u32 as NtStatus;
pub const STATUS_NOT_IMPLEMENTED: NtStatus = 0xC000_0002u32 as NtStatus;
pub const STATUS_NOT_SUPPORTED: NtStatus = 0xC000_00BBu32 as NtStatus;
pub const STATUS_IO_TIMEOUT: NtStatus = 0xC000_00B5u32 as NtStatus;
pub const STATUS_IO_DEVICE_ERROR: NtStatus = 0xC000_009Cu32 as NtStatus;
pub const STATUS_DEVICE_NOT_CONNECTED: NtStatus = 0xC000_009Du32 as NtStatus;
pub const STATUS_FILE_CORRUPT_ERROR: NtStatus = 0xC000_0102u32 as NtStatus;
pub const STATUS_SHARING_VIOLATION: NtStatus = 0xC000_0043u32 as NtStatus;
pub const STATUS_DEVICE_BUSY: NtStatus = 0x8000_0011u32 as NtStatus;
pub const STATUS_TOO_MANY_OPENED_FILES: NtStatus = 0xC000_011Fu32 as NtStatus;
pub const STATUS_INSUFFICIENT_RESOURCES: NtStatus = 0xC000_009Au32 as NtStatus;
pub const STATUS_NO_EAS_ON_FILE: NtStatus = 0xC000_0052u32 as NtStatus;
pub const STATUS_END_OF_FILE: NtStatus = 0xC000_0011u32 as NtStatus;
pub const STATUS_BUFFER_OVERFLOW: NtStatus = 0x8000_0005u32 as NtStatus;
pub const STATUS_UNEXPECTED_NETWORK_ERROR: NtStatus = 0xC000_00C4u32 as NtStatus;
pub const STATUS_CANNOT_DELETE: NtStatus = 0xC000_0121u32 as NtStatus;

/// Translate an adapter error into the status WinFsp returns to the caller.
pub fn status_from_adapter_error(error: &AdapterError) -> NtStatus {
    match error {
        // A callback that outran its deadline is a link problem, not a file
        // problem: reporting a timeout lets an application retry rather than
        // conclude the file is gone.
        AdapterError::CallbackTimedOut => STATUS_IO_TIMEOUT,
        // The kernel is holding a reference to something the adapter no longer
        // knows. On Windows a stale reference is reported on the handle.
        AdapterError::UnknownInode => STATUS_INVALID_HANDLE,
        AdapterError::UnknownHandle
        | AdapterError::UnknownDirectoryHandle
        | AdapterError::HandleInodeMismatch
        | AdapterError::InvalidAccess => STATUS_INVALID_HANDLE,
        AdapterError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        AdapterError::InvalidName => STATUS_OBJECT_NAME_INVALID,
        AdapterError::InvalidRange => STATUS_INVALID_PARAMETER,
        // The server sent something that does not fit the protocol. This is a
        // corrupt answer, not a corrupt file, but `STATUS_FILE_CORRUPT_ERROR`
        // is the closest thing Windows surfaces and it stops an application
        // from treating the bytes as valid.
        AdapterError::InvalidRemoteName
        | AdapterError::UnexpectedMetadata
        | AdapterError::UnexpectedReadLength
        | AdapterError::UnexpectedWriteLength => STATUS_FILE_CORRUPT_ERROR,
        // Two entries in one directory that differ only by case cannot both be
        // addressed through a case-insensitive namespace, so refusing the
        // lookup is the only answer that cannot silently open the wrong file.
        AdapterError::AmbiguousName => STATUS_OBJECT_NAME_COLLISION,
        AdapterError::Runtime(_) | AdapterError::StateUnavailable => STATUS_IO_DEVICE_ERROR,
        AdapterError::InvalidCapabilities => STATUS_UNEXPECTED_NETWORK_ERROR,
        AdapterError::InodeSpaceExhausted | AdapterError::HandleSpaceExhausted => {
            STATUS_TOO_MANY_OPENED_FILES
        }
        AdapterError::StaleRevision => STATUS_FILE_CORRUPT_ERROR,
        AdapterError::RequestTooLarge(_) => STATUS_FILE_TOO_LARGE,
        AdapterError::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        AdapterError::Unsupported => STATUS_NOT_SUPPORTED,
        AdapterError::Client(client) => status_from_client_error(client),
    }
}

/// Translate a client-core error into an `NTSTATUS`.
pub fn status_from_client_error(error: &ClientError) -> NtStatus {
    match error {
        ClientError::Transport(_) => STATUS_UNEXPECTED_NETWORK_ERROR,
        ClientError::UnexpectedResponse => STATUS_UNEXPECTED_NETWORK_ERROR,
        ClientError::ReadTooLarge(_) | ClientError::WriteTooLarge(_) => STATUS_FILE_TOO_LARGE,
        ClientError::StaleRevision => STATUS_FILE_CORRUPT_ERROR,
        // "The device is not connected" is what makes Explorer say the network
        // location is unavailable and offer to retry, rather than reporting a
        // generic failure the user cannot act on.
        ClientError::Offline | ClientError::OfflineCacheMiss => STATUS_DEVICE_NOT_CONNECTED,
        // A mutation that may or may not have reached the server must never be
        // reported as a clean failure, or an application will retry a write
        // that already happened. An I/O error is the honest answer.
        ClientError::AmbiguousMutation => STATUS_IO_DEVICE_ERROR,
        ClientError::Server(code, _) => status_from_error_code(*code),
    }
}

/// Translate a protocol error code into an `NTSTATUS`.
pub fn status_from_error_code(code: ErrorCode) -> NtStatus {
    match code {
        ErrorCode::Unauthenticated | ErrorCode::PermissionDenied => STATUS_ACCESS_DENIED,
        ErrorCode::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        ErrorCode::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        ErrorCode::NotDirectory => STATUS_NOT_A_DIRECTORY,
        ErrorCode::IsDirectory => STATUS_FILE_IS_A_DIRECTORY,
        ErrorCode::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        ErrorCode::NoAttribute => STATUS_NO_EAS_ON_FILE,
        ErrorCode::NoData => STATUS_END_OF_FILE,
        ErrorCode::NotTty => STATUS_INVALID_PARAMETER,
        ErrorCode::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        // A revision conflict means someone else changed the node underneath
        // us. Windows has no `ESTALE`; a sharing violation is what an
        // application already knows how to retry.
        ErrorCode::Conflict => STATUS_SHARING_VIOLATION,
        ErrorCode::WouldBlock => STATUS_DEVICE_BUSY,
        ErrorCode::NoSpace => STATUS_DISK_FULL,
        ErrorCode::Busy => STATUS_DEVICE_BUSY,
        ErrorCode::NotSupported => STATUS_NOT_SUPPORTED,
        ErrorCode::Offline => STATUS_DEVICE_NOT_CONNECTED,
        ErrorCode::InvalidNode | ErrorCode::InvalidHandle => STATUS_INVALID_HANDLE,
        ErrorCode::InvalidRequest => STATUS_INVALID_PARAMETER,
        ErrorCode::UnsupportedVersion => STATUS_UNEXPECTED_NETWORK_ERROR,
        ErrorCode::TooLarge => STATUS_FILE_TOO_LARGE,
        ErrorCode::Timeout => STATUS_IO_TIMEOUT,
        ErrorCode::Internal => STATUS_IO_DEVICE_ERROR,
    }
}

/// Whether a status denotes "this name does not exist here".
///
/// WinFsp probes for absent files constantly — every create checks first — so
/// the bridge uses this to keep an ordinary miss out of the error log.
pub fn is_expected_absence(status: NtStatus) -> bool {
    status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quickfs_client_core::ClientError;

    fn server(code: ErrorCode) -> AdapterError {
        AdapterError::Client(ClientError::Server(code, String::new()))
    }

    #[test]
    fn every_status_this_module_returns_is_a_failure_code() {
        // An `NTSTATUS` whose top two bits are zero means success, and
        // returning one from a failed callback would make WinFsp report a
        // completed operation that never happened.
        let codes = [
            ErrorCode::NotFound,
            ErrorCode::AlreadyExists,
            ErrorCode::PermissionDenied,
            ErrorCode::NotEmpty,
            ErrorCode::NoSpace,
            ErrorCode::Offline,
            ErrorCode::Internal,
            ErrorCode::Timeout,
            ErrorCode::Conflict,
        ];
        for code in codes {
            let status = status_from_error_code(code);
            assert!(
                status < 0 || status == STATUS_DEVICE_BUSY,
                "{code:?} produced a non-error status {status:#x}"
            );
        }
    }

    #[test]
    fn the_shell_visible_errors_map_to_the_codes_explorer_understands() {
        assert_eq!(
            status_from_adapter_error(&AdapterError::NotFound),
            STATUS_OBJECT_NAME_NOT_FOUND
        );
        assert_eq!(
            status_from_adapter_error(&server(ErrorCode::AlreadyExists)),
            STATUS_OBJECT_NAME_COLLISION
        );
        assert_eq!(
            status_from_adapter_error(&server(ErrorCode::NotEmpty)),
            STATUS_DIRECTORY_NOT_EMPTY
        );
        assert_eq!(
            status_from_adapter_error(&server(ErrorCode::PermissionDenied)),
            STATUS_ACCESS_DENIED
        );
        assert_eq!(
            status_from_adapter_error(&AdapterError::ReadOnly),
            STATUS_MEDIA_WRITE_PROTECTED
        );
        assert_eq!(
            status_from_adapter_error(&server(ErrorCode::NoSpace)),
            STATUS_DISK_FULL
        );
    }

    #[test]
    fn a_dropped_link_is_reported_as_a_disconnect_and_not_as_a_missing_file() {
        // If a disconnect surfaced as "file not found", an application would
        // conclude the user's data had been deleted.
        for error in [
            AdapterError::Client(ClientError::Offline),
            AdapterError::Client(ClientError::OfflineCacheMiss),
            server(ErrorCode::Offline),
        ] {
            let status = status_from_adapter_error(&error);
            assert_eq!(status, STATUS_DEVICE_NOT_CONNECTED, "{error:?}");
            assert!(!is_expected_absence(status));
        }
    }

    #[test]
    fn an_ambiguous_mutation_never_looks_like_a_clean_failure() {
        // Reporting anything an application treats as "nothing happened" would
        // invite it to retry a write the server may already have applied.
        let status =
            status_from_adapter_error(&AdapterError::Client(ClientError::AmbiguousMutation));
        assert_eq!(status, STATUS_IO_DEVICE_ERROR);
        assert_ne!(status, STATUS_OBJECT_NAME_NOT_FOUND);
        assert_ne!(status, STATUS_SUCCESS);
    }

    #[test]
    fn a_timed_out_callback_is_distinguishable_from_a_broken_one() {
        assert_eq!(
            status_from_adapter_error(&AdapterError::CallbackTimedOut),
            STATUS_IO_TIMEOUT
        );
        assert_ne!(
            status_from_adapter_error(&AdapterError::CallbackTimedOut),
            status_from_adapter_error(&AdapterError::StateUnavailable)
        );
    }

    #[test]
    fn only_genuine_absences_are_treated_as_expected() {
        assert!(is_expected_absence(STATUS_OBJECT_NAME_NOT_FOUND));
        assert!(is_expected_absence(STATUS_OBJECT_PATH_NOT_FOUND));
        assert!(!is_expected_absence(STATUS_ACCESS_DENIED));
        assert!(!is_expected_absence(STATUS_DEVICE_NOT_CONNECTED));
        assert!(!is_expected_absence(STATUS_SUCCESS));
    }

    #[test]
    fn a_name_only_windows_dislikes_is_invalid_and_not_missing() {
        // Windows distinguishes "you may not ask for that name" from "that name
        // is not here", and installers rely on the difference.
        assert_eq!(
            status_from_adapter_error(&AdapterError::InvalidName),
            STATUS_OBJECT_NAME_INVALID
        );
        assert_ne!(
            status_from_adapter_error(&AdapterError::InvalidName),
            STATUS_OBJECT_NAME_NOT_FOUND
        );
    }
}
