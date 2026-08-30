// SPDX-License-Identifier: Apache-2.0
//! An in-memory [`RemoteFilesystem`] for testing the Windows bridge.
//!
//! The bridge's interesting behaviour — path resolution, handle lifetime,
//! read/write coalescing, rename, delete-on-close — is only observable against
//! a filesystem that actually stores things. A mock that returns canned answers
//! would let a bridge that never writes anything pass its tests.
//!
//! So this is a small but real filesystem: nodes with parents and children,
//! byte contents, revisions that advance on mutation, and handles that must be
//! opened before use and are rejected afterwards. It also counts the operations
//! that cost a round trip against a real server, which is how the caching tests
//! assert that Explorer's callback storms do not become network traffic.
//!
//! It is compiled only for tests.

use async_trait::async_trait;
use quickfs_client_core::{
    ClientError, CreatedFile, OpenedFile, RangeRead, RemoteFilesystem, Result as ClientResult,
    WriteResult,
};
use quickfs_fs_adapter::Adapter;
use quickfs_protocol::{
    AttributeChanges, DirectoryEntry, DirectoryView, DirectoryViewEntry, DirectoryViewOptions,
    ErrorCode, FileHandle, FileOpenOptions, FilesystemCapabilities, FilesystemStats, Metadata,
    Name, NodeId, NodeKind, ROOT_NODE, RenameMode,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

/// The callback deadline tests run with. Long enough that a slow machine does
/// not fail a test, short enough that a deadlock is reported rather than hung.
pub const TEST_CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Build an [`Adapter`] over a mock, with its own runtime.
pub fn adapter_for(mock: Arc<MockFilesystem>) -> Adapter {
    #[allow(clippy::expect_used)]
    Adapter::new(mock, TEST_CALLBACK_TIMEOUT).expect("the test runtime must start")
}

#[derive(Clone)]
struct Node {
    kind: NodeKind,
    mode: u32,
    revision: u64,
    modified_unix_ms: u64,
    created_unix_ms: u64,
    data: Vec<u8>,
    /// Ordered so enumeration is deterministic, which the directory marker in
    /// `read_directory` depends on.
    children: BTreeMap<Name, NodeId>,
    parent: NodeId,
}

impl Node {
    fn directory(parent: NodeId, mode: u32) -> Self {
        Self {
            kind: NodeKind::Directory,
            mode,
            revision: 1,
            modified_unix_ms: 1_700_000_000_000,
            created_unix_ms: 1_700_000_000_000,
            data: Vec::new(),
            children: BTreeMap::new(),
            parent,
        }
    }

    fn file(parent: NodeId, mode: u32, data: Vec<u8>) -> Self {
        Self {
            kind: NodeKind::File,
            mode,
            revision: 1,
            modified_unix_ms: 1_700_000_000_000,
            created_unix_ms: 1_700_000_000_000,
            data,
            children: BTreeMap::new(),
            parent,
        }
    }

    fn metadata(&self, node: NodeId) -> Metadata {
        Metadata {
            node,
            kind: self.kind,
            size: self.data.len() as u64,
            mode: self.mode,
            allocated_blocks: self.data.len().div_ceil(512) as u64,
            revision: self.revision,
            accessed_unix_ms: self.modified_unix_ms,
            modified_unix_ms: self.modified_unix_ms,
            created_unix_ms: Some(self.created_unix_ms),
            backup_unix_ms: None,
            link_count: 1,
            device_major: 0,
            device_minor: 0,
        }
    }
}

#[derive(Default)]
struct State {
    nodes: HashMap<NodeId, Node>,
    handles: HashMap<FileHandle, NodeId>,
}

/// Counts of the operations that would cost a network round trip.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Traffic {
    pub directory_listings: usize,
    pub metadata_fetches: usize,
    pub opens: usize,
    pub reads: usize,
    pub writes: usize,
}

