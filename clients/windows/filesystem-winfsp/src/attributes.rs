// SPDX-License-Identifier: Apache-2.0
//! POSIX metadata to Windows file attributes and times.
//!
//! POSIX permissions and Windows ACLs are not the same model and this module
//! does not pretend otherwise. What it defines is a small, total, lossless-where
//! -it-can-be mapping, plus an explicit statement of what is dropped:
//!
//! | quicKFS state              | Windows presentation                     | Persisted back? |
//! |----------------------------|------------------------------------------|-----------------|
//! | `NodeKind::Directory`      | `FILE_ATTRIBUTE_DIRECTORY`               | n/a             |
//! | owner write bit clear      | `FILE_ATTRIBUTE_READONLY`                | yes, via `mode` |
//! | leading `.` in the name    | `FILE_ATTRIBUTE_HIDDEN`                  | no              |
//! | group and other mode bits  | not shown                                | no              |
//! | uid / gid                  | not shown; the mount owner owns every node | no            |
//! | `FILE_ATTRIBUTE_ARCHIVE`   | always set on files                      | no              |
//! | `FILE_ATTRIBUTE_SYSTEM`    | never set                                | no              |
//!
//! The pieces that are *not* persisted are accepted and discarded rather than
//! rejected. Windows applications set the archive bit constantly, and a copy
//! that fails because the destination cannot store it would be a worse outcome
//! than a copy whose archive bit does not survive. The one attribute a user can
//! actually change and observe — read-only — is mapped onto the mode bits and
//! does survive, because that is the one the shell exposes in a file's
//! properties dialog.
//!
//! Ownership deserves the same honesty. The mount presents every node as owned
//! by the account that started the mount, with rights taken from the *owner*
//! mode bits, and grants nothing to any other account. That mirrors what the
//! macFUSE bridge does on macOS, where every node reports the mounting user's
//! uid. Real authorisation still happens on the server against the
//! authenticated quicKFS account; the Windows-side descriptor can only ever be
//! at least as restrictive, never more permissive.

use quickfs_protocol::NodeKind;

// The `FILE_ATTRIBUTE_*` constants, defined here rather than imported so that
// the mapping and its tests build on every platform in the workspace.
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Owner write bits in a POSIX mode.
const OWNER_WRITE: u32 = 0o200;
/// Every write bit; clearing read-only restores the ones the umask allows.
const ALL_WRITE: u32 = 0o222;

/// 100-nanosecond intervals between the Windows epoch (1601-01-01) and the Unix
/// epoch (1970-01-01).
pub const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

/// Convert Unix milliseconds to a Windows `FILETIME`.
///
/// Saturates instead of wrapping: a corrupt or hostile timestamp must not be
/// able to turn into a plausible-looking different time, and Windows treats a
/// saturated time as simply very far in the future.
pub fn filetime_from_unix_ms(milliseconds: u64) -> u64 {
    milliseconds
        .saturating_mul(10_000)
        .saturating_add(UNIX_EPOCH_AS_FILETIME)
}

/// Convert a Windows `FILETIME` to Unix milliseconds.
///
/// Returns `None` for the two values that do not denote a time: `0` means "do
/// not change this timestamp" in `SetBasicInformation`, and any time before the
/// Unix epoch cannot be represented in the protocol's unsigned milliseconds.
pub fn unix_ms_from_filetime(filetime: u64) -> Option<u64> {
    if filetime == 0 {
        return None;
    }
    filetime
        .checked_sub(UNIX_EPOCH_AS_FILETIME)
        .map(|since_epoch| since_epoch / 10_000)
}

/// The Windows attribute word for one node.
pub fn file_attributes(kind: NodeKind, mode: u32, hidden: bool) -> u32 {
    let mut attributes = match kind {
        NodeKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
        // A symlink is presented as its own kind of placeholder rather than as
        // a resolved target: the export may point outside itself, and silently
        // following such a link is how a share turns into a traversal.
        NodeKind::Symlink => FILE_ATTRIBUTE_REPARSE_POINT,
        _ => FILE_ATTRIBUTE_ARCHIVE,
    };
    if mode & OWNER_WRITE == 0 {
        attributes |= FILE_ATTRIBUTE_READONLY;
    }
    if hidden {
        attributes |= FILE_ATTRIBUTE_HIDDEN;
    }
    if attributes == 0 {
        attributes = FILE_ATTRIBUTE_NORMAL;
    }
    attributes
}

