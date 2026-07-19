// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const DEFAULT_MAX_READ_SIZE: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_WRITE_SIZE: u64 = 8 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_read_size: u64,
    pub max_write_size: u64,
    pub max_open_handles: usize,
    pub max_known_nodes: usize,
    pub max_total_known_nodes: usize,
    /// Aggregate number of child metadata/xattr tasks used by directory views.
    pub max_directory_entry_tasks: usize,
    pub request_timeout_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_read_size: DEFAULT_MAX_READ_SIZE,
            max_write_size: DEFAULT_MAX_WRITE_SIZE,
            max_open_handles: 1024,
            max_known_nodes: 8_192,
            max_total_known_nodes: 65_536,
            max_directory_entry_tasks: 64,
            request_timeout_ms: 30_000,
        }
    }
}
impl Limits {
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms)
    }
}
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("invalid byte range")]
    InvalidRange,
    #[error("unsafe path component")]
    UnsafePath,
    #[error("invalid filename")]
    InvalidFilename,
}
pub fn validate_range(offset: u64, length: u64, maximum: u64) -> Result<(), ValidationError> {
    if length > maximum || offset.checked_add(length).is_none() {
        Err(ValidationError::InvalidRange)
    } else {
        Ok(())
    }
}
pub fn normalize_relative(path: &Path) -> Result<PathBuf, ValidationError> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(v) => out.push(v),
            Component::CurDir => {}
            _ => return Err(ValidationError::UnsafePath),
        }
    }
    Ok(out)
}
pub fn validate_filename(name: &[u8]) -> Result<(), ValidationError> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        Err(ValidationError::InvalidFilename)
    } else {
        Ok(())
    }
}
pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn ranges() {
        assert!(validate_range(u64::MAX, 2, 8).is_err());
        assert!(validate_range(2, 0, 8).is_ok());
    }
    #[test]
    fn paths() {
        assert!(normalize_relative(Path::new("../x")).is_err());
        assert_eq!(
            normalize_relative(Path::new("a/./b")).unwrap(),
            PathBuf::from("a/b")
        );
    }

    #[test]
    fn filenames_are_validated_as_lossless_unix_bytes() {
        assert!(validate_filename(b"clip.mov").is_ok());
        assert!(validate_filename(&[0xff, b'x']).is_ok());
        assert!(validate_filename(b"../clip").is_err());
        assert!(validate_filename(b"bad\0name").is_err());
    }
}