/// A small, real, in-memory filesystem.
pub struct MockFilesystem {
    state: Mutex<State>,
    capabilities: FilesystemCapabilities,
    directory_listings: AtomicUsize,
    metadata_fetches: AtomicUsize,
    opens: AtomicUsize,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl MockFilesystem {
    /// An empty writable export.
    pub fn empty() -> Arc<Self> {
        let mut nodes = HashMap::new();
        nodes.insert(ROOT_NODE, Node::directory(ROOT_NODE, 0o755));
        Arc::new(Self {
            state: Mutex::new(State {
                nodes,
                handles: HashMap::new(),
            }),
            capabilities: writable_capabilities(),
            directory_listings: AtomicUsize::new(0),
            metadata_fetches: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        })
    }

    /// A read-only export, for the tests that assert a mount refuses mutation
    /// before touching the network.
    pub fn read_only() -> Arc<Self> {
        let filesystem = Self::empty();
        // `Arc::get_mut` is available because nothing has cloned it yet.
        let mut filesystem = filesystem;
        #[allow(clippy::expect_used)]
        let inner = Arc::get_mut(&mut filesystem).expect("sole owner");
        inner.capabilities.writable = false;
        filesystem
    }

    /// `\docs\notes.txt`, `\docs\images\`, `\readme.md`.
    pub fn tree() -> Arc<Self> {
        let filesystem = Self::empty();
        let docs = filesystem.insert_directory(ROOT_NODE, "docs", 0o755);
        filesystem.insert_file(docs, "notes.txt", 0o644, b"hello world".to_vec());
        filesystem.insert_directory(docs, "images", 0o755);
        filesystem.insert_file(ROOT_NODE, "readme.md", 0o644, b"# readme".to_vec());
        filesystem
    }

    /// `count` sibling directories, each holding one `leaf` file.
    pub fn wide(count: usize) -> Arc<Self> {
        let filesystem = Self::empty();
        for index in 0..count {
            let directory = filesystem.insert_directory(ROOT_NODE, &format!("dir{index}"), 0o755);
            filesystem.insert_file(directory, "leaf", 0o644, vec![b'x'; index]);
        }
        filesystem
    }

    pub fn insert_directory(&self, parent: NodeId, name: &str, mode: u32) -> NodeId {
        self.insert(parent, name, Node::directory(parent, mode))
    }

    pub fn insert_file(&self, parent: NodeId, name: &str, mode: u32, data: Vec<u8>) -> NodeId {
        self.insert(parent, name, Node::file(parent, mode, data))
    }

    fn insert(&self, parent: NodeId, name: &str, node: Node) -> NodeId {
        let id = NodeId(Uuid::new_v4());
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("test state must not be poisoned");
        state.nodes.insert(id, node);
        #[allow(clippy::expect_used)]
        state
            .nodes
            .get_mut(&parent)
            .expect("parent must exist")
            .children
            .insert(Name::from(name.to_owned()), id);
        id
    }

    /// The bytes stored for a node, for asserting what actually reached the
    /// server rather than what the client believes it wrote.
    pub fn contents(&self, node: NodeId) -> Option<Vec<u8>> {
        #[allow(clippy::expect_used)]
        let state = self.state.lock().expect("test state must not be poisoned");
        state.nodes.get(&node).map(|node| node.data.clone())
    }

    /// Look a path up directly, bypassing the client, so a test can check what
    /// the server ended up holding.
    pub fn lookup_path(&self, components: &[&str]) -> Option<NodeId> {
        #[allow(clippy::expect_used)]
        let state = self.state.lock().expect("test state must not be poisoned");
        let mut current = ROOT_NODE;
        for component in components {
            let name = Name::from((*component).to_owned());
            current = *state.nodes.get(&current)?.children.get(&name)?;
        }
        Some(current)
    }

    /// How many file handles the client has open but not closed. A leak here is
    /// a leak of a server-side descriptor.
    pub fn open_handles(&self) -> usize {
        #[allow(clippy::expect_used)]
        let state = self.state.lock().expect("test state must not be poisoned");
        state.handles.len()
    }

    pub fn directory_listings(&self) -> usize {
        self.directory_listings.load(Ordering::Relaxed)
    }

    pub fn traffic(&self) -> Traffic {
        Traffic {
            directory_listings: self.directory_listings.load(Ordering::Relaxed),
            metadata_fetches: self.metadata_fetches.load(Ordering::Relaxed),
            opens: self.opens.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
        }
    }

    fn locked(&self) -> ClientResult<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| server(ErrorCode::Internal, "test state poisoned"))
    }
}

/// The capability set the tests mount against, exposed so the mount-parameter
/// tests can vary one field at a time.
pub fn writable_capabilities_for_tests() -> FilesystemCapabilities {
    writable_capabilities()
}

