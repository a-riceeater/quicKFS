// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use crate::{
    ClientError, CreatedFile, DirectorySnapshot, OpenedFile, RangeRead, RemoteFilesystem, Result,
    WriteResult, XattrRead,
};
use async_trait::async_trait;
use futures::future::join_all;
use quickfs_cache::{
    DirectoryCache, FilesystemStateCache, MemoryCache, MetadataCache, NodeCacheInvalidation,
    RangeCache, RangeKey, RevisionKey,
};
use quickfs_protocol::{
    AttributeChanges, DirectoryEntry, DirectoryView, DirectoryViewEntry, DirectoryViewOptions,
    ErrorCode, FileAccess, FileHandle, FileLock, FileOpenOptions, FilesystemCapabilities,
    FilesystemStats, Metadata, Name, NodeId, ROOT_NODE, RenameMode, SafeIoctl, SeekWhence,
    SpecialNodeKind, XattrSetMode,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{Mutex, OnceCell, RwLock};
use uuid::Uuid;

const SMALL_READ_AHEAD_BLOCK_SIZE: u64 = 1024 * 1024;
const SMALL_READ_THRESHOLD: u64 = 1024 * 1024;

pub trait FilesystemCache:
    MetadataCache + DirectoryCache + RangeCache + FilesystemStateCache + NodeCacheInvalidation
{
}
impl<T> FilesystemCache for T where
    T: MetadataCache + DirectoryCache + RangeCache + FilesystemStateCache + NodeCacheInvalidation
{
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachePolicy {
    /// Remote reads are aligned to blocks so nearby and overlapping random
    /// reads can be served without another network round trip.
    pub block_size: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            block_size: crate::MAX_CLIENT_READ_SIZE,
        }
    }
}

#[derive(Clone)]
struct CachedHandle {
    node: NodeId,
    inner: Option<FileHandle>,
    revision: u64,
    size: u64,
    mutation: Arc<Mutex<()>>,
}

struct RangeFetch {
    result: OnceCell<std::result::Result<Arc<Vec<u8>>, SharedFetchError>>,
}

#[derive(Clone)]
enum SharedFetchError {
    Server(ErrorCode, String),
    UnexpectedResponse,
    ReadTooLarge(u64),
    WriteTooLarge(u64),
    StaleRevision,
    Offline,
    OfflineCacheMiss,
    AmbiguousMutation,
}

impl From<ClientError> for SharedFetchError {
    fn from(error: ClientError) -> Self {
        match error {
            // A transport failure has already passed through the resilient
            // reconnect layer. Followers of the same fetch must observe the
            // same offline result instead of starting another reconnect storm.
            ClientError::Transport(_) | ClientError::Offline => Self::Offline,
            ClientError::Server(code, message) => Self::Server(code, message),
            ClientError::UnexpectedResponse => Self::UnexpectedResponse,
            ClientError::ReadTooLarge(limit) => Self::ReadTooLarge(limit),
            ClientError::WriteTooLarge(limit) => Self::WriteTooLarge(limit),
            ClientError::StaleRevision => Self::StaleRevision,
            ClientError::OfflineCacheMiss => Self::OfflineCacheMiss,
            ClientError::AmbiguousMutation => Self::AmbiguousMutation,
        }
    }
}

impl From<SharedFetchError> for ClientError {
    fn from(error: SharedFetchError) -> Self {
        match error {
            SharedFetchError::Server(code, message) => Self::Server(code, message),
            SharedFetchError::UnexpectedResponse => Self::UnexpectedResponse,
            SharedFetchError::ReadTooLarge(limit) => Self::ReadTooLarge(limit),
            SharedFetchError::WriteTooLarge(limit) => Self::WriteTooLarge(limit),
            SharedFetchError::StaleRevision => Self::StaleRevision,
            SharedFetchError::Offline => Self::Offline,
            SharedFetchError::OfflineCacheMiss => Self::OfflineCacheMiss,
            SharedFetchError::AmbiguousMutation => Self::AmbiguousMutation,
        }
    }
}

/// Adds a revision-keyed read-through/offline cache without weakening remote
/// write, fsync, or lock semantics. Offline access is intentionally read-only.
pub struct CachedFilesystem {
    inner: Arc<dyn RemoteFilesystem>,
    cache: Arc<dyn FilesystemCache>,
    policy: CachePolicy,
    handles: RwLock<HashMap<FileHandle, CachedHandle>>,
    capabilities: RwLock<Option<FilesystemCapabilities>>,
    refreshing_directories: Arc<Mutex<HashSet<NodeId>>>,
    range_fetches: Mutex<HashMap<RangeKey, Arc<RangeFetch>>>,
    directory_parents: RwLock<HashMap<NodeId, NodeId>>,
}

