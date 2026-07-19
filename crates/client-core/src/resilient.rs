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
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;
use zeroize::Zeroizing;

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
    connection: RwLock<ConnectionSlot>,
    reconnect: Mutex<ReconnectState>,
    failed_generation: AtomicU64,
    server_epoch: Option<Uuid>,
    handles: RwLock<HashMap<FileHandle, HandleState>>,
}

#[derive(Default)]
struct ReconnectState {
    /// A failed reconnect batch is shared by all operations that were queued
    /// behind it. Without this short cooldown, every queued FUSE request runs
    /// another complete reconnect batch serially while the server is offline.
    last_failure: Option<(u64, Instant)>,
}

impl ResilientFilesystem {
    pub async fn connect_authenticated(
        config: AuthenticatedConnectionConfig,
        policy: ReconnectPolicy,
    ) -> Result<Self> {
        let connector: Arc<dyn RemoteFilesystemConnector> =
            Arc::new(AuthenticatedConnector { config });
        let initial = connector.connect().await?;
        Self::new(initial, connector, policy).await
    }

    pub async fn new(
        initial: Arc<dyn RemoteFilesystem>,
        connector: Arc<dyn RemoteFilesystemConnector>,
        policy: ReconnectPolicy,
    ) -> Result<Self> {
        if policy.attempts == 0 {
            return Err(ClientError::Server(
                quickfs_protocol::ErrorCode::InvalidRequest,
                "reconnect attempts must be greater than zero".into(),
            ));
        }
        let server_epoch = capability_epoch(initial.as_ref()).await?;
        Ok(Self {
            connector,
            policy,
            connection: RwLock::new(ConnectionSlot {
                generation: 1,
                filesystem: initial,
            }),
            reconnect: Mutex::new(ReconnectState::default()),
            failed_generation: AtomicU64::new(0),
            server_epoch,
            handles: RwLock::new(HashMap::new()),
        })
    }

    async fn healthy_connection(&self) -> Result<ConnectionSlot> {
        let current = self.connection.read().await.clone();
        if self.failed_generation.load(Ordering::Acquire) == current.generation {
            self.reconnect_after(current.generation).await
        } else {
            Ok(current)
        }
    }

    fn mark_failed(&self, generation: u64) {
        self.failed_generation.store(generation, Ordering::Release);
    }

