// SPDX-License-Identifier: Apache-2.0
//! Translation between the Windows namespace and quicKFS names.
//!
//! WinFsp hands the file system absolute, backslash-separated UTF-16 paths
//! (`\`, `\photos\raw.dng`). quicKFS addresses nodes by an opaque id reached
//! through per-component lookups, and its names are arbitrary non-empty byte
//! strings without `/` or NUL. This module owns every rule for getting from one
//! to the other, and nothing here touches WinFsp or Win32, so the whole surface
//! is unit tested on every platform the workspace builds on.
//!
//! Two validation levels exist on purpose:
//!
//! * [`components`] is what a *lookup* must accept. It rejects only what could
//!   change which node a path names — empty components, `.`, `..`, embedded
//!   separators, NUL — because a file created on the Linux server may legally
//!   carry a character Windows itself would never produce, and hiding such a
//!   file behind a lookup failure would make it undeletable from Windows.
//! * [`validate_creatable`] is what a *create* or *rename target* must pass. It
//!   additionally rejects names Win32 cannot round-trip, so quicKFS never
//!   creates a file on the user's behalf that the same user cannot afterwards
//!   open, rename, or delete through Explorer. NTFS answers such requests with
//!   `STATUS_OBJECT_NAME_INVALID`, and so do we.

/// Longest single path component, in UTF-16 code units.
///
/// This is both the NTFS limit reported as `MaxComponentLength` and the
/// capacity of WinFsp's `FSP_FSCTL_DIR_INFO::FileNameBuf`, so a longer name
/// could not be returned from a directory enumeration even if the server
/// accepted it.
pub const MAX_COMPONENT_UTF16_UNITS: usize = 255;

/// Why a Windows path or name cannot be used against a quicKFS export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathError {
    /// The path did not start at the volume root.
    NotAbsolute,
    /// Two consecutive separators, or a separator with nothing after it in a
    /// path that is not the root.
    EmptyComponent,
    /// A `.` or `..` component. The Windows I/O manager resolves these before
    /// a request reaches a file system, so one arriving here is either a
    /// malformed request or an attempt to escape the export; either way it is
    /// refused rather than interpreted.
    RelativeComponent,
    /// A component longer than [`MAX_COMPONENT_UTF16_UNITS`].
    ComponentTooLong,
    /// An unpaired UTF-16 surrogate, which has no UTF-8 encoding and therefore
    /// no quicKFS name.
    NotRepresentable,
    /// The operation names the volume root where a child was required.
    IsRoot,
    /// A character Win32 reserves and cannot round-trip through a path.
    InvalidCharacter,
    /// A trailing `.` or space, which Win32 strips from every path it builds,
    /// leaving the created file unreachable by the name it was asked for.
    TrailingDotOrSpace,
    /// A DOS device name, which Win32 resolves to a device rather than to a
    /// file in the current directory.
    ReservedName,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotAbsolute => "path does not start at the volume root",
            Self::EmptyComponent => "path contains an empty component",
            Self::RelativeComponent => "path contains a '.' or '..' component",
            Self::ComponentTooLong => "path component is longer than 255 characters",
            Self::NotRepresentable => "path contains an unpaired surrogate",
            Self::IsRoot => "operation requires a name below the volume root",
            Self::InvalidCharacter => "name contains a character Windows reserves",
            Self::TrailingDotOrSpace => "name ends with a dot or a space",
            Self::ReservedName => "name is a reserved Windows device name",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PathError {}

/// Characters Win32 refuses inside a path component. `\` and `/` are separators
/// and are caught earlier; the rest are the reserved set plus the control
/// characters.
const RESERVED_CHARACTERS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// DOS device names. Win32 resolves these to devices no matter which directory
/// they appear in, so a file created under one of them could never be opened
/// again through an ordinary path.
const RESERVED_NAMES: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
];

/// Split an absolute WinFsp path into its components.
///
/// The root is an empty component list. A single trailing separator is
/// tolerated because some callers append one to directory paths; anything else
/// empty is an error.
pub fn components(path: &[u16]) -> Result<Vec<String>, PathError> {
    let text = String::from_utf16(path).map_err(|_| PathError::NotRepresentable)?;
    components_from_str(&text)
}

/// [`components`] for a path that is already a Rust string.
pub fn components_from_str(path: &str) -> Result<Vec<String>, PathError> {
    let rest = path.strip_prefix('\\').ok_or(PathError::NotAbsolute)?;
    // The root may be written `\`; a trailing separator anywhere else is
    // dropped so `\dir\` and `\dir` name the same node.
    let rest = rest.strip_suffix('\\').unwrap_or(rest);
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for component in rest.split('\\') {
        validate_component(component)?;
        result.push(component.to_owned());
    }
    Ok(result)
}

