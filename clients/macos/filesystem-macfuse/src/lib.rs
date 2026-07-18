// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
//! Read-only bridge from macFUSE's synchronous callbacks to the authenticated
//! asynchronous [`RemoteFilesystem`] protocol.

use quickfs_client_core::{ClientError, MAX_CLIENT_READ_SIZE, RemoteFilesystem};
use quickfs_protocol::{
    DirectoryEntry as RemoteDirectoryEntry, FileHandle as RemoteFileHandle, Metadata, NodeId,
    NodeKind, ROOT_NODE,
};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::runtime::{Builder, Runtime};

pub const ROOT_INODE: u64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct LookupResult {
    pub inode: u64,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryEntry {
    pub inode: u64,
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryListing {
    pub parent_inode: u64,
    pub entries: Vec<DirectoryEntry>,
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
    #[error("directory entry was not found")]
    NotFound,
    #[error("invalid lookup name")]
    InvalidName,
    #[error("server returned an invalid directory entry name")]
    InvalidRemoteName,
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
    #[error("FUSE read exceeds the client safety limit of {0} bytes")]
    ReadTooLarge(u64),
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
}

/// A single authenticated mount session. The same Tokio runtime and remote
/// connection are retained for every callback until unmount.
pub struct Adapter {
    remote: Arc<dyn RemoteFilesystem>,
    callback_timeout: Duration,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, RemoteFileHandle>>,
    next_inode: AtomicU64,
    next_handle: AtomicU64,
    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    owner_uid: u32,
    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    owner_gid: u32,
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
            remote,
            callback_timeout,
            inodes: Mutex::new(InodeTable {
                by_inode: HashMap::from([(ROOT_INODE, root)]),
                by_node: HashMap::from([(ROOT_NODE, ROOT_INODE)]),
            }),
            handles: Mutex::new(HashMap::new()),
            next_inode: AtomicU64::new(ROOT_INODE + 1),
            next_handle: AtomicU64::new(1),
            #[cfg(all(target_os = "macos", feature = "macfuse"))]
            owner_uid: rustix::process::geteuid().as_raw(),
            #[cfg(all(target_os = "macos", feature = "macfuse"))]
            owner_gid: rustix::process::getegid().as_raw(),
            runtime,
        }
    }

    pub fn callback_timeout(&self) -> Duration {
        self.callback_timeout
    }

    pub fn remote(&self) -> &Arc<dyn RemoteFilesystem> {
        &self.remote
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    /// Resolve a child name beneath an inode. macFUSE issues this operation
    /// before most child `getattr` and `open` calls.
    pub fn lookup(&self, parent_inode: u64, name: &str) -> Result<LookupResult, AdapterError> {
        validate_name(name).map_err(|()| AdapterError::InvalidName)?;
        let parent = self.inode_record(parent_inode)?;
        self.execute(async {
            let entries = self.remote.list_directory(parent.node).await?;
            let entry = entries
                .into_iter()
                .find(|entry| entry.name == name)
                .ok_or(AdapterError::NotFound)?;
            validate_remote_name(&entry)?;
            let inode = self.remember_inode(entry.node, parent_inode)?;
            let metadata = self.remote.get_metadata(entry.node).await?;
            validate_metadata(entry.node, &metadata)?;
            Ok(LookupResult { inode, metadata })
        })
    }

    pub fn getattr(&self, inode: u64) -> Result<Metadata, AdapterError> {
        let record = self.inode_record(inode)?;
        self.execute(async {
            let metadata = self.remote.get_metadata(record.node).await?;
            validate_metadata(record.node, &metadata)?;
            Ok(metadata)
        })
    }

    pub fn readdir(&self, inode: u64) -> Result<DirectoryListing, AdapterError> {
        let record = self.inode_record(inode)?;
        self.execute(async {
            let remote_entries = self.remote.list_directory(record.node).await?;
            let mut entries = Vec::with_capacity(remote_entries.len());
            for entry in remote_entries {
                validate_remote_name(&entry)?;
                // The v3 server resolves safe in-export symlink targets to an
                // opaque target node, but does not expose a readlink operation.
                // Match the existing CLI semantics by presenting that target's
                // actual type to Finder instead of advertising an unusable link.
                let kind = if entry.kind == NodeKind::Symlink {
                    let metadata = self.remote.get_metadata(entry.node).await?;
                    validate_metadata(entry.node, &metadata)?;
                    metadata.kind
                } else {
                    entry.kind
                };
                entries.push(DirectoryEntry {
                    inode: self.remember_inode(entry.node, inode)?,
                    name: entry.name,
                    kind,
                });
            }
            Ok(DirectoryListing {
                parent_inode: record.parent_inode,
                entries,
            })
        })
    }

    pub fn open(&self, inode: u64) -> Result<u64, AdapterError> {
        let record = self.inode_record(inode)?;
        self.execute(async {
            let (remote_handle, _, _) = self.remote.open_file(record.node).await?;
            match self.remember_handle(remote_handle) {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    let _ = self.remote.close_file(remote_handle).await;
                    Err(error)
                }
            }
        })
    }

    pub fn read(&self, handle: u64, offset: u64, length: u64) -> Result<Vec<u8>, AdapterError> {
        if length > MAX_CLIENT_READ_SIZE || offset.checked_add(length).is_none() {
            return Err(AdapterError::ReadTooLarge(MAX_CLIENT_READ_SIZE));
        }
        let remote_handle = self.remote_handle(handle)?;
        self.execute(async {
            let data = self
                .remote
                .read_range(remote_handle, offset, length)
                .await?;
            if u64::try_from(data.len()).unwrap_or(u64::MAX) > length {
                return Err(AdapterError::UnexpectedReadLength);
            }
            Ok(data)
        })
    }

    pub fn release(&self, handle: u64) -> Result<(), AdapterError> {
        let remote_handle = self.take_remote_handle(handle)?;
        self.execute(async {
            self.remote.close_file(remote_handle).await?;
            Ok(())
        })
    }

    fn execute<T, F>(&self, future: F) -> Result<T, AdapterError>
    where
        F: Future<Output = Result<T, AdapterError>>,
    {
        self.runtime.block_on(async {
            tokio::time::timeout(self.callback_timeout, future)
                .await
                .map_err(|_| AdapterError::CallbackTimedOut)?
        })
    }

    fn inode_record(&self, inode: u64) -> Result<InodeRecord, AdapterError> {
        self.inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .by_inode
            .get(&inode)
            .copied()
            .ok_or(AdapterError::UnknownInode)
    }

    fn remember_inode(&self, node: NodeId, parent_inode: u64) -> Result<u64, AdapterError> {
        let mut table = self
            .inodes
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        if let Some(inode) = table.by_node.get(&node) {
            return Ok(*inode);
        }
        let inode = self
            .next_inode
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AdapterError::InodeSpaceExhausted)?;
        table
            .by_inode
            .insert(inode, InodeRecord { node, parent_inode });
        table.by_node.insert(node, inode);
        Ok(inode)
    }

    fn remember_handle(&self, remote: RemoteFileHandle) -> Result<u64, AdapterError> {
        let mut handles = self
            .handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?;
        let handle = self
            .next_handle
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AdapterError::HandleSpaceExhausted)?;
        handles.insert(handle, remote);
        Ok(handle)
    }

    fn remote_handle(&self, handle: u64) -> Result<RemoteFileHandle, AdapterError> {
        self.handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .get(&handle)
            .copied()
            .ok_or(AdapterError::UnknownHandle)
    }

    fn take_remote_handle(&self, handle: u64) -> Result<RemoteFileHandle, AdapterError> {
        self.handles
            .lock()
            .map_err(|_| AdapterError::StateUnavailable)?
            .remove(&handle)
            .ok_or(AdapterError::UnknownHandle)
    }
}