/// The mode a node should have after Windows sets or clears its read-only
/// attribute, or `None` when the request would not change anything.
///
/// The mapping is deliberately asymmetric. Setting read-only clears every write
/// bit, because "read-only" in Explorer means nobody writes this. Clearing it
/// restores only the *owner* write bit, because Windows cannot tell us anything
/// about group or other and inferring a grant for them from a Windows checkbox
/// would widen access on the server that the user never asked for. So `0o444`
/// becomes `0o644`, not `0o666`.
///
/// Returning `None` for a no-op keeps `SetBasicInformation` — which Windows
/// issues on almost every file close — from turning into a network round trip
/// whenever nothing has actually changed.
pub fn apply_readonly(mode: u32, readonly: bool) -> Option<u32> {
    let currently_readonly = mode & OWNER_WRITE == 0;
    if currently_readonly == readonly {
        return None;
    }
    let updated = if readonly {
        mode & !ALL_WRITE
    } else {
        mode | OWNER_WRITE
    };
    (updated != mode).then_some(updated)
}

/// The `GENERIC`-equivalent access mask an account should be granted for a node
/// with this mode, in the numeric form SDDL accepts.
///
/// Only the owner bits are consulted, because the descriptor names exactly one
/// account: the mount owner. Group and other bits have no Windows account to
/// attach to and are dropped rather than approximated onto `Everyone`, which
/// would widen access on a multi-user machine.
pub fn access_mask(mode: u32, kind: NodeKind) -> u32 {
    // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | SYNCHRONIZE, and the write set
    // separately, spelled out so the mapping does not depend on which
    // `windows-sys` release is in the tree.
    const READ: u32 = 0x0012_0089;
    const WRITE: u32 = 0x0012_0116;
    const EXECUTE: u32 = 0x0012_00A0;
    const DELETE: u32 = 0x0001_0000;

    let mut mask = 0;
    if mode & 0o400 != 0 {
        mask |= READ;
    }
    if mode & OWNER_WRITE != 0 {
        // Deleting a file is governed by the parent directory on Unix, but
        // Windows checks `DELETE` on the object itself, so a writable node has
        // to carry it or nothing could ever be removed through Explorer.
        mask |= WRITE | DELETE;
    }
    if mode & 0o100 != 0 || kind == NodeKind::Directory {
        // Traversing a directory is the execute bit on Unix and
        // `FILE_TRAVERSE` (same numeric value as `FILE_EXECUTE`) on Windows.
        mask |= EXECUTE;
    }
    mask
}

