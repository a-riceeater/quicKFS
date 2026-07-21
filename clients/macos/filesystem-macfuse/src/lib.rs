// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
//! macFUSE bridge for an authenticated asynchronous [`RemoteFilesystem`].
//!
//! Native callbacks dispatch reply-owning tasks onto the one Tokio runtime
//! retained by [`Adapter`]. Synchronous methods remain available for focused
//! unit tests and non-native callers.

use quickfs_client_core::{
    ClientError, DirectorySnapshot, MAX_CLIENT_READ_SIZE, MAX_CLIENT_WRITE_SIZE, RemoteFilesystem,
};
use quickfs_protocol::{
    AttributeChanges, DirectoryEntry as RemoteDirectoryEntry, DirectoryView, DirectoryViewOptions,
    FileAccess, FileHandle as RemoteFileHandle, FileLock, FileOpenOptions, FilesystemCapabilities,
    FilesystemStats, Metadata, Name, NodeId, NodeKind, ROOT_NODE, RenameMode, SafeIoctl,
    SeekWhence, SpecialNodeKind, XattrSetMode, XattrSnapshot,
};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock},
};
use unicode_normalization::UnicodeNormalization;

pub const ROOT_INODE: u64 = 1;

/// How long one fetched `FilesystemStats` answers subsequent statfs
/// callbacks. Keeps the volume-registration probes and macOS's periodic
/// statfs pollers (Finder, storage management) at memory speed instead of
/// one network round trip per call; statfs carries no coherence contract,
/// and one second matches the attribute TTL the mount already reports.
const FILESYSTEM_STATS_TTL: Duration = Duration::from_secs(1);

