// SPDX-License-Identifier: Apache-2.0
//! Extended attributes presented as NTFS named streams.
//!
//! A quicKFS node can carry extended attributes — Finder information and
//! resource forks written from macOS, `user.*` attributes written on Linux.
//! Windows has no xattr API, but it has alternate data streams, and the two map
//! cleanly: the attribute `com.apple.ResourceFork` on `photo.jpg` is the stream
//! `photo.jpg:com.apple.ResourceFork`. Windows applications that already carry
//! streams across copies — Explorer, `robocopy`, the `Zone.Identifier` mark a
//! browser adds to a download — therefore carry quicKFS attributes too, and a
//! macOS client sees whatever a Windows one wrote.
//!
//! Two rules keep this from changing what existing names mean:
//!
//! * A file whose stored name contains `:` — created from Linux — keeps
//!   opening as that file. Only when no such file exists is `a:b` read as the
//!   stream `b` of `a`.
//! * An attribute whose name Windows cannot spell as a stream is left out of
//!   stream listings rather than mangled.

/// How a final path component addresses a stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamName<'a> {
    /// The file or directory carrying the stream.
    pub base: &'a str,
    /// The stream's name; empty for the unnamed default data stream.
    pub stream: &'a str,
}

/// Why a stream name is unusable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamNameError {
    /// `:$INDEX_ALLOCATION` and the other non-data stream types, an empty base
    /// name, or a character a stream name cannot hold.
    Invalid,
}

/// Split a final path component into a base name and a stream, or `None` when
/// it names no stream at all.
///
/// Accepts `name:stream`, `name:stream:$DATA`, and `name::$DATA` (the default
/// stream, spelled out). The type is compared case-insensitively, as NTFS does.
pub fn split(component: &str) -> Result<Option<StreamName<'_>>, StreamNameError> {
    let Some((base, rest)) = component.split_once(':') else {
        return Ok(None);
    };
    if base.is_empty() {
        return Err(StreamNameError::Invalid);
    }
    let stream = match rest.split_once(':') {
        None => rest,
        Some((stream, kind)) if kind.eq_ignore_ascii_case("$DATA") => stream,
        Some(_) => return Err(StreamNameError::Invalid),
    };
    if !is_stream_name(stream) && !stream.is_empty() {
        return Err(StreamNameError::Invalid);
    }
    Ok(Some(StreamName { base, stream }))
}

/// Whether `name` can be presented as a stream name: valid Unicode, at most
/// 255 UTF-16 units, and none of the characters NTFS reserves in stream names.
pub fn is_stream_name(name: &str) -> bool {
    !name.is_empty()
        && name.encode_utf16().count() <= 255
        && !name.contains(['\\', '/', ':', '\0'])
        && !name.chars().any(|character| (character as u32) < 0x20)
}

/// Size of `FSP_FSCTL_STREAM_INFO` before its name.
const STREAM_INFO_HEADER: usize = 24;

/// Entries are 8-byte aligned in the buffer.
const ALIGNMENT: usize = 8;

/// Pack `(name, size)` pairs into the `GetStreamInfo` buffer format, the safe
/// equivalent of `FspFileSystemAddStreamInfo` per entry plus its end marker.
/// The unnamed default stream is the empty name. Returns the bytes written.
/// When an entry does not fit, packing stops there without the end marker,
/// exactly as a `GetStreamInfo` built on `FspFileSystemAddStreamInfo` does.
pub fn pack(entries: &[(String, u64)], buffer: &mut [u8]) -> usize {
    let mut used = 0;
    for (name, size) in entries {
        let name: Vec<u8> = name
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let length = STREAM_INFO_HEADER + name.len();
        let Ok(length_word) = u16::try_from(length) else {
            continue;
        };
        let aligned = length.div_ceil(ALIGNMENT) * ALIGNMENT;
        if used + aligned > buffer.len() {
            return used;
        }
        let entry = &mut buffer[used..used + aligned];
        entry.fill(0);
        entry[0..2].copy_from_slice(&length_word.to_le_bytes());
        entry[8..16].copy_from_slice(&size.to_le_bytes());
        entry[16..24].copy_from_slice(&allocation(*size).to_le_bytes());
        entry[STREAM_INFO_HEADER..length].copy_from_slice(&name);
        used += aligned;
    }
    // The end marker is a zero size word, written only when it fits.
    if used + 2 <= buffer.len() {
        buffer[used..used + 2].fill(0);
        used += 2;
    }
    used
}

fn allocation(size: u64) -> u64 {
    size.div_ceil(4_096).saturating_mul(4_096)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_of_a_stream_splits_the_same_way() {
        for spelling in ["a.txt:tag", "a.txt:tag:$DATA", "a.txt:tag:$data"] {
            assert_eq!(
                split(spelling).unwrap(),
                Some(StreamName {
                    base: "a.txt",
                    stream: "tag"
                }),
                "{spelling}"
            );
        }
        assert_eq!(
            split("a.txt::$DATA").unwrap(),
            Some(StreamName {
                base: "a.txt",
                stream: ""
            })
        );
        assert_eq!(split("plain.txt").unwrap(), None);
    }

    #[test]
    fn non_data_stream_types_and_bad_names_are_refused() {
        assert!(split("dir:$I30:$INDEX_ALLOCATION").is_err());
        assert!(split(":orphan").is_err());
        assert!(split("a:b\u{1}").is_err());
        assert!(is_stream_name("com.apple.ResourceFork"));
        assert!(is_stream_name("Zone.Identifier"));
        assert!(!is_stream_name("user.a/b"));
        assert!(!is_stream_name(""));
    }

    #[test]
    fn stream_information_is_packed_aligned_and_terminated() {
        let mut buffer = vec![0xAA; 256];
        let entries = vec![(String::new(), 10), ("ab".to_owned(), 5000)];
        let used = pack(&entries, &mut buffer);
        // Default stream: 24 bytes, already aligned. "ab": 28 -> 32. Marker: 2.
        assert_eq!(used, 24 + 32 + 2);
        assert_eq!(u16::from_le_bytes([buffer[0], buffer[1]]), 24);
        assert_eq!(u64::from_le_bytes(buffer[8..16].try_into().unwrap()), 10);
        assert_eq!(u16::from_le_bytes([buffer[24], buffer[25]]), 28);
        assert_eq!(
            u64::from_le_bytes(buffer[24 + 16..24 + 24].try_into().unwrap()),
            8192
        );
        assert_eq!(&buffer[48..52], &[b'a', 0, b'b', 0]);
        assert_eq!(&buffer[56..58], &[0, 0]);

        // A buffer too small for the second entry keeps only the first, and
        // leaves out the end marker: that absence is how WinFsp learns the
        // listing was cut short.
        let mut small = vec![0; 40];
        assert_eq!(pack(&entries, &mut small), 24);
    }
}