/// Round a byte count up to the allocation granularity a Windows volume
/// reports. Explorer shows this as "size on disk"; reporting zero for a
/// non-empty file makes some installers believe the copy failed.
pub fn allocation_size(size: u64, unit: u64) -> u64 {
    if unit == 0 {
        return size;
    }
    size.div_ceil(unit).saturating_mul(unit)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_unix_epoch_is_the_windows_epoch_offset() {
        assert_eq!(filetime_from_unix_ms(0), UNIX_EPOCH_AS_FILETIME);
        assert_eq!(unix_ms_from_filetime(UNIX_EPOCH_AS_FILETIME), Some(0));
        // One second, both ways.
        assert_eq!(
            filetime_from_unix_ms(1_000),
            UNIX_EPOCH_AS_FILETIME + 10_000_000
        );
        assert_eq!(
            unix_ms_from_filetime(UNIX_EPOCH_AS_FILETIME + 10_000_000),
            Some(1_000)
        );
    }

    #[test]
    fn times_that_do_not_denote_an_instant_are_rejected_not_wrapped() {
        // Zero is Windows' "leave this timestamp alone" sentinel.
        assert_eq!(unix_ms_from_filetime(0), None);
        // 1601-01-02 predates the Unix epoch and has no representation.
        assert_eq!(unix_ms_from_filetime(864_000_000_000), None);
        // A time far past the year 30000 saturates rather than wrapping to a
        // plausible recent date.
        assert_eq!(filetime_from_unix_ms(u64::MAX), u64::MAX);
    }

    #[test]
    fn a_directory_is_a_directory_and_a_file_carries_the_archive_bit() {
        let directory = file_attributes(NodeKind::Directory, 0o755, false);
        assert_eq!(
            directory & FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_DIRECTORY
        );
        assert_eq!(directory & FILE_ATTRIBUTE_READONLY, 0);

        let file = file_attributes(NodeKind::File, 0o644, false);
        assert_eq!(file & FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_ARCHIVE);
        assert_eq!(file & FILE_ATTRIBUTE_DIRECTORY, 0);

        let link = file_attributes(NodeKind::Symlink, 0o777, false);
        assert_eq!(
            link & FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_REPARSE_POINT
        );
    }

    #[test]
    fn the_owner_write_bit_is_the_read_only_attribute() {
        assert_eq!(
            file_attributes(NodeKind::File, 0o444, false) & FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_READONLY
        );
        // Group and other write bits are not the owner's, so they do not clear
        // the attribute.
        assert_eq!(
            file_attributes(NodeKind::File, 0o466, false) & FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_READONLY
        );
        assert_eq!(
            file_attributes(NodeKind::File, 0o600, false) & FILE_ATTRIBUTE_READONLY,
            0
        );
    }

    #[test]
    fn hidden_is_presentation_only_and_never_alters_the_mode() {
        let hidden = file_attributes(NodeKind::File, 0o644, true);
        assert_eq!(hidden & FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_HIDDEN);
        // There is no reverse mapping: nothing in this module turns a hidden
        // request into a mode change.
        assert_eq!(apply_readonly(0o644, false), None);
    }

    #[test]
    fn read_only_round_trips_through_the_mode_and_no_ops_are_free() {
        assert_eq!(apply_readonly(0o644, true), Some(0o444));
        assert_eq!(apply_readonly(0o444, false), Some(0o644));
        assert_eq!(apply_readonly(0o750, true), Some(0o550));

        // Clearing read-only restores the owner's write bit and nothing else:
        // a Windows checkbox must never grant the group or the world write
        // access on the server.
        assert_eq!(apply_readonly(0o550, false), Some(0o750));
        assert_eq!(apply_readonly(0o444, false), Some(0o644));
        assert_eq!(apply_readonly(0o400, false), Some(0o600));

        // Already in the requested state: no round trip.
        assert_eq!(apply_readonly(0o644, false), None);
        assert_eq!(apply_readonly(0o444, true), None);
    }

    #[test]
    fn the_access_mask_narrows_with_the_owner_bits_and_never_widens() {
        let full = access_mask(0o700, NodeKind::File);
        let read_only = access_mask(0o400, NodeKind::File);
        let none = access_mask(0o000, NodeKind::File);

        assert_ne!(full & 0x0002, 0, "writable nodes carry FILE_WRITE_DATA");
        assert_eq!(read_only & 0x0002, 0, "read-only nodes do not");
        assert_eq!(none, 0, "a mode with no owner bits grants nothing");
        // Every bit a narrower mode grants is also granted by a wider one.
        assert_eq!(read_only & full, read_only);
        // A directory is always traversable even with no execute bit, because
        // an untraversable directory cannot be enumerated at all.
        assert_ne!(access_mask(0o600, NodeKind::Directory) & 0x0020, 0);
    }

    #[test]
    fn allocation_size_rounds_up_to_whole_units() {
        assert_eq!(allocation_size(0, 4_096), 0);
        assert_eq!(allocation_size(1, 4_096), 4_096);
        assert_eq!(allocation_size(4_096, 4_096), 4_096);
        assert_eq!(allocation_size(4_097, 4_096), 8_192);
        // A zero unit cannot round anything and must not divide by zero.
        assert_eq!(allocation_size(1_234, 0), 1_234);
    }
}