/// fuser allocates a 16 MiB receive buffer on macOS. Individual protocol
/// transfers remain bounded by the negotiated client/server limit and are
/// joined inside one FUSE operation.
pub const MAX_FUSE_IO_SIZE: u64 = 16 * 1024 * 1024;
pub const MAX_FUSE_XATTR_SIZE: u64 = 64 * 1024 * 1024;
// One enriched directory view is the consistency unit for Finder callbacks.
// Expiring its child metadata after one second while retaining its names for
// thirty seconds recreates the exact per-child RPC fan-out v6 was meant to
// eliminate, and can make a second `ls` slower than the first.
const DISCOVERED_METADATA_TTL: Duration = Duration::from_secs(30);
const DISCOVERED_DIRECTORY_TTL: Duration = Duration::from_secs(30);
const RELEASED_DIRECTORY_ENTRY_RETENTION: Duration = Duration::from_secs(5);
const MOUNT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq)]
pub struct LookupResult {
    pub inode: u64,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryEntry {
    pub inode: u64,
    pub name: Name,
    pub kind: NodeKind,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryListing {
    pub parent_inode: u64,
    pub revision: u64,
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreatedNode {
    pub inode: u64,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreatedAndOpenedFile {
    pub inode: u64,
    pub metadata: Metadata,
    pub handle: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("failed to create the shared asynchronous runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("remote filesystem operation exceeded the callback deadline")]
    CallbackTimedOut,
    #[error("unknown inode")]
    UnknownInode,
    #[error("unknown file handle")]
    UnknownHandle,
    #[error("unknown directory handle")]
    UnknownDirectoryHandle,
    #[error("file handle does not belong to the requested inode")]
    HandleInodeMismatch,
    #[error("directory entry was not found")]
    NotFound,
    #[error("invalid path component")]
    InvalidName,
    #[error("server returned an invalid directory entry name")]
    InvalidRemoteName,
    #[error("remote directory contains case-insensitively ambiguous names")]
    AmbiguousName,
    #[error("local inode number space is exhausted")]
    InodeSpaceExhausted,
    #[error("local file-handle number space is exhausted")]
    HandleSpaceExhausted,
    #[error("adapter state is unavailable")]
    StateUnavailable,
    #[error("server returned metadata for a different node")]
    UnexpectedMetadata,
    #[error("server returned more bytes than requested")]
    UnexpectedReadLength,
    #[error("server reported a partial write")]
    UnexpectedWriteLength,
    #[error("remote file revision changed during a ranged read")]
    StaleRevision,
    #[error("FUSE request exceeds the adapter safety limit of {0} bytes")]
    RequestTooLarge(u64),
    #[error("server advertised an invalid zero I/O limit")]
    InvalidCapabilities,
    #[error("operation requires a writable export")]
    ReadOnly,
    #[error("operation is not supported by the negotiated filesystem API")]
    Unsupported,
    #[error("file was not opened with the required access mode")]
    InvalidAccess,
    #[error("file offset plus length overflows")]
    InvalidRange,
    #[error(transparent)]
    Client(#[from] ClientError),
}

#[derive(Clone, Copy)]
struct InodeRecord {
    node: NodeId,
    parent_inode: u64,
}

struct InodeTable {
    by_inode: HashMap<u64, InodeRecord>,
    by_node: HashMap<NodeId, u64>,
    by_entry: HashMap<(u64, Name), u64>,
    lookups: HashMap<u64, u64>,
}

#[derive(Clone, Copy)]
struct FileHandleRecord {
    remote: RemoteFileHandle,
    inode: u64,
    access: FileAccess,
    revision: u64,
    size: u64,
    /// Whether a POSIX byte-range lock was ever acquired through this
    /// handle. Releasing locks is the only remote-observable effect a
    /// flush/close of a read-only handle can have, so lock-free read-only
    /// handles skip the remote flush and close in the background.
    used_locks: bool,
}

/// Coalesced not-yet-transmitted bytes for one write handle. macFUSE delivers a
/// streaming copy as thousands of small (~16 KiB) `write` callbacks; buffering
/// contiguous runs into one large `write_range` collapses the request count so
/// throughput is no longer bounded by per-request latency on a high-latency
/// link. The buffer is flushed before any operation that must observe the
/// server's view of the file (read, seek, flush/fsync, close, truncate,
/// preallocation, or a server-side copy), so read-after-write stays coherent.
struct PendingWrite {
    inode: u64,
    start: u64,
    buffer: Vec<u8>,
}

impl PendingWrite {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.buffer.len() as u64)
    }
}

#[derive(Clone)]
struct DirectoryHandleRecord {
    inode: u64,
    listing: Arc<DirectoryListing>,
    current: Metadata,
    parent: Metadata,
}

#[derive(Clone)]
struct CachedMetadata {
    value: Metadata,
    discovered_at: Instant,
}

#[derive(Clone)]
struct CachedDirectory {
    listing: Arc<DirectoryListing>,
    discovered_at: Instant,
}

#[derive(Clone)]
struct CachedXattrs {
    revision: u64,
    names: Vec<Name>,
    sizes: HashMap<Name, u64>,
    values: HashMap<Name, Vec<u8>>,
    discovered_at: Instant,
}

struct AdapterState {
    remote: Arc<dyn RemoteFilesystem>,
    callback_timeout: Duration,
    capabilities: Mutex<Option<FilesystemCapabilities>>,
    filesystem_stats: Mutex<Option<(Instant, FilesystemStats)>>,
    filesystem_stats_refreshing: AtomicBool,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, FileHandleRecord>>,
    pending_writes: Mutex<HashMap<u64, PendingWrite>>,
    handle_operations: Mutex<HashMap<u64, Arc<AsyncRwLock<()>>>>,
    directory_operations: Mutex<HashMap<NodeId, Arc<AsyncMutex<()>>>>,
    directory_handles: Mutex<HashMap<u64, DirectoryHandleRecord>>,
    discovered_metadata: Mutex<HashMap<NodeId, CachedMetadata>>,
    discovered_directories: Mutex<HashMap<u64, CachedDirectory>>,
    discovered_xattrs: Mutex<HashMap<NodeId, CachedXattrs>>,
    next_inode: AtomicU64,
    next_handle: AtomicU64,
    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    owner_uid: u32,
    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    owner_gid: u32,
}

/// One authenticated mount session. Clones share all inode/handle state, the
/// remote connection, and exactly one Tokio runtime.
#[derive(Clone)]
pub struct Adapter {
    state: Arc<AdapterState>,
    runtime: Arc<Runtime>,
}

impl Adapter {
    /// Construct an adapter for tests or non-network implementations. Native
    /// mounts should use [`Self::with_runtime`] so authentication and callbacks
    /// share the runtime on which the QUIC connection was created.
    pub fn new(
        remote: Arc<dyn RemoteFilesystem>,
        callback_timeout: Duration,
    ) -> Result<Self, AdapterError> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("quickfs-remote")
            .build()
            .map_err(AdapterError::Runtime)?;
        Ok(Self::with_runtime(
            remote,
            callback_timeout,
            Arc::new(runtime),
        ))
    }

    pub fn with_runtime(
        remote: Arc<dyn RemoteFilesystem>,
        callback_timeout: Duration,
        runtime: Arc<Runtime>,
    ) -> Self {
        let root = InodeRecord {
            node: ROOT_NODE,
            parent_inode: ROOT_INODE,
        };
        Self {
            state: Arc::new(AdapterState {
                remote,
                callback_timeout,
                capabilities: Mutex::new(None),
                filesystem_stats: Mutex::new(None),
                filesystem_stats_refreshing: AtomicBool::new(false),
                inodes: Mutex::new(InodeTable {
                    by_inode: HashMap::from([(ROOT_INODE, root)]),
                    by_node: HashMap::from([(ROOT_NODE, ROOT_INODE)]),
                    by_entry: HashMap::new(),
                    lookups: HashMap::from([(ROOT_INODE, u64::MAX)]),
                }),
                handles: Mutex::new(HashMap::new()),
                pending_writes: Mutex::new(HashMap::new()),
                handle_operations: Mutex::new(HashMap::new()),
                directory_operations: Mutex::new(HashMap::new()),
                directory_handles: Mutex::new(HashMap::new()),
                discovered_metadata: Mutex::new(HashMap::new()),
                discovered_directories: Mutex::new(HashMap::new()),
                discovered_xattrs: Mutex::new(HashMap::new()),
                next_inode: AtomicU64::new(ROOT_INODE + 1),
                next_handle: AtomicU64::new(1),
                #[cfg(all(target_os = "macos", feature = "macfuse"))]
                owner_uid: rustix::process::geteuid().as_raw(),
                #[cfg(all(target_os = "macos", feature = "macfuse"))]
                owner_gid: rustix::process::getegid().as_raw(),
            }),
            runtime,
        }
    }

    pub fn callback_timeout(&self) -> Duration {
        self.state.callback_timeout
    }

    pub fn remote(&self) -> &Arc<dyn RemoteFilesystem> {
        &self.state.remote
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    pub fn probe_capabilities(&self) -> Result<FilesystemCapabilities, AdapterError> {
        self.block_on(self.capabilities_async())
    }

    pub fn cached_capabilities(&self) -> Option<FilesystemCapabilities> {
        self.state
            .capabilities
            .lock()
            .ok()
            .and_then(|capabilities| capabilities.clone())
    }

    pub fn is_writable(&self) -> bool {
        self.cached_capabilities()
            .is_some_and(|value| value.writable)
    }

    pub fn lookup(&self, parent_inode: u64, name: &str) -> Result<LookupResult, AdapterError> {
        self.block_on(self.lookup_async(parent_inode, Name::from(name)))
    }

    pub fn getattr(&self, inode: u64) -> Result<Metadata, AdapterError> {
        self.block_on(self.getattr_async(inode))
    }

    pub fn readdir(&self, inode: u64) -> Result<Arc<DirectoryListing>, AdapterError> {
        self.block_on(self.readdir_async(inode))
    }

    pub fn open(&self, inode: u64) -> Result<u64, AdapterError> {
        self.block_on(self.open_async(inode, FileOpenOptions::READ_ONLY))
    }

    pub fn read(&self, handle: u64, offset: u64, length: u64) -> Result<Vec<u8>, AdapterError> {
        self.block_on(self.read_async(handle, offset, length))
    }

    pub fn write(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u64, AdapterError> {
        self.block_on(self.write_async(handle, offset, data))
    }

    pub fn release(&self, handle: u64) -> Result<(), AdapterError> {
        self.block_on(self.release_async(handle, false, None))
    }

    pub(crate) async fn capabilities_async(&self) -> Result<FilesystemCapabilities, AdapterError> {
        if let Some(capabilities) = self.cached_capabilities() {
            return Ok(capabilities);
        }
        let capabilities = self
            .execute(async { Ok(self.state.remote.capabilities().await?) })
            .await?;
        if capabilities.max_read_size == 0 || capabilities.max_write_size == 0 {
            return Err(AdapterError::InvalidCapabilities);
        }
        let mut cached = self
            .state
            .capabilities
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if cached.is_none() {
            *cached = Some(capabilities.clone());
        }
        Ok(cached.clone().unwrap_or(capabilities))
    }

    pub(crate) async fn lookup_async(
        &self,
        parent_inode: u64,
        name: Name,
    ) -> Result<LookupResult, AdapterError> {
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        if let Some(listing) = self.cached_directory(parent_inode)? {
            return self.lookup_from_listing(parent_inode, &listing, &name);
        }
        if let Some(inode) = self.find_entry_inode(parent_inode, &name)? {
            let metadata = self.getattr_async(inode).await?;
            self.add_lookup(inode, 1)?;
            return Ok(LookupResult { inode, metadata });
        }
        let listing = self.readdir_async(parent_inode).await?;
        self.lookup_from_listing(parent_inode, &listing, &name)
    }

    pub(crate) async fn getattr_async(&self, inode: u64) -> Result<Metadata, AdapterError> {
        let record = self.inode_record(inode)?;
        let mut metadata = match self.cached_metadata(record.node)? {
            Some(metadata) => metadata,
            None => {
                self.execute(async {
                    let metadata = self.state.remote.get_metadata(record.node).await?;
                    validate_metadata(record.node, &metadata)?;
                    self.remember_metadata(metadata.clone())?;
                    Ok(metadata)
                })
                .await?
            }
        };
        // Coalesced writes have not reached the server yet, so the server's size
        // lags. Report the buffered high-water mark so a `stat` right after a
        // write sees the size the client already accepted. The cached/persisted
        // metadata keeps the server's real size; only the returned value is
        // adjusted, and it collapses back once the buffer is flushed.
        if let Some(end) = self.pending_end_for_inode(inode)
            && end > metadata.size
        {
            metadata.size = end;
        }
        Ok(metadata)
    }

    pub(crate) async fn readdir_async(
        &self,
        inode: u64,
    ) -> Result<Arc<DirectoryListing>, AdapterError> {
        if let Some(listing) = self.cached_directory(inode)? {
            return Ok(listing);
        }
        let record = self.inode_record(inode)?;
        let operation = self.directory_operation(record.node)?;
        let _operation_guard = operation.lock().await;
        if let Some(listing) = self.cached_directory(inode)? {
            return Ok(listing);
        }
        let view = self
            .execute(async {
                Ok(self
                    .state
                    .remote
                    .list_directory_view(record.node, DirectoryViewOptions::NATIVE)
                    .await?)
            })
            .await?;
        self.remember_directory_view(inode, record, view)
    }

    pub(crate) async fn opendir_async(&self, inode: u64) -> Result<u64, AdapterError> {
        let listing = match self.cached_directory(inode)? {
            Some(listing) => listing,
            None => self.readdir_async(inode).await?,
        };
        let metadata = self.getattr_async(inode).await?;
        if metadata.kind != NodeKind::Directory {
            return Err(ClientError::Server(
                quickfs_protocol::ErrorCode::NotDirectory,
                "node is not a directory".into(),
            )
            .into());
        }
        let parent = if listing.parent_inode == inode {
            metadata.clone()
        } else {
            self.getattr_async(listing.parent_inode).await?
        };
        let handle = self.next_handle()?;
        self.state
            .directory_handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .insert(
                handle,
                DirectoryHandleRecord {
                    inode,
                    listing,
                    current: metadata,
                    parent,
                },
            );
        Ok(handle)
    }

    pub(crate) fn directory_listing(
        &self,
        handle: u64,
        inode: u64,
    ) -> Result<Arc<DirectoryListing>, AdapterError> {
        let record = self
            .state
            .directory_handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .get(&handle)
            .cloned()
            .ok_or(AdapterError::UnknownDirectoryHandle)?;
        if record.inode != inode {
            return Err(AdapterError::HandleInodeMismatch);
        }
        Ok(record.listing)
    }

    pub(crate) fn directory_listing_with_metadata(
        &self,
        handle: u64,
        inode: u64,
    ) -> Result<(Arc<DirectoryListing>, Metadata, Metadata), AdapterError> {
        let record = self
            .state
            .directory_handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .get(&handle)
            .cloned()
            .ok_or(AdapterError::UnknownDirectoryHandle)?;
        if record.inode != inode {
            return Err(AdapterError::HandleInodeMismatch);
        }
        Ok((record.listing, record.current, record.parent))
    }

    pub(crate) fn releasedir(&self, handle: u64) -> Result<(), AdapterError> {
        let record = self
            .state
            .directory_handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .remove(&handle)
            .ok_or(AdapterError::UnknownDirectoryHandle)?;
        // Finder closes a plain-readdir handle before issuing child LOOKUP and
        // xattr requests. Keep zero-reference translations briefly, then free
        // only entries that still have no kernel lookup reference. This avoids
        // the immediate forget/lookup race without leaking an entire recursive
        // Finder crawl until disconnect.
        let candidates = record
            .listing
            .entries
            .iter()
            .map(|entry| (entry.inode, 0))
            .collect::<Vec<_>>();
        let adapter = self.clone();
        drop(self.runtime.spawn(async move {
            tokio::time::sleep(RELEASED_DIRECTORY_ENTRY_RETENTION).await;
            let _ = adapter.forget_inodes(&candidates);
        }));
        Ok(())
    }

    pub(crate) async fn open_async(
        &self,
        inode: u64,
        options: FileOpenOptions,
    ) -> Result<u64, AdapterError> {
        if options.access.can_write() {
            self.require_writable().await?;
        }
        let record = self.inode_record(inode)?;
        self.execute(async {
            let opened = self
                .state
                .remote
                .open_file_with_options(record.node, options)
                .await?;
            match self.remember_file_handle(inode, options.access, opened) {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    let _ = self.state.remote.close_file(opened.handle).await;
                    Err(error)
                }
            }
        })
        .await
    }

    pub(crate) async fn create_file_async(
        &self,
        parent_inode: u64,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<CreatedAndOpenedFile, AdapterError> {
        self.require_writable().await?;
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?;
        self.execute(async {
            let created = self
                .state
                .remote
                .create_file(parent.node, name.clone(), mode, options)
                .await?;
            validate_metadata(created.metadata.node, &created.metadata)?;
            self.remember_metadata(created.metadata.clone())?;
            self.invalidate_directory(parent_inode)?;
            let inode = self.remember_entry(created.metadata.node, parent_inode, &name)?;
            self.add_lookup(inode, 1)?;
            match self.remember_file_handle(inode, options.access, created.opened) {
                Ok(handle) => Ok(CreatedAndOpenedFile {
                    inode,
                    metadata: created.metadata,
                    handle,
                }),
                Err(error) => {
                    let _ = self.state.remote.close_file(created.opened.handle).await;
                    Err(error)
                }
            }
        })
        .await
    }

    pub(crate) async fn create_directory_async(
        &self,
        parent_inode: u64,
        name: Name,
        mode: u32,
    ) -> Result<CreatedNode, AdapterError> {
        self.require_writable().await?;
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?;
        self.execute(async {
            let metadata = self
                .state
                .remote
                .create_directory(parent.node, name.clone(), mode)
                .await?;
            validate_metadata(metadata.node, &metadata)?;
            self.remember_metadata(metadata.clone())?;
            self.invalidate_directory(parent_inode)?;
            let inode = self.remember_entry(metadata.node, parent_inode, &name)?;
            self.add_lookup(inode, 1)?;
            Ok(CreatedNode { inode, metadata })
        })
        .await
    }

    pub(crate) async fn create_symlink_async(
        &self,
        parent_inode: u64,
        name: Name,
        target: Vec<u8>,
    ) -> Result<CreatedNode, AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_symlinks {
            return Err(AdapterError::Unsupported);
        }
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?;
        self.execute(async {
            let metadata = self
                .state
                .remote
                .create_symlink(parent.node, name.clone(), target)
                .await?;
            validate_metadata(metadata.node, &metadata)?;
            self.remember_metadata(metadata.clone())?;
            self.invalidate_directory(parent_inode)?;
            let inode = self.remember_entry(metadata.node, parent_inode, &name)?;
            self.add_lookup(inode, 1)?;
            Ok(CreatedNode { inode, metadata })
        })
        .await
    }

    pub(crate) async fn readlink_async(&self, inode: u64) -> Result<Vec<u8>, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_symlinks {
            return Err(AdapterError::Unsupported);
        }
        let record = self.inode_record(inode)?;
        self.execute(async { Ok(self.state.remote.read_link(record.node).await?) })
            .await
    }

    pub(crate) async fn create_hard_link_async(
        &self,
        inode: u64,
        new_parent_inode: u64,
        new_name: Name,
    ) -> Result<CreatedNode, AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_hard_links {
            return Err(AdapterError::Unsupported);
        }
        validate_name(&new_name).map_err(|()| AdapterError::InvalidName)?;
        let node = self.inode_record(inode)?.node;
        let new_parent = self.inode_record(new_parent_inode)?.node;
        self.execute(async {
            let metadata = self
                .state
                .remote
                .create_hard_link(node, new_parent, new_name.clone())
                .await?;
            validate_metadata(node, &metadata)?;
            // Link count metadata is embedded in every existing hardlink
            // parent, not only in the destination directory.
            self.invalidate_node_views(node)?;
            self.invalidate_directory(new_parent_inode)?;
            self.remember_metadata(metadata.clone())?;
            let linked_inode = self.remember_entry(node, new_parent_inode, &new_name)?;
            self.add_lookup(linked_inode, 1)?;
            Ok(CreatedNode {
                inode: linked_inode,
                metadata,
            })
        })
        .await
    }

    pub(crate) async fn create_special_node_async(
        &self,
        parent_inode: u64,
        name: Name,
        kind: SpecialNodeKind,
        mode: u32,
        device_major: u32,
        device_minor: u32,
    ) -> Result<CreatedNode, AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_special_nodes {
            return Err(AdapterError::Unsupported);
        }
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?.node;
        self.execute(async {
            let metadata = self
                .state
                .remote
                .create_special_node(parent, name.clone(), kind, mode, device_major, device_minor)
                .await?;
            validate_metadata(metadata.node, &metadata)?;
            self.remember_metadata(metadata.clone())?;
            self.invalidate_directory(parent_inode)?;
            let inode = self.remember_entry(metadata.node, parent_inode, &name)?;
            self.add_lookup(inode, 1)?;
            Ok(CreatedNode { inode, metadata })
        })
        .await
    }

    pub(crate) async fn remove_async(
        &self,
        parent_inode: u64,
        name: Name,
        directory: bool,
    ) -> Result<(), AdapterError> {
        self.require_writable().await?;
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?;
        let removed_node = self.entry_node(parent_inode, &name)?;
        self.execute(async {
            self.state
                .remote
                .remove_node(parent.node, name.clone(), directory)
                .await?;
            self.invalidate_directory(parent_inode)?;
            if let Some(node) = removed_node {
                self.invalidate_node_views(node)?;
            }
            self.forget_entry(parent_inode, &name)?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn rename_async(
        &self,
        parent_inode: u64,
        name: Name,
        new_parent_inode: u64,
        new_name: Name,
        mode: RenameMode,
    ) -> Result<(), AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_atomic_rename {
            return Err(AdapterError::Unsupported);
        }
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        validate_name(&new_name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?;
        let new_parent = self.inode_record(new_parent_inode)?;
        let source_node = self.entry_node(parent_inode, &name)?;
        let destination_node = self.entry_node(new_parent_inode, &new_name)?;
        self.execute(async {
            self.state
                .remote
                .rename_node(
                    parent.node,
                    name.clone(),
                    new_parent.node,
                    new_name.clone(),
                    mode,
                )
                .await?;
            self.invalidate_directory(parent_inode)?;
            self.invalidate_directory(new_parent_inode)?;
            if let Some(node) = source_node {
                self.invalidate_node_views(node)?;
            }
            if let Some(node) = destination_node {
                self.invalidate_node_views(node)?;
            }
            self.move_entry(parent_inode, &name, new_parent_inode, &new_name, mode)?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn setattr_async(
        &self,
        inode: u64,
        handle: Option<u64>,
        changes: AttributeChanges,
    ) -> Result<Metadata, AdapterError> {
        self.require_writable().await?;
        let record = self.inode_record(inode)?;
        let operation = handle
            .map(|handle| self.file_operation(handle))
            .transpose()?;
        let _operation_guard = match &operation {
            Some(operation) => Some(operation.write().await),
            None => None,
        };
        // A truncate (or any attribute change) must be applied on top of every
        // byte already accepted from the client, so flush the coalescing buffer
        // for this node before the server mutates it.
        match handle {
            Some(handle) => self.flush_pending(handle).await?,
            None if changes.size.is_some() => self.flush_pending_for_inode(inode).await?,
            None => {}
        }
        let remote_handle = match handle {
            Some(handle) => {
                let opened = self.file_handle(handle)?;
                if opened.inode != inode {
                    return Err(AdapterError::HandleInodeMismatch);
                }
                Some(opened.remote)
            }
            None => None,
        };
        let metadata = self
            .execute(async {
                let metadata = self
                    .state
                    .remote
                    .set_attributes(record.node, remote_handle, changes)
                    .await?;
                validate_metadata(record.node, &metadata)?;
                Ok(metadata)
            })
            .await?;
        self.invalidate_node_views(record.node)?;
        self.remember_metadata(metadata.clone())?;
        if let Some(handle) = handle {
            self.update_file_handle(handle, metadata.revision, metadata.size)?;
        }
        Ok(metadata)
    }

    pub(crate) async fn read_async(
        &self,
        handle: u64,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, AdapterError> {
        validate_io_range(offset, length)?;
        let operation = self.file_operation(handle)?;
        // Read-your-own-writes: only when this handle actually has buffered bytes
        // do we take the exclusive guard to flush them; otherwise reads stay
        // fully concurrent (the common media path). The check-then-flush gap only
        // matters for a concurrent overlapping write, which POSIX leaves
        // unordered anyway.
        if self.has_pending(handle) {
            let _flush_guard = operation.write().await;
            self.flush_pending(handle).await?;
        }
        let _operation_guard = operation.read().await;
        let opened = self.file_handle(handle)?;
        if !opened.access.can_read() {
            return Err(AdapterError::InvalidAccess);
        }
        let capabilities = self.capabilities_async().await?;
        let chunk_limit = capabilities.max_read_size.min(MAX_CLIENT_READ_SIZE);
        if chunk_limit == 0 {
            return Err(AdapterError::InvalidCapabilities);
        }
        self.execute(async {
            let capacity = usize::try_from(length)
                .map_err(|_| AdapterError::RequestTooLarge(MAX_FUSE_IO_SIZE))?;
            let mut data = Vec::with_capacity(capacity);
            let mut remaining = length;
            let mut position = offset;
            let mut expected_revision = opened.revision;
            while remaining > 0 {
                let requested = remaining.min(chunk_limit);
                let result = self
                    .state
                    .remote
                    .read_range_versioned(opened.remote, position, requested)
                    .await?;
                let received = u64::try_from(result.data.len())
                    .map_err(|_| AdapterError::UnexpectedReadLength)?;
                if received > requested {
                    return Err(AdapterError::UnexpectedReadLength);
                }
                if result.revision != 0 {
                    if expected_revision != 0 && result.revision != expected_revision {
                        return Err(AdapterError::StaleRevision);
                    }
                    expected_revision = result.revision;
                }
                data.extend_from_slice(&result.data);
                if received < requested {
                    break;
                }
                position = position
                    .checked_add(received)
                    .ok_or(AdapterError::InvalidRange)?;
                remaining -= received;
            }
            Ok(data)
        })
        .await
    }

    pub(crate) async fn write_async(
        &self,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, AdapterError> {
        let length = u64::try_from(data.len())
            .map_err(|_| AdapterError::RequestTooLarge(MAX_FUSE_IO_SIZE))?;
        validate_io_range(offset, length)?;
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        let capabilities = self.require_writable().await?;
        let opened = self.file_handle(handle)?;
        if !opened.access.can_write() {
            return Err(AdapterError::InvalidAccess);
        }
        let threshold = capabilities.max_write_size.min(MAX_CLIENT_WRITE_SIZE);
        if threshold == 0 {
            return Err(AdapterError::InvalidCapabilities);
        }
        // Coalesce a contiguous run of small writes into the per-handle buffer.
        // The FUSE reply reports every byte accepted; the bytes are transmitted
        // when the buffer fills, a non-contiguous write arrives, or an operation
        // that must see the server's view flushes it (`flush_pending`).
        let mut to_flush: Option<(u64, Vec<u8>)> = None;
        let appended = {
            let mut pending = self
                .state
                .pending_writes
                .lock()
                .map_err(|_| AdapterError::StateUnavailable)?;
            let can_append = pending.get(&handle).is_some_and(|existing| {
                existing.end() == offset
                    && (existing.buffer.len() as u64).saturating_add(length) <= threshold
            });
            if can_append {
                pending
                    .get_mut(&handle)
                    .ok_or(AdapterError::UnknownHandle)?
                    .buffer
                    .extend_from_slice(data);
                true
            } else {
                if let Some(removed) = pending.remove(&handle) {
                    to_flush = Some((removed.start, removed.buffer));
                }
                false
            }
        };
        if appended {
            return Ok(length);
        }
        if let Some((start, buffer)) = to_flush {
            self.write_through(handle, start, &buffer).await?;
        }
        if length < threshold {
            self.state
                .pending_writes
                .lock()
                .map_err(|_| AdapterError::StateUnavailable)?
                .insert(
                    handle,
                    PendingWrite {
                        inode: opened.inode,
                        start: offset,
                        buffer: data.to_vec(),
                    },
                );
        } else {
            self.write_through(handle, offset, data).await?;
        }
        Ok(length)
    }

    /// Transmit `data` at `offset` for `handle` immediately, chunked to the
    /// negotiated write limit. The caller must already hold the handle's
    /// exclusive operation guard. Used both to flush the coalescing buffer and
    /// to send writes that are individually at or above the buffer threshold.
    async fn write_through(
        &self,
        handle: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), AdapterError> {
        let length = data.len() as u64;
        if length == 0 {
            return Ok(());
        }
        let opened = self.file_handle(handle)?;
        let capabilities = self.capabilities_async().await?;
        let chunk_limit = capabilities.max_write_size.min(MAX_CLIENT_WRITE_SIZE);
        if chunk_limit == 0 {
            return Err(AdapterError::InvalidCapabilities);
        }
        let (revision, size) = self
            .execute(async {
                let mut written = 0_u64;
                let mut position = offset;
                let mut revision = opened.revision;
                let mut size = opened.size;
                while written < length {
                    let remaining = length - written;
                    let amount = remaining.min(chunk_limit);
                    let start = usize::try_from(written)
                        .map_err(|_| AdapterError::RequestTooLarge(MAX_FUSE_IO_SIZE))?;
                    let end = usize::try_from(written + amount)
                        .map_err(|_| AdapterError::RequestTooLarge(MAX_FUSE_IO_SIZE))?;
                    let result = self
                        .state
                        .remote
                        .write_range(opened.remote, position, &data[start..end])
                        .await?;
                    if result.written != amount {
                        return Err(AdapterError::UnexpectedWriteLength);
                    }
                    written += result.written;
                    position = position
                        .checked_add(result.written)
                        .ok_or(AdapterError::InvalidRange)?;
                    revision = result.revision;
                    size = result.size;
                }
                Ok((revision, size))
            })
            .await?;
        self.update_file_handle(handle, revision, size)?;
        let node = self.inode_record(opened.inode)?.node;
        self.invalidate_node_views(node)?;
        Ok(())
    }

    /// Flush any coalesced bytes for `handle` to the server. The caller must
    /// hold the handle's exclusive operation guard so the flush is ordered with
    /// concurrent writes; the buffer is removed atomically so racing flushers do
    /// not double-transmit.
    async fn flush_pending(&self, handle: u64) -> Result<(), AdapterError> {
        let pending = self
            .state
            .pending_writes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .remove(&handle);
        if let Some(pending) = pending {
            self.write_through(handle, pending.start, &pending.buffer)
                .await?;
        }
        Ok(())
    }

    /// Whether `handle` currently has any coalesced, not-yet-transmitted bytes.
    fn has_pending(&self, handle: u64) -> bool {
        self.state
            .pending_writes
            .lock()
            .map(|pending| pending.contains_key(&handle))
            .unwrap_or(false)
    }

    /// Flush coalesced bytes for every open write handle on `inode`. Used by
    /// path-based operations (a handle-less truncate) that have no single handle
    /// to flush. Best-effort ordering only: it does not hold each handle's guard,
    /// which is acceptable because a path mutation racing a live buffered writer
    /// on the same file is already unordered.
    async fn flush_pending_for_inode(&self, inode: u64) -> Result<(), AdapterError> {
        let handles: Vec<u64> = {
            let pending = self
                .state
                .pending_writes
                .lock()
                .map_err(|_| AdapterError::StateUnavailable)?;
            pending
                .iter()
                .filter(|(_, buffered)| buffered.inode == inode)
                .map(|(handle, _)| *handle)
                .collect()
        };
        for handle in handles {
            self.flush_pending(handle).await?;
        }
        Ok(())
    }

    /// Largest byte offset covered by a still-buffered write for `inode`, if
    /// any. `getattr` reports this so a `stat` immediately after a buffered
    /// write observes the correct size before the bytes reach the server.
    fn pending_end_for_inode(&self, inode: u64) -> Option<u64> {
        self.state
            .pending_writes
            .lock()
            .ok()?
            .values()
            .filter(|pending| pending.inode == inode)
            .map(PendingWrite::end)
            .max()
    }

    pub(crate) async fn flush_async(
        &self,
        handle: u64,
        lock_owner: Option<u64>,
    ) -> Result<(), AdapterError> {
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        self.flush_pending(handle).await?;
        let opened = self.file_handle(handle)?;
        if !opened.access.can_write() && !opened.used_locks {
            // A read-only handle with no locks has nothing a remote flush
            // could observe: no coalesced bytes exist and there are no
            // POSIX locks for the server to release.
            return Ok(());
        }
        self.execute(async {
            self.state
                .remote
                .flush_file(opened.remote, lock_owner)
                .await?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn fsync_async(
        &self,
        handle: u64,
        data_only: bool,
    ) -> Result<(), AdapterError> {
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        self.flush_pending(handle).await?;
        let opened = self.file_handle(handle)?;
        self.execute(async {
            self.state
                .remote
                .sync_file(opened.remote, data_only)
                .await?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn fsyncdir_async(&self, inode: u64, handle: u64) -> Result<(), AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_directory_sync {
            return if capabilities.writable {
                Err(AdapterError::Unsupported)
            } else {
                Ok(())
            };
        }
        self.directory_listing(handle, inode)?;
        let record = self.inode_record(inode)?;
        self.execute(async {
            self.state.remote.sync_directory(record.node).await?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn allocate_async(
        &self,
        handle: u64,
        offset: u64,
        length: u64,
    ) -> Result<(), AdapterError> {
        validate_allocation_range(offset, length)?;
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_preallocation {
            return Err(AdapterError::Unsupported);
        }
        let opened = self.file_handle(handle)?;
        if !opened.access.can_write() {
            return Err(AdapterError::InvalidAccess);
        }
        self.flush_pending(handle).await?;
        let opened = self.file_handle(handle)?;
        let result = self
            .execute(async {
                Ok(self
                    .state
                    .remote
                    .allocate_file(opened.remote, offset, length)
                    .await?)
            })
            .await?;
        self.update_file_handle(handle, result.revision, result.size)?;
        let node = self.inode_record(opened.inode)?.node;
        self.invalidate_node_views(node)?;
        Ok(())
    }

    pub(crate) async fn copy_file_range_async(
        &self,
        input_handle: u64,
        input_offset: u64,
        output_handle: u64,
        output_offset: u64,
        length: u64,
    ) -> Result<u64, AdapterError> {
        validate_range(input_offset, length)?;
        validate_range(output_offset, length)?;
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_copy_file_range {
            return Err(AdapterError::Unsupported);
        }
        let input = self.file_handle(input_handle)?;
        let output = self.file_handle(output_handle)?;
        if !input.access.can_read() || !output.access.can_write() {
            return Err(AdapterError::InvalidAccess);
        }
        let input_operation = self.file_operation(input_handle)?;
        let output_operation = self.file_operation(output_handle)?;
        let same_operation = Arc::ptr_eq(&input_operation, &output_operation);
        let (first, second) = if input_handle <= output_handle {
            (&input_operation, &output_operation)
        } else {
            (&output_operation, &input_operation)
        };
        let _first = first.write().await;
        let _second = if same_operation {
            None
        } else {
            Some(second.write().await)
        };
        // The server copies from its own bytes, so both sides must be flushed:
        // the source so the copied range is current, the destination so the
        // copy lands after any writes the client already accepted.
        self.flush_pending(input_handle).await?;
        if input_handle != output_handle {
            self.flush_pending(output_handle).await?;
        }
        let input = self.file_handle(input_handle)?;
        let output = self.file_handle(output_handle)?;
        let result = self
            .execute(async {
                let result = self
                    .state
                    .remote
                    .copy_file_range(
                        input.remote,
                        input_offset,
                        output.remote,
                        output_offset,
                        length,
                    )
                    .await?;
                if result.written > length {
                    return Err(AdapterError::UnexpectedWriteLength);
                }
                Ok(result)
            })
            .await?;
        self.update_file_handle(output_handle, result.revision, result.size)?;
        let node = self.inode_record(output.inode)?.node;
        self.invalidate_node_views(node)?;
        Ok(result.written)
    }

    pub(crate) async fn lseek_async(
        &self,
        handle: u64,
        offset: u64,
        whence: SeekWhence,
    ) -> Result<u64, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_seek_data_hole {
            return Err(AdapterError::Unsupported);
        }
        // SEEK_DATA/SEEK_HOLE are answered from the server's data map, so any
        // buffered writes must reach the server first or a hole/data boundary
        // would be reported for bytes the client already accepted.
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        self.flush_pending(handle).await?;
        let opened = self.file_handle(handle)?;
        self.execute(async {
            Ok(self
                .state
                .remote
                .seek_file(opened.remote, offset, whence)
                .await?)
        })
        .await
    }

    pub(crate) async fn safe_ioctl_async(
        &self,
        handle: u64,
        operation: SafeIoctl,
    ) -> Result<u64, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_safe_ioctl {
            return Err(AdapterError::Unsupported);
        }
        let opened = self.file_handle(handle)?;
        self.execute(async {
            Ok(self
                .state
                .remote
                .safe_ioctl(opened.remote, operation)
                .await?)
        })
        .await
    }

    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    pub(crate) fn poll_events(
        &self,
        handle: u64,
        requested: fuser::PollEvents,
    ) -> Result<fuser::PollEvents, AdapterError> {
        let opened = self.file_handle(handle)?;
        let mut ready = fuser::PollEvents::empty();
        if opened.access.can_read() {
            ready |= fuser::PollEvents::POLLIN | fuser::PollEvents::POLLRDNORM;
        }
        if opened.access.can_write() {
            ready |= fuser::PollEvents::POLLOUT | fuser::PollEvents::POLLWRNORM;
        }
        Ok(ready & requested)
    }

    pub(crate) async fn map_block_async(
        &self,
        inode: u64,
        block_size: u32,
        block: u64,
    ) -> Result<u64, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_bmap {
            return Err(AdapterError::Unsupported);
        }
        let node = self.inode_record(inode)?.node;
        self.execute(async { Ok(self.state.remote.map_block(node, block_size, block).await?) })
            .await
    }

    pub(crate) async fn exchange_data_async(
        &self,
        parent_inode: u64,
        name: Name,
        new_parent_inode: u64,
        new_name: Name,
        options: u64,
    ) -> Result<(), AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_exchange_data {
            return Err(AdapterError::Unsupported);
        }
        validate_name(&name).map_err(|()| AdapterError::InvalidName)?;
        validate_name(&new_name).map_err(|()| AdapterError::InvalidName)?;
        let source_node = self.entry_node(parent_inode, &name)?;
        let destination_node = self.entry_node(new_parent_inode, &new_name)?;
        let parent = self.inode_record(parent_inode)?.node;
        let new_parent = self.inode_record(new_parent_inode)?.node;
        self.execute(async {
            self.state
                .remote
                .exchange_data(parent, name.clone(), new_parent, new_name.clone(), options)
                .await?;
            Ok(())
        })
        .await?;
        if let Some(node) = source_node {
            self.invalidate_node_views(node)?;
        }
        if let Some(node) = destination_node {
            self.invalidate_node_views(node)?;
        }
        Ok(())
    }

    pub(crate) async fn set_volume_name_async(&self, name: Name) -> Result<(), AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_volume_rename {
            return Err(AdapterError::Unsupported);
        }
        let cached_name =
            String::from_utf8(name.as_bytes().to_vec()).map_err(|_| AdapterError::InvalidName)?;
        self.execute(async {
            self.state.remote.set_volume_name(name).await?;
            Ok(())
        })
        .await?;
        if let Ok(mut cached) = self.state.capabilities.lock()
            && let Some(capabilities) = cached.as_mut()
        {
            capabilities.volume_name = cached_name;
        }
        Ok(())
    }

    pub(crate) async fn get_lock_async(
        &self,
        handle: u64,
        lock: FileLock,
    ) -> Result<Option<FileLock>, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_locks {
            return Err(AdapterError::Unsupported);
        }
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        let opened = self.file_handle(handle)?;
        self.execute(async { Ok(self.state.remote.get_lock(opened.remote, lock).await?) })
            .await
    }

    pub(crate) async fn set_lock_async(
        &self,
        handle: u64,
        lock: FileLock,
        wait: bool,
    ) -> Result<(), AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_locks {
            return Err(AdapterError::Unsupported);
        }
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        let opened = self.file_handle(handle)?;
        self.execute(async {
            self.state
                .remote
                .set_lock(opened.remote, lock, wait)
                .await?;
            Ok(())
        })
        .await?;
        if let Ok(mut handles) = self.state.handles.lock()
            && let Some(record) = handles.get_mut(&handle)
        {
            record.used_locks = true;
        }
        Ok(())
    }

    pub(crate) async fn statfs_async(&self) -> Result<FilesystemStats, AdapterError> {
        // Serve statfs from the adapter cache and refresh it in the
        // background once it ages past the TTL, so a statfs callback never
        // blocks on the network after the first fetch. macOS statfs-polls
        // mounted volumes aggressively (volume registration, Finder, storage
        // management); paying one round trip per poll floods a high-latency
        // link and stalls CoreServices' volume-registration deadline.
        // Staleness is bounded by the TTL plus one refresh round trip, and
        // statfs carries no coherence contract.
        let cached_statistics = match self.state.filesystem_stats.lock() {
            Ok(cached) => *cached,
            Err(_) => None,
        };
        if let Some((fetched_at, statistics)) = cached_statistics {
            if fetched_at.elapsed() >= FILESYSTEM_STATS_TTL
                && !self
                    .state
                    .filesystem_stats_refreshing
                    .swap(true, Ordering::AcqRel)
            {
                let adapter = self.clone();
                let refresh = self.runtime.spawn(async move {
                    let refreshed = adapter
                        .execute(async { Ok(adapter.state.remote.stat_filesystem().await?) })
                        .await;
                    if let Ok(statistics) = refreshed
                        && let Ok(mut cached) = adapter.state.filesystem_stats.lock()
                    {
                        *cached = Some((Instant::now(), statistics));
                    }
                    // On error the stale entry stays; the next aged callback
                    // retries the refresh.
                    adapter
                        .state
                        .filesystem_stats_refreshing
                        .store(false, Ordering::Release);
                });
                drop(refresh);
            }
            return Ok(statistics);
        }
        let statistics = self
            .execute(async { Ok(self.state.remote.stat_filesystem().await?) })
            .await?;
        if let Ok(mut cached) = self.state.filesystem_stats.lock() {
            *cached = Some((Instant::now(), statistics));
        }
        Ok(statistics)
    }

    /// Warm every cache a macOS volume-registration probe touches, so the
    /// first kernel callbacks after `mount(2)` are answered from memory
    /// instead of paying one network round trip each.
    ///
    /// CoreServices registers a freshly mounted volume by probing it
    /// (statfs, root getattr, AppleDouble sidecar lookups) under a short
    /// internal deadline. On a high-latency link those probes each cost a
    /// full round trip and the registration races its deadline; when it
    /// loses, `coreservicesd` permanently records a broken file-ID tree for
    /// the volume and every LaunchServices/Finder interaction with it fails
    /// with `EIO` for the life of the mount, while plain path-based syscalls
    /// keep working. Serving the registration window from warm caches keeps
    /// a WAN mount indistinguishable from a loopback mount here. Failures
    /// are non-fatal: an unwarmed mount is exactly as functional as before,
    /// it merely re-enters the race.
    pub fn prewarm_for_mount(&self) -> Result<(), AdapterError> {
        self.runtime.clone().block_on(async {
            self.readdir_async(ROOT_INODE).await?;
            self.getattr_async(ROOT_INODE).await?;
            self.statfs_async().await?;
            Ok(())
        })
    }

    pub(crate) async fn xattr_size_async(
        &self,
        inode: u64,
        name: Name,
    ) -> Result<u64, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_xattrs {
            return Err(AdapterError::Unsupported);
        }
        let node = self.inode_record(inode)?.node;
        let revision = self.known_metadata_revision(node).unwrap_or(0);
        if let Some(cached) = self.cached_xattrs(node)? {
            if !cached.names.contains(&name) {
                return Err(no_attribute_error());
            }
            if let Some(size) = cached.sizes.get(&name) {
                return Ok(*size);
            }
        }
        let size = self
            .execute(async {
                let read = self
                    .state
                    .remote
                    .get_xattr(node, name.clone(), 0, 0)
                    .await?;
                if read.total_size > MAX_FUSE_XATTR_SIZE {
                    return Err(AdapterError::RequestTooLarge(MAX_FUSE_XATTR_SIZE));
                }
                Ok(read.total_size)
            })
            .await?;
        self.remember_xattr_size(node, revision, name, size)?;
        Ok(size)
    }

    pub(crate) async fn get_xattr_async(
        &self,
        inode: u64,
        name: Name,
    ) -> Result<Vec<u8>, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_xattrs {
            return Err(AdapterError::Unsupported);
        }
        let node = self.inode_record(inode)?.node;
        let chunk = capabilities.max_read_size.min(MAX_CLIENT_READ_SIZE);
        if chunk == 0 {
            return Err(AdapterError::InvalidCapabilities);
        }
        let revision = self.known_metadata_revision(node).unwrap_or(0);
        let mut known_size = None;
        if let Some(cached) = self.cached_xattrs(node)? {
            if !cached.names.contains(&name) {
                return Err(no_attribute_error());
            }
            if let Some(value) = cached.values.get(&name) {
                return Ok(value.clone());
            }
            known_size = cached.sizes.get(&name).copied();
        }
        let value = self
            .execute(async {
                let (total_size, mut value) = if let Some(total_size) = known_size {
                    (total_size, Vec::new())
                } else {
                    let first = self
                        .state
                        .remote
                        .get_xattr(node, name.clone(), 0, chunk)
                        .await?;
                    let expected = first.total_size.min(chunk);
                    if first.data.len() as u64 != expected {
                        return Err(AdapterError::UnexpectedReadLength);
                    }
                    (first.total_size, first.data)
                };
                if total_size > MAX_FUSE_XATTR_SIZE {
                    return Err(AdapterError::RequestTooLarge(MAX_FUSE_XATTR_SIZE));
                }
                let capacity = usize::try_from(total_size)
                    .map_err(|_| AdapterError::RequestTooLarge(MAX_FUSE_XATTR_SIZE))?;
                value.reserve(capacity.saturating_sub(value.len()));
                let mut offset = value.len() as u64;
                while offset < total_size {
                    let amount = (total_size - offset).min(chunk);
                    let read = self
                        .state
                        .remote
                        .get_xattr(node, name.clone(), offset, amount)
                        .await?;
                    if read.total_size != total_size || read.data.len() as u64 != amount {
                        return Err(AdapterError::UnexpectedReadLength);
                    }
                    value.extend_from_slice(&read.data);
                    offset += amount;
                }
                Ok(value)
            })
            .await?;
        self.remember_xattr_value(node, revision, name, value.clone())?;
        Ok(value)
    }

    pub(crate) async fn set_xattr_async(
        &self,
        inode: u64,
        name: Name,
        value: Vec<u8>,
        mode: XattrSetMode,
        position: u32,
    ) -> Result<(), AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_xattrs {
            return Err(AdapterError::Unsupported);
        }
        if value.len() as u64 > MAX_FUSE_XATTR_SIZE {
            return Err(AdapterError::RequestTooLarge(MAX_FUSE_XATTR_SIZE));
        }
        let chunk = capabilities.max_write_size.min(MAX_CLIENT_WRITE_SIZE);
        if chunk == 0 {
            return Err(AdapterError::InvalidCapabilities);
        }
        let node = self.inode_record(inode)?.node;
        self.execute(async {
            if value.is_empty() {
                self.state
                    .remote
                    .set_xattr(node, name, &[], mode, position)
                    .await?;
                return Ok(());
            }
            let mut consumed = 0_u64;
            while consumed < value.len() as u64 {
                let amount = (value.len() as u64 - consumed).min(chunk);
                let start = consumed as usize;
                let end = (consumed + amount) as usize;
                let chunk_position = u64::from(position)
                    .checked_add(consumed)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(AdapterError::InvalidRange)?;
                self.state
                    .remote
                    .set_xattr(
                        node,
                        name.clone(),
                        &value[start..end],
                        if consumed == 0 {
                            mode
                        } else {
                            XattrSetMode::Replace
                        },
                        chunk_position,
                    )
                    .await?;
                consumed += amount;
            }
            Ok(())
        })
        .await?;
        self.invalidate_node_views(node)?;
        Ok(())
    }

    pub(crate) async fn list_xattrs_async(&self, inode: u64) -> Result<Vec<Name>, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if !capabilities.supports_xattrs {
            return Err(AdapterError::Unsupported);
        }
        let node = self.inode_record(inode)?.node;
        if let Some(cached) = self.cached_xattrs(node)? {
            return Ok(cached.names);
        }
        let revision = self.known_metadata_revision(node).unwrap_or(0);
        let names = self
            .execute(async { Ok(self.state.remote.list_xattrs(node).await?) })
            .await?;
        self.remember_xattr_snapshot(
            node,
            revision,
            XattrSnapshot {
                names: names.clone(),
                inline_values: Vec::new(),
            },
        )?;
        Ok(names)
    }

    pub(crate) async fn remove_xattr_async(
        &self,
        inode: u64,
        name: Name,
    ) -> Result<(), AdapterError> {
        let capabilities = self.require_writable().await?;
        if !capabilities.supports_xattrs {
            return Err(AdapterError::Unsupported);
        }
        let node = self.inode_record(inode)?.node;
        self.execute(async {
            self.state.remote.remove_xattr(node, name).await?;
            Ok(())
        })
        .await?;
        self.invalidate_node_views(node)?;
        Ok(())
    }

    pub(crate) async fn release_async(
        &self,
        handle: u64,
        flush: bool,
        lock_owner: Option<u64>,
    ) -> Result<(), AdapterError> {
        let operation = self.file_operation(handle)?;
        let _operation_guard = operation.write().await;
        if let Ok(opened) = self.file_handle(handle)
            && !opened.access.can_write()
            && !opened.used_locks
        {
            // No coalesced bytes and no locks: the reply does not depend on
            // the server, so close the remote descriptor in the background
            // instead of charging one round trip per sidecar/preview read
            // to the caller. Errors only leak a server handle, which
            // disconnect cleanup reclaims.
            let opened = self.take_file_handle(handle)?;
            let adapter = self.clone();
            drop(self.runtime.spawn(async move {
                let _ = adapter
                    .execute(async {
                        adapter.state.remote.close_file(opened.remote).await?;
                        Ok(())
                    })
                    .await;
            }));
            let _ = self.forget_inode(opened.inode, 0);
            return Ok(());
        }
        // Transmit any coalesced bytes before closing. If this fails the handle
        // is still closed below so the remote descriptor is not leaked, but the
        // error is surfaced to the caller (close/last-flush is the sanctioned
        // point for a deferred write error to appear).
        let pending_result = self.flush_pending(handle).await;
        let opened = self.take_file_handle(handle)?;
        let flush_result = if flush {
            self.execute(async {
                self.state
                    .remote
                    .flush_file(opened.remote, lock_owner)
                    .await?;
                Ok(())
            })
            .await
        } else {
            Ok(())
        };
        let flush_result = pending_result.and(flush_result);
        let close_result = self
            .execute(async {
                self.state.remote.close_file(opened.remote).await?;
                Ok(())
            })
            .await;
        let result = flush_result.and(close_result);
        let _ = self.forget_inode(opened.inode, 0);
        result
    }

    pub(crate) fn destroy_mount(&self) {
        let handles = match self.state.handles.lock() {
            Ok(mut handles) => handles
                .drain()
                .map(|(handle, record)| (handle, record.remote))
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        let mut operations = match self.state.handle_operations.lock() {
            Ok(mut operations) => operations.drain().collect::<HashMap<_, _>>(),
            Err(_) => HashMap::new(),
        };
        if let Ok(mut directories) = self.state.directory_handles.lock() {
            directories.clear();
        }
        if let Ok(mut directories) = self.state.discovered_directories.lock() {
            directories.clear();
        }
        if let Ok(mut xattrs) = self.state.discovered_xattrs.lock() {
            xattrs.clear();
        }
        if let Ok(mut directory_operations) = self.state.directory_operations.lock() {
            directory_operations.clear();
        }
        // Closing the authenticated transport releases every server handle.
        // Individual closes are best-effort and never hold unmount hostage
        // when the server is unavailable.
        for (handle, remote_handle) in handles {
            let remote = Arc::clone(&self.state.remote);
            let operation = operations.remove(&handle);
            drop(self.runtime.spawn(async move {
                if let Some(operation) = operation {
                    let _operation_guard = operation.write().await;
                    let _ = tokio::time::timeout(
                        MOUNT_CLEANUP_TIMEOUT,
                        remote.close_file(remote_handle),
                    )
                    .await;
                } else {
                    let _ = tokio::time::timeout(
                        MOUNT_CLEANUP_TIMEOUT,
                        remote.close_file(remote_handle),
                    )
                    .await;
                }
            }));
        }
    }

    fn block_on<T>(
        &self,
        future: impl Future<Output = Result<T, AdapterError>>,
    ) -> Result<T, AdapterError> {
        self.runtime.block_on(future)
    }

    async fn execute<T>(
        &self,
        future: impl Future<Output = Result<T, AdapterError>>,
    ) -> Result<T, AdapterError> {
        tokio::time::timeout(self.callback_timeout(), future)
            .await
            .map_err(|_| AdapterError::CallbackTimedOut)?
    }

    async fn require_writable(&self) -> Result<FilesystemCapabilities, AdapterError> {
        let capabilities = self.capabilities_async().await?;
        if capabilities.writable {
            Ok(capabilities)
        } else {
            Err(AdapterError::ReadOnly)
        }
    }

    fn inode_record(&self, inode: u64) -> Result<InodeRecord, AdapterError> {
        self.state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .by_inode
            .get(&inode)
            .copied()
            .ok_or(AdapterError::UnknownInode)
    }

    fn cached_metadata(&self, node: NodeId) -> Result<Option<Metadata>, AdapterError> {
        let mut metadata = self
            .state
            .discovered_metadata
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if metadata
            .get(&node)
            .is_some_and(|entry| entry.discovered_at.elapsed() <= DISCOVERED_METADATA_TTL)
        {
            return Ok(metadata.get(&node).map(|entry| entry.value.clone()));
        }
        metadata.remove(&node);
        Ok(None)
    }

    fn remember_metadata(&self, value: Metadata) -> Result<(), AdapterError> {
        self.state
            .discovered_metadata
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .insert(
                value.node,
                CachedMetadata {
                    value,
                    discovered_at: Instant::now(),
                },
            );
        Ok(())
    }

    fn known_metadata_revision(&self, node: NodeId) -> Option<u64> {
        self.state
            .discovered_metadata
            .lock()
            .ok()
            .and_then(|metadata| metadata.get(&node).map(|entry| entry.value.revision))
    }

    fn cached_directory(&self, inode: u64) -> Result<Option<Arc<DirectoryListing>>, AdapterError> {
        let mut directories = self
            .state
            .discovered_directories
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if directories
            .get(&inode)
            .is_some_and(|entry| entry.discovered_at.elapsed() <= DISCOVERED_DIRECTORY_TTL)
        {
            return Ok(directories.get(&inode).map(|entry| entry.listing.clone()));
        }
        directories.remove(&inode);
        Ok(None)
    }

    fn directory_operation(&self, node: NodeId) -> Result<Arc<AsyncMutex<()>>, AdapterError> {
        Ok(self
            .state
            .directory_operations
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .entry(node)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone())
    }

    fn remember_directory_view(
        &self,
        inode: u64,
        record: InodeRecord,
        view: DirectoryView,
    ) -> Result<Arc<DirectoryListing>, AdapterError> {
        validate_metadata(record.node, &view.directory)?;
        if view.directory.kind != NodeKind::Directory {
            return Err(AdapterError::UnexpectedMetadata);
        }
        let parent_node = self.inode_record(record.parent_inode)?.node;
        validate_metadata(parent_node, &view.parent)?;
        if view.parent.kind != NodeKind::Directory {
            return Err(AdapterError::UnexpectedMetadata);
        }
        self.remember_metadata(view.directory.clone())?;
        self.remember_metadata(view.parent.clone())?;
        if let Some(xattrs) = view.xattrs {
            self.remember_xattr_snapshot(record.node, view.directory.revision, xattrs)?;
        }

        let mut entries = Vec::with_capacity(view.entries.len());
        for entry in view.entries {
            if let Some(xattrs) = entry.xattrs {
                self.remember_xattr_snapshot(
                    entry.entry.node,
                    entry.entry.metadata.revision,
                    xattrs,
                )?;
            }
            entries.push(entry.entry);
        }
        self.remember_directory_snapshot(
            inode,
            record.parent_inode,
            DirectorySnapshot {
                revision: view.revision,
                entries,
            },
        )
    }

    fn remember_directory_snapshot(
        &self,
        inode: u64,
        parent_inode: u64,
        snapshot: DirectorySnapshot,
    ) -> Result<Arc<DirectoryListing>, AdapterError> {
        validate_case_insensitive_directory(&snapshot.entries)?;
        self.reconcile_directory_entries(
            inode,
            snapshot
                .entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect(),
        )?;
        let mut entries = Vec::with_capacity(snapshot.entries.len());
        for entry in snapshot.entries {
            validate_remote_name(&entry)?;
            validate_metadata(entry.node, &entry.metadata)?;
            if entry.kind != entry.metadata.kind {
                return Err(AdapterError::UnexpectedMetadata);
            }
            self.remember_metadata(entry.metadata.clone())?;
            entries.push(DirectoryEntry {
                inode: self.remember_entry(entry.node, inode, &entry.name)?,
                name: entry.name,
                kind: entry.kind,
                metadata: entry.metadata,
            });
        }
        let listing = Arc::new(DirectoryListing {
            parent_inode,
            revision: snapshot.revision,
            entries,
        });
        self.state
            .discovered_directories
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .insert(
                inode,
                CachedDirectory {
                    listing: listing.clone(),
                    discovered_at: Instant::now(),
                },
            );
        Ok(listing)
    }

    fn lookup_from_listing(
        &self,
        parent_inode: u64,
        listing: &DirectoryListing,
        requested: &Name,
    ) -> Result<LookupResult, AdapterError> {
        let index = select_entry_index(&listing.entries, |entry| &entry.name, requested)?;
        let entry = &listing.entries[index];
        let inode = self.remember_entry(entry.metadata.node, parent_inode, &entry.name)?;
        self.add_lookup(inode, 1)?;
        self.remember_metadata(entry.metadata.clone())?;
        Ok(LookupResult {
            inode,
            metadata: entry.metadata.clone(),
        })
    }

    fn invalidate_directory(&self, inode: u64) -> Result<(), AdapterError> {
        let node = self.inode_record(inode)?.node;
        self.invalidate_node_views(node)
    }

    /// Remove every local projection that embeds this node's metadata. A file
    /// write or xattr update changes the child's revision without changing its
    /// parent directory revision, so invalidating only the node's own metadata
    /// would let a later cached lookup resurrect the stale revision. Scanning
    /// the small set of active native views also covers every hard-link parent.
    fn invalidate_node_views(&self, node: NodeId) -> Result<(), AdapterError> {
        let inode = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .by_node
            .get(&node)
            .copied();
        self.state
            .discovered_metadata
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .remove(&node);
        self.state
            .discovered_xattrs
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .remove(&node);
        self.state
            .discovered_directories
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .retain(|directory_inode, cached| {
                Some(*directory_inode) != inode
                    && !cached
                        .listing
                        .entries
                        .iter()
                        .any(|entry| entry.metadata.node == node)
            });
        Ok(())
    }

    fn entry_node(&self, parent_inode: u64, name: &Name) -> Result<Option<NodeId>, AdapterError> {
        let table = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        Ok(table
            .by_entry
            .get(&(parent_inode, name.clone()))
            .and_then(|inode| table.by_inode.get(inode))
            .map(|record| record.node))
    }

    fn cached_xattrs(&self, node: NodeId) -> Result<Option<CachedXattrs>, AdapterError> {
        let mut xattrs = self
            .state
            .discovered_xattrs
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let known_revision = self.known_metadata_revision(node);
        if xattrs.get(&node).is_some_and(|entry| {
            known_revision.is_none_or(|revision| revision == entry.revision)
                && entry.discovered_at.elapsed() <= DISCOVERED_DIRECTORY_TTL
        }) {
            return Ok(xattrs.get(&node).cloned());
        }
        xattrs.remove(&node);
        Ok(None)
    }

    fn remember_xattr_snapshot(
        &self,
        node: NodeId,
        revision: u64,
        snapshot: XattrSnapshot,
    ) -> Result<(), AdapterError> {
        let mut unique_names = snapshot.names.clone();
        unique_names.sort();
        unique_names.dedup();
        if unique_names.len() != snapshot.names.len() {
            return Err(AdapterError::UnexpectedMetadata);
        }
        let mut values = HashMap::new();
        let mut sizes = HashMap::new();
        for value in snapshot.inline_values {
            if !snapshot.names.contains(&value.name)
                || values
                    .insert(value.name.clone(), value.value.clone())
                    .is_some()
            {
                return Err(AdapterError::UnexpectedMetadata);
            }
            sizes.insert(value.name, value.value.len() as u64);
        }
        self.state
            .discovered_xattrs
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .insert(
                node,
                CachedXattrs {
                    revision,
                    names: snapshot.names,
                    sizes,
                    values,
                    discovered_at: Instant::now(),
                },
            );
        Ok(())
    }

    fn remember_xattr_size(
        &self,
        node: NodeId,
        revision: u64,
        name: Name,
        size: u64,
    ) -> Result<(), AdapterError> {
        let mut xattrs = self
            .state
            .discovered_xattrs
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if let Some(cached) = xattrs.get_mut(&node)
            && cached.revision == revision
            && cached.names.contains(&name)
        {
            cached.sizes.insert(name, size);
            cached.discovered_at = Instant::now();
        }
        Ok(())
    }

    fn remember_xattr_value(
        &self,
        node: NodeId,
        revision: u64,
        name: Name,
        value: Vec<u8>,
    ) -> Result<(), AdapterError> {
        let mut xattrs = self
            .state
            .discovered_xattrs
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if let Some(cached) = xattrs.get_mut(&node)
            && cached.revision == revision
            && cached.names.contains(&name)
        {
            cached.sizes.insert(name.clone(), value.len() as u64);
            cached.values.insert(name, value);
            cached.discovered_at = Instant::now();
        }
        Ok(())
    }

    fn find_entry_inode(
        &self,
        parent_inode: u64,
        requested: &Name,
    ) -> Result<Option<u64>, AdapterError> {
        let table = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if let Some(inode) = table.by_entry.get(&(parent_inode, requested.clone())) {
            return Ok(Some(*inode));
        }
        let requested = match std::str::from_utf8(requested.as_bytes()) {
            Ok(requested) => normalized_case_name(requested),
            Err(_) => return Ok(None),
        };
        let matching = table
            .by_entry
            .iter()
            .filter_map(|((parent, name), inode)| {
                (*parent == parent_inode
                    && std::str::from_utf8(name.as_bytes())
                        .ok()
                        .is_some_and(|name| normalized_case_name(name) == requested))
                .then_some(*inode)
            })
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [inode] => Ok(Some(*inode)),
            [] => Ok(None),
            _ => Err(AdapterError::AmbiguousName),
        }
    }

    fn reconcile_directory_entries(
        &self,
        parent_inode: u64,
        current_names: std::collections::HashSet<Name>,
    ) -> Result<(), AdapterError> {
        self.state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .by_entry
            .retain(|(parent, name), _| *parent != parent_inode || current_names.contains(name));
        Ok(())
    }

    pub(crate) fn remember_entry(
        &self,
        node: NodeId,
        parent_inode: u64,
        name: &Name,
    ) -> Result<u64, AdapterError> {
        let mut table = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let inode = if let Some(inode) = table.by_node.get(&node) {
            *inode
        } else {
            let inode = self
                .state
                .next_inode
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    current.checked_add(1)
                })
                .map_err(|_| AdapterError::InodeSpaceExhausted)?;
            table
                .by_inode
                .insert(inode, InodeRecord { node, parent_inode });
            table.by_node.insert(node, inode);
            inode
        };
        table.by_entry.insert((parent_inode, name.clone()), inode);
        Ok(inode)
    }

    pub(crate) fn add_lookup(&self, inode: u64, amount: u64) -> Result<(), AdapterError> {
        let mut table = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let count = table.lookups.entry(inode).or_default();
        *count = count.saturating_add(amount);
        Ok(())
    }

    pub(crate) fn forget_inode(&self, inode: u64, amount: u64) -> Result<(), AdapterError> {
        self.forget_inodes(&[(inode, amount)])
    }

    pub(crate) fn forget_inodes(&self, requests: &[(u64, u64)]) -> Result<(), AdapterError> {
        let mut forgotten = Vec::new();
        for (inode, amount) in requests {
            if let Some(node) = self.evict_inode(*inode, *amount)? {
                if let Ok(mut directories) = self.state.discovered_directories.lock() {
                    directories.remove(inode);
                }
                if let Ok(mut operations) = self.state.directory_operations.lock() {
                    operations.remove(&node);
                }
                forgotten.push(node);
            }
        }
        if !forgotten.is_empty() {
            let remote = Arc::clone(&self.state.remote);
            let timeout = self.callback_timeout();
            drop(self.runtime.spawn(async move {
                let _ = tokio::time::timeout(timeout, remote.forget_nodes(forgotten)).await;
            }));
        }
        Ok(())
    }

    fn evict_inode(&self, inode: u64, amount: u64) -> Result<Option<NodeId>, AdapterError> {
        if inode == ROOT_INODE {
            return Ok(None);
        }
        let in_use = self
            .state
            .handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .values()
            .any(|handle| handle.inode == inode)
            || self
                .state
                .directory_handles
                .lock()
                .map_err(|_| AdapterError::StateUnavailable)?
                .values()
                .any(|handle| handle.inode == inode);
        let mut table = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let count = table.lookups.entry(inode).or_default();
        *count = count.saturating_sub(amount);
        if *count != 0 || in_use {
            return Ok(None);
        }
        table.lookups.remove(&inode);
        let forgotten = if let Some(record) = table.by_inode.remove(&inode) {
            table.by_node.remove(&record.node);
            Some(record.node)
        } else {
            None
        };
        table
            .by_entry
            .retain(|_, entry_inode| *entry_inode != inode);
        Ok(forgotten)
    }

    fn forget_entry(&self, parent_inode: u64, name: &Name) -> Result<(), AdapterError> {
        self.state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .by_entry
            .remove(&(parent_inode, name.clone()));
        Ok(())
    }

    fn move_entry(
        &self,
        parent_inode: u64,
        name: &Name,
        new_parent_inode: u64,
        new_name: &Name,
        mode: RenameMode,
    ) -> Result<(), AdapterError> {
        let mut table = self
            .state
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let source_key = (parent_inode, name.clone());
        let destination_key = (new_parent_inode, new_name.clone());
        let source = table.by_entry.remove(&source_key);
        let destination = table.by_entry.remove(&destination_key);
        if mode == RenameMode::Exchange
            && let Some(destination) = destination
        {
            table.by_entry.insert(source_key, destination);
            if let Some(record) = table.by_inode.get_mut(&destination) {
                record.parent_inode = parent_inode;
            }
        }
        if let Some(source) = source {
            table.by_entry.insert(destination_key, source);
            if let Some(record) = table.by_inode.get_mut(&source) {
                record.parent_inode = new_parent_inode;
            }
        }
        Ok(())
    }

    fn next_handle(&self) -> Result<u64, AdapterError> {
        self.state
            .next_handle
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AdapterError::HandleSpaceExhausted)
    }

    fn remember_file_handle(
        &self,
        inode: u64,
        access: FileAccess,
        opened: quickfs_client_core::OpenedFile,
    ) -> Result<u64, AdapterError> {
        let handle = self.next_handle()?;
        let mut handles = self
            .state
            .handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let mut operations = self
            .state
            .handle_operations
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        handles.insert(
            handle,
            FileHandleRecord {
                remote: opened.handle,
                inode,
                access,
                revision: opened.revision,
                size: opened.size,
                used_locks: false,
            },
        );
        operations.insert(handle, Arc::new(AsyncRwLock::new(())));
        Ok(handle)
    }

    fn file_handle(&self, handle: u64) -> Result<FileHandleRecord, AdapterError> {
        self.state
            .handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .get(&handle)
            .copied()
            .ok_or(AdapterError::UnknownHandle)
    }

    fn update_file_handle(
        &self,
        handle: u64,
        revision: u64,
        size: u64,
    ) -> Result<(), AdapterError> {
        let mut handles = self
            .state
            .handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let record = handles
            .get_mut(&handle)
            .ok_or(AdapterError::UnknownHandle)?;
        record.revision = revision;
        record.size = size;
        Ok(())
    }

    fn file_operation(&self, handle: u64) -> Result<Arc<AsyncRwLock<()>>, AdapterError> {
        self.state
            .handle_operations
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .get(&handle)
            .cloned()
            .ok_or(AdapterError::UnknownHandle)
    }

    fn take_file_handle(&self, handle: u64) -> Result<FileHandleRecord, AdapterError> {
        let mut handles = self
            .state
            .handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let mut operations = self
            .state
            .handle_operations
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let record = handles.remove(&handle).ok_or(AdapterError::UnknownHandle)?;
        operations.remove(&handle);
        // Defensive: a correctly-closed handle is already flushed in
        // `release_async`. Drop any residue so a reused handle id never inherits
        // stale buffered bytes.
        if let Ok(mut pending) = self.state.pending_writes.lock() {
            pending.remove(&handle);
        }
        Ok(record)
    }

    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    pub(crate) fn owner_uid(&self) -> u32 {
        self.state.owner_uid
    }

    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    pub(crate) fn owner_gid(&self) -> u32 {
        self.state.owner_gid
    }
}

fn validate_io_range(offset: u64, length: u64) -> Result<(), AdapterError> {
    if length > MAX_FUSE_IO_SIZE {
        return Err(AdapterError::RequestTooLarge(MAX_FUSE_IO_SIZE));
    }
    validate_range(offset, length)
}

fn validate_allocation_range(offset: u64, length: u64) -> Result<(), AdapterError> {
    if length == 0 {
        return Err(AdapterError::InvalidRange);
    }
    validate_range(offset, length)
}

fn validate_range(offset: u64, length: u64) -> Result<(), AdapterError> {
    offset
        .checked_add(length)
        .map(|_| ())
        .ok_or(AdapterError::InvalidRange)
}

fn no_attribute_error() -> AdapterError {
    ClientError::Server(
        quickfs_protocol::ErrorCode::NoAttribute,
        "extended attribute does not exist".into(),
    )
    .into()
}

fn validate_name(name: &Name) -> Result<(), ()> {
    let name = name.as_bytes();
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        Err(())
    } else {
        Ok(())
    }
}

fn validate_remote_name(entry: &RemoteDirectoryEntry) -> Result<(), AdapterError> {
    validate_name(&entry.name).map_err(|()| AdapterError::InvalidRemoteName)
}

/// fuser 0.17 advertises `FUSE_CASE_INSENSITIVE` unconditionally on macOS.
/// APFS also commonly presents canonically decomposed names, so compare NFD
/// lowercase forms while preserving exact spelling as the first choice.
fn normalized_case_name(name: &str) -> String {
    name.nfd().flat_map(char::to_lowercase).collect()
}

/// Select the single entry a macOS name may address: an exact byte match
/// first, then the Unicode NFD/case-insensitive fallback for valid UTF-8,
/// rejecting ambiguity. Works by reference so a lookup in a large cached
/// directory does not clone the whole listing.
fn select_entry_index<T>(
    entries: &[T],
    name_of: impl Fn(&T) -> &Name,
    requested: &Name,
) -> Result<usize, AdapterError> {
    let mut exact = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| *name_of(entry) == *requested);
    match (exact.next(), exact.next()) {
        (Some((index, _)), None) => return Ok(index),
        (Some(_), Some(_)) => return Err(AdapterError::AmbiguousName),
        (None, _) => {}
    }

    let requested = std::str::from_utf8(requested.as_bytes())
        .map(normalized_case_name)
        .map_err(|_| AdapterError::NotFound)?;
    let mut matching = entries.iter().enumerate().filter(|(_, entry)| {
        std::str::from_utf8(name_of(entry).as_bytes())
            .ok()
            .is_some_and(|name| normalized_case_name(name) == requested)
    });
    match (matching.next(), matching.next()) {
        (Some((index, _)), None) => Ok(index),
        (Some(_), Some(_)) => Err(AdapterError::AmbiguousName),
        (None, _) => Err(AdapterError::NotFound),
    }
}

#[cfg(test)]
fn select_remote_entry(
    mut entries: Vec<RemoteDirectoryEntry>,
    requested: &Name,
) -> Result<RemoteDirectoryEntry, AdapterError> {
    let index = select_entry_index(&entries, |entry| &entry.name, requested)?;
    Ok(entries.swap_remove(index))
}

fn validate_case_insensitive_directory(
    entries: &[RemoteDirectoryEntry],
) -> Result<(), AdapterError> {
    let mut names = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        validate_remote_name(entry)?;
        if let Ok(name) = std::str::from_utf8(entry.name.as_bytes())
            && !names.insert(normalized_case_name(name))
        {
            return Err(AdapterError::AmbiguousName);
        }
    }
    Ok(())
}

