// SPDX-License-Identifier: Apache-2.0
//! POSIX symbolic links presented as Windows symbolic-link reparse points.
//!
//! A quicKFS link stores a POSIX target: a byte string such as `../music` or
//! `/srv/media/x`. Windows stores a symbolic link as an
//! `IO_REPARSE_TAG_SYMLINK` reparse buffer holding a UTF-16 path. This module
//! translates between the two, and is deliberately strict about one thing:
//! **a link presented through the mount can never resolve outside the mount.**
//!
//! * Relative targets map directly, with `/` becoming `\`.
//! * Absolute targets name a path on the *server*, which means nothing on the
//!   Windows machine; following one literally would open `\srv\media\x` on the
//!   user's own drive. They are presented instead as relative links that climb
//!   to the export root and descend from there, so `/a/b` from `\x\y\link`
//!   becomes `..\..\a\b`. That is the reading under which an export holding
//!   absolute links into itself keeps working, and every other reading stays
//!   inside the volume.
//! * Relative targets are resolved the same way, clamped at the export root,
//!   and written back as the shortest relative path, so `..` past the root
//!   cannot climb out of the volume either.
//! * A target that Windows would read as a drive, a device, or a UNC path —
//!   anything with `:` or `\` — is not presented as a link at all.
//!
//! Links created from Windows go the other way and must be relative: an
//! absolute Windows target (`C:\x`) has no meaning on the server.

/// `IO_REPARSE_TAG_SYMLINK`.
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// `SYMLINK_FLAG_RELATIVE`: the substitute name is relative to the link's
/// directory.
const SYMLINK_FLAG_RELATIVE: u32 = 1;

/// Bytes before `PathBuffer`: the generic reparse header (tag, data length,
/// reserved) and the symbolic-link header (four name offsets/lengths, flags).
const SYMLINK_HEADER_BYTES: usize = 8 + 12;

/// Why a link cannot be translated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparseError {
    /// The target is empty, not UTF-8, or contains `:`, `\` or NUL.
    UnrepresentableTarget,
    /// The buffer is not a well-formed symbolic-link reparse buffer.
    Malformed,
    /// A reparse tag other than a symbolic link (a junction, a cloud file…).
    UnsupportedTag,
    /// A Windows link with an absolute target, which the server cannot store
    /// meaningfully.
    AbsoluteTarget,
    /// The caller's buffer is too small for the encoded link.
    BufferTooSmall,
}

/// The Windows spelling of a POSIX link target, as a relative path.
///
/// `link_directory` is the components of the directory holding the link,
/// relative to the export root. The target is first resolved against it —
/// clamped at the root, as the server clamps it — and then written as the
/// shortest relative path from the link's directory, so no spelling of the
/// target, however many `..` it holds, can climb out of the volume.
pub fn windows_target(target: &[u8], link_directory: &[String]) -> Result<String, ReparseError> {
    let text = std::str::from_utf8(target).map_err(|_| ReparseError::UnrepresentableTarget)?;
    if text.is_empty() || text.contains([':', '\\', '\0']) {
        return Err(ReparseError::UnrepresentableTarget);
    }
    let resolved =
        target_components(link_directory, target).ok_or(ReparseError::UnrepresentableTarget)?;
    let shared = link_directory
        .iter()
        .zip(&resolved)
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts: Vec<&str> = vec![".."; link_directory.len() - shared];
    parts.extend(resolved[shared..].iter().map(String::as_str));
    Ok(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("\\")
    })
}

/// The POSIX target to store for a link Windows asked to create.
pub fn posix_target(target: &str, relative: bool) -> Result<Vec<u8>, ReparseError> {
    if !relative {
        return Err(ReparseError::AbsoluteTarget);
    }
    if target.is_empty() || target.contains([':', '\0', '/']) || target.starts_with('\\') {
        return Err(ReparseError::UnrepresentableTarget);
    }
    Ok(target.replace('\\', "/").into_bytes())
}