impl CachedFilesystem {
    pub fn new(
        inner: Arc<dyn RemoteFilesystem>,
        cache: Arc<dyn FilesystemCache>,
        policy: CachePolicy,
    ) -> Result<Self> {
        if policy.block_size == 0 || policy.block_size > crate::MAX_CLIENT_READ_SIZE {
            return Err(ClientError::Server(
                ErrorCode::InvalidRequest,
                "cache block size must be within the client read limit".into(),
            ));
        }
        Ok(Self {
            inner,
            cache,
            policy,
            handles: RwLock::new(HashMap::new()),
            capabilities: RwLock::new(None),
            refreshing_directories: Arc::new(Mutex::new(HashSet::new())),
            range_fetches: Mutex::new(HashMap::new()),
            directory_parents: RwLock::new(HashMap::from([(ROOT_NODE, ROOT_NODE)])),
        })
    }

    async fn cache_metadata(&self, metadata: Metadata) -> Metadata {
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        metadata
    }

    async fn cached_metadata(&self, node: NodeId) -> Result<Metadata> {
        MetadataCache::get(self.cache.as_ref(), node)
            .await
            .ok_or(ClientError::OfflineCacheMiss)
    }

    async fn remember_handle(
        &self,
        node: NodeId,
        inner: Option<FileHandle>,
        revision: u64,
        size: u64,
        _options: FileOpenOptions,
    ) -> OpenedFile {
        let logical = FileHandle(Uuid::new_v4());
        self.handles.write().await.insert(
            logical,
            CachedHandle {
                node,
                inner,
                revision,
                size,
                mutation: Arc::new(Mutex::new(())),
            },
        );
        OpenedFile {
            handle: logical,
            revision,
            size,
        }
    }

    async fn handle(&self, logical: FileHandle) -> Result<CachedHandle> {
        self.handles
            .read()
            .await
            .get(&logical)
            .cloned()
            .ok_or_else(|| {
                ClientError::Server(ErrorCode::InvalidHandle, "unknown cached handle".into())
            })
    }

    async fn invalidate_node(&self, node: NodeId) {
        NodeCacheInvalidation::invalidate_node_state(self.cache.as_ref(), node).await;
    }

    /// In-memory-only node invalidation for the streaming write hot path. See
    /// `NodeCacheInvalidation::invalidate_node_memory`.
    async fn invalidate_node_memory(&self, node: NodeId) {
        NodeCacheInvalidation::invalidate_node_memory(self.cache.as_ref(), node).await;
    }

    async fn offline_directory(&self, node: NodeId) -> Result<DirectorySnapshot> {
        let metadata = self.cached_metadata(node).await?;
        let key = RevisionKey {
            node,
            revision: metadata.revision,
        };
        DirectoryCache::get(self.cache.as_ref(), key)
            .await
            .map(|entries| DirectorySnapshot {
                revision: metadata.revision,
                entries,
            })
            .ok_or(ClientError::OfflineCacheMiss)
    }