    async fn reconnect_after(&self, failed_generation: u64) -> Result<ConnectionSlot> {
        let mut reconnect = self.reconnect.lock().await;
        let current = self.connection.read().await.clone();
        if current.generation != failed_generation
            || self.failed_generation.load(Ordering::Acquire) != failed_generation
        {
            return Ok(current);
        }
        if reconnect
            .last_failure
            .is_some_and(|(generation, failed_at)| {
                generation == failed_generation && failed_at.elapsed() < self.policy.maximum_backoff
            })
        {
            return Err(ClientError::Offline);
        }

        let mut backoff = self.policy.initial_backoff;
        for attempt in 0..self.policy.attempts {
            match self.connector.connect().await {
                Ok(filesystem) => {
                    let epoch = capability_epoch(filesystem.as_ref()).await?;
                    if self.server_epoch.is_some() && epoch != self.server_epoch {
                        return Err(ClientError::Server(
                            quickfs_protocol::ErrorCode::Conflict,
                            "server epoch changed; existing inode and cache state is stale".into(),
                        ));
                    }
                    let slot = ConnectionSlot {
                        generation: failed_generation.saturating_add(1),
                        filesystem,
                    };
                    *self.connection.write().await = slot.clone();
                    self.failed_generation.store(0, Ordering::Release);
                    reconnect.last_failure = None;
                    return Ok(slot);
                }
                Err(_) if attempt + 1 < self.policy.attempts => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff
                        .checked_mul(2)
                        .unwrap_or(self.policy.maximum_backoff)
                        .min(self.policy.maximum_backoff);
                }
                Err(_) => {
                    reconnect.last_failure = Some((failed_generation, Instant::now()));
                    return Err(ClientError::Offline);
                }
            }
        }
        Err(ClientError::Offline)
    }

    async fn safe_call<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: Fn(Arc<dyn RemoteFilesystem>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
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
    }

    async fn mutation_call<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(Arc<dyn RemoteFilesystem>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let slot = self.healthy_connection().await?;
        match operation(slot.filesystem).await {
            Err(ClientError::Transport(_)) => {
                self.mark_failed(slot.generation);
                Err(ClientError::AmbiguousMutation)
            }
            result => result,
        }
    }

    async fn open_remote(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> Result<(ConnectionSlot, OpenedFile)> {
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
            let _ = current.filesystem.close_file(reopened.handle).await;
            return Err(ClientError::StaleRevision);
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
        let (slot, state) = self.remote_handle(handle).await?;
        match slot
            .filesystem
            .read_range_versioned(state.remote, offset, length)
            .await
        {
            Ok(read) if read.revision == state.revision => Ok(read),
            Ok(_) => Err(ClientError::StaleRevision),
            Err(ClientError::Transport(_)) => {
                self.mark_failed(slot.generation);
                let (_, reopened) = self.remote_handle(handle).await?;
                let replacement = self.healthy_connection().await?;
                let read = replacement
                    .filesystem
                    .read_range_versioned(reopened.remote, offset, length)
                    .await?;
                if read.revision == reopened.revision {
                    Ok(read)
                } else {
                    Err(ClientError::StaleRevision)
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<WriteResult> {
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
    }

    async fn flush_file(&self, handle: FileHandle, lock_owner: Option<u64>) -> Result<()> {
        let (slot, state) = self.remote_handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        match slot.filesystem.flush_file(state.remote, lock_owner).await {
            Err(ClientError::Transport(_)) => {
                self.mark_failed(slot.generation);
                Err(ClientError::AmbiguousMutation)
            }
            result => result,
        }
    }

    async fn sync_file(&self, handle: FileHandle, data_only: bool) -> Result<()> {
        let (slot, state) = self.remote_handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        match slot.filesystem.sync_file(state.remote, data_only).await {
            Err(ClientError::Transport(_)) => {
                self.mark_failed(slot.generation);
                Err(ClientError::AmbiguousMutation)
            }
            result => result,
        }
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
    }

    async fn seek_file(&self, handle: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
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
    }

    async fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64> {
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
    }

    async fn set_lock(&self, handle: FileHandle, lock: FileLock, wait: bool) -> Result<()> {
        let (slot, state) = self.remote_handle(handle).await?;
        match slot.filesystem.set_lock(state.remote, lock, wait).await {
            Ok(()) => {
                let mut handles = self.handles.write().await;
                if let Some(current) = handles.get_mut(&handle) {
                    current.lock_history.push(lock);
                    if lock.kind == LockKind::Unlock && current.lock_history.len() > 4_096 {
                        current.lock_history.clear();
                    }
                }
                Ok(())
            }
            Err(ClientError::Transport(_)) => {
                self.mark_failed(slot.generation);
                Err(ClientError::AmbiguousMutation)
            }
            Err(error) => Err(error),
        }
    }

    async fn close_file(&self, handle: FileHandle) -> Result<()> {
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
    }

    #[async_trait]
    impl RemoteFilesystemConnector for QueueConnector {
        async fn connect(&self) -> Result<Arc<dyn RemoteFilesystem>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
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
        })
    }

    fn test_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            attempts: 1,
            initial_backoff: Duration::ZERO,
            maximum_backoff: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn reconnects_and_retries_safe_operations() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 1));
        first.fail_ping.store(true, Ordering::SeqCst);
        let second = Arc::new(ScriptedFilesystem::new(epoch, 1));
        let connector = connector(second);
        let filesystem = ResilientFilesystem::new(first, connector.clone(), test_policy())
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
        });
        let filesystem = ResilientFilesystem::new(
            first,
            connector.clone(),
            ReconnectPolicy {
                attempts: 1,
                initial_backoff: Duration::ZERO,
                maximum_backoff: Duration::from_secs(1),
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
        let filesystem = ResilientFilesystem::new(first, connector, test_policy())
            .await
            .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        assert_eq!(filesystem.read_range(handle, 2, 4).await.unwrap(), b"2345");
        assert_eq!(second.opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejects_mixed_revision_reads_after_reconnect() {
        let epoch = Uuid::new_v4();
        let first = Arc::new(ScriptedFilesystem::new(epoch, 3));
        first.fail_read.store(true, Ordering::SeqCst);
        let second = Arc::new(ScriptedFilesystem::new(epoch, 4));
        let filesystem = ResilientFilesystem::new(first, connector(second), test_policy())
            .await
            .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

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
        let filesystem = ResilientFilesystem::new(first, connector.clone(), test_policy())
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
        let filesystem =
            ResilientFilesystem::new(first.clone(), connector(replacement.clone()), test_policy())
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

    #[test]
    fn transport_failures_remain_distinct_from_server_errors() {
        assert!(!matches!(
            ClientError::Server(ErrorCode::Timeout, "server timeout".into()),
            ClientError::Transport(_)
        ));
    }
}