/// Encode a relative symbolic-link reparse buffer into `buffer`, returning the
/// number of bytes written.
pub fn encode_symlink(target: &str, buffer: &mut [u8]) -> Result<usize, ReparseError> {
    let path: Vec<u8> = target
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let path_bytes = u16::try_from(path.len()).map_err(|_| ReparseError::UnrepresentableTarget)?;
    // The substitute and print names are the same string, stored once each.
    let names = usize::from(path_bytes) * 2;
    let total = SYMLINK_HEADER_BYTES + names;
    let data_length = u16::try_from(total - 8).map_err(|_| ReparseError::UnrepresentableTarget)?;
    if buffer.len() < total {
        return Err(ReparseError::BufferTooSmall);
    }
    let mut cursor = 0;
    let mut put = |bytes: &[u8]| {
        buffer[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        cursor += bytes.len();
    };
    put(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    put(&data_length.to_le_bytes());
    put(&0_u16.to_le_bytes()); // reserved
    put(&0_u16.to_le_bytes()); // substitute name offset
    put(&path_bytes.to_le_bytes()); // substitute name length
    put(&path_bytes.to_le_bytes()); // print name offset
    put(&path_bytes.to_le_bytes()); // print name length
    put(&SYMLINK_FLAG_RELATIVE.to_le_bytes());
    put(&path);
    put(&path);
    Ok(total)
}

/// Decode a symbolic-link reparse buffer into its substitute name and whether
/// it is relative.
pub fn decode_symlink(buffer: &[u8]) -> Result<(String, bool), ReparseError> {
    let word = |at: usize| -> Result<u16, ReparseError> {
        buffer
            .get(at..at + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .ok_or(ReparseError::Malformed)
    };
    let tag = buffer
        .get(0..4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or(ReparseError::Malformed)?;
    if tag != IO_REPARSE_TAG_SYMLINK {
        return Err(ReparseError::UnsupportedTag);
    }
    let substitute_offset = usize::from(word(8)?);
    let substitute_length = usize::from(word(10)?);
    let flags = buffer
        .get(16..20)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or(ReparseError::Malformed)?;
    let start = SYMLINK_HEADER_BYTES + substitute_offset;
    let name = buffer
        .get(start..start + substitute_length)
        .ok_or(ReparseError::Malformed)?;
    let (pairs, rest) = name.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(ReparseError::Malformed);
    }
    let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
    let text = String::from_utf16(&units).map_err(|_| ReparseError::Malformed)?;
    Ok((text, flags & SYMLINK_FLAG_RELATIVE != 0))
}

/// The components a link's target names, relative to the export root, given
/// the components of the directory holding the link. `..` past the root stays
/// at the root, as it does on the server.
pub fn target_components(link_directory: &[String], target: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(target).ok()?;
    let mut components: Vec<String> = if text.starts_with('/') {
        Vec::new()
    } else {
        link_directory.to_vec()
    };
    for component in text.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            name => components.push(name.to_owned()),
        }
    }
    Some(components)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn directory(components: &[&str]) -> Vec<String> {
        components
            .iter()
            .map(|component| (*component).to_owned())
            .collect()
    }

    #[test]
    fn relative_targets_map_to_the_shortest_windows_path() {
        let here = directory(&["a", "b", "c"]);
        assert_eq!(
            windows_target(b"../music/a.mp3", &here).unwrap(),
            "..\\music\\a.mp3"
        );
        assert_eq!(windows_target(b"sibling", &here).unwrap(), "sibling");
        assert_eq!(windows_target(b"x//y/", &here).unwrap(), "x\\y");
        assert_eq!(windows_target(b"./z", &[]).unwrap(), "z");
    }

    #[test]
    fn absolute_targets_climb_to_the_export_root_instead_of_leaving_the_mount() {
        assert_eq!(
            windows_target(b"/a/b", &directory(&["x", "y"])).unwrap(),
            "..\\..\\a\\b"
        );
        assert_eq!(windows_target(b"/a", &[]).unwrap(), "a");
        assert_eq!(windows_target(b"/", &[]).unwrap(), ".");
        // An absolute target into the link's own directory stays short.
        assert_eq!(
            windows_target(b"/x/y/z", &directory(&["x", "y"])).unwrap(),
            "z"
        );
    }

    #[test]
    fn no_number_of_parent_steps_climbs_out_of_the_volume() {
        assert_eq!(
            windows_target(b"../../../../etc/passwd", &directory(&["a"])).unwrap(),
            "..\\etc\\passwd"
        );
        assert_eq!(
            windows_target(b"../..", &directory(&["a", "b"])).unwrap(),
            "..\\.."
        );
    }

    #[test]
    fn targets_windows_would_read_as_drives_or_devices_are_not_links() {
        for target in [
            &b"C:/Windows"[..],
            b"a\\b",
            b"\\\\server\\share",
            b"",
            b"x\0y",
            b"\xff",
        ] {
            assert_eq!(
                windows_target(target, &directory(&["a"])),
                Err(ReparseError::UnrepresentableTarget),
                "{target:?}"
            );
        }
    }

    #[test]
    fn a_reparse_buffer_round_trips() {
        let mut buffer = vec![0; 256];
        let written = encode_symlink("..\\café\\x", &mut buffer).unwrap();
        let (target, relative) = decode_symlink(&buffer[..written]).unwrap();
        assert_eq!(target, "..\\café\\x");
        assert!(relative);
        assert_eq!(
            encode_symlink("..\\café\\x", &mut buffer[..written - 1]),
            Err(ReparseError::BufferTooSmall)
        );
    }

    #[test]
    fn windows_links_must_be_relative_to_reach_the_server() {
        assert_eq!(posix_target("..\\a\\b", true).unwrap(), b"../a/b");
        assert_eq!(
            posix_target("\\??\\C:\\x", false),
            Err(ReparseError::AbsoluteTarget)
        );
        assert_eq!(
            posix_target("C:x", true),
            Err(ReparseError::UnrepresentableTarget)
        );
        assert_eq!(
            posix_target("\\rooted", true),
            Err(ReparseError::UnrepresentableTarget)
        );
    }

    #[test]
    fn other_reparse_tags_and_truncated_buffers_are_refused() {
        let mut junction = vec![0; 32];
        junction[0..4].copy_from_slice(&0xA000_0003_u32.to_le_bytes());
        assert_eq!(decode_symlink(&junction), Err(ReparseError::UnsupportedTag));
        assert_eq!(decode_symlink(&[0x0c, 0, 0]), Err(ReparseError::Malformed));
        let mut buffer = vec![0; 64];
        encode_symlink("abc", &mut buffer).unwrap();
        assert_eq!(
            decode_symlink(&buffer[..SYMLINK_HEADER_BYTES + 2]),
            Err(ReparseError::Malformed)
        );
    }

    #[test]
    fn target_components_resolve_against_the_link_directory_and_stop_at_the_root() {
        let directory = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(
            target_components(&directory, b"../c/./d").unwrap(),
            vec!["a", "c", "d"]
        );
        assert_eq!(target_components(&directory, b"/x").unwrap(), vec!["x"]);
        assert_eq!(
            target_components(&directory, b"../../../../y").unwrap(),
            vec!["y"]
        );
    }
}