fn writable_capabilities() -> FilesystemCapabilities {
    FilesystemCapabilities {
        server_epoch: Uuid::from_u128(7),
        writable: true,
        supports_locks: false,
        supports_atomic_rename: true,
        supports_directory_sync: true,
        supports_preallocation: true,
        supports_symlinks: false,
        supports_xattrs: false,
        supports_hard_links: false,
        supports_special_nodes: false,
        supports_copy_file_range: false,
        supports_seek_data_hole: false,
        supports_safe_ioctl: false,
        supports_poll: false,
        supports_bmap: false,
        supports_exchange_data: false,
        supports_volume_rename: false,
        supports_backup_time: false,
        supports_readdirplus: true,
        persistent_node_ids: true,
        restart_lock_replay: false,
        volume_name: "quickfs-test".into(),
        max_read_size: 1024 * 1024,
        max_write_size: 1024 * 1024,
    }
}

fn server(code: ErrorCode, message: &str) -> ClientError {
    ClientError::Server(code, message.into())
}

#[async_trait]
impl RemoteFilesystem for MockFilesystem {
    async fn ping(&self, nonce: u64) -> ClientResult<u64> {
        Ok(nonce)
    }

    async fn capabilities(&self) -> ClientResult<FilesystemCapabilities> {
        Ok(self.capabilities.clone())
    }

    async fn stat_filesystem(&self) -> ClientResult<FilesystemStats> {
        Ok(FilesystemStats {
            blocks: 1024 * 1024,
            blocks_free: 512 * 1024,
            blocks_available: 512 * 1024,
            files: 1_000_000,
            files_free: 999_000,
            block_size: 4096,
            name_length: 255,
            fragment_size: 4096,
        })
    }