    async fn refresh_directory_in_background(&self, node: NodeId) {
        {
            let mut refreshing = self.refreshing_directories.lock().await;
            if !refreshing.insert(node) {
                return;
            }
        }
        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);
        let refreshing = Arc::clone(&self.refreshing_directories);
        tokio::spawn(async move {
            if let Ok(snapshot) = inner.list_directory_snapshot(node).await {
                let key = RevisionKey {
                    node,
                    revision: snapshot.revision,
                };
                DirectoryCache::store_readthrough_snapshot(cache.as_ref(), key, snapshot.entries)
                    .await;
                if let Ok(metadata) = inner.get_metadata(node).await
                    && metadata.revision == snapshot.revision
                {
                    MetadataCache::store_readthrough(cache.as_ref(), metadata).await;
                }
            }
            refreshing.lock().await.remove(&node);
        });
    }

    async fn cached_range(
        &self,
        state: &CachedHandle,
        offset: u64,
        length: u64,
    ) -> Option<Vec<u8>> {
        let available = state.size.saturating_sub(offset);
        let length = length.min(available);
        RangeCache::get(
            self.cache.as_ref(),
            RangeKey {
                file: RevisionKey {
                    node: state.node,
                    revision: state.revision,
                },
                offset,
                length,
            },
        )
        .await
    }

    fn blocks_for(&self, state: &CachedHandle, offset: u64, length: u64) -> Result<Vec<RangeKey>> {
        if offset.checked_add(length).is_none() {
            return Err(ClientError::Server(
                ErrorCode::InvalidRequest,
                "read range overflows".into(),
            ));
        }
        let available = state.size.saturating_sub(offset);
        let requested = length.min(available);
        if requested == 0 {
            return Ok(Vec::new());
        }
        let requested_end = offset
            .checked_add(requested)
            .ok_or(ClientError::UnexpectedResponse)?;
        // Header probes and thumbnail reads should not pull 16 MiB from every
        // file Finder touches. Sequential/copy-sized requests retain the large
        // aligned block, while small random reads use a bounded 1 MiB window.
        let block_size = if length < SMALL_READ_THRESHOLD {
            self.policy.block_size.min(SMALL_READ_AHEAD_BLOCK_SIZE)
        } else {
            self.policy.block_size
        };
        let mut block_offset = offset / block_size * block_size;
        let mut blocks = Vec::new();
        while block_offset < requested_end {
            let block_length = self
                .policy
                .block_size
                .min(block_size)
                .min(state.size.saturating_sub(block_offset));
            blocks.push(RangeKey {
                file: RevisionKey {
                    node: state.node,
                    revision: state.revision,
                },
                offset: block_offset,
                length: block_length,
            });
            block_offset = block_offset
                .checked_add(block_size)
                .ok_or(ClientError::UnexpectedResponse)?;
        }
        Ok(blocks)
    }

    async fn fetch_block(
        &self,
        inner_handle: FileHandle,
        state: &CachedHandle,
        block: RangeKey,
    ) -> Result<Vec<u8>> {
        let fetch = {
            let mut fetches = self.range_fetches.lock().await;
            fetches
                .entry(block)
                .or_insert_with(|| {
                    Arc::new(RangeFetch {
                        result: OnceCell::new(),
                    })
                })
                .clone()
        };
        let result = fetch
            .result
            .get_or_init(|| async {
                let loaded = async {
                    if let Some(data) = RangeCache::get(self.cache.as_ref(), block).await {
                        return Ok(Arc::new(data));
                    }
                    let read = self
                        .inner
                        .read_range_versioned(inner_handle, block.offset, block.length)
                        .await?;
                    if read.revision != state.revision {
                        return Err(ClientError::StaleRevision);
                    }
                    let actual = u64::try_from(read.data.len())
                        .map_err(|_| ClientError::UnexpectedResponse)?;
                    if actual > block.length {
                        return Err(ClientError::UnexpectedResponse);
                    }
                    if actual > 0 {
                        let actual_key = RangeKey {
                            length: actual,
                            ..block
                        };
                        RangeCache::store_readthrough(
                            self.cache.as_ref(),
                            actual_key,
                            read.data.clone(),
                        )
                        .await;
                    }
                    Ok(Arc::new(read.data))
                }
                .await;
                loaded.map_err(SharedFetchError::from)
            })
            .await
            .clone();
        let mut fetches = self.range_fetches.lock().await;
        if fetches
            .get(&block)
            .is_some_and(|registered| Arc::ptr_eq(registered, &fetch))
        {
            fetches.remove(&block);
        }
        result
            .map(|data| data.as_ref().clone())
            .map_err(ClientError::from)
    }

    async fn update_handle(&self, logical: FileHandle, result: WriteResult) {
        if let Some(state) = self.handles.write().await.get_mut(&logical) {
            state.revision = result.revision;
            state.size = result.size;
        }
    }
}

fn is_offline(error: &ClientError) -> bool {
    matches!(error, ClientError::Transport(_) | ClientError::Offline)
}

fn require_online_handle(state: &CachedHandle) -> Result<FileHandle> {
    state.inner.ok_or(ClientError::Offline)
}

#[async_trait]
impl RemoteFilesystem for CachedFilesystem {
    async fn ping(&self, nonce: u64) -> Result<u64> {
        self.inner.ping(nonce).await
    }

    async fn capabilities(&self) -> Result<FilesystemCapabilities> {
        match self.inner.capabilities().await {
            Ok(capabilities) => {
                *self.capabilities.write().await = Some(capabilities.clone());
                Ok(capabilities)
            }
            Err(error) if is_offline(&error) => self
                .capabilities
                .read()
                .await
                .clone()
                .ok_or(ClientError::Offline),
            Err(error) => Err(error),
        }
    }

    async fn stat_filesystem(&self) -> Result<FilesystemStats> {
        match self.inner.stat_filesystem().await {
            Ok(statistics) => {
                self.cache
                    .store_readthrough_filesystem_stats(statistics)
                    .await;
                Ok(statistics)
            }
            Err(error) if is_offline(&error) => self
                .cache
                .get_filesystem_stats()
                .await
                .ok_or(ClientError::OfflineCacheMiss),
            Err(error) => Err(error),
        }
    }

