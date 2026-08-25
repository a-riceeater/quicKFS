// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use crate::{
    ClientError, CreatedFile, DirectorySnapshot, NetworkFilesystem, OpenedFile, RangeRead,
    RemoteFilesystem, Result, ServerTrust, WriteResult, XattrRead,
};
use async_trait::async_trait;
use quickfs_protocol::{
    AttributeChanges, DirectoryEntry, DirectoryView, DirectoryViewOptions, FileHandle, FileLock,
    FileOpenOptions, FilesystemCapabilities, FilesystemStats, LockKind, Metadata, Name, NodeId,
    RenameMode, SafeIoctl, SeekWhence, SpecialNodeKind, XattrSetMode,
};
use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, RwLock};
use uuid::Uuid;
use zeroize::Zeroizing;

tokio::task_local! {
    static OPERATION_GENERATION: Arc<AtomicU64>;
}

/// Connection details retained in zeroizing process memory so a mounted
/// filesystem can authenticate a replacement QUIC connection.
pub struct AuthenticatedConnectionConfig {
    pub server: SocketAddr,
    pub server_name: String,
    pub trust: ServerTrust,
    pub username: String,
    password: Zeroizing<String>,
    pub timeout: Duration,
}

impl AuthenticatedConnectionConfig {
    pub fn new(
        server: SocketAddr,
        server_name: String,
        trust: ServerTrust,
        username: String,
        password: String,
        timeout: Duration,
    ) -> Self {
        Self {
            server,
            server_name,
            trust,
            username,
            password: Zeroizing::new(password),
            timeout,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconnectPolicy {
    /// Number of fresh connection attempts made for one failed operation.
    pub attempts: usize,
    pub initial_backoff: Duration,
    pub maximum_backoff: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            initial_backoff: Duration::from_millis(100),
            maximum_backoff: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResilientFilesystemOptions {
    pub reconnect: ReconnectPolicy,
    /// Hard deadline for one logical filesystem operation, including all
    /// reconnect, authentication, handle-recovery, and retry work.
    pub operation_deadline: Duration,
}

impl Default for ResilientFilesystemOptions {
    fn default() -> Self {
        Self {
            reconnect: ReconnectPolicy::default(),
            operation_deadline: Duration::from_secs(120),
        }
    }
}

#[async_trait]
pub trait RemoteFilesystemConnector: Send + Sync {
    async fn connect(&self) -> Result<Arc<dyn RemoteFilesystem>>;
}

struct AuthenticatedConnector {
    config: AuthenticatedConnectionConfig,
}

#[async_trait]
impl RemoteFilesystemConnector for AuthenticatedConnector {
    async fn connect(&self) -> Result<Arc<dyn RemoteFilesystem>> {
        let transport = self
            .config
            .trust
            .connect(
                self.config.server,
                &self.config.server_name,
                self.config.timeout,
            )
            .await?;
        let filesystem = NetworkFilesystem::authenticate(
            transport,
            self.config.username.clone(),
            self.config.password.to_string(),
        )
        .await?;
        Ok(Arc::new(filesystem))
    }
}

#[derive(Clone)]
struct ConnectionSlot {
    generation: u64,
    filesystem: Arc<dyn RemoteFilesystem>,
}

#[derive(Clone)]
struct HandleState {
    node: NodeId,
    options: FileOpenOptions,
    remote: FileHandle,
    revision: u64,
    size: u64,
    generation: u64,
    lock_history: Vec<FileLock>,
    mutation: Arc<Mutex<()>>,
}

/// Keeps inode-facing identifiers and file handles stable while replacing a
/// failed authenticated transport. Read-only operations are retried once after
/// reconnection. Mutations are deliberately not retried when their outcome is
/// ambiguous.
pub struct ResilientFilesystem {
    connector: Arc<dyn RemoteFilesystemConnector>,
    policy: ReconnectPolicy,
    operation_deadline: Duration,
    connection: Arc<RwLock<ConnectionSlot>>,
    reconnect: Arc<Mutex<ReconnectState>>,
    reconnect_notify: Arc<Notify>,
    failed_generation: Arc<AtomicU64>,
    server_epoch: Option<Uuid>,
    handles: RwLock<HashMap<FileHandle, HandleState>>,
}

#[derive(Default)]
struct ReconnectState {
    in_progress: Option<u64>,
    /// A failed reconnect batch is shared by all operations that were queued
    /// behind it. Without this short cooldown, every queued FUSE request runs
    /// another complete reconnect batch serially while the server is offline.
    last_failure: Option<(u64, Instant, ReconnectFailure)>,
}

#[derive(Clone, Copy)]
enum ReconnectFailure {
    Offline,
    EpochChanged,
}

impl ReconnectFailure {
    fn error(self) -> ClientError {
        match self {
            Self::Offline => ClientError::Offline,
            Self::EpochChanged => ClientError::Server(
                quickfs_protocol::ErrorCode::Conflict,
                "server epoch changed; existing inode and cache state is stale".into(),
            ),
        }
    }
}

impl ResilientFilesystem {
    pub async fn connect_authenticated(
        config: AuthenticatedConnectionConfig,
        options: ResilientFilesystemOptions,
    ) -> Result<Self> {
        let connector: Arc<dyn RemoteFilesystemConnector> =
            Arc::new(AuthenticatedConnector { config });
        let initial = connector.connect().await?;
        Self::new(initial, connector, options).await
    }

    pub async fn new(
        initial: Arc<dyn RemoteFilesystem>,
        connector: Arc<dyn RemoteFilesystemConnector>,
        options: ResilientFilesystemOptions,
    ) -> Result<Self> {
        if options.reconnect.attempts == 0 || options.operation_deadline.is_zero() {
            return Err(ClientError::Server(
                quickfs_protocol::ErrorCode::InvalidRequest,
                "reconnect attempts and operation deadline must be greater than zero".into(),
            ));
        }
        let server_epoch = capability_epoch(initial.as_ref()).await?;
        Ok(Self {
            connector,
            policy: options.reconnect,
            operation_deadline: options.operation_deadline,
            connection: Arc::new(RwLock::new(ConnectionSlot {
                generation: 1,
                filesystem: initial,
            })),
            reconnect: Arc::new(Mutex::new(ReconnectState::default())),
            reconnect_notify: Arc::new(Notify::new()),
            failed_generation: Arc::new(AtomicU64::new(0)),
            server_epoch,
            handles: RwLock::new(HashMap::new()),
        })
    }

    async fn healthy_connection(&self) -> Result<ConnectionSlot> {
        let current = self.connection.read().await.clone();
        let slot = if self.failed_generation.load(Ordering::Acquire) == current.generation {
            self.reconnect_after(current.generation).await
        } else {
            Ok(current)
        }?;
        Self::observe_generation(slot.generation);
        Ok(slot)
    }

    fn observe_generation(generation: u64) {
        let _ = OPERATION_GENERATION.try_with(|observed| {
            observed.store(generation, Ordering::Release);
        });
    }

    fn mark_failed(&self, generation: u64) {
        self.failed_generation
            .fetch_max(generation, Ordering::AcqRel);
    }

    async fn reconnect_after(&self, failed_generation: u64) -> Result<ConnectionSlot> {
        loop {
            let notified = self.reconnect_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let current = self.connection.read().await.clone();
            if current.generation != failed_generation
                || self.failed_generation.load(Ordering::Acquire) != failed_generation
            {
                Self::observe_generation(current.generation);
                return Ok(current);
            }

            let mut reconnect = self.reconnect.lock().await;
            if let Some((generation, failed_at, failure)) = reconnect.last_failure
                && generation == failed_generation
                && failed_at.elapsed() < self.policy.maximum_backoff
            {
                return Err(failure.error());
            }
            if reconnect.in_progress == Some(failed_generation) {
                drop(reconnect);
                notified.await;
                continue;
            }

            reconnect.in_progress = Some(failed_generation);
            drop(reconnect);
            self.spawn_reconnect(failed_generation);
            notified.await;
        }
    }

    fn spawn_reconnect(&self, failed_generation: u64) {
        let connector = self.connector.clone();
        let policy = self.policy;
        let connection = self.connection.clone();
        let reconnect = self.reconnect.clone();
        let reconnect_notify = self.reconnect_notify.clone();
        let failed = self.failed_generation.clone();
        let expected_epoch = self.server_epoch;
        tokio::spawn(async move {
            let mut backoff = policy.initial_backoff;
            let mut outcome = Err(ReconnectFailure::Offline);
            for attempt in 0..policy.attempts {
                if let Ok(filesystem) = connector.connect().await {
                    match capability_epoch(filesystem.as_ref()).await {
                        Ok(epoch) if expected_epoch.is_none() || epoch == expected_epoch => {
                            outcome = Ok(filesystem);
                            break;
                        }
                        Ok(_) => {
                            outcome = Err(ReconnectFailure::EpochChanged);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                if attempt + 1 < policy.attempts {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff
                        .checked_mul(2)
                        .unwrap_or(policy.maximum_backoff)
                        .min(policy.maximum_backoff);
                }
            }

            let mut state = reconnect.lock().await;
            match outcome {
                Ok(filesystem) => {
                    let mut current = connection.write().await;
                    if current.generation == failed_generation
                        && failed.load(Ordering::Acquire) == failed_generation
                    {
                        *current = ConnectionSlot {
                            generation: failed_generation.saturating_add(1),
                            filesystem,
                        };
                        failed.store(0, Ordering::Release);
                    }
                    state.last_failure = None;
                }
                Err(failure) => {
                    state.last_failure = Some((failed_generation, Instant::now(), failure));
                }
            }
            state.in_progress = None;
            drop(state);
            reconnect_notify.notify_waiters();
        });
    }

    async fn operation<T>(
        &self,
        mutation: bool,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if OPERATION_GENERATION.try_with(|_| ()).is_ok() {
            return future.await;
        }

        let generation = Arc::new(AtomicU64::new(0));
        OPERATION_GENERATION
            .scope(generation.clone(), async {
                match tokio::time::timeout(self.operation_deadline, future).await {
                    Ok(result) => result,
                    Err(_) => {
                        let generation = generation.load(Ordering::Acquire);
                        if generation != 0 {
                            self.mark_failed(generation);
                        }
                        if mutation {
                            Err(ClientError::AmbiguousMutation)
                        } else {
                            Err(ClientError::Server(
                                quickfs_protocol::ErrorCode::Timeout,
                                "resilient operation deadline exceeded".into(),
                            ))
                        }
                    }
                }
            })
            .await
    }

    async fn safe_call<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: Fn(Arc<dyn RemoteFilesystem>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.operation(false, async {
            let first = self.healthy_connection().await?;
            match operation(first.filesystem.clone()).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(first.generation);
                    let replacement = self.reconnect_after(first.generation).await?;
                    let result = operation(replacement.filesystem).await;
                    if matches!(result, Err(ClientError::Transport(_))) {
                        self.mark_failed(replacement.generation);
                    }
                    result
                }
                result => result,
            }
        })
        .await
    }

    async fn mutation_call<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(Arc<dyn RemoteFilesystem>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.operation(true, async {
            let slot = self.healthy_connection().await?;
            match operation(slot.filesystem).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                result => result,
            }
        })
        .await
    }

    async fn open_remote(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> Result<(ConnectionSlot, OpenedFile)> {
        self.operation(options.truncate, async {
            let slot = self.healthy_connection().await?;
            let result = slot.filesystem.open_file_with_options(node, options).await;
            match result {
                Ok(opened) => Ok((slot, opened)),
                Err(ClientError::Transport(_)) if !options.truncate => {
                    self.mark_failed(slot.generation);
                    let replacement = self.reconnect_after(slot.generation).await?;
                    let opened = replacement
                        .filesystem
                        .open_file_with_options(node, options)
                        .await?;
                    Ok((replacement, opened))
                }
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn remember_open(
        &self,
        node: NodeId,
        options: FileOpenOptions,
        slot: ConnectionSlot,
        opened: OpenedFile,
    ) -> OpenedFile {
        let logical = FileHandle(Uuid::new_v4());
        self.handles.write().await.insert(
            logical,
            HandleState {
                node,
                options,
                remote: opened.handle,
                revision: opened.revision,
                size: opened.size,
                generation: slot.generation,
                lock_history: Vec::new(),
                mutation: Arc::new(Mutex::new(())),
            },
        );
        OpenedFile {
            handle: logical,
            revision: opened.revision,
            size: opened.size,
        }
    }

    async fn remote_handle(&self, logical: FileHandle) -> Result<(ConnectionSlot, HandleState)> {
        let slot = self.healthy_connection().await?;
        let existing = self
            .handles
            .read()
            .await
            .get(&logical)
            .cloned()
            .ok_or_else(invalid_handle)?;
        if existing.generation == slot.generation {
            return Ok((slot, existing));
        }

        let mut handles = self.handles.write().await;
        let state = handles.get_mut(&logical).ok_or_else(invalid_handle)?;
        let current = self.healthy_connection().await?;
        if state.generation == current.generation {
            return Ok((current, state.clone()));
        }

        let mut reopen_options = state.options;
        reopen_options.truncate = false;
        let reopened = current
            .filesystem
            .open_file_with_options(state.node, reopen_options)
            .await?;
        if reopened.revision != state.revision {
            // A read-only handle without lock history adopts the newer
            // revision: POSIX keeps an open descriptor readable while other
            // actors update the file, and macOS mutates constantly (Finder
            // sets timestamps on fresh copies, bumping the revision). Only a
            // writable or locked handle cannot silently continue on content
            // that changed underneath it.
            if state.options.access == quickfs_protocol::FileAccess::ReadOnly
                && state.lock_history.is_empty()
            {
                state.revision = reopened.revision;
            } else {
                let _ = current.filesystem.close_file(reopened.handle).await;
                return Err(ClientError::StaleRevision);
            }
        }
        for lock in &state.lock_history {
            current
                .filesystem
                .set_lock(reopened.handle, *lock, false)
                .await?;
        }
        state.remote = reopened.handle;
        state.size = reopened.size;
        state.generation = current.generation;
        Ok((current, state.clone()))
    }

    /// Records that the remote file moved to `revision` while `logical` was
    /// open. Revisions only move forward, so a stale concurrent adoption
    /// (two reads racing) can never roll the handle's view back.
    async fn adopt_revision(&self, logical: FileHandle, revision: u64) {
        if let Some(state) = self.handles.write().await.get_mut(&logical)
            && revision > state.revision
        {
            state.revision = revision;
        }
    }

    async fn update_written_handle(
        &self,
        logical: FileHandle,
        remote: FileHandle,
        generation: u64,
        result: WriteResult,
    ) {
        if let Some(state) = self.handles.write().await.get_mut(&logical)
            && state.remote == remote
            && state.generation == generation
        {
            state.revision = result.revision;
            state.size = result.size;
        }
    }
}

async fn capability_epoch(filesystem: &dyn RemoteFilesystem) -> Result<Option<Uuid>> {
    match filesystem.capabilities().await {
        Ok(capabilities) => Ok(Some(capabilities.server_epoch)),
        Err(ClientError::Server(quickfs_protocol::ErrorCode::NotSupported, _)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Fold one acknowledged lock operation into a handle's replayable lock state.
///
/// The history holds the handle's *current* advisory-lock set — disjoint
/// per-owner ranges mirroring how the server replaces an owner's overlapping
/// range — rather than an append-only log. This keeps replay after reconnect
/// exact (an unlock genuinely removes its range, a partial unlock splits the
/// surviving range) and keeps memory bounded by the number of ranges the
/// server itself was willing to accept, instead of growing with every
/// operation and eventually discarding still-held locks.
fn apply_lock_to_history(history: &mut Vec<FileLock>, lock: FileLock) {
    let mut next = Vec::with_capacity(history.len() + 2);
    for existing in history.drain(..) {
        if existing.owner != lock.owner || existing.start > lock.end || lock.start > existing.end {
            next.push(existing);
            continue;
        }
        if existing.start < lock.start {
            next.push(FileLock {
                end: lock.start - 1,
                ..existing
            });
        }
        if existing.end > lock.end {
            next.push(FileLock {
                start: lock.end.saturating_add(1),
                ..existing
            });
        }
    }
    if lock.kind != LockKind::Unlock {
        next.push(lock);
    }
    *history = next;
}

fn invalid_handle() -> ClientError {
    ClientError::Server(
        quickfs_protocol::ErrorCode::InvalidHandle,
        "unknown logical file handle".into(),
    )
}

#[async_trait]
impl RemoteFilesystem for ResilientFilesystem {
    async fn ping(&self, nonce: u64) -> Result<u64> {
        self.safe_call(move |filesystem| async move { filesystem.ping(nonce).await })
            .await
    }

    async fn capabilities(&self) -> Result<FilesystemCapabilities> {
        self.safe_call(|filesystem| async move { filesystem.capabilities().await })
            .await
    }

    async fn stat_filesystem(&self) -> Result<FilesystemStats> {
        self.safe_call(|filesystem| async move { filesystem.stat_filesystem().await })
            .await
    }

    async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
        self.safe_call(move |filesystem| async move { filesystem.get_metadata(node).await })
            .await
    }

    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        Ok(self.list_directory_snapshot(node).await?.entries)
    }

    async fn list_directory_snapshot(&self, node: NodeId) -> Result<DirectorySnapshot> {
        self.safe_call(
            move |filesystem| async move { filesystem.list_directory_snapshot(node).await },
        )
        .await
    }

    async fn list_directory_view(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
    ) -> Result<DirectoryView> {
        self.safe_call(move |filesystem| async move {
            filesystem.list_directory_view(node, options).await
        })
        .await
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
        let (slot, opened) = self.open_remote(node, options).await?;
        Ok(self.remember_open(node, options, slot, opened).await)
    }

    async fn create_file(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<CreatedFile> {
        self.operation(true, async {
            let slot = self.healthy_connection().await?;
            let created = match slot
                .filesystem
                .create_file(parent, name, mode, options)
                .await
            {
                Ok(created) => created,
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    return Err(ClientError::AmbiguousMutation);
                }
                Err(error) => return Err(error),
            };
            let opened = self
                .remember_open(created.metadata.node, options, slot, created.opened)
                .await;
            Ok(CreatedFile {
                metadata: created.metadata,
                opened,
            })
        })
        .await
    }

    async fn create_directory(&self, parent: NodeId, name: Name, mode: u32) -> Result<Metadata> {
        self.mutation_call(move |filesystem| async move {
            filesystem.create_directory(parent, name, mode).await
        })
        .await
    }

    async fn create_symlink(
        &self,
        parent: NodeId,
        name: Name,
        target: Vec<u8>,
    ) -> Result<Metadata> {
        self.mutation_call(move |filesystem| async move {
            filesystem.create_symlink(parent, name, target).await
        })
        .await
    }

    async fn create_hard_link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: Name,
    ) -> Result<Metadata> {
        self.mutation_call(move |filesystem| async move {
            filesystem
                .create_hard_link(node, new_parent, new_name)
                .await
        })
        .await
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
        self.mutation_call(move |filesystem| async move {
            filesystem
                .create_special_node(parent, name, kind, mode, device_major, device_minor)
                .await
        })
        .await
    }

    async fn remove_node(&self, parent: NodeId, name: Name, directory: bool) -> Result<()> {
        self.mutation_call(move |filesystem| async move {
            filesystem.remove_node(parent, name, directory).await
        })
        .await
    }

    async fn rename_node(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    ) -> Result<()> {
        self.mutation_call(move |filesystem| async move {
            filesystem
                .rename_node(parent, name, new_parent, new_name, mode)
                .await
        })
        .await
    }

    async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
        self.safe_call(move |filesystem| async move { filesystem.read_link(node).await })
            .await
    }

    async fn set_attributes(
        &self,
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> Result<Metadata> {
        self.operation(true, async {
            let mapped = if let Some(logical) = handle {
                Some(self.remote_handle(logical).await?.1)
            } else {
                None
            };
            let _mutation = match &mapped {
                Some(state) => Some(state.mutation.lock().await),
                None => None,
            };
            let remote_handle = mapped.as_ref().map(|state| state.remote);
            let metadata = self
                .mutation_call(move |filesystem| async move {
                    filesystem
                        .set_attributes(node, remote_handle, changes)
                        .await
                })
                .await?;
            if let Some(logical) = handle
                && let Some(state) = self.handles.write().await.get_mut(&logical)
            {
                state.revision = metadata.revision;
                state.size = metadata.size;
            }
            Ok(metadata)
        })
        .await
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
        self.operation(false, async {
            let (slot, state) = self.remote_handle(handle).await?;
            match slot
                .filesystem
                .read_range_versioned(state.remote, offset, length)
                .await
            {
                // A read observing a revision the handle has not seen yet is not
                // an error: another actor updated the file while it was open
                // (macOS does this routinely — Finder stamps a fresh copy's
                // timestamps, bumping the revision). Adopt the newer revision so
                // the handle keeps serving reads; the caller sees which revision
                // the bytes belong to via `RangeRead::revision`.
                Ok(read) => {
                    if read.revision != state.revision {
                        self.adopt_revision(handle, read.revision).await;
                    }
                    Ok(read)
                }
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    let (_, reopened) = self.remote_handle(handle).await?;
                    let replacement = self.healthy_connection().await?;
                    let read = replacement
                        .filesystem
                        .read_range_versioned(reopened.remote, offset, length)
                        .await?;
                    if read.revision != reopened.revision {
                        self.adopt_revision(handle, read.revision).await;
                    }
                    Ok(read)
                }
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<WriteResult> {
        self.operation(true, async {
            let (slot, state) = self.remote_handle(handle).await?;
            let _mutation = state.mutation.lock().await;
            match slot
                .filesystem
                .write_range(state.remote, offset, data)
                .await
            {
                Ok(result) => {
                    self.update_written_handle(handle, state.remote, slot.generation, result)
                        .await;
                    Ok(result)
                }
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn flush_file(&self, handle: FileHandle, lock_owner: Option<u64>) -> Result<()> {
        self.operation(true, async {
            let (slot, state) = self.remote_handle(handle).await?;
            let _mutation = state.mutation.lock().await;
            match slot.filesystem.flush_file(state.remote, lock_owner).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                result => result,
            }
        })
        .await
    }

    async fn sync_file(&self, handle: FileHandle, data_only: bool) -> Result<()> {
        self.operation(true, async {
            let (slot, state) = self.remote_handle(handle).await?;
            let _mutation = state.mutation.lock().await;
            match slot.filesystem.sync_file(state.remote, data_only).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                result => result,
            }
        })
        .await
    }

    async fn sync_directory(&self, node: NodeId) -> Result<()> {
        self.mutation_call(move |filesystem| async move { filesystem.sync_directory(node).await })
            .await
    }

    async fn allocate_file(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        self.operation(true, async {
            let (slot, state) = self.remote_handle(handle).await?;
            let _mutation = state.mutation.lock().await;
            match slot
                .filesystem
                .allocate_file(state.remote, offset, length)
                .await
            {
                Ok(result) => {
                    self.update_written_handle(handle, state.remote, slot.generation, result)
                        .await;
                    Ok(result)
                }
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn get_xattr(
        &self,
        node: NodeId,
        name: Name,
        offset: u64,
        length: u64,
    ) -> Result<XattrRead> {
        self.safe_call(move |filesystem| {
            let name = name.clone();
            async move { filesystem.get_xattr(node, name, offset, length).await }
        })
        .await
    }

    async fn set_xattr(
        &self,
        node: NodeId,
        name: Name,
        value: &[u8],
        mode: XattrSetMode,
        position: u32,
    ) -> Result<()> {
        let value = value.to_vec();
        self.mutation_call(move |filesystem| async move {
            filesystem
                .set_xattr(node, name, &value, mode, position)
                .await
        })
        .await
    }

    async fn list_xattrs(&self, node: NodeId) -> Result<Vec<Name>> {
        self.safe_call(move |filesystem| async move { filesystem.list_xattrs(node).await })
            .await
    }

    async fn remove_xattr(&self, node: NodeId, name: Name) -> Result<()> {
        self.mutation_call(
            move |filesystem| async move { filesystem.remove_xattr(node, name).await },
        )
        .await
    }

    async fn copy_file_range(
        &self,
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        self.operation(true, async {
            let (input_slot, input_state) = self.remote_handle(input).await?;
            let (output_slot, output_state) = self.remote_handle(output).await?;
            if input_slot.generation != output_slot.generation {
                return Err(ClientError::StaleRevision);
            }
            let _mutation = output_state.mutation.lock().await;
            match input_slot
                .filesystem
                .copy_file_range(
                    input_state.remote,
                    input_offset,
                    output_state.remote,
                    output_offset,
                    length,
                )
                .await
            {
                Ok(result) => {
                    self.update_written_handle(
                        output,
                        output_state.remote,
                        output_slot.generation,
                        result,
                    )
                    .await;
                    Ok(result)
                }
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(output_slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn seek_file(&self, handle: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
        self.operation(false, async {
            let (slot, state) = self.remote_handle(handle).await?;
            match slot
                .filesystem
                .seek_file(state.remote, offset, whence)
                .await
            {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    let (_, reopened) = self.remote_handle(handle).await?;
                    self.healthy_connection()
                        .await?
                        .filesystem
                        .seek_file(reopened.remote, offset, whence)
                        .await
                }
                result => result,
            }
        })
        .await
    }

    async fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64> {
        self.operation(false, async {
            let (slot, state) = self.remote_handle(handle).await?;
            match slot.filesystem.safe_ioctl(state.remote, operation).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    let (_, reopened) = self.remote_handle(handle).await?;
                    self.healthy_connection()
                        .await?
                        .filesystem
                        .safe_ioctl(reopened.remote, operation)
                        .await
                }
                result => result,
            }
        })
        .await
    }

    async fn map_block(&self, node: NodeId, block_size: u32, block: u64) -> Result<u64> {
        self.safe_call(move |filesystem| async move {
            filesystem.map_block(node, block_size, block).await
        })
        .await
    }

    async fn exchange_data(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        options: u64,
    ) -> Result<()> {
        self.mutation_call(move |filesystem| async move {
            filesystem
                .exchange_data(parent, name, new_parent, new_name, options)
                .await
        })
        .await
    }

    async fn set_volume_name(&self, name: Name) -> Result<()> {
        self.mutation_call(move |filesystem| async move { filesystem.set_volume_name(name).await })
            .await
    }

    async fn forget_nodes(&self, nodes: Vec<NodeId>) -> Result<()> {
        self.safe_call(move |filesystem| {
            let nodes = nodes.clone();
            async move { filesystem.forget_nodes(nodes).await }
        })
        .await
    }

    async fn get_lock(&self, handle: FileHandle, lock: FileLock) -> Result<Option<FileLock>> {
        self.operation(false, async {
            let (slot, state) = self.remote_handle(handle).await?;
            match slot.filesystem.get_lock(state.remote, lock).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    let (_, reopened) = self.remote_handle(handle).await?;
                    self.healthy_connection()
                        .await?
                        .filesystem
                        .get_lock(reopened.remote, lock)
                        .await
                }
                result => result,
            }
        })
        .await
    }

    async fn set_lock(&self, handle: FileHandle, lock: FileLock, wait: bool) -> Result<()> {
        self.operation(true, async {
            let (slot, state) = self.remote_handle(handle).await?;
            match slot.filesystem.set_lock(state.remote, lock, wait).await {
                Ok(()) => {
                    let mut handles = self.handles.write().await;
                    if let Some(current) = handles.get_mut(&handle) {
                        apply_lock_to_history(&mut current.lock_history, lock);
                    }
                    Ok(())
                }
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Err(ClientError::AmbiguousMutation)
                }
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn close_file(&self, handle: FileHandle) -> Result<()> {
        self.operation(false, async {
            let mutation = self
                .handles
                .read()
                .await
                .get(&handle)
                .map(|state| state.mutation.clone())
                .ok_or_else(invalid_handle)?;
            let _mutation = mutation.lock().await;
            let state = self
                .handles
                .write()
                .await
                .remove(&handle)
                .ok_or_else(invalid_handle)?;
            let slot = self.connection.read().await.clone();
            if state.generation != slot.generation {
                return Ok(());
            }
            match slot.filesystem.close_file(state.remote).await {
                Err(ClientError::Transport(_)) => {
                    self.mark_failed(slot.generation);
                    Ok(())
                }
                result => result,
            }
        })
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quickfs_protocol::{ErrorCode, NodeKind, ROOT_NODE};
    use quickfs_transport_quic::TransportError;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize},
    };

    const FILE_NODE: NodeId = NodeId(Uuid::from_u128(44));

    struct ScriptedFilesystem {
        epoch: Uuid,
        revision: u64,
        data: Vec<u8>,
        fail_ping: AtomicBool,
        fail_read: AtomicBool,
        fail_write: AtomicBool,
        ping_delay_ms: AtomicU64,
        write_delay_ms: AtomicU64,
        opens: AtomicUsize,
        locks: Mutex<Vec<FileLock>>,
    }

    impl ScriptedFilesystem {
        fn new(epoch: Uuid, revision: u64) -> Self {
            Self {
                epoch,
                revision,
                data: b"0123456789abcdef".to_vec(),
                fail_ping: AtomicBool::new(false),
                fail_read: AtomicBool::new(false),
                fail_write: AtomicBool::new(false),
                ping_delay_ms: AtomicU64::new(0),
                write_delay_ms: AtomicU64::new(0),
                opens: AtomicUsize::new(0),
                locks: Mutex::new(Vec::new()),
            }
        }

        fn transport_failure() -> ClientError {
            ClientError::Transport(TransportError::Timeout)
        }
    }

    #[async_trait]
    impl RemoteFilesystem for ScriptedFilesystem {
        async fn ping(&self, nonce: u64) -> Result<u64> {
            tokio::time::sleep(Duration::from_millis(
                self.ping_delay_ms.load(Ordering::SeqCst),
            ))
            .await;
            if self.fail_ping.swap(false, Ordering::SeqCst) {
                Err(Self::transport_failure())
            } else {
                Ok(nonce)
            }
        }

        async fn capabilities(&self) -> Result<FilesystemCapabilities> {
            Ok(FilesystemCapabilities {
                server_epoch: self.epoch,
                writable: true,
                supports_locks: true,
                supports_atomic_rename: true,
                supports_directory_sync: true,
                supports_preallocation: true,
                supports_symlinks: true,
                supports_xattrs: false,
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
                max_read_size: crate::MAX_CLIENT_READ_SIZE,
                max_write_size: crate::MAX_CLIENT_WRITE_SIZE,
            })
        }

        async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
            Ok(Metadata {
                node,
                kind: if node == ROOT_NODE {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                },
                size: self.data.len() as u64,
                mode: 0o644,
                allocated_blocks: self.data.len().div_ceil(512) as u64,
                revision: self.revision,
                accessed_unix_ms: 1,
                modified_unix_ms: 1,
                created_unix_ms: Some(1),
                backup_unix_ms: None,
                link_count: if node == ROOT_NODE { 2 } else { 1 },
                device_major: 0,
                device_minor: 0,
            })
        }

        async fn list_directory(&self, _node: NodeId) -> Result<Vec<DirectoryEntry>> {
            Ok(vec![DirectoryEntry {
                node: FILE_NODE,
                name: "clip".into(),
                kind: NodeKind::File,
                metadata: self.get_metadata(FILE_NODE).await?,
            }])
        }

        async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)> {
            let opened = self
                .open_file_with_options(node, FileOpenOptions::READ_ONLY)
                .await?;
            Ok((opened.handle, opened.revision, opened.size))
        }

        async fn open_file_with_options(
            &self,
            _node: NodeId,
            _options: FileOpenOptions,
        ) -> Result<OpenedFile> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(OpenedFile {
                handle: FileHandle(Uuid::new_v4()),
                revision: self.revision,
                size: self.data.len() as u64,
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
            if self.fail_read.swap(false, Ordering::SeqCst) {
                return Err(Self::transport_failure());
            }
            let start = usize::try_from(offset).unwrap();
            let end = usize::try_from(offset.saturating_add(length))
                .unwrap()
                .min(self.data.len());
            Ok(RangeRead {
                revision: self.revision,
                data: self.data[start..end].to_vec(),
            })
        }

        async fn write_range(
            &self,
            _handle: FileHandle,
            _offset: u64,
            data: &[u8],
        ) -> Result<WriteResult> {
            tokio::time::sleep(Duration::from_millis(
                self.write_delay_ms.load(Ordering::SeqCst),
            ))
            .await;
            if self.fail_write.swap(false, Ordering::SeqCst) {
                return Err(Self::transport_failure());
            }
            Ok(WriteResult {
                written: data.len() as u64,
                revision: self.revision + 1,
                size: self.data.len() as u64,
            })
        }

        async fn set_lock(&self, _handle: FileHandle, lock: FileLock, _wait: bool) -> Result<()> {
            self.locks.lock().await.push(lock);
            Ok(())
        }

        async fn close_file(&self, _handle: FileHandle) -> Result<()> {
            Ok(())
        }
    }

    struct QueueConnector {
        filesystems: Mutex<VecDeque<Arc<dyn RemoteFilesystem>>>,
        calls: AtomicUsize,
        delay: Duration,
        started: Notify,
    }

    #[async_trait]
    impl RemoteFilesystemConnector for QueueConnector {
        async fn connect(&self) -> Result<Arc<dyn RemoteFilesystem>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            tokio::time::sleep(self.delay).await;
            self.filesystems
                .lock()
                .await
                .pop_front()
                .ok_or(ClientError::Offline)
        }
    }

    fn connector(filesystem: Arc<dyn RemoteFilesystem>) -> Arc<QueueConnector> {
        Arc::new(QueueConnector {
            filesystems: Mutex::new(VecDeque::from([filesystem])),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            started: Notify::new(),
        })
    }

    fn test_options() -> ResilientFilesystemOptions {
        ResilientFilesystemOptions {
            reconnect: ReconnectPolicy {
                attempts: 1,
                initial_backoff: Duration::ZERO,
                maximum_backoff: Duration::ZERO,
            },
            operation_deadline: Duration::from_secs(1),
        }
    }

    fn deadline_options(deadline: Duration) -> ResilientFilesystemOptions {
        ResilientFilesystemOptions {
            operation_deadline: deadline,
            ..test_options()
        }
    }

    fn is_operation_timeout(result: &Result<u64>) -> bool {
        matches!(
            result,
            Err(ClientError::Server(ErrorCode::Timeout, message))
                if message == "resilient operation deadline exceeded"
        )
    }

    #[tokio::test]
    async fn normal_operation_completes_before_deadline() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.ping_delay_ms.store(5, Ordering::SeqCst);
        let filesystem = ResilientFilesystem::new(
            first.clone(),
            connector(first),
            deadline_options(Duration::from_millis(80)),
        )
        .await
        .unwrap();

        assert_eq!(filesystem.ping(17).await.unwrap(), 17);
    }

    #[tokio::test]
    async fn blocked_operation_returns_at_deadline() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.ping_delay_ms.store(500, Ordering::SeqCst);
        let filesystem = ResilientFilesystem::new(
            first.clone(),
            connector(first),
            deadline_options(Duration::from_millis(35)),
        )
        .await
        .unwrap();

        let started = Instant::now();
        let result = filesystem.ping(1).await;
        let elapsed = started.elapsed();
        assert!(is_operation_timeout(&result));
        assert!(elapsed >= Duration::from_millis(25), "elapsed: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(150), "elapsed: {elapsed:?}");
    }

    #[tokio::test]
    async fn retry_does_not_reset_operation_deadline() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.fail_ping.store(true, Ordering::SeqCst);
        first.ping_delay_ms.store(30, Ordering::SeqCst);
        let replacement = Arc::new(ScriptedFilesystem::new(epoch, 1));
        replacement.ping_delay_ms.store(30, Ordering::SeqCst);
        let filesystem = ResilientFilesystem::new(
            first,
            connector(replacement),
            deadline_options(Duration::from_millis(45)),
        )
        .await
        .unwrap();

        let started = Instant::now();
        let result = filesystem.ping(3).await;
        let elapsed = started.elapsed();
        assert!(is_operation_timeout(&result));
        assert!(elapsed < Duration::from_millis(120), "elapsed: {elapsed:?}");
    }

    #[tokio::test]
    async fn timed_out_waiter_does_not_cancel_shared_reconnect() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.fail_ping.store(true, Ordering::SeqCst);
        first.ping_delay_ms.store(20, Ordering::SeqCst);
        let replacement = Arc::new(ScriptedFilesystem::new(epoch, 1));
        let connector = Arc::new(QueueConnector {
            filesystems: Mutex::new(VecDeque::from([replacement as Arc<dyn RemoteFilesystem>])),
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(35),
            started: Notify::new(),
        });
        let filesystem = Arc::new(
            ResilientFilesystem::new(
                first,
                connector.clone(),
                deadline_options(Duration::from_millis(45)),
            )
            .await
            .unwrap(),
        );

        let first_waiter = {
            let filesystem = filesystem.clone();
            tokio::spawn(async move { filesystem.ping(1).await })
        };
        connector.started.notified().await;
        let second_result = filesystem.ping(2).await;
        let first_result = first_waiter.await.unwrap();

        assert!(is_operation_timeout(&first_result));
        assert_eq!(second_result.unwrap(), 2);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timed_out_mutation_is_ambiguous_and_not_retried() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.write_delay_ms.store(500, Ordering::SeqCst);
        let replacement = Arc::new(ScriptedFilesystem::new(epoch, 1));
        let connector = connector(replacement);
        let filesystem = ResilientFilesystem::new(
            first,
            connector.clone(),
            deadline_options(Duration::from_millis(35)),
        )
        .await
        .unwrap();
        let handle = filesystem
            .open_file_with_options(
                FILE_NODE,
                FileOpenOptions {
                    access: quickfs_protocol::FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            )
            .await
            .unwrap()
            .handle;

        assert!(matches!(
            filesystem.write_range(handle, 0, b"x").await,
            Err(ClientError::AmbiguousMutation)
        ));
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reconnects_and_retries_safe_operations() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.fail_ping.store(true, Ordering::SeqCst);
        let second = Arc::new(ScriptedFilesystem::new(epoch, 1));
        let connector = connector(second);
        let filesystem = ResilientFilesystem::new(first, connector.clone(), test_options())
            .await
            .unwrap();

        assert_eq!(filesystem.ping(91).await.unwrap(), 91);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_share_a_failed_reconnect_batch() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        let connector = Arc::new(QueueConnector {
            filesystems: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            started: Notify::new(),
        });
        let filesystem = ResilientFilesystem::new(
            first,
            connector.clone(),
            ResilientFilesystemOptions {
                reconnect: ReconnectPolicy {
                    attempts: 1,
                    initial_backoff: Duration::ZERO,
                    maximum_backoff: Duration::from_secs(1),
                },
                operation_deadline: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        filesystem.mark_failed(1);

        let (first, second, third) =
            tokio::join!(filesystem.ping(1), filesystem.ping(2), filesystem.ping(3));

        assert!(matches!(first, Err(ClientError::Offline)));
        assert!(matches!(second, Err(ClientError::Offline)));
        assert!(matches!(third, Err(ClientError::Offline)));
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reopens_read_handle_only_at_the_same_revision() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 3));
        first.fail_read.store(true, Ordering::SeqCst);
        let second = Arc::new(ScriptedFilesystem::new(epoch, 3));
        let connector = connector(second.clone());
        let filesystem = ResilientFilesystem::new(first, connector, test_options())
            .await
            .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        assert_eq!(filesystem.read_range(handle, 2, 4).await.unwrap(), b"2345");
        assert_eq!(second.opens.load(Ordering::SeqCst), 1);
    }

    /// A read-only handle that reconnects onto a newer file revision keeps
    /// working: POSIX guarantees an open descriptor stays readable while other
    /// actors update the file (Finder stamps fresh copies' timestamps, bumping
    /// the revision). The handle adopts the new revision instead of failing
    /// every subsequent read with ESTALE.
    #[tokio::test]
    async fn read_only_handle_adopts_newer_revision_after_reconnect() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 3));
        first.fail_read.store(true, Ordering::SeqCst);
        let second = Arc::new(ScriptedFilesystem::new(epoch, 4));
        let filesystem = ResilientFilesystem::new(first, connector(second), test_options())
            .await
            .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        assert_eq!(filesystem.read_range(handle, 0, 2).await.unwrap(), b"01");
        // The adopted revision is now the handle's view.
        let read = filesystem.read_range_versioned(handle, 0, 2).await.unwrap();
        assert_eq!(read.revision, 4);
    }

    /// A writable handle cannot silently continue onto content that changed
    /// underneath it across a reconnect; it still fails stale.
    #[tokio::test]
    async fn writable_handle_rejects_revision_drift_after_reconnect() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 3));
        first.fail_read.store(true, Ordering::SeqCst);
        let second = Arc::new(ScriptedFilesystem::new(epoch, 4));
        let filesystem = ResilientFilesystem::new(first, connector(second), test_options())
            .await
            .unwrap();
        let handle = filesystem
            .open_file_with_options(
                FILE_NODE,
                FileOpenOptions {
                    access: quickfs_protocol::FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            )
            .await
            .unwrap()
            .handle;

        assert!(matches!(
            filesystem.read_range(handle, 0, 2).await,
            Err(ClientError::StaleRevision)
        ));
    }

    #[tokio::test]
    async fn never_retries_an_ambiguous_write() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 8));
        first.fail_write.store(true, Ordering::SeqCst);
        let replacement = Arc::new(ScriptedFilesystem::new(epoch, 8));
        let connector = connector(replacement);
        let filesystem = ResilientFilesystem::new(first, connector.clone(), test_options())
            .await
            .unwrap();
        let options = FileOpenOptions {
            access: quickfs_protocol::FileAccess::ReadWrite,
            truncate: false,
            append: false,
        };
        let handle = filesystem
            .open_file_with_options(FILE_NODE, options)
            .await
            .unwrap()
            .handle;

        assert!(matches!(
            filesystem.write_range(handle, 0, b"x").await,
            Err(ClientError::AmbiguousMutation)
        ));
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reopens_handles_and_replays_locks_after_same_epoch_daemon_restart() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 8));
        let replacement = Arc::new(ScriptedFilesystem::new(epoch, 8));
        let filesystem = ResilientFilesystem::new(
            first.clone(),
            connector(replacement.clone()),
            test_options(),
        )
        .await
        .unwrap();
        let handle = filesystem
            .open_file_with_options(
                FILE_NODE,
                FileOpenOptions {
                    access: quickfs_protocol::FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            )
            .await
            .unwrap()
            .handle;
        let lock = FileLock {
            owner: 99,
            start: 1_024,
            end: 2_047,
            kind: LockKind::Write,
            pid: 42,
        };
        filesystem.set_lock(handle, lock, false).await.unwrap();
        assert_eq!(*first.locks.lock().await, [lock]);

        first.fail_ping.store(true, Ordering::SeqCst);
        assert_eq!(filesystem.ping(7).await.unwrap(), 7);
        assert_eq!(filesystem.read_range(handle, 0, 2).await.unwrap(), b"01");
        assert_eq!(replacement.opens.load(Ordering::SeqCst), 1);
        assert_eq!(*replacement.locks.lock().await, [lock]);
    }

    fn history_lock(owner: u64, start: u64, end: u64, kind: LockKind) -> FileLock {
        FileLock {
            owner,
            start,
            end,
            kind,
            pid: 42,
        }
    }

    #[test]
    fn lock_history_compacts_instead_of_growing_or_discarding() {
        let mut history = Vec::new();
        // Two disjoint locks for one owner, one for another owner.
        apply_lock_to_history(&mut history, history_lock(1, 0, 9, LockKind::Write));
        apply_lock_to_history(&mut history, history_lock(1, 20, 29, LockKind::Write));
        apply_lock_to_history(&mut history, history_lock(2, 0, 9, LockKind::Read));
        assert_eq!(history.len(), 3);

        // Re-locking an overlapping range replaces it rather than appending, so
        // the history mirrors the server's current lock state.
        apply_lock_to_history(&mut history, history_lock(1, 0, 9, LockKind::Read));
        assert_eq!(history.len(), 3);
        assert!(
            history
                .iter()
                .any(|lock| lock.owner == 1 && lock.start == 0 && lock.kind == LockKind::Read)
        );

        // A partial unlock splits the surviving range; the other owner's
        // overlapping lock is untouched.
        apply_lock_to_history(&mut history, history_lock(1, 3, 5, LockKind::Unlock));
        let owner_one: Vec<_> = history.iter().filter(|lock| lock.owner == 1).collect();
        assert!(
            owner_one
                .iter()
                .any(|lock| lock.start == 0 && lock.end == 2)
        );
        assert!(
            owner_one
                .iter()
                .any(|lock| lock.start == 6 && lock.end == 9)
        );
        assert!(
            history
                .iter()
                .any(|lock| lock.owner == 2 && lock.start == 0 && lock.end == 9)
        );

        // A full unlock of the split ranges genuinely removes them: no dead
        // records linger and nothing outside the range is dropped with them.
        apply_lock_to_history(&mut history, history_lock(1, 0, 15, LockKind::Unlock));
        assert_eq!(history.len(), 2);
        assert!(
            history
                .iter()
                .all(|lock| lock.owner != 1 || lock.start == 20)
        );
    }

    /// After an unlock, a reconnect must replay exactly the locks still held —
    /// the unlocked range must not resurface and the held locks must not be
    /// silently discarded (the old history was cleared wholesale once it grew
    /// past a threshold, losing held locks from replay).
    #[tokio::test]
    async fn reconnect_replays_only_locks_still_held_after_unlock() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 8));
        let replacement = Arc::new(ScriptedFilesystem::new(epoch, 8));
        let filesystem = ResilientFilesystem::new(
            first.clone(),
            connector(replacement.clone()),
            test_options(),
        )
        .await
        .unwrap();
        let handle = filesystem
            .open_file_with_options(
                FILE_NODE,
                FileOpenOptions {
                    access: quickfs_protocol::FileAccess::ReadWrite,
                    truncate: false,
                    append: false,
                },
            )
            .await
            .unwrap()
            .handle;
        let released = history_lock(9, 0, 99, LockKind::Write);
        let kept = history_lock(9, 200, 299, LockKind::Write);
        filesystem.set_lock(handle, released, false).await.unwrap();
        filesystem.set_lock(handle, kept, false).await.unwrap();
        filesystem
            .set_lock(
                handle,
                FileLock {
                    kind: LockKind::Unlock,
                    ..released
                },
                false,
            )
            .await
            .unwrap();

        first.fail_ping.store(true, Ordering::SeqCst);
        assert_eq!(filesystem.ping(5).await.unwrap(), 5);
        assert_eq!(filesystem.read_range(handle, 0, 2).await.unwrap(), b"01");
        assert_eq!(*replacement.locks.lock().await, [kept]);
    }

    #[test]
    fn transport_failures_remain_distinct_from_server_errors() {
        assert!(!matches!(
            ClientError::Server(ErrorCode::Timeout, "server timeout".into()),
            ClientError::Transport(_)
        ));
    }
}
