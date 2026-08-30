// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
//! macFUSE bridge for an authenticated asynchronous `RemoteFilesystem`.
//!
//! Everything that is not macFUSE-specific — the inode and handle tables, the
//! caches, the coalescing write buffer, and the callback/runtime bridge —
//! lives in [`quickfs_fs_adapter`] and is shared with the WinFsp bridge. This
//! crate contains only the translation between fuser's callbacks and that
//! adapter, plus the macOS mount lifecycle.

pub use quickfs_fs_adapter::*;

#[cfg(all(target_os = "macos", feature = "macfuse"))]
mod native;

#[cfg(all(target_os = "macos", feature = "macfuse"))]
pub use native::{MacFuseAdapter, MacFuseBackend, MountConfig, mount};

#[cfg(all(target_os = "macos", feature = "macfuse"))]
pub const NATIVE_CALLBACKS_IMPLEMENTED: bool = true;