    async fn get_metadata(&self, node: NodeId) -> ClientResult<Metadata> {
        self.metadata_fetches.fetch_add(1, Ordering::Relaxed);
        let state = self.locked()?;
        state
            .nodes
            .get(&node)
            .map(|entry| entry.metadata(node))
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))
    }

    async fn list_directory(&self, node: NodeId) -> ClientResult<Vec<DirectoryEntry>> {
        self.directory_listings.fetch_add(1, Ordering::Relaxed);
        let state = self.locked()?;
        let directory = state
            .nodes
            .get(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        if directory.kind != NodeKind::Directory {
            return Err(server(ErrorCode::NotDirectory, "not a directory"));
        }
        let mut entries = Vec::with_capacity(directory.children.len());
        for (name, child) in &directory.children {
            let node = state
                .nodes
                .get(child)
                .ok_or_else(|| server(ErrorCode::Internal, "dangling child"))?;
            entries.push(DirectoryEntry {
                node: *child,
                name: name.clone(),
                kind: node.kind,
                metadata: node.metadata(*child),
            });
        }
        Ok(entries)
    }

    async fn list_directory_view(
        &self,
        node: NodeId,
        _options: DirectoryViewOptions,
    ) -> ClientResult<DirectoryView> {
        let entries = self.list_directory(node).await?;
        let state = self.locked()?;
        let directory = state
            .nodes
            .get(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        let parent_id = directory.parent;
        let parent = state
            .nodes
            .get(&parent_id)
            .ok_or_else(|| server(ErrorCode::Internal, "dangling parent"))?;
        Ok(DirectoryView {
            revision: directory.revision,
            directory: directory.metadata(node),
            parent: parent.metadata(parent_id),
            xattrs: None,
            entries: entries
                .into_iter()
                .map(|entry| DirectoryViewEntry {
                    entry,
                    xattrs: None,
                })
                .collect(),
        })
    }

    async fn open_file(&self, node: NodeId) -> ClientResult<(FileHandle, u64, u64)> {
        let opened = self
            .open_file_with_options(node, FileOpenOptions::READ_ONLY)
            .await?;
        Ok((opened.handle, opened.revision, opened.size))
    }

    async fn open_file_with_options(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> ClientResult<OpenedFile> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        let mut state = self.locked()?;
        let entry = state
            .nodes
            .get_mut(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        if entry.kind == NodeKind::Directory {
            return Err(server(ErrorCode::IsDirectory, "is a directory"));
        }
        if options.truncate {
            entry.data.clear();
            entry.revision += 1;
        }
        let (revision, size) = (entry.revision, entry.data.len() as u64);
        let handle = FileHandle(Uuid::new_v4());
        state.handles.insert(handle, node);
        Ok(OpenedFile {
            handle,
            revision,
            size,
        })
    }

    async fn create_file(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> ClientResult<CreatedFile> {
        let node = {
            let mut state = self.locked()?;
            let directory = state
                .nodes
                .get(&parent)
                .ok_or_else(|| server(ErrorCode::NotFound, "no such parent"))?;
            if directory.kind != NodeKind::Directory {
                return Err(server(ErrorCode::NotDirectory, "not a directory"));
            }
            if directory.children.contains_key(&name) {
                return Err(server(ErrorCode::AlreadyExists, "already exists"));
            }
            let node = NodeId(Uuid::new_v4());
            state
                .nodes
                .insert(node, Node::file(parent, mode, Vec::new()));
            state
                .nodes
                .get_mut(&parent)
                .ok_or_else(|| server(ErrorCode::Internal, "parent vanished"))?
                .children
                .insert(name, node);
            node
        };
        let opened = self.open_file_with_options(node, options).await?;
        let state = self.locked()?;
        let metadata = state
            .nodes
            .get(&node)
            .ok_or_else(|| server(ErrorCode::Internal, "node vanished"))?
            .metadata(node);
        Ok(CreatedFile { metadata, opened })
    }

    async fn create_directory(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
    ) -> ClientResult<Metadata> {
        let mut state = self.locked()?;
        let directory = state
            .nodes
            .get(&parent)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such parent"))?;
        if directory.kind != NodeKind::Directory {
            return Err(server(ErrorCode::NotDirectory, "not a directory"));
        }
        if directory.children.contains_key(&name) {
            return Err(server(ErrorCode::AlreadyExists, "already exists"));
        }
        let node = NodeId(Uuid::new_v4());
        state.nodes.insert(node, Node::directory(parent, mode));
        state
            .nodes
            .get_mut(&parent)
            .ok_or_else(|| server(ErrorCode::Internal, "parent vanished"))?
            .children
            .insert(name, node);
        let metadata = state
            .nodes
            .get(&node)
            .ok_or_else(|| server(ErrorCode::Internal, "node vanished"))?
            .metadata(node);
        Ok(metadata)
    }

    async fn remove_node(&self, parent: NodeId, name: Name, directory: bool) -> ClientResult<()> {
        let mut state = self.locked()?;
        let node = *state
            .nodes
            .get(&parent)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such parent"))?
            .children
            .get(&name)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such entry"))?;
        let target = state
            .nodes
            .get(&node)
            .ok_or_else(|| server(ErrorCode::Internal, "dangling child"))?;
        let is_directory = target.kind == NodeKind::Directory;
        if directory != is_directory {
            return Err(if is_directory {
                server(ErrorCode::IsDirectory, "is a directory")
            } else {
                server(ErrorCode::NotDirectory, "not a directory")
            });
        }
        if is_directory && !target.children.is_empty() {
            return Err(server(ErrorCode::NotEmpty, "directory not empty"));
        }
        state
            .nodes
            .get_mut(&parent)
            .ok_or_else(|| server(ErrorCode::Internal, "parent vanished"))?
            .children
            .remove(&name);
        state.nodes.remove(&node);
        Ok(())
    }

    async fn rename_node(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    ) -> ClientResult<()> {
        let mut state = self.locked()?;
        let node = *state
            .nodes
            .get(&parent)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such parent"))?
            .children
            .get(&name)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such entry"))?;
        let existing = state
            .nodes
            .get(&new_parent)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such destination"))?
            .children
            .get(&new_name)
            .copied();
        match (mode, existing) {
            (RenameMode::NoReplace, Some(_)) => {
                return Err(server(ErrorCode::AlreadyExists, "destination exists"));
            }
            (RenameMode::Exchange, None) => {
                return Err(server(ErrorCode::NotFound, "destination does not exist"));
            }
            (RenameMode::Replace, Some(replaced)) => {
                let target = state
                    .nodes
                    .get(&replaced)
                    .ok_or_else(|| server(ErrorCode::Internal, "dangling destination"))?;
                if target.kind == NodeKind::Directory && !target.children.is_empty() {
                    return Err(server(ErrorCode::NotEmpty, "destination not empty"));
                }
                state.nodes.remove(&replaced);
            }
            _ => {}
        }
        if mode == RenameMode::Exchange {
            let other = existing.ok_or_else(|| server(ErrorCode::Internal, "exchange target"))?;
            state
                .nodes
                .get_mut(&parent)
                .ok_or_else(|| server(ErrorCode::Internal, "parent vanished"))?
                .children
                .insert(name, other);
            state
                .nodes
                .get_mut(&new_parent)
                .ok_or_else(|| server(ErrorCode::Internal, "destination vanished"))?
                .children
                .insert(new_name, node);
            return Ok(());
        }
        state
            .nodes
            .get_mut(&parent)
            .ok_or_else(|| server(ErrorCode::Internal, "parent vanished"))?
            .children
            .remove(&name);
        state
            .nodes
            .get_mut(&new_parent)
            .ok_or_else(|| server(ErrorCode::Internal, "destination vanished"))?
            .children
            .insert(new_name, node);
        state
            .nodes
            .get_mut(&node)
            .ok_or_else(|| server(ErrorCode::Internal, "node vanished"))?
            .parent = new_parent;
        Ok(())
    }

    async fn set_attributes(
        &self,
        node: NodeId,
        _handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> ClientResult<Metadata> {
        let mut state = self.locked()?;
        let entry = state
            .nodes
            .get_mut(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        if let Some(size) = changes.size {
            let size = usize::try_from(size)
                .map_err(|_| server(ErrorCode::TooLarge, "size does not fit in memory"))?;
            entry.data.resize(size, 0);
            entry.revision += 1;
        }
        if let Some(mode) = changes.mode {
            entry.mode = mode & 0o7777;
            entry.revision += 1;
        }
        if let Some(modified) = changes.modified_unix_ms {
            entry.modified_unix_ms = modified;
            entry.revision += 1;
        }
        Ok(entry.metadata(node))
    }

    async fn read_range(
        &self,
        handle: FileHandle,
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
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> ClientResult<RangeRead> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let state = self.locked()?;
        let node = *state
            .handles
            .get(&handle)
            .ok_or_else(|| server(ErrorCode::InvalidHandle, "unknown handle"))?;
        let entry = state
            .nodes
            .get(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(entry.data.len());
        let end = usize::try_from(offset.saturating_add(length))
            .unwrap_or(usize::MAX)
            .min(entry.data.len());
        Ok(RangeRead {
            revision: entry.revision,
            data: entry.data[start..end].to_vec(),
        })
    }

    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> ClientResult<WriteResult> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        let mut state = self.locked()?;
        let node = *state
            .handles
            .get(&handle)
            .ok_or_else(|| server(ErrorCode::InvalidHandle, "unknown handle"))?;
        let entry = state
            .nodes
            .get_mut(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        let start = usize::try_from(offset)
            .map_err(|_| server(ErrorCode::TooLarge, "offset does not fit in memory"))?;
        let end = start
            .checked_add(data.len())
            .ok_or_else(|| server(ErrorCode::TooLarge, "write overflows"))?;
        if entry.data.len() < end {
            entry.data.resize(end, 0);
        }
        entry.data[start..end].copy_from_slice(data);
        entry.revision += 1;
        Ok(WriteResult {
            written: data.len() as u64,
            revision: entry.revision,
            size: entry.data.len() as u64,
        })
    }

    async fn flush_file(&self, handle: FileHandle, _lock_owner: Option<u64>) -> ClientResult<()> {
        let state = self.locked()?;
        state
            .handles
            .get(&handle)
            .map(|_| ())
            .ok_or_else(|| server(ErrorCode::InvalidHandle, "unknown handle"))
    }

    async fn sync_file(&self, handle: FileHandle, _data_only: bool) -> ClientResult<()> {
        self.flush_file(handle, None).await
    }

    async fn sync_directory(&self, _node: NodeId) -> ClientResult<()> {
        Ok(())
    }

    async fn allocate_file(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> ClientResult<WriteResult> {
        let mut state = self.locked()?;
        let node = *state
            .handles
            .get(&handle)
            .ok_or_else(|| server(ErrorCode::InvalidHandle, "unknown handle"))?;
        let entry = state
            .nodes
            .get_mut(&node)
            .ok_or_else(|| server(ErrorCode::NotFound, "no such node"))?;
        let end = usize::try_from(offset.saturating_add(length))
            .map_err(|_| server(ErrorCode::TooLarge, "allocation does not fit in memory"))?;
        if entry.data.len() < end {
            entry.data.resize(end, 0);
        }
        entry.revision += 1;
        Ok(WriteResult {
            written: length,
            revision: entry.revision,
            size: entry.data.len() as u64,
        })
    }

    async fn close_file(&self, handle: FileHandle) -> ClientResult<()> {
        let mut state = self.locked()?;
        state
            .handles
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(|| server(ErrorCode::InvalidHandle, "unknown handle"))
    }
}
