// SPDX-License-Identifier: Apache-2.0
//! Size and clear the persistent offline read cache that mounts share.
//!
//! The cache keeps one directory per server identity, epoch, and user under
//! `<cache_dir>/namespaces`. Clearing removes only that subtree, never the
//! cache root itself, and refuses to follow symbolic links, so a hostile link
//! planted in the cache directory cannot redirect a recursive delete. The app
//! only offers clearing while nothing is mounted, because a live mount holds
//! files open in its namespace.

use crate::model::{BridgeError, BridgeResult};
use serde_json::{Value, json};
use std::{fs, path::Path};

const NAMESPACES: &str = "namespaces";

pub fn usage(cache_dir: &Path) -> BridgeResult<Value> {
    let root = cache_dir.join(NAMESPACES);
    let (mut bytes, mut files, mut namespaces) = (0_u64, 0_u64, 0_u64);
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(BridgeError::invalid(
                "the cache directory is a symbolic link",
            ));
        }
        Ok(_) => {
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    namespaces += 1;
                }
            }
            let mut pending = vec![root];
            while let Some(directory) = pending.pop() {
                for entry in fs::read_dir(&directory)? {
                    let entry = entry?;
                    let kind = entry.file_type()?;
                    if kind.is_dir() {
                        pending.push(entry.path());
                    } else if kind.is_file() {
                        bytes = bytes.saturating_add(entry.metadata()?.len());
                        files += 1;
                    }
                }
            }
        }
    }
    Ok(json!({ "bytes": bytes, "files": files, "namespaces": namespaces }))
}

pub fn clear(cache_dir: &Path) -> BridgeResult<Value> {
    let before = usage(cache_dir)?;
    let root = cache_dir.join(NAMESPACES);
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(BridgeError::invalid(
                "the cache directory is not a real directory",
            ));
        }
        // `remove_dir_all` does not follow symbolic links inside the tree.
        Ok(_) => fs::remove_dir_all(&root)?,
    }
    Ok(json!({ "freed": before["bytes"] }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn measures_and_clears_only_the_namespaces_tree() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path();
        assert_eq!(usage(cache).unwrap()["bytes"], 0);

        let namespace = cache.join(NAMESPACES).join("abc");
        fs::create_dir_all(namespace.join("ranges")).unwrap();
        fs::write(namespace.join("ranges/block"), vec![7_u8; 1000]).unwrap();
        fs::write(namespace.join("manifest"), vec![1_u8; 24]).unwrap();
        fs::write(cache.join("keep.txt"), b"not cache data").unwrap();

        let measured = usage(cache).unwrap();
        assert_eq!(measured["bytes"], 1024);
        assert_eq!(measured["files"], 2);
        assert_eq!(measured["namespaces"], 1);

        assert_eq!(clear(cache).unwrap()["freed"], 1024);
        assert!(!cache.join(NAMESPACES).exists());
        assert!(cache.join("keep.txt").exists());
        assert_eq!(usage(cache).unwrap()["bytes"], 0);
        assert_eq!(clear(cache).unwrap()["freed"], 0);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_namespaces_directory() {
        let directory = tempfile::tempdir().unwrap();
        let elsewhere = directory.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("precious"), b"x").unwrap();
        let cache = directory.path().join("cache");
        fs::create_dir(&cache).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cache.join(NAMESPACES)).unwrap();

        assert!(usage(&cache).is_err());
        assert!(clear(&cache).is_err());
        assert!(elsewhere.join("precious").exists());
    }
}