/// Split an absolute path into the components of its parent directory and the
/// final name. Fails with [`PathError::IsRoot`] for the volume root, which has
/// no parent inside the export.
pub fn split_parent(path: &[u16]) -> Result<(Vec<String>, String), PathError> {
    let mut components = components(path)?;
    let name = components.pop().ok_or(PathError::IsRoot)?;
    Ok((components, name))
}

/// The rules every component must satisfy, whether it is being looked up or
/// created. Each one exists because breaking it would change *which* node the
/// path names rather than merely how it looks.
fn validate_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if component == "." || component == ".." {
        return Err(PathError::RelativeComponent);
    }
    if component.encode_utf16().count() > MAX_COMPONENT_UTF16_UNITS {
        return Err(PathError::ComponentTooLong);
    }
    if component.contains('/') || component.contains('\0') {
        // `/` is a quicKFS name separator and NUL terminates a C string; a name
        // carrying either could address a different node on the server than the
        // one Windows asked for.
        return Err(PathError::InvalidCharacter);
    }
    Ok(())
}

/// The additional rules a name must satisfy before quicKFS will create it.
///
/// Passing these is not required to *find* an existing node: a file created on
/// the Linux server may carry any of the characters rejected here, and it stays
/// enumerable and deletable from Windows. What this prevents is quicKFS
/// creating, on a Windows user's behalf, a file that the same user could not
/// then reach through Explorer or any ordinary Win32 path.
pub fn validate_creatable(name: &str) -> Result<(), PathError> {
    validate_component(name)?;
    if name.chars().any(|character| {
        RESERVED_CHARACTERS.contains(&character) || (character as u32) < 0x20 || character == '\\'
    }) {
        return Err(PathError::InvalidCharacter);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(PathError::TrailingDotOrSpace);
    }
    if is_reserved_device_name(name) {
        return Err(PathError::ReservedName);
    }
    Ok(())
}

/// Whether `name` is a DOS device name, with or without an extension.
///
/// `NUL`, `nul.txt` and `Nul.tar.gz` all resolve to the null device, so the
/// stem before the first dot is what matters and the comparison is
/// case-insensitive.
pub fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    // Win32 also strips trailing spaces from the stem before resolving it, so
    // `"CON "` is still the console device.
    let stem = stem.trim_end_matches(' ').to_ascii_uppercase();
    RESERVED_NAMES.contains(&stem.as_str())
}

/// Whether a name should carry `FILE_ATTRIBUTE_HIDDEN`.
///
/// Unix has no hidden bit; a leading dot is the convention, and presenting it
/// as the Windows hidden attribute is what keeps `.git` and friends out of an
/// ordinary Explorer view. It is a presentation choice, not stored state: see
/// `attributes::apply_readonly` for the one attribute that is persisted.
pub fn is_hidden_name(name: &[u8]) -> bool {
    name.first() == Some(&b'.')
}

