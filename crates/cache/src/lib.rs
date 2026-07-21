// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use async_trait::async_trait;
use dashmap::DashMap;
use quickfs_protocol::{DirectoryEntry, FilesystemStats, Metadata, NodeId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

const CACHE_FORMAT_VERSION: u32 = 2;
const MAX_MANIFEST_SIZE: u64 = 32 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 100_000;
const MAX_NAMESPACE_COMPONENT_LENGTH: usize = 1_024;
const MANIFEST_FILE: &str = "manifest.json";
const LOCK_FILE: &str = ".cache.lock";
const DEFAULT_HOT_RANGE_BYTES: usize = 256 * 1024 * 1024;
/// Maximum queued writer jobs executed under one deferred-commit batch. The
/// batch bounds how many state mutations share a single manifest write, so a
/// flood of read-through stores costs one manifest serialization per batch
/// instead of one per store.
const WRITER_BATCH_MAX: usize = 256;
/// Opportunistic read-through stores are dropped (memory tiers still serve
/// them) once this many writer jobs are queued. Coherence-critical operations
/// are never dropped. Bounds the writer backlog so waited operations cannot
/// stall behind minutes of speculative persistence.
const MAX_PENDING_STORE_JOBS: usize = 1_024;
/// Companion bound on queued range payload bytes. Each queued range store owns
/// a copy of its payload, so this also caps the writer queue's memory.
const MAX_PENDING_STORE_BYTES: u64 = 128 * 1024 * 1024;
static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct RevisionKey {
    pub node: NodeId,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct RangeKey {
    pub file: RevisionKey,
    pub offset: u64,
    pub length: u64,
}

/// Separates cached data belonging to different servers, exports, and
/// authorization scopes. The namespace is serialized into the persistent
/// manifest and its digest selects a private on-disk directory.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct CacheNamespace {
    pub server_identity_sha256: [u8; 32],
    pub export_id: String,
    pub authorization_scope: String,
}

impl CacheNamespace {
    pub fn new(
        server_identity_sha256: [u8; 32],
        export_id: impl Into<String>,
        authorization_scope: impl Into<String>,
    ) -> Result<Self, CacheError> {
        let namespace = Self {
            server_identity_sha256,
            export_id: export_id.into(),
            authorization_scope: authorization_scope.into(),
        };
        namespace.validate()?;
        Ok(namespace)
    }

    fn validate(&self) -> Result<(), CacheError> {
        validate_namespace_component(&self.export_id)?;
        validate_namespace_component(&self.authorization_scope)
    }

    fn directory_name(&self) -> Result<String, CacheError> {
        let serialized = serde_json::to_vec(self)?;
        Ok(hex::encode(Sha256::digest(serialized)))
    }
}

fn validate_namespace_component(value: &str) -> Result<(), CacheError> {
    if value.is_empty() || value.len() > MAX_NAMESPACE_COMPONENT_LENGTH || value.contains('\0') {
        Err(CacheError::InvalidNamespace)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheStats {
    pub entries: usize,
    pub payload_bytes: u64,
    pub maximum_payload_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache manifest serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("cache namespace components must contain 1-1024 non-NUL bytes")]
    InvalidNamespace,
    #[error("cache storage is not a private, owner-controlled directory or file")]
    UnsafeStorage,
    #[error("the cache namespace is already open by another process")]
    CacheInUse,
    #[error("cache size limit must be greater than zero")]
    InvalidSizeLimit,
    #[error("invalid or inconsistent cached byte range")]
    InvalidRange,
    #[error("cache entry exceeds the configured payload budget")]
    EntryTooLarge,
    #[error("cache manifest is unsupported or malformed")]
    InvalidManifest,
    #[error("cache manifest belongs to a different namespace")]
    NamespaceMismatch,
    #[error("cached data is missing, truncated, or corrupt")]
    CorruptEntry,
    #[error("cache state lock is unavailable")]
    StateUnavailable,
}

#[async_trait]
pub trait MetadataCache: Send + Sync {
    async fn get(&self, node: NodeId) -> Option<Metadata>;
    async fn insert(&self, value: Metadata);
    /// Store metadata discovered by a read without extending the read's
    /// critical path. In-memory caches complete inline; persistent adapters
    /// may enqueue durable work on a blocking worker.
    async fn store_readthrough(&self, value: Metadata) {
        self.insert(value).await;
    }
    async fn invalidate(&self, node: NodeId);
}

#[async_trait]
pub trait DirectoryCache: Send + Sync {
    async fn get(&self, key: RevisionKey) -> Option<Vec<DirectoryEntry>>;
    async fn insert(&self, key: RevisionKey, value: Vec<DirectoryEntry>);
    /// Store a directory snapshot and its already-returned child metadata.
    /// Persistent implementations can commit the complete snapshot in one
    /// manifest transaction instead of one transaction per child.
    async fn store_readthrough_snapshot(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        self.insert(key, value).await;
    }
    async fn invalidate(&self, node: NodeId);
    /// Invalidate snapshots that embed metadata for `node` as one of their
    /// children. File data, xattr, and attribute mutations do not necessarily
    /// change the parent directory's revision, so key-only invalidation is not
    /// sufficient for coherent readdirplus/offline metadata.
    async fn invalidate_containing(&self, _node: NodeId) {}
}

#[async_trait]
pub trait RangeCache: Send + Sync {
    /// Return the requested range when it is fully covered. Implementations may
    /// assemble it from larger, overlapping, or adjacent entries of the same
    /// node revision.
    async fn get(&self, key: RangeKey) -> Option<Vec<u8>>;
    async fn insert(&self, key: RangeKey, value: Vec<u8>);
    async fn store_readthrough(&self, key: RangeKey, value: Vec<u8>) {
        self.insert(key, value).await;
    }
    async fn invalidate(&self, node: NodeId);
}

#[async_trait]
pub trait FilesystemStateCache: Send + Sync {
    async fn get_filesystem_stats(&self) -> Option<FilesystemStats>;
    async fn insert_filesystem_stats(&self, value: FilesystemStats);
    async fn store_readthrough_filesystem_stats(&self, value: FilesystemStats) {
        self.insert_filesystem_stats(value).await;
    }
}

/// Atomically invalidate every cache projection whose correctness depends on
/// one node. Persistent implementations use one manifest transaction so a
/// stream of small FUSE writes does not pay several fsyncs per chunk.
#[async_trait]
pub trait NodeCacheInvalidation: Send + Sync {
    async fn invalidate_node_state(&self, node: NodeId);

    /// Invalidate only the volatile in-memory projections of a node, never the
    /// durable manifest. Correctness for online reads depends only on the
    /// in-memory caches; durable range/directory entries are revision-keyed, so
    /// a stale on-disk entry is never served for a newer revision and is
    /// eventually reclaimed by LRU or refreshed on the next read. The streaming
    /// write path uses this so a 40 MiB copy does not enqueue thousands of
    /// per-chunk manifest transactions on the single cache-writer thread, whose
    /// backlog would otherwise stall later durable operations.
    async fn invalidate_node_memory(&self, node: NodeId) {
        self.invalidate_node_state(node).await;
    }
}

#[derive(Default)]
pub struct MemoryCache {
    metadata: DashMap<NodeId, Metadata>,
    directories: DashMap<RevisionKey, Vec<DirectoryEntry>>,
    ranges: DashMap<RangeKey, Vec<u8>>,
    statistics: Mutex<Option<FilesystemStats>>,
}

#[async_trait]
impl MetadataCache for MemoryCache {
    async fn get(&self, node: NodeId) -> Option<Metadata> {
        self.metadata.get(&node).map(|value| value.clone())
    }

    async fn insert(&self, value: Metadata) {
        self.metadata.insert(value.node, value);
    }

    async fn invalidate(&self, node: NodeId) {
        self.metadata.remove(&node);
    }
}

#[async_trait]
impl DirectoryCache for MemoryCache {
    async fn get(&self, key: RevisionKey) -> Option<Vec<DirectoryEntry>> {
        self.directories.get(&key).map(|value| value.clone())
    }

    async fn insert(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        self.directories.insert(key, value);
    }

    async fn store_readthrough_snapshot(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        for entry in &value {
            self.metadata.insert(entry.node, entry.metadata.clone());
        }
        self.directories.insert(key, value);
    }

    async fn invalidate(&self, node: NodeId) {
        self.directories.retain(|key, _| key.node != node);
    }

    async fn invalidate_containing(&self, node: NodeId) {
        self.directories
            .retain(|_, entries| !entries.iter().any(|entry| entry.node == node));
    }
}

#[async_trait]
impl RangeCache for MemoryCache {
    async fn get(&self, key: RangeKey) -> Option<Vec<u8>> {
        if key.length == 0 {
            return key.offset.checked_add(key.length).map(|_| Vec::new());
        }
        let segments = self
            .ranges
            .iter()
            .filter(|entry| entry.key().file == key.file)
            .map(|entry| Segment {
                key: *entry.key(),
                offset: entry.key().offset,
                data: entry.value().clone(),
            })
            .collect();
        assemble_range(key, segments).map(|assembled| assembled.data)
    }

    async fn insert(&self, key: RangeKey, value: Vec<u8>) {
        if valid_range_payload(key, &value) && !value.is_empty() {
            self.ranges.insert(key, value);
        }
    }

    async fn invalidate(&self, node: NodeId) {
        self.ranges.retain(|key, _| key.file.node != node);
    }
}

#[async_trait]
impl NodeCacheInvalidation for MemoryCache {
    async fn invalidate_node_state(&self, node: NodeId) {
        self.metadata.remove(&node);
        self.directories.retain(|key, entries| {
            key.node != node && !entries.iter().any(|entry| entry.node == node)
        });
        self.ranges.retain(|key, _| key.file.node != node);
    }
}

#[async_trait]
impl FilesystemStateCache for MemoryCache {
    async fn get_filesystem_stats(&self) -> Option<FilesystemStats> {
        self.statistics.lock().ok().and_then(|value| *value)
    }

    async fn insert_filesystem_stats(&self, value: FilesystemStats) {
        if let Ok(mut statistics) = self.statistics.lock() {
            *statistics = Some(value);
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
enum EntryKey {
    Metadata(NodeId),
    Directory(RevisionKey),
    Range(RangeKey),
    FilesystemStats,
}

impl EntryKey {
    fn node(&self) -> Option<NodeId> {
        match self {
            Self::Metadata(node) => Some(*node),
            Self::Directory(key) => Some(key.node),
            Self::Range(key) => Some(key.file.node),
            Self::FilesystemStats => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DiskEntry {
    key: EntryKey,
    file_name: String,
    stored_length: u64,
    sha256: [u8; 32],
    last_access: u64,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    namespace: CacheNamespace,
    clock: u64,
    entries: Vec<DiskEntry>,
}

#[derive(Clone, Default)]
struct PersistentState {
    clock: u64,
    payload_bytes: u64,
    entries: HashMap<EntryKey, DiskEntry>,
}

/// A single-process, bounded persistent cache. Entries are stored in a private
/// namespace directory, payloads and manifests are replaced atomically, and
/// every payload is verified against its recorded SHA-256 digest before use.
pub struct PersistentCache {
    namespace: CacheNamespace,
    directory: PathBuf,
    maximum_payload_bytes: u64,
    state: Mutex<PersistentState>,
    /// Group-commit state for the writer thread. While a batch is active,
    /// `commit_state` applies mutations in memory and records the manifest as
    /// dirty; `flush_deferred_commits` then writes the manifest once for the
    /// whole batch. Guarded by its own mutex but only ever manipulated while
    /// the state mutex is held (or by the writer thread between jobs), so the
    /// two never race. Lock order is always `state` before `deferred`.
    deferred: Mutex<DeferredCommits>,
    /// Total manifests written since open; used by tests and diagnostics to
    /// prove that batched mutations coalesce into few manifest writes.
    manifest_writes: AtomicU64,
    _lock_file: File,
}

#[derive(Default)]
struct DeferredCommits {
    active: bool,
    dirty: bool,
    /// Payload files orphaned by deferred commits. They stay on disk until the
    /// batch's single manifest write lands, preserving the existing crash
    /// ordering (manifest first, then file removal).
    pending_removals: Vec<String>,
}

/// Runs persistent-cache filesystem work outside Tokio's asynchronous worker
/// threads. Read-through stores are best-effort background work so filesystem
/// callbacks never wait for fsync or a contended cache manifest lock.
#[derive(Clone)]
pub struct NonBlockingPersistentCache {
    inner: Arc<PersistentCache>,
    writer: mpsc::Sender<PersistentWrite>,
    memory: Arc<MemoryCache>,
    hot_ranges: Arc<HotRangeCache>,
    /// Writer jobs queued but not yet executed. Opportunistic read-through
    /// stores consult this so a flood of speculative persistence (read-ahead,
    /// thumbnail sweeps) can never grow the queue without bound.
    pending_jobs: Arc<AtomicUsize>,
    /// Bytes of range payloads owned by queued jobs; the memory bound for the
    /// writer queue.
    pending_store_bytes: Arc<AtomicU64>,
    store_job_limit: usize,
    store_byte_limit: u64,
}

struct HotRangeCache {
    maximum_bytes: usize,
    state: Mutex<HotRangeState>,
}

#[derive(Default)]
struct HotRangeState {
    entries: HashMap<RangeKey, Vec<u8>>,
    least_recently_used: VecDeque<RangeKey>,
    bytes: usize,
}

struct PersistentWrite {
    job: Box<dyn FnOnce(&PersistentCache) + Send + 'static>,
    /// Present for `enqueue_and_wait` jobs. Fired only after the batch's
    /// manifest flush, so a waiter observing completion knows its mutation is
    /// durable, exactly as before batching.
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl HotRangeCache {
    fn new(maximum_bytes: usize) -> Self {
        Self {
            maximum_bytes,
            state: Mutex::new(HotRangeState::default()),
        }
    }

    fn get(&self, key: RangeKey) -> Option<Vec<u8>> {
        let request_end = key.offset.checked_add(key.length)?;
        if key.length == 0 {
            return Some(Vec::new());
        }
        let output_capacity = usize::try_from(key.length).ok()?;
        let mut state = self.state.lock().ok()?;
        let mut output = Vec::with_capacity(output_capacity);
        let mut used = Vec::new();
        let mut position = key.offset;
        while position < request_end {
            let (candidate, candidate_end) = state
                .entries
                .keys()
                .filter_map(|candidate| {
                    let end = candidate.offset.checked_add(candidate.length)?;
                    (candidate.file == key.file && candidate.offset <= position && end > position)
                        .then_some((*candidate, end))
                })
                .max_by_key(|(_, end)| *end)?;
            let data = state.entries.get(&candidate)?;
            let copy_end = candidate_end.min(request_end);
            let start = usize::try_from(position.checked_sub(candidate.offset)?).ok()?;
            let end = usize::try_from(copy_end.checked_sub(candidate.offset)?).ok()?;
            output.extend_from_slice(data.get(start..end)?);
            used.push(candidate);
            position = copy_end;
        }
        for used_key in used.into_iter().collect::<HashSet<_>>() {
            state
                .least_recently_used
                .retain(|candidate| *candidate != used_key);
            state.least_recently_used.push_back(used_key);
        }
        Some(output)
    }

    fn insert(&self, key: RangeKey, value: Vec<u8>) {
        if !valid_range_payload(key, &value) || value.is_empty() || value.len() > self.maximum_bytes
        {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(previous) = state.entries.remove(&key) {
            state.bytes = state.bytes.saturating_sub(previous.len());
        }
        state
            .least_recently_used
            .retain(|candidate| *candidate != key);
        state.bytes = state.bytes.saturating_add(value.len());
        state.entries.insert(key, value);
        state.least_recently_used.push_back(key);
        while state.bytes > self.maximum_bytes {
            let Some(evicted) = state.least_recently_used.pop_front() else {
                break;
            };
            if let Some(value) = state.entries.remove(&evicted) {
                state.bytes = state.bytes.saturating_sub(value.len());
            }
        }
    }

    fn invalidate(&self, node: NodeId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let removed_bytes = state
            .entries
            .iter()
            .filter(|(key, _)| key.file.node == node)
            .map(|(_, value)| value.len())
            .sum::<usize>();
        state.entries.retain(|key, _| key.file.node != node);
        state.bytes = state.bytes.saturating_sub(removed_bytes);
        state
            .least_recently_used
            .retain(|key| key.file.node != node);
    }
}

impl NonBlockingPersistentCache {
    pub fn open(
        root: impl AsRef<Path>,
        namespace: CacheNamespace,
        maximum_payload_bytes: u64,
    ) -> Result<Self, CacheError> {
        let inner = Arc::new(PersistentCache::open(
            root,
            namespace,
            maximum_payload_bytes,
        )?);
        let (writer, writes) = mpsc::channel::<PersistentWrite>();
        let worker_cache = Arc::clone(&inner);
        let pending_jobs = Arc::new(AtomicUsize::new(0));
        let worker_pending_jobs = Arc::clone(&pending_jobs);
        std::thread::Builder::new()
            .name("quickfs-cache-writer".into())
            .spawn(move || {
                // Group commit: drain a batch of queued jobs, run them with
                // manifest writes deferred, then write the manifest once for
                // the whole batch. A backlog of N read-through stores costs
                // N/WRITER_BATCH_MAX manifest serializations instead of N,
                // which is what keeps a large cache's manifest (megabytes of
                // JSON) from monopolizing the state lock and the CPU.
                while let Ok(first) = writes.recv() {
                    let mut batch = vec![first];
                    while batch.len() < WRITER_BATCH_MAX {
                        match writes.try_recv() {
                            Ok(next) => batch.push(next),
                            Err(_) => break,
                        }
                    }
                    worker_cache.begin_deferred_commits();
                    let mut completions = Vec::new();
                    for write in batch {
                        (write.job)(&worker_cache);
                        if let Some(done) = write.done {
                            completions.push(done);
                        }
                        worker_pending_jobs.fetch_sub(1, Ordering::Relaxed);
                    }
                    let _ = worker_cache.flush_deferred_commits();
                    for done in completions {
                        let _ = done.send(());
                    }
                }
            })?;
        let hot_range_bytes =
            usize::try_from(maximum_payload_bytes.min(DEFAULT_HOT_RANGE_BYTES as u64))
                .map_err(|_| CacheError::InvalidSizeLimit)?;
        Ok(Self {
            inner,
            writer,
            memory: Arc::new(MemoryCache::default()),
            hot_ranges: Arc::new(HotRangeCache::new(hot_range_bytes)),
            pending_jobs,
            pending_store_bytes: Arc::new(AtomicU64::new(0)),
            store_job_limit: MAX_PENDING_STORE_JOBS,
            store_byte_limit: MAX_PENDING_STORE_BYTES,
        })
    }

    pub fn namespace(&self) -> &CacheNamespace {
        self.inner.namespace()
    }

    /// Total manifest files written by the underlying persistent cache.
    pub fn manifest_writes(&self) -> u64 {
        self.inner.manifest_writes()
    }

    #[cfg(test)]
    fn set_store_limits(&mut self, jobs: usize, bytes: u64) {
        self.store_job_limit = jobs;
        self.store_byte_limit = bytes;
    }

    #[cfg(test)]
    fn persistent(&self) -> &PersistentCache {
        &self.inner
    }

    fn enqueue(&self, write: impl FnOnce(&PersistentCache) + Send + 'static) {
        self.pending_jobs.fetch_add(1, Ordering::Relaxed);
        if self
            .writer
            .send(PersistentWrite {
                job: Box::new(write),
                done: None,
            })
            .is_err()
        {
            self.pending_jobs.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Queues an opportunistic read-through store unless the writer backlog is
    /// already over its bounds. Dropping is safe: the in-memory tiers were
    /// updated by the caller, so only a later cold start or eviction pays an
    /// extra network fetch. Coherence-critical work must use `enqueue` /
    /// `enqueue_and_wait`, which never drop.
    fn enqueue_store_behind(&self, write: impl FnOnce(&PersistentCache) + Send + 'static) {
        if self.pending_jobs.load(Ordering::Relaxed) > self.store_job_limit {
            return;
        }
        self.enqueue(write);
    }

    async fn enqueue_and_wait(&self, write: impl FnOnce(&PersistentCache) + Send + 'static) {
        let (complete, completed) = tokio::sync::oneshot::channel();
        self.pending_jobs.fetch_add(1, Ordering::Relaxed);
        if self
            .writer
            .send(PersistentWrite {
                job: Box::new(write),
                done: Some(complete),
            })
            .is_ok()
        {
            let _ = completed.await;
        } else {
            self.pending_jobs.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl PersistentCache {
    pub fn open(
        root: impl AsRef<Path>,
        namespace: CacheNamespace,
        maximum_payload_bytes: u64,
    ) -> Result<Self, CacheError> {
        namespace.validate()?;
        if maximum_payload_bytes == 0 {
            return Err(CacheError::InvalidSizeLimit);
        }

        let root = root.as_ref();
        ensure_private_directory(root)?;
        let namespaces = root.join("namespaces");
        ensure_private_directory(&namespaces)?;
        let directory = namespaces.join(namespace.directory_name()?);
        ensure_private_directory(&directory)?;
        let lock_file = acquire_namespace_lock(&directory.join(LOCK_FILE))?;
        let state = load_manifest(&directory, &namespace)?;
        let cache = Self {
            namespace,
            directory,
            maximum_payload_bytes,
            state: Mutex::new(state),
            deferred: Mutex::new(DeferredCommits::default()),
            manifest_writes: AtomicU64::new(0),
            _lock_file: lock_file,
        };
        cache.enforce_budget()?;
        cache.remove_orphaned_files()?;
        Ok(cache)
    }

    pub fn namespace(&self) -> &CacheNamespace {
        &self.namespace
    }

    pub fn stats(&self) -> Result<CacheStats, CacheError> {
        let state = self.lock_state()?;
        Ok(CacheStats {
            entries: state.entries.len(),
            payload_bytes: state.payload_bytes,
            maximum_payload_bytes: self.maximum_payload_bytes,
        })
    }

    pub fn get_metadata_value(&self, node: NodeId) -> Result<Option<Metadata>, CacheError> {
        let key = EntryKey::Metadata(node);
        let bytes = match self.read_key(&key)? {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        let value: Metadata = self.decode_value(&key, &bytes)?;
        if value.node != node {
            self.remove_corrupt_key(&key)?;
            return Err(CacheError::CorruptEntry);
        }
        Ok(Some(value))
    }

    pub fn insert_metadata_value(&self, value: Metadata) -> Result<(), CacheError> {
        let key = EntryKey::Metadata(value.node);
        let bytes = serde_json::to_vec(&value)?;
        self.insert_entry(key, &bytes)
    }

    pub fn get_directory_value(
        &self,
        key: RevisionKey,
    ) -> Result<Option<Vec<DirectoryEntry>>, CacheError> {
        let entry_key = EntryKey::Directory(key);
        let bytes = match self.read_key(&entry_key)? {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        self.decode_value(&entry_key, &bytes).map(Some)
    }

    pub fn insert_directory_value(
        &self,
        key: RevisionKey,
        value: &[DirectoryEntry],
    ) -> Result<(), CacheError> {
        let bytes = serde_json::to_vec(value)?;
        self.insert_entry(EntryKey::Directory(key), &bytes)
    }

    pub fn insert_directory_snapshot_value(
        &self,
        key: RevisionKey,
        value: &[DirectoryEntry],
    ) -> Result<(), CacheError> {
        let mut entries = Vec::with_capacity(value.len().saturating_add(1));
        entries.push((EntryKey::Directory(key), serde_json::to_vec(value)?));
        for entry in value {
            entries.push((
                EntryKey::Metadata(entry.node),
                serde_json::to_vec(&entry.metadata)?,
            ));
        }
        self.insert_entries(entries)
    }

    pub fn get_range_value(&self, key: RangeKey) -> Result<Option<Vec<u8>>, CacheError> {
        if key.offset.checked_add(key.length).is_none() {
            return Err(CacheError::InvalidRange);
        }
        if key.length == 0 {
            return Ok(Some(Vec::new()));
        }
        if key.length > self.maximum_payload_bytes {
            return Ok(None);
        }

        let mut state = self.lock_state()?;
        let candidates: Vec<(EntryKey, DiskEntry)> = state
            .entries
            .iter()
            .filter_map(|(entry_key, entry)| match entry_key {
                EntryKey::Range(candidate)
                    if candidate.file == key.file && ranges_overlap(*candidate, key) =>
                {
                    Some((entry_key.clone(), entry.clone()))
                }
                _ => None,
            })
            .collect();

        let mut segments = Vec::with_capacity(candidates.len());
        for (entry_key, entry) in candidates {
            let data = match self.read_disk_entry(&entry) {
                Ok(data) => data,
                Err(CacheError::CorruptEntry) => {
                    self.remove_keys_locked(&mut state, &[entry_key])?;
                    return Err(CacheError::CorruptEntry);
                }
                Err(error) => return Err(error),
            };
            let EntryKey::Range(range) = entry_key.clone() else {
                return Err(CacheError::InvalidManifest);
            };
            if !valid_range_payload(range, &data) {
                self.remove_keys_locked(&mut state, &[entry_key])?;
                return Err(CacheError::CorruptEntry);
            }
            segments.push(Segment {
                key: entry_key,
                offset: range.offset,
                data,
            });
        }

        let Some(assembled) = assemble_range(key, segments) else {
            return Ok(None);
        };
        touch_entries(&mut state, &assembled.used_keys);
        Ok(Some(assembled.data))
    }

    fn get_covering_range_value(
        &self,
        key: RangeKey,
    ) -> Result<Option<(RangeKey, Vec<u8>)>, CacheError> {
        let request_end = key
            .offset
            .checked_add(key.length)
            .ok_or(CacheError::InvalidRange)?;
        if key.length == 0 {
            return Ok(Some((key, Vec::new())));
        }
        if key.length > self.maximum_payload_bytes {
            return Ok(None);
        }
        let mut state = self.lock_state()?;
        let candidate = state
            .entries
            .iter()
            .filter_map(|(entry_key, entry)| {
                let EntryKey::Range(range) = entry_key else {
                    return None;
                };
                let range_end = range.offset.checked_add(range.length)?;
                (range.file == key.file && range.offset <= key.offset && range_end >= request_end)
                    .then_some((*range, entry_key.clone(), entry.clone()))
            })
            .min_by_key(|(range, _, _)| range.length);
        let Some((range, entry_key, entry)) = candidate else {
            return Ok(None);
        };
        let data = match self.read_disk_entry(&entry) {
            Ok(data) if valid_range_payload(range, &data) => data,
            Ok(_) | Err(CacheError::CorruptEntry) => {
                self.remove_keys_locked(&mut state, std::slice::from_ref(&entry_key))?;
                return Err(CacheError::CorruptEntry);
            }
            Err(error) => return Err(error),
        };
        touch_entries(&mut state, std::slice::from_ref(&entry_key));
        Ok(Some((range, data)))
    }

    pub fn insert_range_value(&self, key: RangeKey, value: &[u8]) -> Result<(), CacheError> {
        if !valid_range_payload(key, value) {
            return Err(CacheError::InvalidRange);
        }
        if value.is_empty() {
            return Ok(());
        }
        self.insert_entry(EntryKey::Range(key), value)
    }

    pub fn get_filesystem_stats_value(&self) -> Result<Option<FilesystemStats>, CacheError> {
        let key = EntryKey::FilesystemStats;
        let bytes = match self.read_key(&key)? {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        self.decode_value(&key, &bytes).map(Some)
    }

    pub fn insert_filesystem_stats_value(&self, value: FilesystemStats) -> Result<(), CacheError> {
        self.insert_entry(EntryKey::FilesystemStats, &serde_json::to_vec(&value)?)
    }

    pub fn invalidate_node(&self, node: NodeId) -> Result<(), CacheError> {
        let mut state = self.lock_state()?;
        let mut removed = state
            .entries
            .keys()
            .filter(|key| key.node() == Some(node))
            .cloned()
            .collect::<HashSet<_>>();
        let candidates = state
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                (matches!(key, EntryKey::Directory(_)) && !removed.contains(key))
                    .then_some((key.clone(), entry.clone()))
            })
            .collect::<Vec<_>>();
        for (key, entry) in candidates {
            let entries = match self.read_disk_entry(&entry) {
                Ok(bytes) => serde_json::from_slice::<Vec<DirectoryEntry>>(&bytes).ok(),
                Err(CacheError::CorruptEntry) => None,
                Err(error) => return Err(error),
            };
            if entries
                .as_ref()
                .is_none_or(|entries| entries.iter().any(|entry| entry.node == node))
            {
                removed.insert(key);
            }
        }
        self.remove_keys_locked(&mut state, &removed.into_iter().collect::<Vec<_>>())
    }

    fn invalidate_metadata(&self, node: NodeId) -> Result<(), CacheError> {
        self.remove_matching(
            |key| matches!(key, EntryKey::Metadata(candidate) if *candidate == node),
        )
    }

    fn invalidate_directories(&self, node: NodeId) -> Result<(), CacheError> {
        self.remove_matching(
            |key| matches!(key, EntryKey::Directory(candidate) if candidate.node == node),
        )
    }

    fn invalidate_directories_containing(&self, node: NodeId) -> Result<(), CacheError> {
        let mut state = self.lock_state()?;
        let candidates = state
            .entries
            .iter()
            .filter_map(|(key, entry)| {
                matches!(key, EntryKey::Directory(_)).then_some((key.clone(), entry.clone()))
            })
            .collect::<Vec<_>>();
        let mut removed = Vec::new();
        for (key, entry) in candidates {
            let entries = match self.read_disk_entry(&entry) {
                Ok(bytes) => serde_json::from_slice::<Vec<DirectoryEntry>>(&bytes).ok(),
                Err(CacheError::CorruptEntry) => None,
                Err(error) => return Err(error),
            };
            if entries
                .as_ref()
                .is_none_or(|entries| entries.iter().any(|entry| entry.node == node))
            {
                removed.push(key);
            }
        }
        self.remove_keys_locked(&mut state, &removed)
    }

    fn invalidate_ranges(&self, node: NodeId) -> Result<(), CacheError> {
        self.remove_matching(
            |key| matches!(key, EntryKey::Range(candidate) if candidate.file.node == node),
        )
    }

    fn read_key(&self, key: &EntryKey) -> Result<Option<Vec<u8>>, CacheError> {
        let mut state = self.lock_state()?;
        let Some(entry) = state.entries.get(key).cloned() else {
            return Ok(None);
        };
        match self.read_disk_entry(&entry) {
            Ok(bytes) => {
                touch_entries(&mut state, std::slice::from_ref(key));
                Ok(Some(bytes))
            }
            Err(CacheError::CorruptEntry) => {
                self.remove_keys_locked(&mut state, std::slice::from_ref(key))?;
                Err(CacheError::CorruptEntry)
            }
            Err(error) => Err(error),
        }
    }

    fn decode_value<T: for<'de> Deserialize<'de>>(
        &self,
        key: &EntryKey,
        bytes: &[u8],
    ) -> Result<T, CacheError> {
        match serde_json::from_slice(bytes) {
            Ok(value) => Ok(value),
            Err(_) => {
                self.remove_corrupt_key(key)?;
                Err(CacheError::CorruptEntry)
            }
        }
    }

    fn remove_corrupt_key(&self, key: &EntryKey) -> Result<(), CacheError> {
        let mut state = self.lock_state()?;
        self.remove_keys_locked(&mut state, std::slice::from_ref(key))
    }

    fn insert_entry(&self, key: EntryKey, bytes: &[u8]) -> Result<(), CacheError> {
        self.insert_entries(vec![(key, bytes.to_vec())])
    }

    fn insert_entries(&self, entries: Vec<(EntryKey, Vec<u8>)>) -> Result<(), CacheError> {
        // Phase 1, no lock held: validate sizes and hash payloads. Hashing a
        // multi-megabyte block is the expensive part of a store and used to
        // run under the state mutex, starving every concurrent cache read.
        struct PreparedEntry {
            key: EntryKey,
            bytes: Vec<u8>,
            file_name: String,
            sha256: [u8; 32],
            stored_length: u64,
        }
        let mut prepared = Vec::new();
        for (key, bytes) in entries {
            let stored_length =
                u64::try_from(bytes.len()).map_err(|_| CacheError::EntryTooLarge)?;
            if stored_length > self.maximum_payload_bytes {
                return Err(CacheError::EntryTooLarge);
            }
            let key_bytes = serde_json::to_vec(&key)?;
            let mut hasher = Sha256::new();
            hasher.update(&key_bytes);
            hasher.update(&bytes);
            let file_name = format!("{}.bin", hex::encode(hasher.finalize()));
            let sha256: [u8; 32] = Sha256::digest(&bytes).into();
            prepared.push(PreparedEntry {
                key,
                bytes,
                file_name,
                sha256,
                stored_length,
            });
        }

        // Phase 2, brief lock: drop stores whose manifest entry is already
        // current. This is a manifest-only comparison; the old disk-content
        // verification re-hashed the stored payload under the lock, and a
        // corrupt file is self-healed on read anyway (the entry is removed and
        // the next store rewrites it).
        {
            let state = self.lock_state()?;
            prepared.retain(|entry| {
                !state.entries.get(&entry.key).is_some_and(|existing| {
                    existing.file_name == entry.file_name
                        && existing.stored_length == entry.stored_length
                        && existing.sha256 == entry.sha256
                })
            });
        }
        if prepared.is_empty() {
            return Ok(());
        }

        // Phase 3, no lock held: write the payload files. Names are
        // content-addressed (hash of key + payload), so concurrent writers of
        // the same content converge on identical bytes and the atomic rename
        // makes the last one win harmlessly.
        let mut written = Vec::new();
        for entry in &prepared {
            match write_private_atomic(&self.directory.join(&entry.file_name), &entry.bytes) {
                Ok(()) => written.push(entry.file_name.clone()),
                Err(error) => {
                    let state = self.lock_state()?;
                    let _ = self.remove_unreferenced_files(&state, written);
                    return Err(error);
                }
            }
        }

        // Phase 4: take the lock once to splice the new entries in.
        let mut state = self.lock_state()?;
        let result = (|| -> Result<(), CacheError> {
            let mut proposed = state.clone();
            for entry in prepared {
                let last_access = next_clock(&mut proposed);
                if let Some(previous) = proposed.entries.remove(&entry.key) {
                    proposed.payload_bytes = proposed
                        .payload_bytes
                        .checked_sub(previous.stored_length)
                        .ok_or(CacheError::InvalidManifest)?;
                }
                proposed.payload_bytes = proposed
                    .payload_bytes
                    .checked_add(entry.stored_length)
                    .ok_or(CacheError::EntryTooLarge)?;
                proposed.entries.insert(
                    entry.key.clone(),
                    DiskEntry {
                        key: entry.key,
                        file_name: entry.file_name,
                        stored_length: entry.stored_length,
                        sha256: entry.sha256,
                        last_access,
                    },
                );
            }
            if proposed.entries.len() > MAX_MANIFEST_ENTRIES {
                return Err(CacheError::InvalidManifest);
            }
            evict_to_budget(&mut proposed, self.maximum_payload_bytes)?;
            self.commit_state(&mut state, proposed)
        })();

        // Written files that did not end up referenced (eviction, error) are
        // removed; on error the removal itself is best-effort.
        let cleanup = self.remove_unreferenced_files(&state, written);
        result.and(cleanup)
    }

    fn remove_matching(&self, predicate: impl Fn(&EntryKey) -> bool) -> Result<(), CacheError> {
        let mut state = self.lock_state()?;
        let keys: Vec<_> = state
            .entries
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect();
        self.remove_keys_locked(&mut state, &keys)
    }

    fn remove_keys_locked(
        &self,
        state: &mut PersistentState,
        keys: &[EntryKey],
    ) -> Result<(), CacheError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut proposed = state.clone();
        for key in keys {
            if let Some(entry) = proposed.entries.remove(key) {
                proposed.payload_bytes = proposed
                    .payload_bytes
                    .checked_sub(entry.stored_length)
                    .ok_or(CacheError::InvalidManifest)?;
            }
        }
        self.commit_state(state, proposed)
    }

    fn enforce_budget(&self) -> Result<(), CacheError> {
        let mut state = self.lock_state()?;
        if state.payload_bytes <= self.maximum_payload_bytes {
            return Ok(());
        }
        let mut proposed = state.clone();
        evict_to_budget(&mut proposed, self.maximum_payload_bytes)?;
        self.commit_state(&mut state, proposed)
    }

    fn commit_state(
        &self,
        state: &mut PersistentState,
        proposed: PersistentState,
    ) -> Result<(), CacheError> {
        let retained: HashSet<_> = proposed
            .entries
            .values()
            .map(|entry| entry.file_name.as_str())
            .collect();
        let obsolete: Vec<_> = state
            .entries
            .values()
            .filter(|entry| !retained.contains(entry.file_name.as_str()))
            .map(|entry| entry.file_name.clone())
            .collect();
        {
            let mut deferred = self
                .deferred
                .lock()
                .map_err(|_| CacheError::StateUnavailable)?;
            if deferred.active {
                // Inside a writer batch: apply in memory now, persist once at
                // the batch flush. Obsolete payload files are also deferred so
                // the manifest-before-removal crash ordering is preserved.
                *state = proposed;
                deferred.dirty = true;
                deferred.pending_removals.extend(obsolete);
                return Ok(());
            }
        }
        self.write_manifest(&proposed)?;
        *state = proposed;
        let pending = {
            let mut deferred = self
                .deferred
                .lock()
                .map_err(|_| CacheError::StateUnavailable)?;
            deferred.dirty = false;
            std::mem::take(&mut deferred.pending_removals)
        };
        self.remove_unreferenced_files(state, pending.into_iter().chain(obsolete))?;
        sync_directory(&self.directory)?;
        Ok(())
    }

    /// Removes payload files that are no longer referenced by the current
    /// state. The referenced check protects deferred removals: a name queued
    /// for deletion in one batch can be re-created by a later content-addressed
    /// insert, and deleting it then would orphan a live manifest entry.
    fn remove_unreferenced_files(
        &self,
        state: &PersistentState,
        file_names: impl IntoIterator<Item = String>,
    ) -> Result<(), CacheError> {
        for file_name in file_names {
            if !state
                .entries
                .values()
                .any(|entry| entry.file_name == file_name)
            {
                remove_cache_file(&self.directory.join(file_name))?;
            }
        }
        Ok(())
    }

    /// Enters group-commit mode: subsequent `commit_state` calls apply their
    /// mutations in memory and defer the manifest write until
    /// [`Self::flush_deferred_commits`]. Only the writer thread calls this,
    /// bracketing one batch of queued jobs.
    fn begin_deferred_commits(&self) {
        if let Ok(mut deferred) = self.deferred.lock() {
            deferred.active = true;
        }
    }

    /// Leaves group-commit mode, writing the manifest once if any deferred
    /// commit dirtied it and then removing the batch's obsolete payload files.
    /// On a manifest write error the dirty flag and pending removals are kept,
    /// so the next commit or flush retries them.
    fn flush_deferred_commits(&self) -> Result<(), CacheError> {
        let state = self.lock_state()?;
        let mut deferred = self
            .deferred
            .lock()
            .map_err(|_| CacheError::StateUnavailable)?;
        deferred.active = false;
        if !deferred.dirty {
            return Ok(());
        }
        self.write_manifest(&state)?;
        deferred.dirty = false;
        let pending = std::mem::take(&mut deferred.pending_removals);
        drop(deferred);
        self.remove_unreferenced_files(&state, pending)?;
        sync_directory(&self.directory)?;
        Ok(())
    }

    /// Total manifest files written since this cache was opened.
    pub fn manifest_writes(&self) -> u64 {
        self.manifest_writes.load(Ordering::Relaxed)
    }

    fn write_manifest(&self, state: &PersistentState) -> Result<(), CacheError> {
        let mut entries: Vec<_> = state.entries.values().cloned().collect();
        entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));
        let manifest = Manifest {
            format_version: CACHE_FORMAT_VERSION,
            namespace: self.namespace.clone(),
            clock: state.clock,
            entries,
        };
        let bytes = serde_json::to_vec(&manifest)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_SIZE {
            return Err(CacheError::InvalidManifest);
        }
        write_private_atomic(&self.directory.join(MANIFEST_FILE), &bytes)?;
        self.manifest_writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn read_disk_entry(&self, entry: &DiskEntry) -> Result<Vec<u8>, CacheError> {
        if !valid_cache_file_name(&entry.file_name) {
            return Err(CacheError::InvalidManifest);
        }
        let path = self.directory.join(&entry.file_name);
        let file = match open_private_file(&path) {
            Ok(file) => file,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(CacheError::CorruptEntry);
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if metadata.len() != entry.stored_length || entry.stored_length > self.maximum_payload_bytes
        {
            return Err(CacheError::CorruptEntry);
        }
        let capacity =
            usize::try_from(entry.stored_length).map_err(|_| CacheError::CorruptEntry)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(entry.stored_length.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.stored_length
            || <[u8; 32]>::from(Sha256::digest(&bytes)) != entry.sha256
        {
            return Err(CacheError::CorruptEntry);
        }
        Ok(bytes)
    }

    fn remove_orphaned_files(&self) -> Result<(), CacheError> {
        let state = self.lock_state()?;
        let referenced: HashSet<_> = state
            .entries
            .values()
            .map(|entry| entry.file_name.as_str())
            .collect();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let temporary = name.starts_with(".tmp-");
            let orphaned_payload = valid_cache_file_name(name) && !referenced.contains(name);
            if temporary || orphaned_payload {
                remove_cache_file(&entry.path())?;
            }
        }
        sync_directory(&self.directory)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, PersistentState>, CacheError> {
        self.state.lock().map_err(|_| CacheError::StateUnavailable)
    }
}

#[async_trait]
impl MetadataCache for PersistentCache {
    async fn get(&self, node: NodeId) -> Option<Metadata> {
        self.get_metadata_value(node).ok().flatten()
    }

    async fn insert(&self, value: Metadata) {
        let _ = self.insert_metadata_value(value);
    }

    async fn invalidate(&self, node: NodeId) {
        let _ = self.invalidate_metadata(node);
    }
}

#[async_trait]
impl DirectoryCache for PersistentCache {
    async fn get(&self, key: RevisionKey) -> Option<Vec<DirectoryEntry>> {
        self.get_directory_value(key).ok().flatten()
    }

    async fn insert(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        let _ = self.insert_directory_value(key, &value);
    }

    async fn store_readthrough_snapshot(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        let _ = self.insert_directory_snapshot_value(key, &value);
    }

    async fn invalidate(&self, node: NodeId) {
        let _ = self.invalidate_directories(node);
    }

    async fn invalidate_containing(&self, node: NodeId) {
        let _ = self.invalidate_directories_containing(node);
    }
}

#[async_trait]
impl RangeCache for PersistentCache {
    async fn get(&self, key: RangeKey) -> Option<Vec<u8>> {
        self.get_range_value(key).ok().flatten()
    }

    async fn insert(&self, key: RangeKey, value: Vec<u8>) {
        let _ = self.insert_range_value(key, &value);
    }

    async fn invalidate(&self, node: NodeId) {
        let _ = self.invalidate_ranges(node);
    }
}

#[async_trait]
impl FilesystemStateCache for PersistentCache {
    async fn get_filesystem_stats(&self) -> Option<FilesystemStats> {
        self.get_filesystem_stats_value().ok().flatten()
    }

    async fn insert_filesystem_stats(&self, value: FilesystemStats) {
        let _ = self.insert_filesystem_stats_value(value);
    }
}

#[async_trait]
impl NodeCacheInvalidation for PersistentCache {
    async fn invalidate_node_state(&self, node: NodeId) {
        let _ = self.invalidate_node(node);
    }
}

#[async_trait]
impl MetadataCache for NonBlockingPersistentCache {
    async fn get(&self, node: NodeId) -> Option<Metadata> {
        if let Some(value) = MetadataCache::get(self.memory.as_ref(), node).await {
            return Some(value);
        }
        let cache = Arc::clone(&self.inner);
        let value =
            tokio::task::spawn_blocking(move || cache.get_metadata_value(node).ok().flatten())
                .await
                .ok()
                .flatten();
        if let Some(value) = &value {
            MetadataCache::insert(self.memory.as_ref(), value.clone()).await;
        }
        value
    }

    async fn insert(&self, value: Metadata) {
        MetadataCache::insert(self.memory.as_ref(), value.clone()).await;
        self.enqueue_and_wait(move |cache| {
            let _ = cache.insert_metadata_value(value);
        })
        .await;
    }

    async fn store_readthrough(&self, value: Metadata) {
        MetadataCache::insert(self.memory.as_ref(), value.clone()).await;
        self.enqueue_store_behind(move |cache| {
            let _ = cache.insert_metadata_value(value);
        });
    }

    async fn invalidate(&self, node: NodeId) {
        MetadataCache::invalidate(self.memory.as_ref(), node).await;
        self.enqueue_and_wait(move |cache| {
            let _ = cache.invalidate_metadata(node);
        })
        .await;
    }
}

#[async_trait]
impl DirectoryCache for NonBlockingPersistentCache {
    async fn get(&self, key: RevisionKey) -> Option<Vec<DirectoryEntry>> {
        if let Some(value) = DirectoryCache::get(self.memory.as_ref(), key).await {
            return Some(value);
        }
        let cache = Arc::clone(&self.inner);
        let value =
            tokio::task::spawn_blocking(move || cache.get_directory_value(key).ok().flatten())
                .await
                .ok()
                .flatten();
        if let Some(value) = &value {
            DirectoryCache::store_readthrough_snapshot(self.memory.as_ref(), key, value.clone())
                .await;
        }
        value
    }

    async fn insert(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        DirectoryCache::insert(self.memory.as_ref(), key, value.clone()).await;
        self.enqueue_and_wait(move |cache| {
            let _ = cache.insert_directory_value(key, &value);
        })
        .await;
    }

    async fn store_readthrough_snapshot(&self, key: RevisionKey, value: Vec<DirectoryEntry>) {
        DirectoryCache::store_readthrough_snapshot(self.memory.as_ref(), key, value.clone()).await;
        self.enqueue_store_behind(move |cache| {
            let _ = cache.insert_directory_snapshot_value(key, &value);
        });
    }

    async fn invalidate(&self, node: NodeId) {
        DirectoryCache::invalidate(self.memory.as_ref(), node).await;
        self.enqueue_and_wait(move |cache| {
            let _ = cache.invalidate_directories(node);
        })
        .await;
    }

    async fn invalidate_containing(&self, node: NodeId) {
        DirectoryCache::invalidate_containing(self.memory.as_ref(), node).await;
        self.enqueue_and_wait(move |cache| {
            let _ = cache.invalidate_directories_containing(node);
        })
        .await;
    }
}

#[async_trait]
impl RangeCache for NonBlockingPersistentCache {
    async fn get(&self, key: RangeKey) -> Option<Vec<u8>> {
        if let Some(value) = self.hot_ranges.get(key) {
            return Some(value);
        }
        let cache = Arc::clone(&self.inner);
        let loaded =
            tokio::task::spawn_blocking(move || match cache.get_covering_range_value(key) {
                Ok(Some(value)) => Some(value),
                Ok(None) => cache
                    .get_range_value(key)
                    .ok()
                    .flatten()
                    .map(|value| (key, value)),
                Err(_) => None,
            })
            .await
            .ok()
            .flatten();
        let (loaded_key, value) = loaded?;
        self.hot_ranges.insert(loaded_key, value);
        self.hot_ranges.get(key)
    }

    async fn insert(&self, key: RangeKey, value: Vec<u8>) {
        self.hot_ranges.insert(key, value.clone());
        self.enqueue_and_wait(move |cache| {
            let _ = cache.insert_range_value(key, &value);
        })
        .await;
    }

    async fn store_readthrough(&self, key: RangeKey, value: Vec<u8>) {
        self.hot_ranges.insert(key, value.clone());
        // Range payloads dominate the writer queue's memory, so they are
        // additionally bounded by bytes: sequential read-ahead can otherwise
        // queue hundreds of megabytes of block copies faster than the disk
        // absorbs them.
        let bytes = u64::try_from(value.len()).unwrap_or(u64::MAX);
        if self.pending_jobs.load(Ordering::Relaxed) > self.store_job_limit
            || self
                .pending_store_bytes
                .load(Ordering::Relaxed)
                .saturating_add(bytes)
                > self.store_byte_limit
        {
            return;
        }
        self.pending_store_bytes.fetch_add(bytes, Ordering::Relaxed);
        let pending_store_bytes = Arc::clone(&self.pending_store_bytes);
        self.enqueue(move |cache| {
            let _ = cache.insert_range_value(key, &value);
            pending_store_bytes.fetch_sub(bytes, Ordering::Relaxed);
        });
    }

    async fn invalidate(&self, node: NodeId) {
        self.hot_ranges.invalidate(node);
        self.enqueue_and_wait(move |cache| {
            let _ = cache.invalidate_ranges(node);
        })
        .await;
    }
}

#[async_trait]
impl FilesystemStateCache for NonBlockingPersistentCache {
    async fn get_filesystem_stats(&self) -> Option<FilesystemStats> {
        if let Some(value) = self.memory.get_filesystem_stats().await {
            return Some(value);
        }
        let cache = Arc::clone(&self.inner);
        let value =
            tokio::task::spawn_blocking(move || cache.get_filesystem_stats_value().ok().flatten())
                .await
                .ok()
                .flatten();
        if let Some(value) = value {
            self.memory.insert_filesystem_stats(value).await;
        }
        value
    }

    async fn insert_filesystem_stats(&self, value: FilesystemStats) {
        self.memory.insert_filesystem_stats(value).await;
        self.enqueue_and_wait(move |cache| {
            let _ = cache.insert_filesystem_stats_value(value);
        })
        .await;
    }

    async fn store_readthrough_filesystem_stats(&self, value: FilesystemStats) {
        self.memory.insert_filesystem_stats(value).await;
        self.enqueue_store_behind(move |cache| {
            let _ = cache.insert_filesystem_stats_value(value);
        });
    }
}

#[async_trait]
impl NodeCacheInvalidation for NonBlockingPersistentCache {
    async fn invalidate_node_state(&self, node: NodeId) {
        // The in-memory and hot-range caches are invalidated synchronously, so
        // online reads after a mutation never observe stale data. The durable
        // manifest eviction only reclaims space and preserves offline coherence
        // for revision-keyed entries, so enqueue it fire-and-forget: awaiting a
        // manifest fsync here made every low-volume mutation pay one durable
        // sync (~30 ms each on APFS). Enqueued work still runs in order on the
        // writer thread. High-frequency callers (streaming writes) must use
        // `invalidate_node_memory` instead to avoid flooding that queue.
        NodeCacheInvalidation::invalidate_node_state(self.memory.as_ref(), node).await;
        self.hot_ranges.invalidate(node);
        self.enqueue(move |cache| {
            let _ = cache.invalidate_node(node);
        });
    }

    async fn invalidate_node_memory(&self, node: NodeId) {
        NodeCacheInvalidation::invalidate_node_state(self.memory.as_ref(), node).await;
        self.hot_ranges.invalidate(node);
    }
}

struct Segment<K> {
    key: K,
    offset: u64,
    data: Vec<u8>,
}

struct Assembled<K> {
    data: Vec<u8>,
    used_keys: Vec<K>,
}

fn assemble_range<K: Clone>(
    requested: RangeKey,
    mut segments: Vec<Segment<K>>,
) -> Option<Assembled<K>> {
    let requested_end = requested.offset.checked_add(requested.length)?;
    let capacity = usize::try_from(requested.length).ok()?;
    segments.retain(|segment| {
        segment
            .offset
            .checked_add(u64::try_from(segment.data.len()).unwrap_or(u64::MAX))
            .is_some_and(|end| segment.offset < requested_end && end > requested.offset)
    });
    segments.sort_by(|left, right| {
        left.offset
            .cmp(&right.offset)
            .then_with(|| right.data.len().cmp(&left.data.len()))
    });

    let mut cursor = requested.offset;
    let mut data = Vec::with_capacity(capacity);
    let mut used_keys = Vec::new();
    for segment in segments {
        if segment.offset > cursor {
            break;
        }
        let segment_length = u64::try_from(segment.data.len()).ok()?;
        let segment_end = segment.offset.checked_add(segment_length)?;
        if segment_end <= cursor {
            continue;
        }
        let copy_end = segment_end.min(requested_end);
        let source_start = usize::try_from(cursor.checked_sub(segment.offset)?).ok()?;
        let source_end = usize::try_from(copy_end.checked_sub(segment.offset)?).ok()?;
        data.extend_from_slice(segment.data.get(source_start..source_end)?);
        used_keys.push(segment.key);
        cursor = copy_end;
        if cursor == requested_end {
            return Some(Assembled { data, used_keys });
        }
    }
    None
}

fn valid_range_payload(key: RangeKey, value: &[u8]) -> bool {
    key.offset.checked_add(key.length).is_some()
        && usize::try_from(key.length).ok() == Some(value.len())
}

fn ranges_overlap(left: RangeKey, right: RangeKey) -> bool {
    match (
        left.offset.checked_add(left.length),
        right.offset.checked_add(right.length),
    ) {
        (Some(left_end), Some(right_end)) => left.offset < right_end && right.offset < left_end,
        _ => false,
    }
}

fn next_clock(state: &mut PersistentState) -> u64 {
    state.clock = state.clock.saturating_add(1);
    state.clock
}

fn touch_entries(state: &mut PersistentState, keys: &[EntryKey]) {
    for key in keys {
        let tick = next_clock(state);
        if let Some(entry) = state.entries.get_mut(key) {
            entry.last_access = tick;
        }
    }
}

fn evict_to_budget(state: &mut PersistentState, maximum: u64) -> Result<(), CacheError> {
    while state.payload_bytes > maximum {
        let key = state
            .entries
            .iter()
            .min_by(|left, right| {
                left.1
                    .last_access
                    .cmp(&right.1.last_access)
                    .then_with(|| left.1.file_name.cmp(&right.1.file_name))
            })
            .map(|(key, _)| key.clone())
            .ok_or(CacheError::InvalidManifest)?;
        let entry = state
            .entries
            .remove(&key)
            .ok_or(CacheError::InvalidManifest)?;
        state.payload_bytes = state
            .payload_bytes
            .checked_sub(entry.stored_length)
            .ok_or(CacheError::InvalidManifest)?;
    }
    Ok(())
}

fn load_manifest(
    directory: &Path,
    namespace: &CacheNamespace,
) -> Result<PersistentState, CacheError> {
    let path = directory.join(MANIFEST_FILE);
    let file = match open_private_file(&path) {
        Ok(file) => file,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistentState::default());
        }
        Err(error) => return Err(error),
    };
    let declared = file.metadata()?.len();
    if declared > MAX_MANIFEST_SIZE {
        return Err(CacheError::InvalidManifest);
    }
    let capacity = usize::try_from(declared).map_err(|_| CacheError::InvalidManifest)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_MANIFEST_SIZE.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_SIZE {
        return Err(CacheError::InvalidManifest);
    }
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| CacheError::InvalidManifest)?;
    if manifest.format_version != CACHE_FORMAT_VERSION
        || manifest.entries.len() > MAX_MANIFEST_ENTRIES
    {
        return Err(CacheError::InvalidManifest);
    }
    if manifest.namespace != *namespace {
        return Err(CacheError::NamespaceMismatch);
    }

    let mut entries = HashMap::with_capacity(manifest.entries.len());
    let mut payload_bytes = 0_u64;
    for entry in manifest.entries {
        if !valid_cache_file_name(&entry.file_name) {
            return Err(CacheError::InvalidManifest);
        }
        payload_bytes = payload_bytes
            .checked_add(entry.stored_length)
            .ok_or(CacheError::InvalidManifest)?;
        let key = entry.key.clone();
        if entries.insert(key, entry).is_some() {
            return Err(CacheError::InvalidManifest);
        }
    }
    Ok(PersistentState {
        clock: manifest.clock,
        payload_bytes,
        entries,
    })
}

fn valid_cache_file_name(name: &str) -> bool {
    name.len() == 68
        && name.ends_with(".bin")
        && name[..64].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ensure_private_directory(path: &Path) -> Result<(), CacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory_metadata(&metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_private_directory(path) {
                Ok(()) => {
                    set_private_directory_permissions(path)?;
                    validate_private_directory_metadata(&fs::symlink_metadata(path)?)
                }
                Err(create_error) if create_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    validate_private_directory_metadata(&fs::symlink_metadata(path)?)
                }
                Err(create_error) => Err(CacheError::Io(create_error)),
            }
        }
        Err(error) => Err(CacheError::Io(error)),
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

fn validate_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), CacheError> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CacheError::UnsafeStorage);
    }
    validate_private_ownership_and_mode(metadata)
}

fn validate_private_file_metadata(metadata: &fs::Metadata) -> Result<(), CacheError> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(CacheError::UnsafeStorage);
    }
    validate_private_ownership_and_mode(metadata)
}

#[cfg(unix)]
fn validate_private_ownership_and_mode(metadata: &fs::Metadata) -> Result<(), CacheError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        Err(CacheError::UnsafeStorage)
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn validate_private_ownership_and_mode(_metadata: &fs::Metadata) -> Result<(), CacheError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), CacheError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), CacheError> {
    Ok(())
}

fn acquire_namespace_lock(path: &Path) -> Result<File, CacheError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        validate_private_file_metadata(&metadata)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    validate_private_file_metadata(&file.metadata()?)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| CacheError::CacheInUse)?;
    Ok(file)
}

fn open_private_file(path: &Path) -> Result<File, CacheError> {
    let before = fs::symlink_metadata(path)?;
    validate_private_file_metadata(&before)?;
    let file = File::open(path)?;
    let after = file.metadata()?;
    validate_private_file_metadata(&after)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(CacheError::UnsafeStorage);
        }
    }
    Ok(file)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let directory = path.parent().ok_or(CacheError::UnsafeStorage)?;
    let mut last_collision = None;
    for _ in 0..32 {
        let sequence = TEMPORARY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(".tmp-{}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(CacheError::Io(error)),
        };
        let result = (|| -> Result<(), CacheError> {
            file.write_all(bytes)?;
            file.sync_all()?;
            validate_private_file_metadata(&file.metadata()?)?;
            drop(file);
            fs::rename(&temporary, path)?;
            sync_directory(directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }
    Err(CacheError::Io(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a cache temporary file",
        )
    })))
}

fn remove_cache_file(path: &Path) -> Result<(), CacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_file_metadata(&metadata)?;
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CacheError::Io(error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CacheError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CacheError> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use quickfs_protocol::{Name, NodeId, NodeKind, ROOT_NODE};
    use uuid::Uuid;

    fn revision(revision: u64) -> RevisionKey {
        RevisionKey {
            node: ROOT_NODE,
            revision,
        }
    }

    fn range(revision: u64, offset: u64, length: u64) -> RangeKey {
        RangeKey {
            file: self::revision(revision),
            offset,
            length,
        }
    }

    fn namespace(identity: u8, scope: &str) -> CacheNamespace {
        CacheNamespace::new([identity; 32], "export-one", scope).unwrap()
    }

    fn open_cache(
        temporary: &tempfile::TempDir,
        namespace: CacheNamespace,
        maximum: u64,
    ) -> PersistentCache {
        PersistentCache::open(temporary.path().join("cache"), namespace, maximum).unwrap()
    }

    fn metadata(node: NodeId, revision: u64) -> Metadata {
        Metadata {
            node,
            kind: NodeKind::File,
            size: 0,
            mode: 0o644,
            allocated_blocks: 0,
            revision,
            accessed_unix_ms: 33,
            modified_unix_ms: 34,
            created_unix_ms: Some(32),
            backup_unix_ms: None,
            link_count: 1,
            device_major: 0,
            device_minor: 0,
        }
    }

    fn directory_entry(node: NodeId, name: &str, revision: u64) -> DirectoryEntry {
        DirectoryEntry {
            node,
            name: name.into(),
            kind: NodeKind::File,
            metadata: metadata(node, revision),
        }
    }

    #[tokio::test]
    async fn memory_cache_assembles_contained_and_unaligned_ranges_by_revision() {
        let cache = MemoryCache::default();
        RangeCache::insert(&cache, range(7, 0, 4), b"abcd".to_vec()).await;
        RangeCache::insert(&cache, range(7, 4, 4), b"efgh".to_vec()).await;

        assert_eq!(
            RangeCache::get(&cache, range(7, 2, 4)).await.unwrap(),
            b"cdef"
        );
        assert_eq!(
            RangeCache::get(&cache, range(7, 1, 2)).await.unwrap(),
            b"bc"
        );
        assert!(RangeCache::get(&cache, range(8, 2, 4)).await.is_none());
        assert!(RangeCache::get(&cache, range(7, 3, 6)).await.is_none());
    }

    #[tokio::test]
    async fn memory_cache_invalidates_every_directory_embedding_changed_child_metadata() {
        let cache = MemoryCache::default();
        let changed = NodeId(Uuid::from_u128(40));
        let unchanged = NodeId(Uuid::from_u128(41));
        let other_directory = NodeId(Uuid::from_u128(42));
        let root_key = revision(12);
        let other_key = RevisionKey {
            node: other_directory,
            revision: 13,
        };
        DirectoryCache::insert(
            &cache,
            root_key,
            vec![directory_entry(changed, "changed", 20)],
        )
        .await;
        DirectoryCache::insert(
            &cache,
            other_key,
            vec![directory_entry(unchanged, "unchanged", 21)],
        )
        .await;

        DirectoryCache::invalidate_containing(&cache, changed).await;

        assert!(DirectoryCache::get(&cache, root_key).await.is_none());
        assert!(DirectoryCache::get(&cache, other_key).await.is_some());
    }

    #[tokio::test]
    async fn nonblocking_persistent_cache_keeps_large_blocks_in_a_bounded_hot_tier() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = NonBlockingPersistentCache::open(
            temporary.path().join("cache"),
            namespace(10, "alice"),
            1_024,
        )
        .unwrap();
        let block = range(7, 0, 16);
        RangeCache::insert(&cache, block, b"0123456789abcdef".to_vec()).await;

        cache.hot_ranges.state.lock().unwrap().entries.clear();
        cache
            .hot_ranges
            .state
            .lock()
            .unwrap()
            .least_recently_used
            .clear();
        cache.hot_ranges.state.lock().unwrap().bytes = 0;

        assert_eq!(
            RangeCache::get(&cache, range(7, 2, 2)).await.unwrap(),
            b"23"
        );
        cache.inner.invalidate_ranges(block.file.node).unwrap();
        assert_eq!(
            RangeCache::get(&cache, range(7, 10, 3)).await.unwrap(),
            b"abc"
        );
    }

    #[test]
    fn hot_range_tier_evicts_least_recently_used_blocks_to_its_byte_budget() {
        let cache = HotRangeCache::new(8);
        cache.insert(range(3, 0, 6), b"abcdef".to_vec());
        cache.insert(range(3, 6, 6), b"ghijkl".to_vec());

        assert!(cache.get(range(3, 0, 1)).is_none());
        assert_eq!(cache.get(range(3, 7, 2)).unwrap(), b"hi");
        assert!(cache.state.lock().unwrap().bytes <= 8);
    }

    #[test]
    fn persistent_ranges_are_revision_isolated_and_assembled() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(1, "alice"), 1_024);
        cache.insert_range_value(range(3, 0, 5), b"abcde").unwrap();
        cache.insert_range_value(range(3, 5, 5), b"fghij").unwrap();
        cache.insert_range_value(range(4, 0, 5), b"VWXYZ").unwrap();

        assert_eq!(
            cache.get_range_value(range(3, 3, 5)).unwrap().unwrap(),
            b"defgh"
        );
        assert_eq!(
            cache.get_range_value(range(4, 1, 3)).unwrap().unwrap(),
            b"WXY"
        );
        assert!(cache.get_range_value(range(5, 0, 1)).unwrap().is_none());
    }

    #[test]
    fn persistent_cache_survives_reopen_with_metadata_and_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_namespace = namespace(2, "alice");
        let filesystem_stats = FilesystemStats {
            blocks: 20,
            blocks_free: 12,
            blocks_available: 10,
            files: 30,
            files_free: 18,
            block_size: 4_096,
            name_length: 255,
            fragment_size: 4_096,
        };
        {
            let cache = open_cache(&temporary, cache_namespace.clone(), 4_096);
            cache
                .insert_range_value(range(9, 11, 6), b"stored")
                .unwrap();
            cache
                .insert_metadata_value(Metadata {
                    node: ROOT_NODE,
                    kind: NodeKind::Directory,
                    size: 0,
                    mode: 0o755,
                    allocated_blocks: 0,
                    revision: 12,
                    accessed_unix_ms: 33,
                    modified_unix_ms: 34,
                    created_unix_ms: Some(32),
                    backup_unix_ms: None,
                    link_count: 2,
                    device_major: 0,
                    device_minor: 0,
                })
                .unwrap();
            cache
                .insert_directory_value(
                    revision(12),
                    &[DirectoryEntry {
                        node: ROOT_NODE,
                        name: "entry".into(),
                        kind: NodeKind::File,
                        metadata: Metadata {
                            node: ROOT_NODE,
                            kind: NodeKind::File,
                            size: 0,
                            mode: 0o644,
                            allocated_blocks: 0,
                            revision: 12,
                            accessed_unix_ms: 33,
                            modified_unix_ms: 34,
                            created_unix_ms: Some(32),
                            backup_unix_ms: None,
                            link_count: 1,
                            device_major: 0,
                            device_minor: 0,
                        },
                    }],
                )
                .unwrap();
            cache
                .insert_filesystem_stats_value(filesystem_stats)
                .unwrap();
        }

        let reopened = open_cache(&temporary, cache_namespace, 4_096);
        assert_eq!(
            reopened.get_range_value(range(9, 12, 4)).unwrap().unwrap(),
            b"tore"
        );
        assert_eq!(
            reopened
                .get_metadata_value(ROOT_NODE)
                .unwrap()
                .unwrap()
                .revision,
            12
        );
        assert_eq!(
            reopened.get_directory_value(revision(12)).unwrap().unwrap()[0].name,
            Name::from("entry")
        );
        assert_eq!(
            reopened.get_filesystem_stats_value().unwrap(),
            Some(filesystem_stats)
        );
    }

    #[test]
    fn identical_persistent_insert_is_a_noop() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(7, "alice"), 1_024);
        let key = range(3, 0, 4);
        cache.insert_range_value(key, b"same").unwrap();
        let clock_after_first_insert = cache.state.lock().unwrap().clock;

        cache.insert_range_value(key, b"same").unwrap();

        assert_eq!(cache.state.lock().unwrap().clock, clock_after_first_insert);
        assert_eq!(cache.stats().unwrap().entries, 1);
        assert_eq!(cache.get_range_value(key).unwrap().unwrap(), b"same");
    }

    #[test]
    fn directory_snapshot_batches_child_metadata_and_survives_reopen() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_namespace = namespace(8, "alice");
        let first = NodeId(Uuid::from_u128(1));
        let second = NodeId(Uuid::from_u128(2));
        let entries = vec![
            DirectoryEntry {
                node: first,
                name: "first".into(),
                kind: NodeKind::File,
                metadata: metadata(first, 20),
            },
            DirectoryEntry {
                node: second,
                name: "second".into(),
                kind: NodeKind::File,
                metadata: metadata(second, 21),
            },
        ];
        {
            let cache = open_cache(&temporary, cache_namespace.clone(), 8_192);
            cache
                .insert_directory_snapshot_value(revision(19), &entries)
                .unwrap();
            assert_eq!(cache.stats().unwrap().entries, 3);
        }

        let reopened = open_cache(&temporary, cache_namespace, 8_192);
        assert_eq!(
            reopened.get_directory_value(revision(19)).unwrap().unwrap(),
            entries
        );
        assert_eq!(
            reopened.get_metadata_value(first).unwrap().unwrap(),
            metadata(first, 20)
        );
        assert_eq!(
            reopened.get_metadata_value(second).unwrap().unwrap(),
            metadata(second, 21)
        );
    }

    #[test]
    fn persistent_node_invalidation_removes_owned_and_embedding_entries_together() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(11, "alice"), 8_192);
        let changed = NodeId(Uuid::from_u128(50));
        let unchanged = NodeId(Uuid::from_u128(51));
        let other_directory = NodeId(Uuid::from_u128(52));
        let root_key = revision(30);
        let other_key = RevisionKey {
            node: other_directory,
            revision: 31,
        };
        cache
            .insert_directory_value(root_key, &[directory_entry(changed, "changed", 32)])
            .unwrap();
        cache
            .insert_directory_value(other_key, &[directory_entry(unchanged, "unchanged", 33)])
            .unwrap();
        cache.insert_metadata_value(metadata(changed, 32)).unwrap();
        let changed_range = RangeKey {
            file: RevisionKey {
                node: changed,
                revision: 32,
            },
            offset: 0,
            length: 4,
        };
        cache.insert_range_value(changed_range, b"data").unwrap();

        cache.invalidate_node(changed).unwrap();

        assert!(cache.get_directory_value(root_key).unwrap().is_none());
        assert!(cache.get_directory_value(other_key).unwrap().is_some());
        assert!(cache.get_metadata_value(changed).unwrap().is_none());
        assert!(cache.get_range_value(changed_range).unwrap().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn readthrough_store_does_not_wait_for_a_busy_persistent_writer() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = NonBlockingPersistentCache::open(
            temporary.path().join("cache"),
            namespace(9, "alice"),
            8_192,
        )
        .unwrap();
        let value = metadata(NodeId(Uuid::from_u128(3)), 22);
        let persistent = Arc::clone(&cache.inner);
        let (locked, wait_until_locked) = mpsc::channel();
        let (release, wait_until_released) = mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _writer_blocker = persistent.state.lock().unwrap();
            locked.send(()).unwrap();
            wait_until_released.recv().unwrap();
        });
        wait_until_locked.recv().unwrap();

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            MetadataCache::store_readthrough(&cache, value.clone()),
        )
        .await
        .unwrap();

        release.send(()).unwrap();
        blocker.join().unwrap();
        MetadataCache::invalidate(&cache, value.node).await;
        assert!(MetadataCache::get(&cache, value.node).await.is_none());
        assert!(
            cache
                .inner
                .get_metadata_value(value.node)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn namespaces_do_not_share_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let alice = open_cache(&temporary, namespace(3, "alice"), 1_024);
        let bob = open_cache(&temporary, namespace(3, "bob"), 1_024);
        alice.insert_range_value(range(1, 0, 5), b"alice").unwrap();
        bob.insert_range_value(range(1, 0, 3), b"bob").unwrap();

        assert_eq!(
            alice.get_range_value(range(1, 0, 5)).unwrap().unwrap(),
            b"alice"
        );
        assert_eq!(
            bob.get_range_value(range(1, 0, 3)).unwrap().unwrap(),
            b"bob"
        );
        assert_ne!(alice.directory, bob.directory);
    }

    #[test]
    fn corrupt_payload_is_rejected_and_removed_from_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(4, "alice"), 1_024);
        let key = range(2, 0, 6);
        cache.insert_range_value(key, b"intact").unwrap();
        let file_name = cache
            .state
            .lock()
            .unwrap()
            .entries
            .get(&EntryKey::Range(key))
            .unwrap()
            .file_name
            .clone();
        fs::write(cache.directory.join(file_name), b"broken").unwrap();

        assert!(matches!(
            cache.get_range_value(key),
            Err(CacheError::CorruptEntry)
        ));
        assert!(cache.get_range_value(key).unwrap().is_none());
        assert_eq!(cache.stats().unwrap().entries, 0);
    }

    #[test]
    fn least_recently_used_payloads_are_evicted_to_the_hard_budget() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(5, "alice"), 8);
        let first = range(1, 0, 4);
        let second = range(1, 4, 4);
        let third = range(1, 8, 4);
        cache.insert_range_value(first, b"aaaa").unwrap();
        cache.insert_range_value(second, b"bbbb").unwrap();
        assert_eq!(cache.get_range_value(first).unwrap().unwrap(), b"aaaa");
        cache.insert_range_value(third, b"cccc").unwrap();

        assert!(cache.get_range_value(second).unwrap().is_none());
        assert_eq!(cache.get_range_value(first).unwrap().unwrap(), b"aaaa");
        assert_eq!(cache.get_range_value(third).unwrap().unwrap(), b"cccc");
        assert_eq!(cache.stats().unwrap().payload_bytes, 8);
    }

    /// A deferred-commit batch must apply every mutation with exactly one
    /// manifest write at the flush, and the result must be durable across a
    /// reopen. This is the group-commit contract that keeps a flood of
    /// read-through stores from serializing the manifest once per store.
    #[test]
    fn deferred_commits_coalesce_manifest_writes() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(7, "batch"), 1_024 * 1_024);
        let baseline = cache.manifest_writes();

        cache.begin_deferred_commits();
        for index in 0..20u64 {
            cache
                .insert_range_value(range(1, index * 8, 8), &[index as u8; 8])
                .unwrap();
        }
        assert_eq!(
            cache.manifest_writes(),
            baseline,
            "manifest must not be written while a batch is active"
        );
        cache.flush_deferred_commits().unwrap();
        assert_eq!(cache.manifest_writes(), baseline + 1);

        drop(cache);
        let reopened = open_cache(&temporary, namespace(7, "batch"), 1_024 * 1_024);
        for index in 0..20u64 {
            assert_eq!(
                reopened
                    .get_range_value(range(1, index * 8, 8))
                    .unwrap()
                    .unwrap(),
                vec![index as u8; 8]
            );
        }
    }

    /// Flushing a batch that made no changes must not touch the manifest, and
    /// commits made outside a batch must keep writing it immediately.
    #[test]
    fn deferred_flush_is_a_no_op_when_clean() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(7, "clean"), 1_024);
        let baseline = cache.manifest_writes();
        cache.begin_deferred_commits();
        cache.flush_deferred_commits().unwrap();
        assert_eq!(cache.manifest_writes(), baseline);

        cache.insert_range_value(range(1, 0, 4), b"data").unwrap();
        assert_eq!(cache.manifest_writes(), baseline + 1);
    }

    /// Replacing an entry inside a batch defers the old payload file's removal
    /// to the flush; afterwards only the live payload remains on disk.
    #[test]
    fn deferred_commit_removes_replaced_payloads_at_flush() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(7, "replace"), 1_024);
        cache.insert_range_value(range(1, 0, 4), b"old!").unwrap();

        cache.begin_deferred_commits();
        cache.insert_range_value(range(1, 0, 4), b"new!").unwrap();
        cache.flush_deferred_commits().unwrap();

        assert_eq!(
            cache.get_range_value(range(1, 0, 4)).unwrap().unwrap(),
            b"new!"
        );
        let payloads = fs::read_dir(&cache.directory)
            .unwrap()
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .filter(|name| name.ends_with(".bin"))
            .count();
        assert_eq!(payloads, 1, "replaced payload must be removed at flush");
    }

    /// Re-storing an identical value must be recognized from the manifest
    /// alone and skip payload and manifest writes entirely.
    #[test]
    fn identical_store_skips_manifest_write() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(7, "dedup"), 1_024);
        cache.insert_range_value(range(1, 0, 4), b"data").unwrap();
        let after_first = cache.manifest_writes();
        cache.insert_range_value(range(1, 0, 4), b"data").unwrap();
        assert_eq!(cache.manifest_writes(), after_first);
    }

    /// When the writer backlog exceeds its byte bound, an opportunistic range
    /// store is dropped: the hot tier still serves it, nothing reaches the
    /// durable cache, and a later waited insert is unaffected and durable once
    /// its wait returns.
    #[tokio::test]
    async fn range_store_readthrough_is_dropped_under_backpressure() {
        let temporary = tempfile::tempdir().unwrap();
        let mut cache = NonBlockingPersistentCache::open(
            temporary.path().join("cache"),
            namespace(7, "pressure"),
            1_024 * 1_024,
        )
        .unwrap();
        cache.set_store_limits(usize::MAX, 0);

        let speculative = range(3, 0, 4);
        RangeCache::store_readthrough(&cache, speculative, b"spec".to_vec()).await;
        assert_eq!(
            RangeCache::get(&cache, speculative).await.unwrap(),
            b"spec",
            "memory tier must keep serving a dropped store"
        );

        let demanded = range(3, 8, 4);
        RangeCache::insert(&cache, demanded, b"real".to_vec()).await;
        // The waited insert drained the queue past any earlier job, so the
        // speculative store's absence below proves it was dropped, not merely
        // still queued.
        assert!(
            cache
                .persistent()
                .get_range_value(speculative)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            cache
                .persistent()
                .get_range_value(demanded)
                .unwrap()
                .unwrap(),
            b"real"
        );
        assert!(
            cache.manifest_writes() >= 1,
            "a waited insert must be durable when its wait returns"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persistent_storage_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let cache = open_cache(&temporary, namespace(6, "alice"), 1_024);
        cache.insert_range_value(range(1, 0, 4), b"data").unwrap();
        assert_eq!(
            fs::metadata(&cache.directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for entry in fs::read_dir(&cache.directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                assert_eq!(
                    entry.metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
}