fn validate_name(name: &str) -> Result<(), ()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        Err(())
    } else {
        Ok(())
    }
}

fn validate_remote_name(entry: &RemoteDirectoryEntry) -> Result<(), AdapterError> {
    validate_name(&entry.name).map_err(|()| AdapterError::InvalidRemoteName)
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
pub use native::{MountConfig, mount};

#[cfg(all(target_os = "macos", feature = "macfuse"))]
pub const NATIVE_CALLBACKS_IMPLEMENTED: bool = true;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use quickfs_client_core::Result as ClientResult;
    use quickfs_protocol::{DirectoryEntry as ProtocolDirectoryEntry, ErrorCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    const FILE_BYTES: &[u8] = b"hello from quicKFS";

    struct MockFilesystem {
        delay: Duration,
        close_count: AtomicUsize,
    }

    impl MockFilesystem {
        fn immediate() -> Self {
            Self {
                delay: Duration::ZERO,
                close_count: AtomicUsize::new(0),
            }
        }

        fn delayed(delay: Duration) -> Self {
            Self {
                delay,
                close_count: AtomicUsize::new(0),
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

        async fn get_metadata(&self, node: NodeId) -> ClientResult<Metadata> {
            self.wait().await;
            match node.0.as_u128() {
                0 => Ok(metadata(ROOT_NODE, NodeKind::Directory, 0)),
                1 => Ok(metadata(
                    file_node(),
                    NodeKind::File,
                    u64::try_from(FILE_BYTES.len()).unwrap(),
                )),
                2 => Ok(metadata(directory_node(), NodeKind::Directory, 0)),
                _ => Err(not_found()),
            }
        }

        async fn list_directory(&self, node: NodeId) -> ClientResult<Vec<ProtocolDirectoryEntry>> {
            self.wait().await;
            if node == ROOT_NODE {
                Ok(vec![
                    ProtocolDirectoryEntry {
                        node: file_node(),
                        name: "hello.txt".into(),
                        kind: NodeKind::File,
                    },
                    ProtocolDirectoryEntry {
                        node: directory_node(),
                        name: "folder".into(),
                        kind: NodeKind::Directory,
                    },
                    ProtocolDirectoryEntry {
                        node: directory_node(),
                        name: "folder-link".into(),
                        kind: NodeKind::Symlink,
                    },
                ])
            } else if node == directory_node() {
                Ok(Vec::new())
            } else {
                Err(not_found())
            }
        }

        async fn open_file(&self, node: NodeId) -> ClientResult<(RemoteFileHandle, u64, u64)> {
            self.wait().await;
            if node != file_node() {
                return Err(ClientError::Server(
                    ErrorCode::InvalidRequest,
                    "not a regular file".into(),
                ));
            }
            Ok((remote_handle(), 7, u64::try_from(FILE_BYTES.len()).unwrap()))
        }

        async fn read_range(
            &self,
            handle: RemoteFileHandle,
            offset: u64,
            length: u64,
        ) -> ClientResult<Vec<u8>> {
            self.wait().await;
            if handle != remote_handle() {
                return Err(ClientError::Server(
                    ErrorCode::InvalidHandle,
                    "unknown handle".into(),
                ));
            }
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= FILE_BYTES.len() {
                return Ok(Vec::new());
            }
            let requested_end = offset.saturating_add(length);
            let end = usize::try_from(requested_end)
                .unwrap_or(usize::MAX)
                .min(FILE_BYTES.len());
            Ok(FILE_BYTES[start..end].to_vec())
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
    fn finder_style_lookup_getattr_and_readdir_use_stable_inodes() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote, Duration::from_secs(1)).unwrap();

        let root = adapter.getattr(ROOT_INODE).unwrap();
        assert_eq!(root.node, ROOT_NODE);
        assert_eq!(root.kind, NodeKind::Directory);

        let first = adapter.readdir(ROOT_INODE).unwrap();
        let second = adapter.readdir(ROOT_INODE).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.parent_inode, ROOT_INODE);
        assert_eq!(first.entries.len(), 3);
        assert_eq!(
            first
                .entries
                .iter()
                .find(|entry| entry.name == "folder-link")
                .unwrap()
                .kind,
            NodeKind::Directory
        );

        let file_entry = first
            .entries
            .iter()
            .find(|entry| entry.name == "hello.txt")
            .unwrap();
        let lookup = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();
        assert_eq!(lookup.inode, file_entry.inode);
        assert_eq!(lookup.metadata.kind, NodeKind::File);
        assert_eq!(adapter.getattr(lookup.inode).unwrap(), lookup.metadata);

        assert!(matches!(
            adapter.lookup(ROOT_INODE, "missing"),
            Err(AdapterError::NotFound)
        ));
        assert!(matches!(
            adapter.lookup(ROOT_INODE, "../escape"),
            Err(AdapterError::InvalidName)
        ));
    }

    #[test]
    fn open_read_and_release_translate_local_and_remote_handles() {
        let remote = Arc::new(MockFilesystem::immediate());
        let adapter = Adapter::new(remote.clone(), Duration::from_secs(1)).unwrap();
        let file = adapter.lookup(ROOT_INODE, "hello.txt").unwrap();

        let handle = adapter.open(file.inode).unwrap();
        assert_eq!(adapter.read(handle, 1, 4).unwrap(), b"ello");
        assert!(adapter.read(handle, u64::MAX, 1).is_err());
        assert!(matches!(
            adapter.read(handle, 0, MAX_CLIENT_READ_SIZE + 1),
            Err(AdapterError::ReadTooLarge(MAX_CLIENT_READ_SIZE))
        ));

        adapter.release(handle).unwrap();
        assert_eq!(remote.close_count.load(Ordering::Relaxed), 1);
        assert!(matches!(
            adapter.read(handle, 0, 1),
            Err(AdapterError::UnknownHandle)
        ));
        assert!(matches!(
            adapter.release(handle),
            Err(AdapterError::UnknownHandle)
        ));
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

    fn metadata(node: NodeId, kind: NodeKind, size: u64) -> Metadata {
        Metadata {
            node,
            kind,
            size,
            revision: 7,
            modified_unix_ms: 1_700_000_000_000,
        }
    }

    fn file_node() -> NodeId {
        NodeId(Uuid::from_u128(1))
    }

    fn directory_node() -> NodeId {
        NodeId(Uuid::from_u128(2))
    }

    fn remote_handle() -> RemoteFileHandle {
        RemoteFileHandle(Uuid::from_u128(9))
    }

    fn not_found() -> ClientError {
        ClientError::Server(ErrorCode::NotFound, "node not found".into())
    }
}