/// Convert a quicKFS name into the UTF-16 form a Windows directory entry needs.
///
/// Returns `None` when the name cannot appear in the Windows namespace at all:
/// it is not valid UTF-8, or it exceeds the 255-unit entry limit. Such an entry
/// is omitted from enumeration rather than presented in a mangled form that
/// would not open, which is a documented limitation of the Windows client.
pub fn encode_entry_name(name: &[u8]) -> Option<Vec<u16>> {
    let text = std::str::from_utf8(name).ok()?;
    if text.is_empty() || text.contains('\0') {
        return None;
    }
    let encoded: Vec<u16> = text.encode_utf16().collect();
    if encoded.len() > MAX_COMPONENT_UTF16_UNITS {
        return None;
    }
    Some(encoded)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn utf16(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn the_root_has_no_components_and_a_trailing_separator_is_ignored() {
        assert_eq!(components(&utf16("\\")).unwrap(), Vec::<String>::new());
        assert_eq!(components(&utf16("\\dir")).unwrap(), vec!["dir"]);
        assert_eq!(components(&utf16("\\dir\\")).unwrap(), vec!["dir"]);
        assert_eq!(
            components(&utf16("\\a\\b\\c.txt")).unwrap(),
            vec!["a", "b", "c.txt"]
        );
    }

    #[test]
    fn traversal_and_malformed_paths_are_refused_rather_than_interpreted() {
        assert_eq!(components(&utf16("dir")), Err(PathError::NotAbsolute));
        assert_eq!(
            components(&utf16("\\a\\\\b")),
            Err(PathError::EmptyComponent)
        );
        assert_eq!(
            components(&utf16("\\a\\..\\..\\etc")),
            Err(PathError::RelativeComponent)
        );
        assert_eq!(components(&utf16("\\.")), Err(PathError::RelativeComponent));
        assert_eq!(
            components(&utf16("\\a/b")),
            Err(PathError::InvalidCharacter)
        );
        // A lone high surrogate has no UTF-8 encoding, so it can never name a
        // quicKFS node.
        assert_eq!(
            components(&[0x005C, 0xD800]),
            Err(PathError::NotRepresentable)
        );
    }

    #[test]
    fn a_component_may_reach_but_not_exceed_the_windows_limit() {
        let longest = "a".repeat(MAX_COMPONENT_UTF16_UNITS);
        assert!(components_from_str(&format!("\\{longest}")).is_ok());
        let overlong = "a".repeat(MAX_COMPONENT_UTF16_UNITS + 1);
        assert_eq!(
            components_from_str(&format!("\\{overlong}")),
            Err(PathError::ComponentTooLong)
        );
        // Non-BMP characters cost two units each, so the limit is on units and
        // not on characters.
        let emoji = "\u{1F600}".repeat(MAX_COMPONENT_UTF16_UNITS / 2 + 1);
        assert_eq!(
            components_from_str(&format!("\\{emoji}")),
            Err(PathError::ComponentTooLong)
        );
    }

    #[test]
    fn splitting_a_parent_names_the_final_component() {
        let (parent, name) = split_parent(&utf16("\\a\\b\\c.txt")).unwrap();
        assert_eq!(parent, vec!["a", "b"]);
        assert_eq!(name, "c.txt");

        let (parent, name) = split_parent(&utf16("\\solo")).unwrap();
        assert!(parent.is_empty());
        assert_eq!(name, "solo");

        assert_eq!(split_parent(&utf16("\\")), Err(PathError::IsRoot));
    }

    #[test]
    fn creation_rejects_names_windows_cannot_round_trip() {
        assert!(validate_creatable("ordinary.txt").is_ok());
        assert!(validate_creatable("Ünïcøde 日本語 🎉.txt").is_ok());
        assert!(validate_creatable(".gitignore").is_ok());

        assert_eq!(
            validate_creatable("a:b"),
            Err(PathError::InvalidCharacter),
            "a colon opens an alternate data stream"
        );
        for reserved in ['<', '>', '"', '|', '?', '*'] {
            assert_eq!(
                validate_creatable(&format!("a{reserved}b")),
                Err(PathError::InvalidCharacter)
            );
        }
        assert_eq!(
            validate_creatable("bell\u{7}"),
            Err(PathError::InvalidCharacter)
        );
        assert_eq!(
            validate_creatable("trailing."),
            Err(PathError::TrailingDotOrSpace)
        );
        assert_eq!(
            validate_creatable("trailing "),
            Err(PathError::TrailingDotOrSpace)
        );
    }

    #[test]
    fn creation_rejects_dos_device_names_in_every_spelling() {
        for name in [
            "CON",
            "con",
            "NUL.txt",
            "Aux.tar.gz",
            "com1",
            "LPT9",
            "conin$",
        ] {
            assert_eq!(
                validate_creatable(name),
                Err(PathError::ReservedName),
                "{name} should be refused"
            );
        }
        // Names that merely start with a device name are ordinary files.
        for name in ["CONSOLE", "NULL", "com10", "lpt", "console.txt"] {
            assert!(validate_creatable(name).is_ok(), "{name} should be allowed");
        }
    }

    #[test]
    fn lookup_still_reaches_names_only_the_server_could_have_created() {
        // Every one of these is refused for creation but must remain
        // addressable, otherwise a file made on Linux could not be deleted
        // from Windows.
        for name in ["has:colon", "star*", "trailing.", "CON", "ends "] {
            assert!(
                components_from_str(&format!("\\{name}")).is_ok(),
                "{name} should still be reachable"
            );
        }
    }

    #[test]
    fn entry_names_that_windows_cannot_show_are_omitted() {
        assert_eq!(encode_entry_name(b"file.txt"), Some(utf16("file.txt")));
        assert_eq!(
            encode_entry_name("日本語".as_bytes()),
            Some(utf16("日本語"))
        );
        // Invalid UTF-8: a Linux name that has no Windows spelling.
        assert_eq!(encode_entry_name(&[0xFF, 0xFE, 0x41]), None);
        assert_eq!(encode_entry_name(b""), None);
        assert_eq!(
            encode_entry_name("a".repeat(MAX_COMPONENT_UTF16_UNITS + 1).as_bytes()),
            None
        );
    }

    #[test]
    fn the_hidden_attribute_follows_the_leading_dot_convention() {
        assert!(is_hidden_name(b".git"));
        assert!(is_hidden_name(b".hidden.txt"));
        assert!(!is_hidden_name(b"visible.txt"));
        assert!(!is_hidden_name(b""));
    }
}
