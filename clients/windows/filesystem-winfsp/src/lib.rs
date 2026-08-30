// SPDX-License-Identifier: Apache-2.0
// One module — `handle` — needs `unsafe` to give WinFsp a file context that can
// legitimately be absent; see its documentation for why the safe alternative in
// `winfsp_wrs` cannot be used. Everywhere else in this crate the ban stands.
#![deny(unsafe_code)]
//! WinFsp bridge for an authenticated asynchronous `RemoteFilesystem`.
//!
//! Everything that is not WinFsp-specific — the inode and handle tables, the
//! metadata and directory caches, the coalescing write buffer, reconnect, and
//! the callback/runtime bridge — lives in [`quickfs_fs_adapter`] and is shared
//! with the macFUSE bridge. This crate contains only the translation between
//! WinFsp's path-addressed callbacks and that node-addressed adapter, plus the
//! Windows mount lifecycle.
//!
//! ```text
//! Explorer / applications
//!         |
//!      WinFsp (FSD + user-mode DLL)
//!         |
//! quickfs-filesystem-winfsp   <- this crate: paths, NTSTATUS, handles, ACLs
//!         |
//! quickfs-fs-adapter          <- shared with macFUSE: inodes, caches, buffers
//!         |
//! quickfs-client-core         <- transport, auth, trust, reconnect, caching
//!         |
//!      quicKFS server
//! ```

pub mod attributes;
pub mod path;
pub mod resolve;
pub mod status;

#[cfg(windows)]
pub mod filesystem;
#[cfg(windows)]
mod handle;
#[cfg(windows)]
pub mod mount;

#[cfg(test)]
mod testing;

#[cfg(all(test, windows))]
mod tests;

pub use path::{MAX_COMPONENT_UTF16_UNITS, PathError};
pub use resolve::{DEFAULT_RETAINED_DIRECTORIES, Resolved, Resolver};
pub use status::NtStatus;

// Re-exported so the Windows CLI constructs the shared adapter through the
// bridge it hands it to, rather than depending on the adapter crate directly.
pub use quickfs_fs_adapter::{Adapter, AdapterError};

#[cfg(windows)]
pub use filesystem::{ALLOCATION_UNIT, FileContext, WindowsFilesystem};
#[cfg(windows)]
pub use mount::{Mount, MountConfig, MountError, MountPoint, mount};
