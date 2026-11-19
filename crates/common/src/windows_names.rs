// SPDX-License-Identifier: Apache-2.0
//! Which names a Windows user can actually reach through an ordinary Win32
//! path.
//!
//! A Linux server stores almost any byte string as a name, and Win32 refuses a
//! well-known set of them. Everything on Windows that *creates* a name on the
//! user's behalf — a file made through the WinFsp mount, a file downloaded by
//! the client — checks it here first, so quicKFS never makes something the same
//! user could not afterwards open, rename or delete from Explorer. Existing
//! names on the server are not affected: these rules apply only to creation.
//!
//! The rules are pure string checks and live in this platform-neutral crate so
//! they can be shared and tested on every operating system.

/// Characters Win32 refuses inside a path component, besides the separators
/// `\` and `/` and the control characters.
pub const RESERVED_CHARACTERS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// DOS device names. Win32 resolves these to devices no matter which directory
/// they appear in, so a file created under one of them could never be opened
/// again through an ordinary path.
pub const RESERVED_NAMES: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
];

/// Why a name cannot be created for a Windows user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsNameProblem {
    /// A reserved character, a path separator, or a control character.
    InvalidCharacter,
    /// Win32 silently strips a trailing dot or space, so the name would not
    /// round-trip.
    TrailingDotOrSpace,
    /// A DOS device name such as `CON` or `nul.txt`.
    ReservedName,
}

impl std::fmt::Display for WindowsNameProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCharacter => "contains a character Windows does not allow in a name",
            Self::TrailingDotOrSpace => "ends with a dot or a space, which Windows strips",
            Self::ReservedName => "is a reserved Windows device name",
        })
    }
}

impl std::error::Error for WindowsNameProblem {}

/// Check the Windows-specific rules for creating `name`.
///
/// Emptiness, `.`/`..`, length and NUL are the caller's ordinary name rules and
/// are not repeated here, except that NUL and `/` count as invalid characters.
pub fn check_creatable(name: &str) -> Result<(), WindowsNameProblem> {
    if name.chars().any(|character| {
        RESERVED_CHARACTERS.contains(&character)
            || (character as u32) < 0x20
            || character == '\\'
            || character == '/'
    }) {
        return Err(WindowsNameProblem::InvalidCharacter);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(WindowsNameProblem::TrailingDotOrSpace);
    }
    if is_reserved_device_name(name) {
        return Err(WindowsNameProblem::ReservedName);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_and_unicode_names_are_creatable() {
        for name in [
            "hello.txt",
            "café",
            "日本語.txt",
            "emoji-🎉",
            ".gitignore",
            "a b",
        ] {
            assert_eq!(check_creatable(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn names_win32_cannot_round_trip_are_refused() {
        for name in [
            "bad:name", "a<b", "q?", "star*", "pipe|", "quote\"", "tab\t", "a\\b", "a/b",
        ] {
            assert_eq!(
                check_creatable(name),
                Err(WindowsNameProblem::InvalidCharacter),
                "{name}"
            );
        }
        for name in ["trailing.", "trailing "] {
            assert_eq!(
                check_creatable(name),
                Err(WindowsNameProblem::TrailingDotOrSpace)
            );
        }
        for name in [
            "CON",
            "nul.txt",
            "Com1.tar.gz",
            "lpt9",
            "CON .txt",
            "conout$",
        ] {
            assert_eq!(
                check_creatable(name),
                Err(WindowsNameProblem::ReservedName),
                "{name}"
            );
        }
        // A device name only matters as the whole stem.
        assert_eq!(check_creatable("console.txt"), Ok(()));
        assert_eq!(check_creatable("CONFIG"), Ok(()));
    }
}