fn validate_metadata(node: NodeId, metadata: &Metadata) -> Result<(), AdapterError> {
    if metadata.node == node {
        Ok(())
    } else {
        Err(AdapterError::UnexpectedMetadata)
    }
}

#[cfg(all(target_os = "macos", feature = "macfuse"))]
mod native;

#[cfg(all(target_os = "macos", feature = "macfuse"))]
pub use native::{MacFuseBackend, MountConfig, mount};

#[cfg(all(target_os = "macos", feature = "macfuse"))]
pub const NATIVE_CALLBACKS_IMPLEMENTED: bool = true;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use quickfs_client_core::{
        CreatedFile, OpenedFile, RangeRead, Result as ClientResult, WriteResult,
    };
    use quickfs_protocol::{DirectoryEntry as ProtocolDirectoryEntry, ErrorCode, LockKind};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use uuid::Uuid;

    const FILE_BYTES: &[u8] = b"hello from quicKFS";

    struct MockFilesystem {
        delay: Duration,
        close_count: AtomicUsize,
        read_lengths: Mutex<Vec<u64>>,
        write_lengths: Mutex<Vec<usize>>,
        bytes: Mutex<Vec<u8>>,
        revision: AtomicU64,
        operation_log: Mutex<Vec<String>>,
        delayed_write_response: Option<(u64, Duration)>,
        read_delay: Duration,
        active_reads: AtomicUsize,
        maximum_active_reads: AtomicUsize,
        metadata_requests: AtomicUsize,
        directory_requests: AtomicUsize,
        xattr_requests: AtomicUsize,
    }

    impl MockFilesystem {
        fn immediate() -> Self {
            Self {
                delay: Duration::ZERO,
                close_count: AtomicUsize::new(0),
                read_lengths: Mutex::new(Vec::new()),
                write_lengths: Mutex::new(Vec::new()),
                bytes: Mutex::new(FILE_BYTES.to_vec()),
                revision: AtomicU64::new(7),
                operation_log: Mutex::new(Vec::new()),
                delayed_write_response: None,
                read_delay: Duration::ZERO,
                active_reads: AtomicUsize::new(0),
                maximum_active_reads: AtomicUsize::new(0),
                metadata_requests: AtomicUsize::new(0),
                directory_requests: AtomicUsize::new(0),
                xattr_requests: AtomicUsize::new(0),
            }
        }

        fn delayed(delay: Duration) -> Self {
            Self {
                delay,
                ..Self::immediate()
            }
        }

        fn with_delayed_write_response(offset: u64, delay: Duration) -> Self {
            Self {
                delayed_write_response: Some((offset, delay)),
                ..Self::immediate()
            }
        }

        fn with_read_delay(delay: Duration) -> Self {
            Self {
                read_delay: delay,
                ..Self::immediate()
            }
        }

        async fn wait(&self) {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
        }
    }

    #[async_trait]
    impl RemoteFilesystem for MockFilesystem {
        async fn ping(&self, nonce: u64) -> ClientResult<u64> {
            self.wait().await;
            Ok(nonce)
        }

        async fn capabilities(&self) -> ClientResult<FilesystemCapabilities> {
            Ok(FilesystemCapabilities {
                server_epoch: Uuid::from_u128(99),
                writable: true,
                supports_locks: true,
                supports_atomic_rename: true,
                supports_directory_sync: true,
                supports_preallocation: true,
                supports_symlinks: true,
                supports_xattrs: true,
                supports_hard_links: true,
                supports_special_nodes: true,
                supports_copy_file_range: true,
                supports_seek_data_hole: true,
                supports_safe_ioctl: true,
                supports_poll: true,
                supports_bmap: true,
                supports_exchange_data: true,
                supports_volume_rename: true,
                supports_backup_time: true,
                supports_readdirplus: true,
                persistent_node_ids: true,
                restart_lock_replay: true,
                volume_name: "quicKFS".into(),
                max_read_size: 4,
                max_write_size: 3,
            })
        }

        async fn stat_filesystem(&self) -> ClientResult<FilesystemStats> {
            Ok(FilesystemStats {
                blocks: 100,
                blocks_free: 50,
                blocks_available: 40,
                files: 10,
                files_free: 5,
                block_size: 4096,
                name_length: 255,
                fragment_size: 4096,
            })
        }

        async fn get_metadata(&self, node: NodeId) -> ClientResult<Metadata> {
            self.wait().await;
            self.metadata_requests.fetch_add(1, Ordering::Relaxed);
            match node.0.as_u128() {
                0 => Ok(metadata(ROOT_NODE, NodeKind::Directory, 0)),
                1 => Ok(metadata(
                    file_node(),
                    NodeKind::File,
                    u64::try_from(self.bytes.lock().unwrap().len()).unwrap(),
                )),
                2 => Ok(metadata(directory_node(), NodeKind::Directory, 0)),
                3 => Ok(metadata(symlink_node(), NodeKind::Symlink, 9)),
                _ => Err(not_found()),
            }
        }

        async fn list_directory(&self, node: NodeId) -> ClientResult<Vec<ProtocolDirectoryEntry>> {
            self.wait().await;
            self.directory_requests.fetch_add(1, Ordering::Relaxed);
            if node == ROOT_NODE {
                Ok(vec![
                    ProtocolDirectoryEntry {
                        node: file_node(),
                        name: "hello.txt".into(),
                        kind: NodeKind::File,
                        metadata: metadata(file_node(), NodeKind::File, FILE_BYTES.len() as u64),
                    },
                    ProtocolDirectoryEntry {
                        node: directory_node(),
                        name: "folder".into(),
                        kind: NodeKind::Directory,
                        metadata: metadata(directory_node(), NodeKind::Directory, 0),
                    },
                    ProtocolDirectoryEntry {
                        node: symlink_node(),
                        name: "hello-link".into(),
                        kind: NodeKind::Symlink,
                        metadata: metadata(symlink_node(), NodeKind::Symlink, 9),
                    },
                ])
            } else if node == directory_node() {
                Ok(Vec::new())
            } else {
                Err(not_found())
            }
        }

        async fn list_directory_view(
            &self,
            node: NodeId,
            _options: DirectoryViewOptions,
        ) -> ClientResult<DirectoryView> {
            let entries = self.list_directory(node).await?;
            let directory = match node {
                ROOT_NODE => metadata(ROOT_NODE, NodeKind::Directory, 0),
                node if node == directory_node() => {
                    metadata(directory_node(), NodeKind::Directory, 0)
                }
                _ => return Err(not_found()),
            };
            Ok(DirectoryView {
                revision: directory.revision,
                parent: metadata(ROOT_NODE, NodeKind::Directory, 0),
                directory,
                xattrs: Some(XattrSnapshot {
                    names: Vec::new(),
                    inline_values: Vec::new(),
                }),
                entries: entries
                    .into_iter()
                    .map(|entry| {
                        let xattrs = if entry.node == file_node() {
                            Some(XattrSnapshot {
                                names: vec![Name::from("user.DOSATTRIB")],
                                inline_values: vec![quickfs_protocol::InlineXattr {
                                    name: Name::from("user.DOSATTRIB"),
                                    value: b"0x20".to_vec(),
                                }],
                            })
                        } else {
                            Some(XattrSnapshot {
                                names: Vec::new(),
                                inline_values: Vec::new(),
                            })
                        };
                        quickfs_protocol::DirectoryViewEntry { entry, xattrs }
                    })
                    .collect(),
            })
        }

        async fn open_file(&self, node: NodeId) -> ClientResult<(RemoteFileHandle, u64, u64)> {
            let opened = self
                .open_file_with_options(node, FileOpenOptions::READ_ONLY)
                .await?;
            Ok((opened.handle, opened.revision, opened.size))
        }

        async fn open_file_with_options(
            &self,
            node: NodeId,
            _options: FileOpenOptions,
        ) -> ClientResult<OpenedFile> {
            self.wait().await;
            if node != file_node() {
                return Err(ClientError::Server(
                    ErrorCode::InvalidRequest,
                    "not a regular file".into(),
                ));
            }
            Ok(OpenedFile {
                handle: remote_handle(),
                revision: self.revision.load(Ordering::Relaxed),
                size: u64::try_from(self.bytes.lock().unwrap().len()).unwrap(),
            })
        }

        async fn create_file(
            &self,
            _parent: NodeId,
            name: Name,
            mode: u32,
            options: FileOpenOptions,
        ) -> ClientResult<CreatedFile> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("create:{name}:{mode:o}:{:?}", options.access));
            Ok(CreatedFile {
                metadata: metadata(file_node(), NodeKind::File, 0),
                opened: OpenedFile {
                    handle: remote_handle(),
                    revision: self.revision.load(Ordering::Relaxed),
                    size: 0,
                },
            })
        }

        async fn create_directory(
            &self,
            _parent: NodeId,
            name: Name,
            mode: u32,
        ) -> ClientResult<Metadata> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("mkdir:{name}:{mode:o}"));
            Ok(metadata(NodeId(Uuid::from_u128(4)), NodeKind::Directory, 0))
        }

        async fn create_symlink(
            &self,
            _parent: NodeId,
            name: Name,
            target: Vec<u8>,
        ) -> ClientResult<Metadata> {
            self.operation_log.lock().unwrap().push(format!(
                "symlink:{name}:{}",
                String::from_utf8_lossy(&target)
            ));
            Ok(metadata(
                NodeId(Uuid::from_u128(5)),
                NodeKind::Symlink,
                u64::try_from(target.len()).unwrap(),
            ))
        }

        async fn remove_node(
            &self,
            _parent: NodeId,
            name: Name,
            directory: bool,
        ) -> ClientResult<()> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("remove:{name}:{directory}"));
            Ok(())
        }

        async fn rename_node(
            &self,
            _parent: NodeId,
            name: Name,
            _new_parent: NodeId,
            new_name: Name,
            mode: RenameMode,
        ) -> ClientResult<()> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("rename:{name}:{new_name}:{mode:?}"));
            Ok(())
        }

        async fn read_link(&self, node: NodeId) -> ClientResult<Vec<u8>> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("readlink:{}", node.0));
            Ok(b"hello.txt".to_vec())
        }

        async fn set_attributes(
            &self,
            node: NodeId,
            _handle: Option<RemoteFileHandle>,
            changes: AttributeChanges,
        ) -> ClientResult<Metadata> {
            self.operation_log.lock().unwrap().push(format!(
                "setattr:{:?}:{:?}",
                changes.size, changes.modified_unix_ms
            ));
            if let Some(size) = changes.size {
                let size = usize::try_from(size).unwrap();
                self.bytes.lock().unwrap().resize(size, 0);
            }
            let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
            let mut value = metadata(
                node,
                NodeKind::File,
                u64::try_from(self.bytes.lock().unwrap().len()).unwrap(),
            );
            value.revision = revision;
            if let Some(modified) = changes.modified_unix_ms {
                value.modified_unix_ms = modified;
            }
            if let Some(accessed) = changes.accessed_unix_ms {
                value.accessed_unix_ms = accessed;
            }
            if let Some(mode) = changes.mode {
                value.mode = mode;
            }
            Ok(value)
        }

        async fn read_range(
            &self,
            handle: RemoteFileHandle,
            offset: u64,
            length: u64,
        ) -> ClientResult<Vec<u8>> {
            Ok(self
                .read_range_versioned(handle, offset, length)
                .await?
                .data)
        }

        async fn read_range_versioned(
            &self,
            handle: RemoteFileHandle,
            offset: u64,
            length: u64,
        ) -> ClientResult<RangeRead> {
            self.wait().await;
            if handle != remote_handle() {
                return Err(ClientError::Server(
                    ErrorCode::InvalidHandle,
                    "unknown handle".into(),
                ));
            }
            let active = self.active_reads.fetch_add(1, Ordering::Relaxed) + 1;
            self.maximum_active_reads
                .fetch_max(active, Ordering::Relaxed);
            if !self.read_delay.is_zero() {
                tokio::time::sleep(self.read_delay).await;
            }
            self.read_lengths.lock().unwrap().push(length);
            let bytes = self.bytes.lock().unwrap();
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let data = if start >= bytes.len() {
                Vec::new()
            } else {
                let requested_end = offset.saturating_add(length);
                let end = usize::try_from(requested_end)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                bytes[start..end].to_vec()
            };
            self.active_reads.fetch_sub(1, Ordering::Relaxed);
            Ok(RangeRead {
                revision: self.revision.load(Ordering::Relaxed),
                data,
            })
        }

        async fn write_range(
            &self,
            handle: RemoteFileHandle,
            offset: u64,
            data: &[u8],
        ) -> ClientResult<WriteResult> {
            if handle != remote_handle() {
                return Err(ClientError::Server(
                    ErrorCode::InvalidHandle,
                    "unknown handle".into(),
                ));
            }
            self.write_lengths.lock().unwrap().push(data.len());
            let result = {
                let mut bytes = self.bytes.lock().unwrap();
                let start = usize::try_from(offset).unwrap();
                let end = start + data.len();
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[start..end].copy_from_slice(data);
                WriteResult {
                    written: u64::try_from(data.len()).unwrap(),
                    revision: self.revision.fetch_add(1, Ordering::Relaxed) + 1,
                    size: u64::try_from(bytes.len()).unwrap(),
                }
            };
            if let Some((delayed_offset, delay)) = self.delayed_write_response
                && offset == delayed_offset
            {
                tokio::time::sleep(delay).await;
            }
            Ok(result)
        }

        async fn flush_file(
            &self,
            _handle: RemoteFileHandle,
            lock_owner: Option<u64>,
        ) -> ClientResult<()> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("flush:{lock_owner:?}"));
            Ok(())
        }

        async fn sync_file(&self, _handle: RemoteFileHandle, data_only: bool) -> ClientResult<()> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("fsync:{data_only}"));
            Ok(())
        }

        async fn sync_directory(&self, node: NodeId) -> ClientResult<()> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("fsyncdir:{}", node.0));
            Ok(())
        }

        async fn allocate_file(
            &self,
            _handle: RemoteFileHandle,
            offset: u64,
            length: u64,
        ) -> ClientResult<WriteResult> {
            let end = offset.checked_add(length).unwrap();
            self.bytes
                .lock()
                .unwrap()
                .resize(usize::try_from(end).unwrap(), 0);
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("allocate:{offset}:{length}"));
            let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(WriteResult {
                written: length,
                revision,
                size: end,
            })
        }

        async fn get_lock(
            &self,
            _handle: RemoteFileHandle,
            lock: FileLock,
        ) -> ClientResult<Option<FileLock>> {
            self.operation_log.lock().unwrap().push(format!(
                "getlk:{}:{}:{}:{:?}",
                lock.owner, lock.start, lock.end, lock.kind
            ));
            Ok(Some(lock))
        }

        async fn set_lock(
            &self,
            _handle: RemoteFileHandle,
            lock: FileLock,
            wait: bool,
        ) -> ClientResult<()> {
            self.operation_log.lock().unwrap().push(format!(
                "setlk:{}:{}:{}:{:?}:{wait}",
                lock.owner, lock.start, lock.end, lock.kind
            ));
            Ok(())
        }

        async fn list_xattrs(&self, node: NodeId) -> ClientResult<Vec<Name>> {
            self.wait().await;
            self.xattr_requests.fetch_add(1, Ordering::Relaxed);
            if node == file_node() {
                Ok(vec![Name::from("user.DOSATTRIB")])
            } else {
                Ok(Vec::new())
            }
        }

        async fn forget_nodes(&self, nodes: Vec<NodeId>) -> ClientResult<()> {
            self.operation_log
                .lock()
                .unwrap()
                .push(format!("forget:{}", nodes.len()));
            Ok(())
        }

        async fn close_file(&self, handle: RemoteFileHandle) -> ClientResult<()> {
            self.wait().await;
            if handle != remote_handle() {
                return Err(ClientError::Server(
                    ErrorCode::InvalidHandle,
                    "unknown handle".into(),
                ));
            }
            self.close_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn finder_style_lookup_getattr_and_snapshot_readdir_use_stable_inodes() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();

        let root = adapter.getattr(ROOT_INODE).unwrap();
        assert_eq!(root.node, ROOT_NODE);
        assert_eq!(root.kind, NodeKind::Directory);

        let first = adapter.readdir(ROOT_INODE).unwrap();
        let second = adapter.readdir(ROOT_INODE).unwrap();
        assert_eq!(first, second);
        assert_eq!(remote.xattr_requests.load(Ordering::Relaxed), 0);
        assert_eq!(first.parent_inode, ROOT_INODE);
        assert_eq!(first.entries.len(), 3);
        assert_eq!(
            first
                .entries
                .iter()
                .find(|entry| entry.name == Name::from("hello-link"))
                .unwrap()
                .kind,
            NodeKind::Symlink
        );

        let file_entry = first
            .entries
            .iter()
            .find(|entry| entry.name == Name::from("hello.txt"))
            .unwrap();
        let lookup = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        assert_eq!(lookup.inode, file_entry.inode);
        assert_eq!(lookup.metadata.kind, NodeKind::File);
        let folded_lookup = adapter.lookup(ROOT_INODE, "HeLLo.TxT").unwrap();
        assert_eq!(folded_lookup.inode, lookup.inode);
        assert_eq!(
            adapter.getattr(file_entry.inode).unwrap(),
            file_entry.metadata
        );
        assert_eq!(remote.metadata_requests.load(Ordering::Relaxed), 1);
        assert_eq!(
            adapter
                .block_on(adapter.list_xattrs_async(file_entry.inode))
                .unwrap(),
            [Name::from("user.DOSATTRIB")]
        );
        assert_eq!(
            adapter
                .block_on(adapter.get_xattr_async(file_entry.inode, Name::from("user.DOSATTRIB")))
                .unwrap(),
            b"0x20"
        );
        assert!(matches!(
            adapter.block_on(
                adapter.xattr_size_async(file_entry.inode, Name::from("com.apple.FinderInfo"))
            ),
            Err(AdapterError::Client(ClientError::Server(
                ErrorCode::NoAttribute,
                _
            )))
        ));
        assert_eq!(remote.xattr_requests.load(Ordering::Relaxed), 0);
        assert!(matches!(
            adapter.lookup(ROOT_INODE, ".DS_Store"),
            Err(AdapterError::NotFound)
        ));
        assert_eq!(remote.directory_requests.load(Ordering::Relaxed), 1);

        let handle = adapter.block_on(adapter.opendir_async(ROOT_INODE)).unwrap();
        let (listing, current, parent) = adapter
            .directory_listing_with_metadata(handle, ROOT_INODE)
            .unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(current.node, ROOT_NODE);
        assert_eq!(parent.node, ROOT_NODE);
        assert_eq!(remote.metadata_requests.load(Ordering::Relaxed), 1);
        assert_eq!(remote.directory_requests.load(Ordering::Relaxed), 1);
        adapter.releasedir(handle).unwrap();
        let reopened_lookup = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        assert_eq!(reopened_lookup.inode, file_entry.inode);
        assert_eq!(remote.directory_requests.load(Ordering::Relaxed), 1);
        assert!(
            !remote
                .operation_log
                .lock()
                .unwrap()
                .iter()
                .any(|operation| operation.starts_with("forget:"))
        );
    }

    #[test]
    fn concurrent_cold_directory_callbacks_share_one_enriched_request() {
        let remote = Arc::new(MockFilesystem::delayed(Duration::from_millis(50)));
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let (first, second) = adapter
            .block_on(async {
                let (first, second) = tokio::join!(
                    adapter.readdir_async(ROOT_INODE),
                    adapter.readdir_async(ROOT_INODE)
                );
                Ok((first?, second?))
            })
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(remote.directory_requests.load(Ordering::Relaxed), 1);
        assert_eq!(remote.xattr_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn file_mutation_invalidates_parent_view_that_embeds_old_child_metadata() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote, Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let listing = adapter.readdir(ROOT_INODE).unwrap();
        let file = listing
            .entries
            .iter()
            .find(|entry| entry.name == Name::from("hello.txt"))
            .unwrap();
        let handle = adapter
            .block_on(adapter.open_async(
                file.inode,
                FileOpenOptions {
                    access: FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            ))
            .unwrap();
        assert!(adapter.cached_directory(ROOT_INODE).unwrap().is_some());

        adapter
            .block_on(adapter.write_async(handle, 0, b"updated"))
            .unwrap();

        assert!(adapter.cached_directory(ROOT_INODE).unwrap().is_none());
        adapter.release(handle).unwrap();
    }

    #[test]
    fn arbitrary_range_read_is_split_at_negotiated_limit() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let handle = adapter.open(file.inode).unwrap();

        assert_eq!(adapter.read(handle, 1, 11).unwrap(), b"ello from q");
        assert_eq!(*remote.read_lengths.lock().unwrap(), [4, 4, 3]);
        adapter.release(handle).unwrap();
        // A lock-free read-only handle replies to release immediately and
        // closes the remote descriptor in the background.
        for _ in 0..500 {
            if remote.close_count.load(Ordering::Relaxed) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(remote.close_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn positioned_write_is_split_and_updates_remote_bytes() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let options = FileOpenOptions {
            access: FileAccess::ReadWrite,
            truncate: false,
            append: false,
        };
        let handle = adapter
            .block_on(adapter.open_async(file.inode, options))
            .unwrap();

        assert_eq!(adapter.write(handle, 6, b"media-work").unwrap(), 10);
        assert_eq!(*remote.write_lengths.lock().unwrap(), [3, 3, 3, 1]);
        assert_eq!(adapter.read(handle, 6, 10).unwrap(), b"media-work");
    }

    #[test]
    fn concurrent_writes_cannot_regress_the_local_handle_revision() {
        let remote = Arc::new(MockFilesystem::with_delayed_write_response(
            100,
            Duration::from_millis(30),
        ));
        let adapter = Adapter::new(remote, Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let handle = adapter
            .block_on(adapter.open_async(
                file.inode,
                FileOpenOptions {
                    access: FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            ))
            .unwrap();

        let (first, second) = adapter
            .block_on(async {
                let (first, second) = tokio::join!(
                    adapter.write_async(handle, 100, b"aaa"),
                    adapter.write_async(handle, 103, b"bbb")
                );
                Ok((first?, second?))
            })
            .unwrap();
        assert_eq!((first, second), (3, 3));
        assert_eq!(adapter.file_handle(handle).unwrap().revision, 9);
        assert_eq!(adapter.read(handle, 100, 6).unwrap(), b"aaabbb");
    }

    #[test]
    fn random_reads_on_one_handle_remain_concurrent() {
        let remote = Arc::new(MockFilesystem::with_read_delay(Duration::from_millis(20)));
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let handle = adapter.open(file.inode).unwrap();

        adapter
            .block_on(async {
                let (first, second) = tokio::join!(
                    adapter.read_async(handle, 0, 3),
                    adapter.read_async(handle, 6, 3)
                );
                first?;
                second?;
                Ok(())
            })
            .unwrap();
        assert_eq!(remote.maximum_active_reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn callback_deadline_bounds_remote_work_on_the_shared_runtime() {
        let runtime = Arc::new(Builder::new_multi_thread().enable_all().build().unwrap());
        let remote = Arc::new(MockFilesystem::delayed(Duration::from_millis(50)));
        let adapter = Adapter::with_runtime(remote, Duration::from_millis(5), Arc::clone(&runtime));

        assert!(Arc::ptr_eq(adapter.runtime(), &runtime));
        assert!(matches!(
            adapter.getattr(ROOT_INODE),
            Err(AdapterError::CallbackTimedOut)
        ));
    }

    #[test]
    fn destroy_is_safe_when_invoked_from_a_runtime_worker() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let handle = adapter.open(file.inode).unwrap();
        assert!(handle > 0);

        let mounted = adapter.clone();
        adapter.runtime().block_on(async move {
            mounted.destroy_mount();
            tokio::time::sleep(Duration::from_millis(10)).await;
        });
        assert_eq!(remote.close_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn mutation_durability_lock_and_stat_paths_reach_the_remote_filesystem() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();

        let created_file = adapter
            .block_on(adapter.create_file_async(
                ROOT_INODE,
                "new.mov".into(),
                0o640,
                FileOpenOptions {
                    access: FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            ))
            .unwrap();
        assert_eq!(created_file.metadata.kind, NodeKind::File);
        adapter.release(created_file.handle).unwrap();

        let directory = adapter
            .block_on(adapter.create_directory_async(ROOT_INODE, "media".into(), 0o750))
            .unwrap();
        assert_eq!(directory.metadata.kind, NodeKind::Directory);
        let symlink = adapter
            .block_on(adapter.create_symlink_async(
                ROOT_INODE,
                "current.mov".into(),
                b"hello.txt".to_vec(),
            ))
            .unwrap();
        assert_eq!(
            adapter
                .block_on(adapter.readlink_async(symlink.inode))
                .unwrap(),
            b"hello.txt"
        );

        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let handle = adapter
            .block_on(adapter.open_async(
                file.inode,
                FileOpenOptions {
                    access: FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            ))
            .unwrap();
        let changed = adapter
            .block_on(adapter.setattr_async(
                file.inode,
                Some(handle),
                AttributeChanges {
                    size: Some(24),
                    mode: Some(0o640),
                    accessed_unix_ms: Some(1000),
                    modified_unix_ms: Some(1234),
                    backup_unix_ms: None,
                },
            ))
            .unwrap();
        assert_eq!(changed.size, 24);
        assert_eq!(changed.mode, 0o640);
        assert_eq!(changed.accessed_unix_ms, 1000);
        assert_eq!(changed.modified_unix_ms, 1234);
        adapter
            .block_on(adapter.allocate_async(handle, 24, 8))
            .unwrap();
        adapter
            .block_on(adapter.flush_async(handle, Some(77)))
            .unwrap();
        adapter
            .block_on(adapter.fsync_async(handle, false))
            .unwrap();
        let directory_handle = adapter.block_on(adapter.opendir_async(ROOT_INODE)).unwrap();
        adapter
            .block_on(adapter.fsyncdir_async(ROOT_INODE, directory_handle))
            .unwrap();
        adapter.releasedir(directory_handle).unwrap();

        let lock = FileLock {
            owner: 77,
            start: 4,
            end: 15,
            kind: LockKind::Write,
            pid: 42,
        };
        assert_eq!(
            adapter
                .block_on(adapter.get_lock_async(handle, lock))
                .unwrap(),
            Some(lock)
        );
        adapter
            .block_on(adapter.set_lock_async(handle, lock, true))
            .unwrap();
        let stats = adapter.block_on(adapter.statfs_async()).unwrap();
        assert_eq!(stats.block_size, 4096);

        adapter
            .block_on(adapter.rename_async(
                ROOT_INODE,
                "folder".into(),
                ROOT_INODE,
                "edited".into(),
                RenameMode::Replace,
            ))
            .unwrap();
        adapter
            .block_on(adapter.remove_async(ROOT_INODE, "hello.txt".into(), false))
            .unwrap();
        adapter
            .block_on(adapter.release_async(handle, true, Some(77)))
            .unwrap();

        let log = remote.operation_log.lock().unwrap();
        for expected in [
            "create:new.mov:640:ReadWrite",
            "mkdir:media:750",
            "symlink:current.mov:hello.txt",
            "setattr:Some(24):Some(1234)",
            "allocate:24:8",
            "flush:Some(77)",
            "fsync:false",
            "getlk:77:4:15:Write",
            "setlk:77:4:15:Write:true",
            "rename:folder:edited:Replace",
            "remove:hello.txt:false",
        ] {
            assert!(
                log.iter().any(|operation| operation == expected),
                "{expected}"
            );
        }
        assert!(
            log.iter()
                .any(|operation| operation.starts_with("fsyncdir:"))
        );
        assert_eq!(
            log.iter()
                .filter(|operation| operation.as_str() == "flush:Some(77)")
                .count(),
            2
        );
        assert_eq!(remote.close_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn rejects_oversized_and_overflowing_ranges() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote, Duration::from_secs(1)).unwrap();
        assert!(matches!(
            adapter.read(1, 0, MAX_FUSE_IO_SIZE + 1),
            Err(AdapterError::RequestTooLarge(MAX_FUSE_IO_SIZE))
        ));
        assert!(matches!(
            validate_io_range(u64::MAX, 1),
            Err(AdapterError::InvalidRange)
        ));
        assert!(validate_allocation_range(0, MAX_FUSE_IO_SIZE + 1).is_ok());
        assert!(matches!(
            validate_allocation_range(0, 0),
            Err(AdapterError::InvalidRange)
        ));
        assert!(matches!(
            validate_allocation_range(u64::MAX, 1),
            Err(AdapterError::InvalidRange)
        ));
    }

    #[test]
    fn case_insensitive_lookup_normalizes_unicode_and_rejects_ambiguity() {
        let decomposed = ProtocolDirectoryEntry {
            node: NodeId(Uuid::from_u128(10)),
            name: "Cafe\u{301}.mov".into(),
            kind: NodeKind::File,
            metadata: metadata(NodeId(Uuid::from_u128(10)), NodeKind::File, 0),
        };
        let selected =
            select_remote_entry(vec![decomposed.clone()], &Name::from("CAFÉ.MOV")).unwrap();
        assert_eq!(selected, decomposed);

        let composed = ProtocolDirectoryEntry {
            node: NodeId(Uuid::from_u128(11)),
            name: "Café.mov".into(),
            kind: NodeKind::File,
            metadata: metadata(NodeId(Uuid::from_u128(11)), NodeKind::File, 0),
        };
        let entries = vec![decomposed.clone(), composed.clone()];
        assert_eq!(
            select_remote_entry(entries.clone(), &composed.name).unwrap(),
            composed
        );
        assert!(matches!(
            select_remote_entry(entries.clone(), &Name::from("CAFÉ.MOV")),
            Err(AdapterError::AmbiguousName)
        ));
        assert!(matches!(
            validate_case_insensitive_directory(&entries),
            Err(AdapterError::AmbiguousName)
        ));
    }

    #[test]
    fn non_utf8_names_are_selected_only_by_exact_lossless_bytes() {
        let raw = Name::new(b"clip-\xff.mov".to_vec());
        let entry = ProtocolDirectoryEntry {
            node: NodeId(Uuid::from_u128(12)),
            name: raw.clone(),
            kind: NodeKind::File,
            metadata: metadata(NodeId(Uuid::from_u128(12)), NodeKind::File, 0),
        };
        assert_eq!(
            select_remote_entry(vec![entry.clone()], &raw).unwrap(),
            entry
        );
        assert!(matches!(
            select_remote_entry(vec![entry], &Name::new(b"CLIP-\xff.MOV".to_vec())),
            Err(AdapterError::NotFound)
        ));
    }

    #[test]
    fn forget_and_batch_forget_evict_inodes_and_notify_the_remote_session() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        adapter.probe_capabilities().unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        let directory = adapter.lookup(ROOT_INODE, "folder").unwrap();

        adapter
            .forget_inodes(&[(file.inode, 1), (directory.inode, 1)])
            .unwrap();
        adapter.runtime().block_on(async {
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if remote
                        .operation_log
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|operation| operation == "forget:2")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
        assert!(matches!(
            adapter.getattr(file.inode),
            Err(AdapterError::UnknownInode)
        ));
        assert!(matches!(
            adapter.getattr(directory.inode),
            Err(AdapterError::UnknownInode)
        ));
        assert_ne!(
            adapter.lookup(ROOT_INODE, "hello.txt").unwrap().inode,
            file.inode
        );
    }

    fn metadata(node: NodeId, kind: NodeKind, size: u64) -> Metadata {
        Metadata {
            node,
            kind,
            size,
            mode: match kind {
                NodeKind::Directory => 0o755,
                NodeKind::File => 0o644,
                NodeKind::Symlink => 0o777,
                NodeKind::NamedPipe
                | NodeKind::CharacterDevice
                | NodeKind::BlockDevice
                | NodeKind::Socket => 0o600,
            },
            allocated_blocks: size.div_ceil(512),
            revision: 7,
            accessed_unix_ms: 1_700_000_000_000,
            modified_unix_ms: 1_700_000_000_000,
            created_unix_ms: Some(1_600_000_000_000),
            backup_unix_ms: None,
            link_count: if kind == NodeKind::Directory { 2 } else { 1 },
            device_major: 0,
            device_minor: 0,
        }
    }

    fn file_node() -> NodeId {
        NodeId(Uuid::from_u128(1))
    }

    fn directory_node() -> NodeId {
        NodeId(Uuid::from_u128(2))
    }

    fn symlink_node() -> NodeId {
        NodeId(Uuid::from_u128(3))
    }

    fn remote_handle() -> RemoteFileHandle {
        RemoteFileHandle(Uuid::from_u128(9))
    }

    fn not_found() -> ClientError {
        ClientError::Server(ErrorCode::NotFound, "node not found".into())
    }
}