    async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
        match self.inner.get_metadata(node).await {
            Ok(metadata) => Ok(self.cache_metadata(metadata).await),
            Err(error) if is_offline(&error) => self.cached_metadata(node).await,
            Err(error) => Err(error),
        }
    }

    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        Ok(self.list_directory_snapshot(node).await?.entries)
    }

    async fn list_directory_snapshot(&self, node: NodeId) -> Result<DirectorySnapshot> {
        if let Ok(snapshot) = self.offline_directory(node).await {
            self.refresh_directory_in_background(node).await;
            return Ok(snapshot);
        }
        match self.inner.list_directory_snapshot(node).await {
            Ok(snapshot) => {
                let key = RevisionKey {
                    node,
                    revision: snapshot.revision,
                };
                DirectoryCache::store_readthrough_snapshot(
                    self.cache.as_ref(),
                    key,
                    snapshot.entries.clone(),
                )
                .await;
                Ok(snapshot)
            }
            Err(error) if is_offline(&error) => self.offline_directory(node).await,
            Err(error) => Err(error),
        }
    }

    async fn list_directory_view(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
    ) -> Result<DirectoryView> {
        match self.inner.list_directory_view(node, options).await {
            Ok(view) => {
                let entries = view
                    .entries
                    .iter()
                    .map(|entry| entry.entry.clone())
                    .collect::<Vec<_>>();
                let key = RevisionKey {
                    node,
                    revision: view.revision,
                };
                DirectoryCache::store_readthrough_snapshot(self.cache.as_ref(), key, entries).await;
                self.cache_metadata(view.directory.clone()).await;
                self.cache_metadata(view.parent.clone()).await;
                self.directory_parents
                    .write()
                    .await
                    .insert(node, view.parent.node);
                Ok(view)
            }
            Err(error) if is_offline(&error) => {
                let snapshot = self.offline_directory(node).await?;
                let directory = self.cached_metadata(node).await?;
                let parent_node = self
                    .directory_parents
                    .read()
                    .await
                    .get(&node)
                    .copied()
                    .ok_or(ClientError::OfflineCacheMiss)?;
                let parent = self.cached_metadata(parent_node).await?;
                Ok(DirectoryView {
                    revision: snapshot.revision,
                    parent,
                    directory,
                    xattrs: None,
                    entries: snapshot
                        .entries
                        .into_iter()
                        .map(|entry| DirectoryViewEntry {
                            entry,
                            xattrs: None,
                        })
                        .collect(),
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)> {
        let opened = self
            .open_file_with_options(node, FileOpenOptions::READ_ONLY)
            .await?;
        Ok((opened.handle, opened.revision, opened.size))
    }

    async fn open_file_with_options(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> Result<OpenedFile> {
        match self.inner.open_file_with_options(node, options).await {
            Ok(opened) => Ok(self
                .remember_handle(
                    node,
                    Some(opened.handle),
                    opened.revision,
                    opened.size,
                    options,
                )
                .await),
            Err(error)
                if is_offline(&error)
                    && options.access == FileAccess::ReadOnly
                    && !options.truncate
                    && !options.append =>
            {
                let metadata = self.cached_metadata(node).await?;
                Ok(self
                    .remember_handle(node, None, metadata.revision, metadata.size, options)
                    .await)
            }
            Err(error) => Err(error),
        }
    }

    async fn create_file(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<CreatedFile> {
        let created = self.inner.create_file(parent, name, mode, options).await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), created.metadata.clone()).await;
        let opened = self
            .remember_handle(
                created.metadata.node,
                Some(created.opened.handle),
                created.opened.revision,
                created.opened.size,
                options,
            )
            .await;
        Ok(CreatedFile {
            metadata: created.metadata,
            opened,
        })
    }

    async fn create_directory(&self, parent: NodeId, name: Name, mode: u32) -> Result<Metadata> {
        let metadata = self.inner.create_directory(parent, name, mode).await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        self.directory_parents
            .write()
            .await
            .insert(metadata.node, parent);
        Ok(metadata)
    }

    async fn create_symlink(
        &self,
        parent: NodeId,
        name: Name,
        target: Vec<u8>,
    ) -> Result<Metadata> {
        let cached_target = target.clone();
        let metadata = self.inner.create_symlink(parent, name, target).await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        if u64::try_from(cached_target.len()).ok() == Some(metadata.size) {
            RangeCache::store_readthrough(
                self.cache.as_ref(),
                RangeKey {
                    file: RevisionKey {
                        node: metadata.node,
                        revision: metadata.revision,
                    },
                    offset: 0,
                    length: metadata.size,
                },
                cached_target,
            )
            .await;
        }
        Ok(metadata)
    }

    async fn create_hard_link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: Name,
    ) -> Result<Metadata> {
        let metadata = self
            .inner
            .create_hard_link(node, new_parent, new_name)
            .await?;
        self.invalidate_node(node).await;
        self.invalidate_node(new_parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        Ok(metadata)
    }

    async fn create_special_node(
        &self,
        parent: NodeId,
        name: Name,
        kind: SpecialNodeKind,
        mode: u32,
        device_major: u32,
        device_minor: u32,
    ) -> Result<Metadata> {
        let metadata = self
            .inner
            .create_special_node(parent, name, kind, mode, device_major, device_minor)
            .await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        Ok(metadata)
    }

    async fn remove_node(&self, parent: NodeId, name: Name, directory: bool) -> Result<()> {
        let cached_child = self
            .offline_directory(parent)
            .await
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .entries
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.node)
            });
        self.inner.remove_node(parent, name, directory).await?;
        self.invalidate_node(parent).await;
        if let Some(node) = cached_child {
            self.invalidate_node(node).await;
            self.directory_parents.write().await.remove(&node);
        }
        Ok(())
    }

    async fn rename_node(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    ) -> Result<()> {
        let source = self
            .offline_directory(parent)
            .await
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .entries
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.node)
            });
        let destination = self
            .offline_directory(new_parent)
            .await
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .entries
                    .into_iter()
                    .find(|entry| entry.name == new_name)
                    .map(|entry| entry.node)
            });
        self.inner
            .rename_node(parent, name, new_parent, new_name, mode)
            .await?;
        self.invalidate_node(parent).await;
        self.invalidate_node(new_parent).await;
        let mut directory_parents = self.directory_parents.write().await;
        if let Some(source) = source {
            directory_parents.insert(source, new_parent);
        }
        if mode == RenameMode::Exchange {
            if let Some(destination) = destination {
                directory_parents.insert(destination, parent);
            }
        } else if let Some(destination) = destination {
            directory_parents.remove(&destination);
        }
        Ok(())
    }

    async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
        match self.inner.read_link(node).await {
            Ok(target) => {
                let metadata = match self.inner.get_metadata(node).await {
                    Ok(metadata) => self.cache_metadata(metadata).await,
                    Err(_) => return Ok(target),
                };
                if u64::try_from(target.len()).ok() == Some(metadata.size) {
                    RangeCache::store_readthrough(
                        self.cache.as_ref(),
                        RangeKey {
                            file: RevisionKey {
                                node,
                                revision: metadata.revision,
                            },
                            offset: 0,
                            length: metadata.size,
                        },
                        target.clone(),
                    )
                    .await;
                }
                Ok(target)
            }
            Err(error) if is_offline(&error) => {
                let metadata = self.cached_metadata(node).await?;
                RangeCache::get(
                    self.cache.as_ref(),
                    RangeKey {
                        file: RevisionKey {
                            node,
                            revision: metadata.revision,
                        },
                        offset: 0,
                        length: metadata.size,
                    },
                )
                .await
                .ok_or(ClientError::OfflineCacheMiss)
            }
            Err(error) => Err(error),
        }
    }

    async fn set_attributes(
        &self,
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> Result<Metadata> {
        let state = match handle {
            Some(logical) => Some(self.handle(logical).await?),
            None => None,
        };
        let _mutation = match &state {
            Some(state) => Some(state.mutation.lock().await),
            None => None,
        };
        let mapped = state.as_ref().map(require_online_handle).transpose()?;
        let metadata = self.inner.set_attributes(node, mapped, changes).await?;
        self.invalidate_node(node).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        if let Some(logical) = handle
            && let Some(state) = self.handles.write().await.get_mut(&logical)
        {
            state.revision = metadata.revision;
            state.size = metadata.size;
        }
        Ok(metadata)
    }

    async fn read_range(&self, handle: FileHandle, offset: u64, length: u64) -> Result<Vec<u8>> {
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
    ) -> Result<RangeRead> {
        let state = self.handle(handle).await?;
        if offset.checked_add(length).is_none() {
            return Err(ClientError::Server(
                ErrorCode::InvalidRequest,
                "read range overflows".into(),
            ));
        }
        let Some(inner_handle) = state.inner else {
            return self
                .cached_range(&state, offset, length)
                .await
                .map(|data| RangeRead {
                    revision: state.revision,
                    data,
                })
                .ok_or(ClientError::OfflineCacheMiss);
        };

        let blocks = self.blocks_for(&state, offset, length)?;
        if blocks.is_empty() {
            return Ok(RangeRead {
                revision: state.revision,
                data: Vec::new(),
            });
        }
        let assembled = MemoryCache::default();
        // Every block, including a cache lookup, passes through the per-block
        // gate. Concurrent kernel read-ahead callbacks therefore share one
        // persistent-cache verification or one network request instead of all
        // hashing the same large block before they reach the fetch gate.
        let reads = blocks
            .iter()
            .map(|block| async { (*block, self.fetch_block(inner_handle, &state, *block).await) });
        let mut offline_error = false;
        for (block, result) in join_all(reads).await {
            match result {
                Ok(data) => {
                    let actual =
                        u64::try_from(data.len()).map_err(|_| ClientError::UnexpectedResponse)?;
                    if actual > block.length {
                        return Err(ClientError::UnexpectedResponse);
                    }
                    if actual > 0 {
                        let actual_key = RangeKey {
                            length: actual,
                            ..block
                        };
                        RangeCache::insert(&assembled, actual_key, data).await;
                    }
                }
                Err(error) if is_offline(&error) => offline_error = true,
                Err(error) => return Err(error),
            }
        }
        let requested = RangeKey {
            file: RevisionKey {
                node: state.node,
                revision: state.revision,
            },
            offset,
            length: length.min(state.size.saturating_sub(offset)),
        };
        if let Some(data) = RangeCache::get(&assembled, requested).await {
            return Ok(RangeRead {
                revision: state.revision,
                data,
            });
        }
        if let Some(data) = self.cached_range(&state, offset, length).await {
            return Ok(RangeRead {
                revision: state.revision,
                data,
            });
        }
        if offline_error {
            Err(ClientError::OfflineCacheMiss)
        } else {
            Err(ClientError::UnexpectedResponse)
        }
    }

    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<WriteResult> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        let inner_handle = require_online_handle(&state)?;
        let result = self.inner.write_range(inner_handle, offset, data).await?;
        // Invalidate only the volatile in-memory projections. A media copy is
        // delivered by the kernel as thousands of small FUSE writes; doing any
        // durable per-chunk cache work (a range insert or a manifest
        // invalidation) floods the single cache-writer thread and its backlog
        // later stalls unrelated durable operations. Written ranges are also
        // revision-orphaned by the next chunk, so persisting them is pure churn;
        // a later read repopulates the cache through the read fill path.
        self.invalidate_node_memory(state.node).await;
        self.update_handle(handle, result).await;
        Ok(result)
    }

    async fn flush_file(&self, handle: FileHandle, lock_owner: Option<u64>) -> Result<()> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        if let Some(inner) = state.inner {
            self.inner.flush_file(inner, lock_owner).await
        } else {
            // Offline handles are necessarily read-only. There are no dirty
            // bytes or remote locks to flush, so FUSE close must still succeed.
            Ok(())
        }
    }

    async fn sync_file(&self, handle: FileHandle, data_only: bool) -> Result<()> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        if let Some(inner) = state.inner {
            self.inner.sync_file(inner, data_only).await
        } else {
            // A cached read-only handle has no pending mutations.
            Ok(())
        }
    }

    async fn sync_directory(&self, node: NodeId) -> Result<()> {
        self.inner.sync_directory(node).await
    }

    async fn allocate_file(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        let result = self
            .inner
            .allocate_file(require_online_handle(&state)?, offset, length)
            .await?;
        self.invalidate_node(state.node).await;
        self.update_handle(handle, result).await;
        Ok(result)
    }

    async fn get_xattr(
        &self,
        node: NodeId,
        name: Name,
        offset: u64,
        length: u64,
    ) -> Result<XattrRead> {
        self.inner.get_xattr(node, name, offset, length).await
    }

    async fn set_xattr(
        &self,
        node: NodeId,
        name: Name,
        value: &[u8],
        mode: XattrSetMode,
        position: u32,
    ) -> Result<()> {
        self.inner
            .set_xattr(node, name, value, mode, position)
            .await?;
        self.invalidate_node(node).await;
        Ok(())
    }

    async fn list_xattrs(&self, node: NodeId) -> Result<Vec<Name>> {
        self.inner.list_xattrs(node).await
    }

    async fn remove_xattr(&self, node: NodeId, name: Name) -> Result<()> {
        self.inner.remove_xattr(node, name).await?;
        self.invalidate_node(node).await;
        Ok(())
    }

    async fn copy_file_range(
        &self,
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        let input_state = self.handle(input).await?;
        let output_state = self.handle(output).await?;
        let input_handle = require_online_handle(&input_state)?;
        let output_handle = require_online_handle(&output_state)?;
        let result = self
            .inner
            .copy_file_range(
                input_handle,
                input_offset,
                output_handle,
                output_offset,
                length,
            )
            .await?;
        self.invalidate_node(output_state.node).await;
        self.update_handle(output, result).await;
        Ok(result)
    }

    async fn seek_file(&self, handle: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
        let state = self.handle(handle).await?;
        self.inner
            .seek_file(require_online_handle(&state)?, offset, whence)
            .await
    }

    async fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64> {
        let state = self.handle(handle).await?;
        self.inner
            .safe_ioctl(require_online_handle(&state)?, operation)
            .await
    }

    async fn map_block(&self, node: NodeId, block_size: u32, block: u64) -> Result<u64> {
        self.inner.map_block(node, block_size, block).await
    }

    async fn exchange_data(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        options: u64,
    ) -> Result<()> {
        self.inner
            .exchange_data(parent, name, new_parent, new_name, options)
            .await?;
        self.invalidate_node(parent).await;
        self.invalidate_node(new_parent).await;
        Ok(())
    }

    async fn set_volume_name(&self, name: Name) -> Result<()> {
        self.inner.set_volume_name(name).await
    }

    async fn forget_nodes(&self, nodes: Vec<NodeId>) -> Result<()> {
        self.inner.forget_nodes(nodes).await
    }

    async fn get_lock(&self, handle: FileHandle, lock: FileLock) -> Result<Option<FileLock>> {
        let state = self.handle(handle).await?;
        self.inner
            .get_lock(require_online_handle(&state)?, lock)
            .await
    }

    async fn set_lock(&self, handle: FileHandle, lock: FileLock, wait: bool) -> Result<()> {
        let state = self.handle(handle).await?;
        self.inner
            .set_lock(require_online_handle(&state)?, lock, wait)
            .await
    }

    async fn close_file(&self, handle: FileHandle) -> Result<()> {
        let mutation = self.handle(handle).await?.mutation;
        let _mutation = mutation.lock().await;
        let state = self.handles.write().await.remove(&handle).ok_or_else(|| {
            ClientError::Server(ErrorCode::InvalidHandle, "unknown cached handle".into())
        })?;
        if let Some(inner) = state.inner {
            self.inner.close_file(inner).await
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quickfs_cache::MemoryCache;
    use quickfs_protocol::{DirectoryRevision, NodeKind, ROOT_NODE};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const FILE_NODE: NodeId = NodeId(Uuid::from_u128(7));
    const LINK_NODE: NodeId = NodeId(Uuid::from_u128(8));
    const LINK_TARGET: &[u8] = b"clip.mov";

    fn filesystem_stats() -> FilesystemStats {
        FilesystemStats {
            blocks: 1_000,
            blocks_free: 600,
            blocks_available: 500,
            files: 100,
            files_free: 80,
            block_size: 4_096,
            name_length: 255,
            fragment_size: 4_096,
        }
    }

    struct ToggleFilesystem {
        offline: AtomicBool,
        fail_reads: AtomicBool,
        reads: AtomicUsize,
        read_lengths: std::sync::Mutex<Vec<u64>>,
        metadata_reads: AtomicUsize,
        directory_reads: AtomicUsize,
        directory_delay_ms: AtomicU64,
        read_delay_ms: AtomicU64,
        data: Vec<u8>,
    }

    impl ToggleFilesystem {
        fn check_online(&self) -> Result<()> {
            if self.offline.load(Ordering::SeqCst) {
                Err(ClientError::Offline)
            } else {
                Ok(())
            }
        }

        fn metadata(node: NodeId) -> Metadata {
            Metadata {
                node,
                kind: match node {
                    ROOT_NODE => NodeKind::Directory,
                    LINK_NODE => NodeKind::Symlink,
                    _ => NodeKind::File,
                },
                size: match node {
                    FILE_NODE => 2 * 1024 * 1024,
                    LINK_NODE => LINK_TARGET.len() as u64,
                    _ => 0,
                },
                mode: if node == FILE_NODE { 0o644 } else { 0o755 },
                allocated_blocks: if node == FILE_NODE { 4_096 } else { 0 },
                revision: match node {
                    FILE_NODE => 17,
                    LINK_NODE => 18,
                    _ => 9,
                },
                accessed_unix_ms: 1,
                modified_unix_ms: 1,
                created_unix_ms: Some(1),
                backup_unix_ms: None,
                link_count: if node == ROOT_NODE { 2 } else { 1 },
                device_major: 0,
                device_minor: 0,
            }
        }
    }

    #[async_trait]
    impl RemoteFilesystem for ToggleFilesystem {
        async fn ping(&self, nonce: u64) -> Result<u64> {
            self.check_online()?;
            Ok(nonce)
        }

        async fn stat_filesystem(&self) -> Result<FilesystemStats> {
            self.check_online()?;
            Ok(filesystem_stats())
        }

        async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
            self.check_online()?;
            self.metadata_reads.fetch_add(1, Ordering::SeqCst);
            Ok(Self::metadata(node))
        }

        async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
            Ok(self.list_directory_snapshot(node).await?.entries)
        }

        async fn list_directory_snapshot(&self, _node: NodeId) -> Result<DirectorySnapshot> {
            self.check_online()?;
            self.directory_reads.fetch_add(1, Ordering::SeqCst);
            let delay_ms = self.directory_delay_ms.load(Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Ok(DirectorySnapshot {
                revision: 9 as DirectoryRevision,
                entries: vec![DirectoryEntry {
                    node: FILE_NODE,
                    name: "clip.mov".into(),
                    kind: NodeKind::File,
                    metadata: Self::metadata(FILE_NODE),
                }],
            })
        }

        async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)> {
            let opened = self
                .open_file_with_options(node, FileOpenOptions::READ_ONLY)
                .await?;
            Ok((opened.handle, opened.revision, opened.size))
        }

        async fn open_file_with_options(
            &self,
            node: NodeId,
            _options: FileOpenOptions,
        ) -> Result<OpenedFile> {
            self.check_online()?;
            Ok(OpenedFile {
                handle: FileHandle(Uuid::new_v4()),
                revision: Self::metadata(node).revision,
                size: Self::metadata(node).size,
            })
        }

        async fn read_range(
            &self,
            handle: FileHandle,
            offset: u64,
            length: u64,
        ) -> Result<Vec<u8>> {
            Ok(self
                .read_range_versioned(handle, offset, length)
                .await?
                .data)
        }

        async fn read_range_versioned(
            &self,
            _handle: FileHandle,
            offset: u64,
            length: u64,
        ) -> Result<RangeRead> {
            self.check_online()?;
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.read_lengths.lock().unwrap().push(length);
            let delay_ms = self.read_delay_ms.load(Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(ClientError::Offline);
            }
            let start = usize::try_from(offset).unwrap();
            let end = usize::try_from(offset.saturating_add(length))
                .unwrap()
                .min(self.data.len());
            Ok(RangeRead {
                revision: 17,
                data: self.data[start..end].to_vec(),
            })
        }

        async fn close_file(&self, _handle: FileHandle) -> Result<()> {
            self.check_online()
        }

        async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
            self.check_online()?;
            if node == LINK_NODE {
                Ok(LINK_TARGET.to_vec())
            } else {
                Err(ClientError::Server(
                    ErrorCode::InvalidNode,
                    "not a cached test symlink".into(),
                ))
            }
        }
    }

    #[tokio::test]
    async fn cached_ranges_and_directory_snapshots_work_offline() {
        let data: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(0),
            data: data.clone(),
        });
        let cache = Arc::new(MemoryCache::default());
        let policy = CachePolicy {
            block_size: 1024 * 1024,
        };
        let filesystem = CachedFilesystem::new(inner.clone(), cache.clone(), policy).unwrap();

        filesystem.get_metadata(ROOT_NODE).await.unwrap();
        filesystem.get_metadata(FILE_NODE).await.unwrap();
        assert_eq!(
            filesystem.stat_filesystem().await.unwrap(),
            filesystem_stats()
        );
        assert_eq!(filesystem.read_link(LINK_NODE).await.unwrap(), LINK_TARGET);
        let metadata_reads = inner.metadata_reads.load(Ordering::SeqCst);
        filesystem.list_directory(ROOT_NODE).await.unwrap();
        assert_eq!(inner.metadata_reads.load(Ordering::SeqCst), metadata_reads);
        let opened = filesystem.open_file(FILE_NODE).await.unwrap().0;
        assert_eq!(
            filesystem.read_range(opened, 123, 4096).await.unwrap(),
            data[123..4219]
        );
        filesystem.close_file(opened).await.unwrap();
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);

        inner.offline.store(true, Ordering::SeqCst);
        let offline = CachedFilesystem::new(inner.clone(), cache, policy).unwrap();
        assert_eq!(offline.stat_filesystem().await.unwrap(), filesystem_stats());
        assert_eq!(offline.read_link(LINK_NODE).await.unwrap(), LINK_TARGET);
        assert_eq!(offline.list_directory(ROOT_NODE).await.unwrap().len(), 1);
        let opened = offline.open_file(FILE_NODE).await.unwrap().0;
        assert_eq!(
            offline.read_range(opened, 200, 512).await.unwrap(),
            data[200..712]
        );
        assert!(matches!(
            offline.read_range(opened, 1024 * 1024 + 4, 128).await,
            Err(ClientError::OfflineCacheMiss)
        ));
        offline.flush_file(opened, Some(42)).await.unwrap();
        offline.sync_file(opened, false).await.unwrap();
        offline.close_file(opened).await.unwrap();
    }

    #[tokio::test]
    async fn cached_directory_is_returned_while_one_remote_refresh_runs() {
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(0),
            data: Vec::new(),
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy::default(),
        )
        .unwrap();

        filesystem.get_metadata(ROOT_NODE).await.unwrap();
        filesystem.list_directory(ROOT_NODE).await.unwrap();
        assert_eq!(inner.directory_reads.load(Ordering::SeqCst), 1);

        inner.directory_delay_ms.store(500, Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            filesystem.list_directory(ROOT_NODE),
        )
        .await
        .unwrap()
        .unwrap();
        while inner.directory_reads.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            filesystem.list_directory(ROOT_NODE),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(inner.directory_reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_overlapping_reads_share_one_remote_block_fetch() {
        let data = vec![0x5a; 2 * 1024 * 1024];
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(50),
            data,
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;
        let (first, second) = tokio::join!(
            filesystem.read_range(handle, 64, 4096),
            filesystem.read_range(handle, 128, 4096)
        );
        assert_eq!(first.unwrap(), vec![0x5a; 4096]);
        assert_eq!(second.unwrap(), vec![0x5a; 4096]);
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn small_random_read_uses_bounded_read_ahead_with_large_policy_blocks() {
        let data = vec![0x5a; 2 * 1024 * 1024];
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(0),
            data,
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy::default(),
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        assert_eq!(
            filesystem.read_range(handle, 64, 4096).await.unwrap(),
            vec![0x5a; 4096]
        );
        assert_eq!(*inner.read_lengths.lock().unwrap(), [1024 * 1024]);
    }

    #[tokio::test]
    async fn concurrent_failed_reads_share_one_remote_fetch() {
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(true),
            reads: AtomicUsize::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(50),
            data: vec![0x5a; 2 * 1024 * 1024],
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;
        let (first, second) = tokio::join!(
            filesystem.read_range(handle, 64, 4096),
            filesystem.read_range(handle, 128, 4096)
        );

        assert!(matches!(first, Err(ClientError::OfflineCacheMiss)));
        assert!(matches!(second, Err(ClientError::OfflineCacheMiss)));
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);
    }
}
