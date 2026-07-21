// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

#[cfg(all(unix, not(target_vendor = "apple")))]
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dashmap::DashMap;
use quickfs_common::{Limits, validate_filename, validate_range};
use quickfs_protocol::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{File as StdFile, FileTimes, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{
    Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, RwLock as AsyncRwLock, Semaphore,
};
use tokio::task::JoinSet;
use uuid::Uuid;

#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};

#[cfg(unix)]
use rustix::fs::{AtFlags, FallocateFlags, FileType, Mode, OFlags, RenameFlags, XattrFlags};
#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};

const MAX_LOCK_RECORDS: usize = 65_536;
const MAX_FORGET_NODES: usize = 65_536;
const MAX_SYMLINK_TARGET_SIZE: usize = 16 * 1024;
const MAX_XATTR_SIZE: usize = 64 * 1024 * 1024;
const BACKUP_TIME_XATTR: &[u8] = b"user.quickfs.backup-time";

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("directory is not empty")]
    NotEmpty,
    #[error("extended attribute does not exist")]
    NoAttribute,
    #[error("no data exists at or after the requested offset")]
    NoData,
    #[error("inappropriate ioctl for this object")]
    NotTty,
    #[error("the export is read-only")]
    ReadOnly,
    #[error("the object changed during the operation")]
    Conflict,
    #[error("the requested lock would block")]
    WouldBlock,
    #[error("no space is available")]
    NoSpace,
    #[error("resource is busy")]
    Busy,
    #[error("operation is not supported")]
    NotSupported,
    #[error("invalid node")]
    InvalidNode,
    #[error("invalid handle")]
    InvalidHandle,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("too many handles")]
    TooManyHandles,
    #[error("too many known nodes")]
    TooManyNodes,
    #[error("too many advisory locks")]
    TooManyLocks,
    #[error("directory listing exceeds the control-frame limit")]
    DirectoryTooLarge,
    #[error("server state is unavailable")]
    StateUnavailable,
    #[error("filesystem task failed")]
    TaskFailed,
    #[error("I/O: {0}")]
    Io(std::io::Error),
}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        #[cfg(unix)]
        if let Some(raw) = error.raw_os_error() {
            use rustix::io::Errno;
            let errno = Errno::from_raw_os_error(raw);
            return match errno {
                Errno::NOENT => Self::NotFound,
                Errno::ACCESS | Errno::PERM | Errno::LOOP => Self::PermissionDenied,
                Errno::EXIST => Self::AlreadyExists,
                Errno::NOTDIR => Self::NotDirectory,
                Errno::ISDIR => Self::IsDirectory,
                Errno::NOTEMPTY => Self::NotEmpty,
                #[cfg(any(target_os = "linux", target_os = "android"))]
                Errno::NODATA => Self::NoAttribute,
                #[cfg(target_vendor = "apple")]
                Errno::NOATTR => Self::NoAttribute,
                Errno::NXIO => Self::NoData,
                Errno::NOTTY => Self::NotTty,
                Errno::ROFS => Self::ReadOnly,
                Errno::NOSPC => Self::NoSpace,
                Errno::BUSY => Self::Busy,
                Errno::NOTSUP => Self::NotSupported,
                Errno::AGAIN => Self::WouldBlock,
                _ => Self::Io(error),
            };
        }
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            std::io::ErrorKind::InvalidInput => Self::InvalidRequest(error.to_string()),
            _ => Self::Io(error),
        }
    }
}

impl ServerError {
    pub fn protocol(&self) -> ProtocolError {
        let code = match self {
            Self::NotFound => ErrorCode::NotFound,
            Self::PermissionDenied => ErrorCode::PermissionDenied,
            Self::AlreadyExists => ErrorCode::AlreadyExists,
            Self::NotDirectory => ErrorCode::NotDirectory,
            Self::IsDirectory => ErrorCode::IsDirectory,
            Self::NotEmpty => ErrorCode::NotEmpty,
            Self::NoAttribute => ErrorCode::NoAttribute,
            Self::NoData => ErrorCode::NoData,
            Self::NotTty => ErrorCode::NotTty,
            Self::ReadOnly => ErrorCode::ReadOnly,
            Self::Conflict => ErrorCode::Conflict,
            Self::WouldBlock => ErrorCode::WouldBlock,
            Self::NoSpace => ErrorCode::NoSpace,
            Self::Busy => ErrorCode::Busy,
            Self::NotSupported => ErrorCode::NotSupported,
            Self::InvalidNode => ErrorCode::InvalidNode,
            Self::InvalidHandle => ErrorCode::InvalidHandle,
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::TooManyHandles
            | Self::TooManyNodes
            | Self::TooManyLocks
            | Self::DirectoryTooLarge => ErrorCode::TooLarge,
            Self::StateUnavailable | Self::TaskFailed | Self::Io(_) => ErrorCode::Internal,
        };
        ProtocolError {
            code,
            message: if matches!(
                self,
                Self::StateUnavailable | Self::TaskFailed | Self::Io(_)
            ) {
                "internal server error".into()
            } else {
                self.to_string()
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: 0,
        inode: metadata.len(),
    }
}

struct OpenFile {
    file: StdFile,
    node: NodeId,
    identity: FileIdentity,
    access: FileAccess,
    append: bool,
    operation_gate: Arc<AsyncRwLock<()>>,
    append_operation: Arc<AsyncMutex<()>>,
    _permit: OwnedSemaphorePermit,
}

struct KnownNode {
    identity: FileIdentity,
    paths: BTreeSet<PathBuf>,
    session_references: usize,
    _global_permit: Option<OwnedSemaphorePermit>,
}

struct NodeRegistry {
    by_node: HashMap<NodeId, KnownNode>,
    by_identity: HashMap<FileIdentity, NodeId>,
}

#[derive(Clone)]
struct NodeSnapshot {
    identity: FileIdentity,
    paths: Vec<PathBuf>,
}

#[cfg(unix)]
struct PreparedDirectoryScan {
    directory: Arc<StdFile>,
    relative: PathBuf,
    before: std::fs::Metadata,
    names: Vec<Vec<u8>>,
}

#[cfg(unix)]
struct ScannedDirectoryEntry {
    identity: FileIdentity,
    relative: PathBuf,
    entry: DirectoryEntry,
    xattrs: Option<XattrSnapshot>,
}

#[derive(Clone)]
struct LockRecord {
    identity: FileIdentity,
    session: Uuid,
    lock: FileLock,
}

struct ExportShared {
    #[cfg(not(unix))]
    root: PathBuf,
    #[cfg(unix)]
    root_file: StdFile,
    limits: Limits,
    writable: bool,
    epoch: Uuid,
    node_key: [u8; 32],
    volume_name: Mutex<String>,
    persistence: Option<PersistentExportState>,
    handle_permits: Arc<Semaphore>,
    node_permits: Arc<Semaphore>,
    directory_entry_permits: Arc<Semaphore>,
    nodes: Mutex<NodeRegistry>,
    locks: Mutex<Vec<LockRecord>>,
    lock_notify: Notify,
    append_operations: Mutex<HashMap<FileIdentity, Weak<AsyncMutex<()>>>>,
    content_operations: Mutex<HashMap<FileIdentity, Weak<AsyncRwLock<()>>>>,
}

#[derive(Clone)]
struct PersistentExportState {
    path: PathBuf,
    marker: String,
    resource_forks: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct PersistentExportRecord {
    version: u32,
    marker: String,
    epoch: Uuid,
    node_key: String,
    volume_name: String,
}

#[derive(Clone)]
pub struct Export {
    shared: Arc<ExportShared>,
}

/// Per-connection filesystem state. File handles and advisory-lock ownership
/// are scoped to a session, while opaque node identifiers live on the Export
/// so a reconnect to the same server epoch can keep using cached inodes.
pub struct ExportSession {
    export: Export,
    id: Uuid,
    write_authorized: AtomicBool,
    known_nodes: Mutex<HashMap<NodeId, Option<OwnedSemaphorePermit>>>,
    node_permits: Arc<Semaphore>,
    handles: DashMap<FileHandle, Arc<OpenFile>>,
}

impl Export {
    /// Open a read-only export, preserving the v3 construction behavior.
    pub async fn new(root: impl AsRef<Path>, limits: Limits) -> Result<Self, ServerError> {
        Self::new_with_writes(root, limits, false).await
    }

    /// Open an explicitly writable export.
    pub async fn new_writable(root: impl AsRef<Path>, limits: Limits) -> Result<Self, ServerError> {
        Self::new_with_writes(root, limits, true).await
    }

    pub async fn new_with_writes(
        root: impl AsRef<Path>,
        limits: Limits,
        writable: bool,
    ) -> Result<Self, ServerError> {
        Self::new_with_optional_state(root, limits, writable, None).await
    }

    /// Open an export whose epoch and opaque node-ID key survive daemon
    /// restarts. The state file must be outside the exported tree.
    pub async fn new_persistent_with_writes(
        root: impl AsRef<Path>,
        state_file: impl AsRef<Path>,
        limits: Limits,
        writable: bool,
    ) -> Result<Self, ServerError> {
        Self::new_with_optional_state(
            root,
            limits,
            writable,
            Some(state_file.as_ref().to_path_buf()),
        )
        .await
    }

    async fn new_with_optional_state(
        root: impl AsRef<Path>,
        limits: Limits,
        writable: bool,
        state_file: Option<PathBuf>,
    ) -> Result<Self, ServerError> {
        validate_limits(&limits)?;
        let root = tokio::fs::canonicalize(root).await?;
        let root_metadata = tokio::fs::metadata(&root).await?;
        if !root_metadata.is_dir() {
            return Err(ServerError::InvalidRequest(
                "export root is not a directory".into(),
            ));
        }

        #[cfg(unix)]
        let root_file = open_root(&root)?;

        #[cfg(unix)]
        let root_identity = {
            let opened_metadata = root_file.metadata()?;
            if identity(&opened_metadata) != identity(&root_metadata) {
                return Err(ServerError::Conflict);
            }
            identity(&opened_metadata)
        };
        #[cfg(not(unix))]
        let root_identity = identity(&root_metadata);
        let marker = export_marker(&root, root_identity);
        let (epoch, node_key, volume_name, persistence) = match state_file {
            Some(path) => {
                let record = load_or_create_export_record(&path, &marker)?;
                let key = decode_node_key(&record.node_key)?;
                let resource_forks = prepare_resource_fork_directory(&path)?;
                (
                    record.epoch,
                    key,
                    record.volume_name,
                    Some(PersistentExportState {
                        path,
                        marker,
                        resource_forks,
                    }),
                )
            }
            None => (
                Uuid::new_v4(),
                random_node_key(),
                "quicKFS".to_owned(),
                None,
            ),
        };
        let mut root_paths = BTreeSet::new();
        root_paths.insert(PathBuf::new());
        let root_node = KnownNode {
            identity: root_identity,
            paths: root_paths,
            session_references: usize::MAX,
            _global_permit: None,
        };
        Ok(Self {
            shared: Arc::new(ExportShared {
                #[cfg(not(unix))]
                root,
                #[cfg(unix)]
                root_file,
                handle_permits: Arc::new(Semaphore::new(limits.max_open_handles)),
                node_permits: Arc::new(Semaphore::new(
                    limits.max_total_known_nodes.saturating_sub(1),
                )),
                directory_entry_permits: Arc::new(Semaphore::new(limits.max_directory_entry_tasks)),
                nodes: Mutex::new(NodeRegistry {
                    by_node: HashMap::from([(ROOT_NODE, root_node)]),
                    by_identity: HashMap::from([(root_identity, ROOT_NODE)]),
                }),
                locks: Mutex::new(Vec::new()),
                lock_notify: Notify::new(),
                append_operations: Mutex::new(HashMap::new()),
                content_operations: Mutex::new(HashMap::new()),
                limits,
                writable,
                epoch,
                node_key,
                volume_name: Mutex::new(volume_name),
                persistence,
            }),
        })
    }

    pub fn capabilities(&self) -> FilesystemCapabilities {
        let volume_name = lock_mutex(&self.shared.volume_name)
            .map(|name| name.clone())
            .unwrap_or_else(|_| "quicKFS".into());
        FilesystemCapabilities {
            server_epoch: self.shared.epoch,
            writable: self.shared.writable,
            supports_locks: true,
            supports_atomic_rename: cfg!(unix),
            supports_directory_sync: cfg!(unix),
            supports_preallocation: cfg!(unix),
            supports_symlinks: cfg!(unix),
            supports_xattrs: cfg!(unix),
            supports_hard_links: cfg!(unix),
            supports_special_nodes: cfg!(any(target_os = "linux", target_os = "android")),
            supports_copy_file_range: true,
            supports_seek_data_hole: cfg!(unix),
            supports_safe_ioctl: true,
            supports_poll: true,
            supports_bmap: cfg!(unix),
            supports_exchange_data: cfg!(unix),
            supports_volume_rename: true,
            supports_backup_time: cfg!(unix),
            supports_readdirplus: true,
            persistent_node_ids: self.shared.persistence.is_some(),
            restart_lock_replay: self.shared.persistence.is_some(),
            volume_name,
            max_read_size: self.shared.limits.max_read_size,
            max_write_size: self.shared.limits.max_write_size,
        }
    }

    pub async fn stat_filesystem(&self) -> Result<FilesystemStats, ServerError> {
        #[cfg(unix)]
        {
            let stats =
                rustix::fs::fstatvfs(&self.shared.root_file).map_err(std::io::Error::from)?;
            Ok(FilesystemStats {
                blocks: stats.f_blocks,
                blocks_free: stats.f_bfree,
                blocks_available: stats.f_bavail,
                files: stats.f_files,
                files_free: stats.f_ffree,
                block_size: u32::try_from(stats.f_bsize).unwrap_or(u32::MAX),
                name_length: u32::try_from(stats.f_namemax).unwrap_or(u32::MAX),
                fragment_size: u32::try_from(stats.f_frsize).unwrap_or(u32::MAX),
            })
        }
        #[cfg(not(unix))]
        Err(ServerError::NotSupported)
    }

    pub fn session(&self) -> ExportSession {
        self.session_with_writes(false)
    }

    /// Create a connection session with an explicit write grant. The grant is
    /// still constrained by the export-wide writable setting.
    pub fn session_with_writes(&self, write_authorized: bool) -> ExportSession {
        ExportSession {
            node_permits: Arc::new(Semaphore::new(
                self.shared.limits.max_known_nodes.saturating_sub(1),
            )),
            export: self.clone(),
            id: Uuid::new_v4(),
            write_authorized: AtomicBool::new(write_authorized),
            known_nodes: Mutex::new(HashMap::from([(ROOT_NODE, None)])),
            handles: DashMap::new(),
        }
    }
}

impl ExportSession {
    pub fn session_id(&self) -> Uuid {
        self.id
    }

    pub fn capabilities(&self) -> FilesystemCapabilities {
        let mut capabilities = self.export.capabilities();
        capabilities.writable = self.can_write();
        capabilities
    }

    /// Update the authenticated principal's write grant. Mutating operations
    /// check this value on every call, including operations on existing handles.
    pub fn set_write_authorized(&self, authorized: bool) {
        self.write_authorized.store(authorized, Ordering::Release);
    }

    pub fn write_authorized(&self) -> bool {
        self.write_authorized.load(Ordering::Acquire)
    }

    pub async fn stat_filesystem(&self) -> Result<FilesystemStats, ServerError> {
        self.export.stat_filesystem().await
    }

    pub fn set_volume_name(&self, name: &Name) -> Result<(), ServerError> {
        self.require_writable()?;
        let value = std::str::from_utf8(name.as_bytes())
            .map_err(|_| ServerError::InvalidRequest("volume name must be UTF-8".into()))?;
        if value.is_empty() || value.len() > 255 || value.contains(['/', ':', ',', '\0']) {
            return Err(ServerError::InvalidRequest("invalid volume name".into()));
        }
        let mut current = lock_mutex(&self.export.shared.volume_name)?;
        if let Some(persistence) = &self.export.shared.persistence {
            let record = PersistentExportRecord {
                version: 1,
                marker: persistence.marker.clone(),
                epoch: self.export.shared.epoch,
                node_key: hex::encode(self.export.shared.node_key),
                volume_name: value.to_owned(),
            };
            save_export_record(&persistence.path, &record)?;
        }
        *current = value.to_owned();
        Ok(())
    }

    /// Drop this connection's references to nodes forgotten by the kernel.
    /// Unreferenced global entries remain as a bounded stable-ID cache and are
    /// evicted only when new node capacity is required.
    pub fn forget_nodes(&self, requested: &[NodeId]) -> Result<(), ServerError> {
        if requested.len() > MAX_FORGET_NODES {
            return Err(ServerError::InvalidRequest(
                "too many nodes in one forget request".into(),
            ));
        }
        let requested = requested.iter().copied().collect::<HashSet<_>>();
        let released = {
            let mut known = lock_mutex(&self.known_nodes)?;
            requested
                .into_iter()
                .filter(|node| *node != ROOT_NODE)
                .filter_map(|node| known.remove(&node).map(|_| node))
                .collect::<Vec<_>>()
        };
        for node in released {
            self.release_node_reference(node)?;
        }
        Ok(())
    }

    pub async fn metadata(&self, node: NodeId) -> Result<Metadata, ServerError> {
        let snapshot = self.node_snapshot(node)?;
        #[cfg(unix)]
        {
            if let Some(stat) = self.path_stat(&snapshot)? {
                let mut metadata = to_metadata_from_stat(node, &stat)?;
                metadata.backup_unix_ms = self.read_backup_time(&snapshot).ok().flatten();
                self.enrich_resource_fork_revision(node, &mut metadata);
                return Ok(metadata);
            }
            Err(ServerError::NotFound)
        }
        #[cfg(not(unix))]
        {
            let path = self.absolute_snapshot_path(&snapshot)?;
            Ok(to_metadata(node, &std::fs::symlink_metadata(path)?))
        }
    }

    pub async fn list(&self, node: NodeId) -> Result<Vec<DirectoryEntry>, ServerError> {
        Ok(self.list_with_revision(node).await?.1)
    }

    pub async fn list_with_revision(
        &self,
        node: NodeId,
    ) -> Result<(DirectoryRevision, Vec<DirectoryEntry>), ServerError> {
        #[cfg(unix)]
        {
            let options = DirectoryViewOptions::METADATA_ONLY;
            let (revision, _, entries) = self
                .scan_directory(node, options, Arc::new(AtomicUsize::new(0)))
                .await?;
            Ok((
                revision,
                entries.into_iter().map(|entry| entry.entry).collect(),
            ))
        }
        #[cfg(not(unix))]
        self.list_with_revision_blocking(node)
    }

    /// Build one revision-consistent native directory projection. Entry
    /// metadata and xattrs are discovered beside the export concurrently, so
    /// the client receives one response instead of issuing one request per
    /// Finder callback.
    pub async fn directory_view(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
    ) -> Result<DirectoryView, ServerError> {
        validate_directory_view_options(options)?;
        #[cfg(unix)]
        {
            let remaining_inline =
                Arc::new(AtomicUsize::new(options.inline_xattr_total_size as usize));
            let (view_revision, prepared, entries) = self
                .scan_directory(node, options, Arc::clone(&remaining_inline))
                .await?;
            let resource_forks = self
                .export
                .shared
                .persistence
                .as_ref()
                .map(|state| state.resource_forks.clone());
            let mut directory = to_metadata(node, &prepared.before);
            directory.backup_unix_ms = read_fd_backup_time(&prepared.directory).ok().flatten();
            enrich_resource_fork_revision_at(resource_forks.as_deref(), node, &mut directory);

            let xattrs = if options.include_xattrs {
                let opened = Arc::clone(&prepared.directory);
                let remaining = Arc::clone(&remaining_inline);
                let resource_forks = resource_forks.clone();
                Some(
                    blocking(move || {
                        xattr_snapshot_for_open_file(
                            &opened,
                            node,
                            resource_forks.as_deref(),
                            options.inline_xattr_size as usize,
                            &remaining,
                        )
                    })
                    .await?,
                )
            } else {
                None
            };

            let parent = if prepared.relative.as_os_str().is_empty() {
                directory.clone()
            } else {
                let parent_path = prepared
                    .relative
                    .parent()
                    .unwrap_or_else(|| Path::new(""))
                    .to_path_buf();
                let root = self.export.shared.root_file.try_clone()?;
                let node_key = self.export.shared.node_key;
                let resource_forks = resource_forks.clone();
                let scanned_parent_path = parent_path.clone();
                let (identity, mut metadata) = blocking(move || {
                    let stat = secure_lstat(&root, &scanned_parent_path)?;
                    let identity = identity_from_stat(&stat);
                    let parent_node = stable_node_id(node_key, identity);
                    let mut metadata = to_metadata_from_stat(parent_node, &stat)?;
                    enrich_resource_fork_revision_at(
                        resource_forks.as_deref(),
                        parent_node,
                        &mut metadata,
                    );
                    Ok((identity, metadata))
                })
                .await?;
                let parent_node = self.remember(identity, parent_path)?;
                metadata.node = parent_node;
                metadata
            };

            let opened = Arc::clone(&prepared.directory);
            let final_revision = blocking(move || Ok(revision(&opened.metadata()?))).await?;
            if final_revision != view_revision {
                return Err(ServerError::Conflict);
            }

            Ok(DirectoryView {
                revision: view_revision,
                directory,
                parent,
                xattrs,
                entries: entries
                    .into_iter()
                    .map(|entry| DirectoryViewEntry {
                        entry: entry.entry,
                        xattrs: entry.xattrs,
                    })
                    .collect(),
            })
        }
        #[cfg(not(unix))]
        {
            let (revision, entries) = self.list_with_revision_blocking(node)?;
            let directory = self.metadata(node).await?;
            Ok(DirectoryView {
                revision,
                parent: directory.clone(),
                directory,
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
    }

    #[cfg(unix)]
    async fn scan_directory(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
        remaining_inline: Arc<AtomicUsize>,
    ) -> Result<
        (
            DirectoryRevision,
            PreparedDirectoryScan,
            Vec<ScannedDirectoryEntry>,
        ),
        ServerError,
    > {
        let snapshot = self.node_snapshot(node)?;
        let root = self.export.shared.root_file.try_clone()?;
        let prepared = blocking(move || prepare_directory_scan(&root, &snapshot)).await?;
        let node_key = self.export.shared.node_key;
        let resource_forks = self
            .export
            .shared
            .persistence
            .as_ref()
            .map(|state| state.resource_forks.clone());
        let mut tasks = JoinSet::new();
        for name in &prepared.names {
            let permit = self
                .export
                .shared
                .directory_entry_permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ServerError::StateUnavailable)?;
            let directory = Arc::clone(&prepared.directory);
            let relative = prepared.relative.clone();
            let name = name.clone();
            let resource_forks = resource_forks.clone();
            let remaining_inline = Arc::clone(&remaining_inline);
            tasks.spawn_blocking(move || {
                let _permit = permit;
                scan_directory_entry(
                    &directory,
                    &relative,
                    name,
                    node_key,
                    resource_forks.as_deref(),
                    options,
                    &remaining_inline,
                )
            });
        }

        let mut entries = Vec::with_capacity(prepared.names.len());
        while let Some(result) = tasks.join_next().await {
            entries.push(result.map_err(|_| ServerError::TaskFailed)??);
        }
        let directory = Arc::clone(&prepared.directory);
        let after = blocking(move || Ok(directory.metadata()?)).await?;
        let after_revision = revision(&after);
        if revision(&prepared.before) != after_revision {
            return Err(ServerError::Conflict);
        }

        for scanned in &mut entries {
            let remembered = self.remember(scanned.identity, scanned.relative.clone())?;
            scanned.entry.node = remembered;
            scanned.entry.metadata.node = remembered;
        }
        entries.sort_by(|left, right| left.entry.name.cmp(&right.entry.name));
        Ok((after_revision, prepared, entries))
    }

    /// Perform the descriptor-relative directory walk. Network servers should
    /// call this on a blocking worker because cold or rotational exports can
    /// spend seconds in `readdir`/`statat`.
    pub fn list_with_revision_blocking(
        &self,
        node: NodeId,
    ) -> Result<(DirectoryRevision, Vec<DirectoryEntry>), ServerError> {
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let (directory, relative, before) = self.open_snapshot(
                &snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let before_revision = revision(&before);
            let stream = rustix::fs::Dir::read_from(&directory).map_err(std::io::Error::from)?;
            let mut discovered = Vec::new();
            for entry in stream {
                let entry = entry.map_err(std::io::Error::from)?;
                let bytes = entry.file_name().to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let name = OsStr::from_bytes(bytes);
                let stat = rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(std::io::Error::from)?;
                let kind = node_kind_from_mode(stat.st_mode)?;
                if discovered.len() >= MAX_DIRECTORY_ENTRIES {
                    return Err(ServerError::DirectoryTooLarge);
                }
                let child_identity = identity_from_stat(&stat);
                let child_path = relative.join(name);
                let child_node = self.remember(child_identity, child_path)?;
                let mut metadata = to_metadata_from_stat(child_node, &stat)?;
                self.enrich_resource_fork_revision(child_node, &mut metadata);
                discovered.push(DirectoryEntry {
                    node: child_node,
                    name: Name::new(bytes.to_vec()),
                    kind,
                    metadata,
                });
            }
            let after = directory.metadata()?;
            let after_revision = revision(&after);
            if before_revision != after_revision {
                return Err(ServerError::Conflict);
            }
            discovered.sort_by(|left, right| left.name.cmp(&right.name));
            Ok((after_revision, discovered))
        }
        #[cfg(not(unix))]
        {
            let _ = node;
            Err(ServerError::NotSupported)
        }
    }

    pub async fn open(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> Result<(FileHandle, u64, u64), ServerError> {
        validate_open_options(options, self.can_write())?;
        let permit = self
            .export
            .shared
            .handle_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServerError::TooManyHandles)?;
        let snapshot = self.node_snapshot(node)?;
        #[cfg(unix)]
        let flags = open_flags(options.access, options.append);
        #[cfg(unix)]
        let (file, _, _metadata) = self.open_snapshot(&snapshot, flags, Some(NodeKind::File))?;
        #[cfg(not(unix))]
        let (file, metadata) = {
            let path = self.absolute_snapshot_path(&snapshot)?;
            let file = std::fs::OpenOptions::new()
                .read(options.access.can_read())
                .write(options.access.can_write())
                .append(options.append)
                .open(path)?;
            let metadata = file.metadata()?;
            (file, metadata)
        };
        if options.truncate {
            file.set_len(0)?;
        }
        let metadata = file.metadata()?;
        let handle = self.insert_handle(OpenFile {
            file,
            node,
            identity: snapshot.identity,
            access: options.access,
            append: options.append,
            operation_gate: self.content_operation(snapshot.identity)?,
            append_operation: self.append_operation(snapshot.identity)?,
            _permit: permit,
        });
        Ok((handle, revision(&metadata), metadata.len()))
    }

    pub async fn create_file(
        &self,
        parent: NodeId,
        name: impl AsRef<[u8]>,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<(Metadata, FileHandle, u64, u64), ServerError> {
        let name = name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        validate_open_options(options, true)?;
        let handle_permit = self
            .export
            .shared
            .handle_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServerError::TooManyHandles)?;
        let (session_permit, global_permit) = self.reserve_node()?;
        #[cfg(unix)]
        {
            let name = OsStr::from_bytes(name);
            let parent_snapshot = self.node_snapshot(parent)?;
            let (directory, parent_path, _) = self.open_snapshot(
                &parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let mut flags = open_flags(options.access, options.append);
            flags |= OFlags::CREATE | OFlags::EXCL;
            let owned = rustix::fs::openat(
                &directory,
                name,
                flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode((mode & 0o7777) as rustix::fs::RawMode),
            )
            .map_err(std::io::Error::from)?;
            let file = StdFile::from(owned);
            let metadata = file.metadata()?;
            let file_identity = identity(&metadata);
            let node = self.remember_reserved(
                file_identity,
                parent_path.join(name),
                session_permit,
                global_permit,
            )?;
            let protocol_metadata = to_metadata(node, &metadata);
            let handle = self.insert_handle(OpenFile {
                file,
                node,
                identity: file_identity,
                access: options.access,
                append: options.append,
                operation_gate: self.content_operation(file_identity)?,
                append_operation: self.append_operation(file_identity)?,
                _permit: handle_permit,
            });
            Ok((
                protocol_metadata,
                handle,
                revision(&metadata),
                metadata.len(),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = (
                parent,
                name,
                mode,
                options,
                handle_permit,
                session_permit,
                global_permit,
            );
            Err(ServerError::NotSupported)
        }
    }

    pub async fn create_directory(
        &self,
        parent: NodeId,
        name: impl AsRef<[u8]>,
        mode: u32,
    ) -> Result<Metadata, ServerError> {
        let name = name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        let (session_permit, global_permit) = self.reserve_node()?;
        #[cfg(unix)]
        {
            let name = OsStr::from_bytes(name);
            let parent_snapshot = self.node_snapshot(parent)?;
            let (directory, parent_path, _) = self.open_snapshot(
                &parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            rustix::fs::mkdirat(
                &directory,
                name,
                Mode::from_raw_mode((mode & 0o7777) as rustix::fs::RawMode),
            )
            .map_err(std::io::Error::from)?;
            let path = parent_path.join(name);
            let (child, _, metadata) = open_relative(
                &self.export.shared.root_file,
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY,
            )?;
            drop(child);
            let node =
                self.remember_reserved(identity(&metadata), path, session_permit, global_permit)?;
            Ok(to_metadata(node, &metadata))
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name, mode, session_permit, global_permit);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn create_symlink(
        &self,
        parent: NodeId,
        name: impl AsRef<[u8]>,
        target: &[u8],
    ) -> Result<Metadata, ServerError> {
        let name = name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        validate_symlink_target(target)?;
        let (session_permit, global_permit) = self.reserve_node()?;
        #[cfg(unix)]
        {
            let name = OsStr::from_bytes(name);
            let parent_snapshot = self.node_snapshot(parent)?;
            let (directory, parent_path, _) = self.open_snapshot(
                &parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let target = OsStr::from_bytes(target);
            rustix::fs::symlinkat(target, &directory, name).map_err(std::io::Error::from)?;
            let path = parent_path.join(name);
            let stat = secure_lstat(&self.export.shared.root_file, &path)?;
            let file_identity = identity_from_stat(&stat);
            let node =
                self.remember_reserved(file_identity, path, session_permit, global_permit)?;
            self.metadata(node).await
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name, target, session_permit, global_permit);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn create_hard_link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: impl AsRef<[u8]>,
    ) -> Result<Metadata, ServerError> {
        let new_name = new_name.as_ref();
        self.require_writable()?;
        validate_name(new_name)?;
        #[cfg(unix)]
        {
            let source = self.node_snapshot(node)?;
            let new_parent = self.node_snapshot(new_parent)?;
            let (new_directory, new_parent_path, _) = self.open_snapshot(
                &new_parent,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let new_name = OsStr::from_bytes(new_name);
            for source_path in &source.paths {
                if source_path.as_os_str().is_empty() {
                    return Err(ServerError::PermissionDenied);
                }
                let Ok(stat) = secure_lstat(&self.export.shared.root_file, source_path) else {
                    continue;
                };
                if identity_from_stat(&stat) != source.identity {
                    continue;
                }
                if node_kind_from_mode(stat.st_mode)? == NodeKind::Directory {
                    return Err(ServerError::PermissionDenied);
                }
                let (source_parent, source_name) =
                    open_parent(&self.export.shared.root_file, source_path)?;
                rustix::fs::linkat(
                    &source_parent,
                    &source_name,
                    &new_directory,
                    new_name,
                    AtFlags::empty(),
                )
                .map_err(std::io::Error::from)?;
                self.remember(source.identity, new_parent_path.join(new_name))?;
                return self.metadata(node).await;
            }
            Err(ServerError::NotFound)
        }
        #[cfg(not(unix))]
        {
            let _ = (node, new_parent, new_name);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn create_special_node(
        &self,
        parent: NodeId,
        name: impl AsRef<[u8]>,
        kind: SpecialNodeKind,
        mode: u32,
        device_major: u32,
        device_minor: u32,
    ) -> Result<Metadata, ServerError> {
        let name = name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        let (session_permit, global_permit) = self.reserve_node()?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let parent_snapshot = self.node_snapshot(parent)?;
            let (directory, parent_path, _) = self.open_snapshot(
                &parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let name = OsStr::from_bytes(name);
            let file_type = match kind {
                SpecialNodeKind::NamedPipe => FileType::Fifo,
                SpecialNodeKind::CharacterDevice => FileType::CharacterDevice,
                SpecialNodeKind::BlockDevice => FileType::BlockDevice,
                SpecialNodeKind::Socket => FileType::Socket,
            };
            let device = rustix::fs::makedev(device_major, device_minor);
            rustix::fs::mknodat(
                &directory,
                name,
                file_type,
                Mode::from_raw_mode((mode & 0o7777) as rustix::fs::RawMode),
                device,
            )
            .map_err(std::io::Error::from)?;
            let path = parent_path.join(name);
            let stat = secure_lstat(&self.export.shared.root_file, &path)?;
            let node = self.remember_reserved(
                identity_from_stat(&stat),
                path,
                session_permit,
                global_permit,
            )?;
            self.metadata(node).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = (
                parent,
                name,
                kind,
                mode,
                device_major,
                device_minor,
                session_permit,
                global_permit,
            );
            Err(ServerError::NotSupported)
        }
    }

    pub async fn remove_node(
        &self,
        parent: NodeId,
        name: impl AsRef<[u8]>,
        directory: bool,
    ) -> Result<(), ServerError> {
        let name = name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        #[cfg(unix)]
        {
            let name = OsStr::from_bytes(name);
            let parent_snapshot = self.node_snapshot(parent)?;
            let (parent_file, parent_path, _) = self.open_snapshot(
                &parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let stat = rustix::fs::statat(&parent_file, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let actual_kind = node_kind_from_mode(stat.st_mode)?;
            let removed_node =
                stable_node_id(self.export.shared.node_key, identity_from_stat(&stat));
            let final_link = stat.st_nlink <= 1;
            if directory && actual_kind != NodeKind::Directory {
                return Err(ServerError::NotDirectory);
            }
            if !directory && actual_kind == NodeKind::Directory {
                return Err(ServerError::IsDirectory);
            }
            rustix::fs::unlinkat(
                &parent_file,
                name,
                if directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            )
            .map_err(std::io::Error::from)?;
            self.remove_registry_path(&parent_path.join(name), directory)?;
            if final_link && let Some(resource_fork) = self.resource_fork_path(removed_node) {
                let _ = remove_resource_fork(&resource_fork);
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name, directory);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn rename_node<N: AsRef<[u8]>, M: AsRef<[u8]>>(
        &self,
        parent: NodeId,
        name: N,
        new_parent: NodeId,
        new_name: M,
        mode: RenameMode,
    ) -> Result<(), ServerError> {
        let name = name.as_ref();
        let new_name = new_name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        validate_name(new_name)?;
        #[cfg(unix)]
        {
            let name = OsStr::from_bytes(name);
            let new_name = OsStr::from_bytes(new_name);
            let old_parent_snapshot = self.node_snapshot(parent)?;
            let new_parent_snapshot = self.node_snapshot(new_parent)?;
            let (old_directory, old_parent_path, _) = self.open_snapshot(
                &old_parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let (new_directory, new_parent_path, _) = self.open_snapshot(
                &new_parent_snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            let source = rustix::fs::statat(&old_directory, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let source_identity = identity_from_stat(&source);
            let destination_identity =
                match rustix::fs::statat(&new_directory, new_name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) => Some(identity_from_stat(&stat)),
                    Err(rustix::io::Errno::NOENT) => None,
                    Err(error) => return Err(std::io::Error::from(error).into()),
                };
            match mode {
                RenameMode::Replace => {
                    rustix::fs::renameat(&old_directory, name, &new_directory, new_name)
                }
                RenameMode::NoReplace => rustix::fs::renameat_with(
                    &old_directory,
                    name,
                    &new_directory,
                    new_name,
                    RenameFlags::NOREPLACE,
                ),
                RenameMode::Exchange => rustix::fs::renameat_with(
                    &old_directory,
                    name,
                    &new_directory,
                    new_name,
                    RenameFlags::EXCHANGE,
                ),
            }
            .map_err(std::io::Error::from)?;
            let old_path = old_parent_path.join(name);
            let new_path = new_parent_path.join(new_name);
            self.update_registry_after_rename(
                &old_path,
                &new_path,
                mode,
                source_identity,
                destination_identity,
            )?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name, new_parent, new_name, mode);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn read_link(&self, node: NodeId) -> Result<Vec<u8>, ServerError> {
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            for path in &snapshot.paths {
                let Ok(stat) = secure_lstat(&self.export.shared.root_file, path) else {
                    continue;
                };
                if identity_from_stat(&stat) != snapshot.identity
                    || node_kind_from_mode(stat.st_mode)? != NodeKind::Symlink
                {
                    continue;
                }
                let (parent, name) = open_parent(&self.export.shared.root_file, path)?;
                let target = rustix::fs::readlinkat(&parent, &name, Vec::new())
                    .map_err(std::io::Error::from)?
                    .into_bytes();
                validate_symlink_target(&target)?;
                return Ok(target);
            }
            Err(ServerError::NotFound)
        }
        #[cfg(not(unix))]
        {
            let _ = node;
            Err(ServerError::NotSupported)
        }
    }

    pub async fn get_xattr(
        &self,
        node: NodeId,
        name: &Name,
        offset: u64,
        length: u64,
    ) -> Result<(u64, Vec<u8>), ServerError> {
        validate_xattr_name(name)?;
        validate_range(offset, length, self.export.shared.limits.max_read_size)
            .map_err(|error| ServerError::InvalidRequest(error.to_string()))?;
        if is_resource_fork(name)
            && let Some(path) = self.resource_fork_path(node)
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.read().await;
            return blocking(move || read_resource_fork_range(&path, offset, length)).await;
        }
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.read().await;
            let file = self.open_xattr_file(&snapshot)?;
            let host_name = logical_xattr_name(name)?;
            blocking(move || {
                let value = read_fd_xattr(&file, &host_name, MAX_XATTR_SIZE)?;
                let total = value.len() as u64;
                let start = usize::try_from(offset.min(total))
                    .map_err(|_| ServerError::InvalidRequest("xattr offset overflow".into()))?;
                let end = usize::try_from(offset.saturating_add(length).min(total))
                    .map_err(|_| ServerError::InvalidRequest("xattr range overflow".into()))?;
                Ok((total, value[start..end].to_vec()))
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = (node, name, offset, length);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn list_xattrs(&self, node: NodeId) -> Result<Vec<Name>, ServerError> {
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.read().await;
            let file = self.open_xattr_file(&snapshot)?;
            let mut names = blocking(move || list_fd_xattrs(&file)).await?;
            if self
                .resource_fork_path(node)
                .is_some_and(|path| path.is_file())
                && !names.iter().any(is_resource_fork)
            {
                names.push(Name::from("com.apple.ResourceFork"));
                names.sort();
            }
            Ok(names)
        }
        #[cfg(not(unix))]
        {
            let _ = node;
            Err(ServerError::NotSupported)
        }
    }

    pub async fn set_xattr(
        &self,
        node: NodeId,
        name: &Name,
        value: Vec<u8>,
        mode: XattrSetMode,
        position: u32,
    ) -> Result<(), ServerError> {
        self.require_writable()?;
        validate_xattr_name(name)?;
        if value.len() > MAX_XATTR_SIZE {
            return Err(ServerError::InvalidRequest(
                "extended attribute is too large".into(),
            ));
        }
        if is_resource_fork(name)
            && let Some(path) = self.resource_fork_path(node)
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.write().await;
            return blocking(move || write_resource_fork(&path, &value, mode, position)).await;
        }
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.write().await;
            let file = self.open_xattr_file(&snapshot)?;
            let host_name = logical_xattr_name(name)?;
            blocking(move || {
                let existing = match read_fd_xattr(&file, &host_name, MAX_XATTR_SIZE) {
                    Ok(existing) => Some(existing),
                    Err(ServerError::NoAttribute) => None,
                    Err(error) => return Err(error),
                };
                match (mode, existing.is_some()) {
                    (XattrSetMode::Create, true) => return Err(ServerError::AlreadyExists),
                    (XattrSetMode::Replace, false) => return Err(ServerError::NoAttribute),
                    _ => {}
                }
                let value = if position == 0 {
                    value
                } else {
                    let start = position as usize;
                    let end = start.checked_add(value.len()).ok_or_else(|| {
                        ServerError::InvalidRequest("xattr range overflow".into())
                    })?;
                    if end > MAX_XATTR_SIZE {
                        return Err(ServerError::InvalidRequest(
                            "extended attribute is too large".into(),
                        ));
                    }
                    let mut merged = existing.unwrap_or_default();
                    if merged.len() < end {
                        merged.resize(end, 0);
                    }
                    merged[start..end].copy_from_slice(&value);
                    merged
                };
                rustix::fs::fsetxattr(&file, &host_name, &value, XattrFlags::empty())
                    .map_err(std::io::Error::from)?;
                Ok(())
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = (node, name, value, mode, position);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn remove_xattr(&self, node: NodeId, name: &Name) -> Result<(), ServerError> {
        self.require_writable()?;
        validate_xattr_name(name)?;
        if is_resource_fork(name)
            && let Some(path) = self.resource_fork_path(node)
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.write().await;
            return blocking(move || remove_resource_fork(&path)).await;
        }
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.write().await;
            let file = self.open_xattr_file(&snapshot)?;
            let host_name = logical_xattr_name(name)?;
            blocking(move || {
                rustix::fs::fremovexattr(&file, &host_name).map_err(std::io::Error::from)?;
                Ok(())
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = (node, name);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn set_attributes(
        &self,
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> Result<Metadata, ServerError> {
        if changes.size.is_none()
            && changes.mode.is_none()
            && changes.accessed_unix_ms.is_none()
            && changes.modified_unix_ms.is_none()
            && changes.backup_unix_ms.is_none()
        {
            return self.metadata(node).await;
        }
        self.require_writable()?;
        let accessed = changes
            .accessed_unix_ms
            .map(system_time_from_millis)
            .transpose()?;
        let modified = changes
            .modified_unix_ms
            .map(system_time_from_millis)
            .transpose()?;
        if let Some(handle) = handle {
            let opened = self.open_handle(handle)?;
            if opened.node != node {
                return Err(ServerError::InvalidRequest(
                    "attribute handle does not belong to the node".into(),
                ));
            }
            if changes.size.is_some() && !opened.access.can_write() {
                return Err(ServerError::PermissionDenied);
            }
            let _operation = opened.operation_gate.write().await;
            let opened_for_task = opened.clone();
            let metadata = blocking(move || {
                let metadata =
                    apply_file_changes(&opened_for_task.file, changes, accessed, modified)?;
                write_backup_time(&opened_for_task.file, changes.backup_unix_ms)?;
                Ok(metadata)
            })
            .await?;
            let mut result = to_metadata(node, &metadata);
            result.backup_unix_ms = changes
                .backup_unix_ms
                .or_else(|| read_fd_backup_time(&opened.file).ok().flatten());
            return Ok(result);
        }

        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let flags = if changes.size.is_some() {
                OFlags::WRONLY
            } else {
                OFlags::RDONLY
            };
            match self.open_snapshot(&snapshot, flags, None) {
                Ok((file, _, metadata)) => {
                    if changes.size.is_some() && metadata.is_dir() {
                        return Err(ServerError::IsDirectory);
                    }
                    let metadata = blocking(move || {
                        let metadata = apply_file_changes(&file, changes, accessed, modified)?;
                        write_backup_time(&file, changes.backup_unix_ms)?;
                        Ok(metadata)
                    })
                    .await?;
                    let mut result = to_metadata(node, &metadata);
                    result.backup_unix_ms = changes.backup_unix_ms;
                    return Ok(result);
                }
                Err(error) if changes.size.is_some() => return Err(error),
                Err(_) => {}
            }
            self.set_path_attributes(&snapshot, changes.mode, accessed, modified)?;
            self.metadata(node).await
        }
        #[cfg(not(unix))]
        Err(ServerError::NotSupported)
    }

    pub async fn read(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<(u64, Vec<u8>), ServerError> {
        validate_range(offset, length, self.export.shared.limits.max_read_size)
            .map_err(|error| ServerError::InvalidRequest(error.to_string()))?;
        let opened = self.open_handle(handle)?;
        if !opened.access.can_read() {
            return Err(ServerError::PermissionDenied);
        }
        let _operation = opened.operation_gate.read().await;
        let opened_for_task = opened.clone();
        blocking(move || read_consistent(&opened_for_task.file, offset, length)).await
    }

    pub async fn write(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<(u64, u64, u64), ServerError> {
        self.write_owned(handle, offset, data.to_vec()).await
    }

    /// Write an owned network payload without duplicating its allocation.
    /// Daemon dispatch should prefer this after it has read the bounded raw
    /// request body into a `Vec<u8>`.
    pub async fn write_owned(
        &self,
        handle: FileHandle,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(u64, u64, u64), ServerError> {
        self.require_writable()?;
        let length = u64::try_from(data.len())
            .map_err(|_| ServerError::InvalidRequest("write length does not fit u64".into()))?;
        validate_range(offset, length, self.export.shared.limits.max_write_size)
            .map_err(|error| ServerError::InvalidRequest(error.to_string()))?;
        let opened = self.open_handle(handle)?;
        if !opened.access.can_write() {
            return Err(ServerError::PermissionDenied);
        }
        let _operation = opened.operation_gate.read().await;
        let _append_operation = if opened.append {
            Some(opened.append_operation.lock().await)
        } else {
            None
        };
        let opened_for_task = opened.clone();
        blocking(move || {
            #[cfg(unix)]
            write_all_at(&opened_for_task.file, offset, &data, opened_for_task.append)?;
            #[cfg(not(unix))]
            {
                let mut file = opened_for_task.file.try_clone()?;
                if !opened_for_task.append {
                    file.seek(SeekFrom::Start(offset))?;
                }
                file.write_all(&data)?;
            }
            let metadata = opened_for_task.file.metadata()?;
            Ok((length, revision(&metadata), metadata.len()))
        })
        .await
    }

    pub async fn flush(
        &self,
        handle: FileHandle,
        lock_owner: Option<u64>,
    ) -> Result<(), ServerError> {
        let opened = self.open_handle(handle)?;
        if let Some(owner) = lock_owner {
            self.remove_locks(|record| {
                record.session == self.id
                    && record.identity == opened.identity
                    && record.lock.owner == owner
            })?;
        }
        Ok(())
    }

    pub async fn sync(&self, handle: FileHandle, data_only: bool) -> Result<(), ServerError> {
        let opened = self.open_handle(handle)?;
        let _operation = opened.operation_gate.write().await;
        let opened_for_task = opened.clone();
        blocking(move || {
            if data_only {
                opened_for_task.file.sync_data()?;
            } else {
                opened_for_task.file.sync_all()?;
            }
            Ok(())
        })
        .await
    }

    pub async fn sync_directory(&self, node: NodeId) -> Result<(), ServerError> {
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let (directory, _, _) = self.open_snapshot(
                &snapshot,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Some(NodeKind::Directory),
            )?;
            blocking(move || {
                directory.sync_all()?;
                Ok(())
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = node;
            Err(ServerError::NotSupported)
        }
    }

    pub async fn allocate(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<(u64, u64), ServerError> {
        self.require_writable()?;
        if length == 0 || offset.checked_add(length).is_none() {
            return Err(ServerError::InvalidRequest(
                "invalid allocation range".into(),
            ));
        }
        let opened = self.open_handle(handle)?;
        if !opened.access.can_write() {
            return Err(ServerError::PermissionDenied);
        }
        let _operation = opened.operation_gate.write().await;
        let opened_for_task = opened.clone();
        #[cfg(unix)]
        return blocking(move || {
            rustix::fs::fallocate(
                &opened_for_task.file,
                FallocateFlags::empty(),
                offset,
                length,
            )
            .map_err(std::io::Error::from)?;
            let metadata = opened_for_task.file.metadata()?;
            Ok((revision(&metadata), metadata.len()))
        })
        .await;
        #[cfg(not(unix))]
        Err(ServerError::NotSupported)
    }

    pub async fn copy_file_range(
        &self,
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    ) -> Result<(u64, u64, u64), ServerError> {
        self.require_writable()?;
        if input_offset.checked_add(length).is_none() || output_offset.checked_add(length).is_none()
        {
            return Err(ServerError::InvalidRequest(
                "copy range overflows the file-offset space".into(),
            ));
        }
        let input = self.open_handle(input)?;
        let output = self.open_handle(output)?;
        if !input.access.can_read() || !output.access.can_write() {
            return Err(ServerError::PermissionDenied);
        }
        let same_gate = Arc::ptr_eq(&input.operation_gate, &output.operation_gate);
        let (first, second) = if input.identity <= output.identity {
            (&input.operation_gate, &output.operation_gate)
        } else {
            (&output.operation_gate, &input.operation_gate)
        };
        let _first = first.write().await;
        let _second = if same_gate {
            None
        } else {
            Some(second.write().await)
        };
        let input_for_task = input.clone();
        let output_for_task = output.clone();
        blocking(move || {
            let copied = copy_range_server(
                &input_for_task.file,
                input_offset,
                &output_for_task.file,
                output_offset,
                length,
            )?;
            let metadata = output_for_task.file.metadata()?;
            Ok((copied, revision(&metadata), metadata.len()))
        })
        .await
    }

    pub async fn seek_file(
        &self,
        handle: FileHandle,
        offset: u64,
        whence: SeekWhence,
    ) -> Result<u64, ServerError> {
        let opened = self.open_handle(handle)?;
        if !opened.access.can_read() {
            return Err(ServerError::PermissionDenied);
        }
        let _operation = opened.operation_gate.read().await;
        let opened_for_task = opened.clone();
        #[cfg(unix)]
        return blocking(move || {
            let position = match whence {
                SeekWhence::Data => rustix::fs::SeekFrom::Data(offset),
                SeekWhence::Hole => rustix::fs::SeekFrom::Hole(offset),
            };
            rustix::fs::seek(&opened_for_task.file, position)
                .map_err(std::io::Error::from)
                .map_err(ServerError::from)
        })
        .await;
        #[cfg(not(unix))]
        {
            let _ = (offset, whence, opened_for_task);
            Err(ServerError::NotSupported)
        }
    }

    pub fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64, ServerError> {
        let opened = self.open_handle(handle)?;
        match operation {
            SafeIoctl::BytesAvailable => {
                if !opened.access.can_read() {
                    return Err(ServerError::PermissionDenied);
                }
                Ok(opened.file.metadata()?.len())
            }
        }
    }

    pub async fn map_block(
        &self,
        node: NodeId,
        block_size: u32,
        block: u64,
    ) -> Result<u64, ServerError> {
        if block_size == 0 {
            return Err(ServerError::InvalidRequest(
                "block size must be nonzero".into(),
            ));
        }
        let offset = block
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| ServerError::InvalidRequest("block offset overflow".into()))?;
        #[cfg(unix)]
        {
            let snapshot = self.node_snapshot(node)?;
            let operation = self.content_operation(snapshot.identity)?;
            let _guard = operation.read().await;
            let (file, _, _) =
                self.open_snapshot(&snapshot, OFlags::RDONLY, Some(NodeKind::File))?;
            let data = blocking(move || {
                rustix::fs::seek(&file, rustix::fs::SeekFrom::Data(offset))
                    .map_err(std::io::Error::from)
                    .map_err(ServerError::from)
            })
            .await?;
            if data < offset.saturating_add(u64::from(block_size)) {
                Ok(block)
            } else {
                Err(ServerError::NoData)
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (node, offset);
            Err(ServerError::NotSupported)
        }
    }

    pub async fn exchange_data<N: AsRef<[u8]>, M: AsRef<[u8]>>(
        &self,
        parent: NodeId,
        name: N,
        new_parent: NodeId,
        new_name: M,
        options: u64,
    ) -> Result<(), ServerError> {
        let name = name.as_ref();
        let new_name = new_name.as_ref();
        self.require_writable()?;
        validate_name(name)?;
        validate_name(new_name)?;
        if options != 0 {
            return Err(ServerError::NotSupported);
        }
        #[cfg(unix)]
        {
            let entries = self.list(parent).await?;
            let left = entries
                .into_iter()
                .find(|entry| entry.name.as_bytes() == name)
                .ok_or(ServerError::NotFound)?;
            let entries = if parent == new_parent {
                self.list(parent).await?
            } else {
                self.list(new_parent).await?
            };
            let right = entries
                .into_iter()
                .find(|entry| entry.name.as_bytes() == new_name)
                .ok_or(ServerError::NotFound)?;
            if left.kind != NodeKind::File || right.kind != NodeKind::File {
                return Err(ServerError::InvalidRequest(
                    "exchangedata requires two regular files".into(),
                ));
            }
            if left.node == right.node {
                return Ok(());
            }
            let left_snapshot = self.node_snapshot(left.node)?;
            let right_snapshot = self.node_snapshot(right.node)?;
            let left_resource_fork = self.resource_fork_path(left.node);
            let right_resource_fork = self.resource_fork_path(right.node);
            let left_gate = self.content_operation(left_snapshot.identity)?;
            let right_gate = self.content_operation(right_snapshot.identity)?;
            let (first, second) = if left_snapshot.identity <= right_snapshot.identity {
                (&left_gate, &right_gate)
            } else {
                (&right_gate, &left_gate)
            };
            let _first = first.write().await;
            let _second = second.write().await;
            let (left_file, _, _) =
                self.open_snapshot(&left_snapshot, OFlags::RDWR, Some(NodeKind::File))?;
            let (right_file, _, _) =
                self.open_snapshot(&right_snapshot, OFlags::RDWR, Some(NodeKind::File))?;
            let temporary_name = format!(".quickfs-exchange-{}", Uuid::new_v4());
            let temporary = rustix::fs::openat(
                &self.export.shared.root_file,
                temporary_name.as_str(),
                OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map(StdFile::from)
            .map_err(std::io::Error::from)?;
            let root = StdFile::from(
                rustix::io::dup(&self.export.shared.root_file).map_err(std::io::Error::from)?,
            );
            blocking(move || {
                let result = exchange_file_contents(&left_file, &right_file, &temporary).and_then(
                    |()| match (left_resource_fork, right_resource_fork) {
                        (Some(left), Some(right)) => exchange_resource_forks(&left, &right),
                        _ => Ok(()),
                    },
                );
                let remove = rustix::fs::unlinkat(&root, temporary_name.as_str(), AtFlags::empty())
                    .map_err(std::io::Error::from)
                    .map_err(ServerError::from);
                result.and(remove)
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, name, new_parent, new_name, options);
            Err(ServerError::NotSupported)
        }
    }

    pub fn get_lock(
        &self,
        handle: FileHandle,
        requested: FileLock,
    ) -> Result<Option<FileLock>, ServerError> {
        validate_lock(requested, false)?;
        let opened = self.open_handle(handle)?;
        validate_lock_access(&opened, requested.kind)?;
        let locks = lock_mutex(&self.export.shared.locks)?;
        Ok(first_conflict(&locks, opened.identity, self.id, requested).map(|record| record.lock))
    }

    pub async fn set_lock(
        &self,
        handle: FileHandle,
        requested: FileLock,
        wait: bool,
    ) -> Result<(), ServerError> {
        validate_lock(requested, true)?;
        if requested.kind == LockKind::Write {
            self.require_writable()?;
        }
        let opened = self.open_handle(handle)?;
        validate_lock_access(&opened, requested.kind)?;
        loop {
            let notified = self.export.shared.lock_notify.notified();
            {
                let mut locks = lock_mutex(&self.export.shared.locks)?;
                if requested.kind == LockKind::Unlock {
                    replace_owner_range(&mut locks, opened.identity, self.id, requested, false)?;
                    drop(locks);
                    self.export.shared.lock_notify.notify_waiters();
                    return Ok(());
                }
                if first_conflict(&locks, opened.identity, self.id, requested).is_none() {
                    replace_owner_range(&mut locks, opened.identity, self.id, requested, true)?;
                    drop(locks);
                    self.export.shared.lock_notify.notify_waiters();
                    return Ok(());
                }
            }
            if !wait {
                return Err(ServerError::WouldBlock);
            }
            notified.await;
        }
    }

    pub fn close(&self, handle: FileHandle) -> Result<(), ServerError> {
        let (_, opened) = self
            .handles
            .remove(&handle)
            .ok_or(ServerError::InvalidHandle)?;
        self.remove_locks(|record| {
            record.session == self.id && record.identity == opened.identity
        })?;
        Ok(())
    }

    /// Explicitly release every advisory lock owned by this connection.
    /// This is idempotent and is also called automatically by Drop.
    pub fn cleanup_locks(&self) -> Result<(), ServerError> {
        self.remove_locks(|record| record.session == self.id)
    }

    fn require_writable(&self) -> Result<(), ServerError> {
        if self.can_write() {
            Ok(())
        } else {
            Err(ServerError::ReadOnly)
        }
    }

    fn can_write(&self) -> bool {
        self.export.shared.writable && self.write_authorized()
    }

    fn open_handle(&self, handle: FileHandle) -> Result<Arc<OpenFile>, ServerError> {
        self.handles
            .get(&handle)
            .map(|entry| entry.clone())
            .ok_or(ServerError::InvalidHandle)
    }

    fn insert_handle(&self, opened: OpenFile) -> FileHandle {
        let mut handle = FileHandle(Uuid::new_v4());
        while self.handles.contains_key(&handle) {
            handle = FileHandle(Uuid::new_v4());
        }
        self.handles.insert(handle, Arc::new(opened));
        handle
    }

    fn append_operation(&self, identity: FileIdentity) -> Result<Arc<AsyncMutex<()>>, ServerError> {
        let mut operations = lock_mutex(&self.export.shared.append_operations)?;
        if let Some(operation) = operations.get(&identity).and_then(Weak::upgrade) {
            return Ok(operation);
        }
        if operations.len() >= self.export.shared.limits.max_total_known_nodes {
            operations.retain(|_, operation| operation.strong_count() > 0);
        }
        let operation = Arc::new(AsyncMutex::new(()));
        operations.insert(identity, Arc::downgrade(&operation));
        Ok(operation)
    }

    fn content_operation(
        &self,
        identity: FileIdentity,
    ) -> Result<Arc<AsyncRwLock<()>>, ServerError> {
        let mut operations = lock_mutex(&self.export.shared.content_operations)?;
        if let Some(operation) = operations.get(&identity).and_then(Weak::upgrade) {
            return Ok(operation);
        }
        if operations.len() >= self.export.shared.limits.max_total_known_nodes {
            operations.retain(|_, operation| operation.strong_count() > 0);
        }
        let operation = Arc::new(AsyncRwLock::new(()));
        operations.insert(identity, Arc::downgrade(&operation));
        Ok(operation)
    }

    fn node_snapshot(&self, node: NodeId) -> Result<NodeSnapshot, ServerError> {
        if !lock_mutex(&self.export.shared.nodes)?
            .by_node
            .contains_key(&node)
        {
            self.discover_node(node)?;
        }
        let snapshot = {
            let nodes = lock_mutex(&self.export.shared.nodes)?;
            let known = nodes.by_node.get(&node).ok_or(ServerError::InvalidNode)?;
            NodeSnapshot {
                identity: known.identity,
                paths: known.paths.iter().cloned().collect(),
            }
        };
        self.track_node(node)?;
        Ok(snapshot)
    }

    fn discover_node(&self, requested: NodeId) -> Result<(), ServerError> {
        if requested == ROOT_NODE {
            return Ok(());
        }
        let mut pending = vec![PathBuf::new()];
        let mut visited = 0usize;
        while let Some(relative) = pending.pop() {
            #[cfg(unix)]
            let directory = open_directory(&self.export.shared.root_file, &relative)?;
            #[cfg(unix)]
            let entries = rustix::fs::Dir::read_from(&directory)
                .map_err(std::io::Error::from)?
                .map(|entry| {
                    let entry = entry.map_err(std::io::Error::from)?;
                    let name = entry.file_name().to_bytes().to_vec();
                    let stat = rustix::fs::statat(
                        &directory,
                        OsStr::from_bytes(&name),
                        AtFlags::SYMLINK_NOFOLLOW,
                    )
                    .map_err(std::io::Error::from)?;
                    Ok::<_, std::io::Error>((OsString::from_vec(name), stat))
                })
                .collect::<Result<Vec<_>, _>>()?;
            #[cfg(not(unix))]
            let entries = std::fs::read_dir(self.export.shared.root.join(&relative))?
                .map(|entry| {
                    let entry = entry?;
                    let metadata = std::fs::symlink_metadata(entry.path())?;
                    Ok::<_, std::io::Error>((entry.file_name(), metadata))
                })
                .collect::<Result<Vec<_>, _>>()?;

            for (name, metadata_or_stat) in entries {
                if name.as_os_str().as_bytes() == b"." || name.as_os_str().as_bytes() == b".." {
                    continue;
                }
                visited = visited.saturating_add(1);
                if visited > self.export.shared.limits.max_total_known_nodes {
                    return Err(ServerError::TooManyNodes);
                }
                let child = relative.join(name);
                #[cfg(unix)]
                let file_identity = identity_from_stat(&metadata_or_stat);
                #[cfg(not(unix))]
                let file_identity = identity(&metadata_or_stat);
                if stable_node_id(self.export.shared.node_key, file_identity) == requested {
                    let remembered = self.remember(file_identity, child)?;
                    return if remembered == requested {
                        Ok(())
                    } else {
                        Err(ServerError::StateUnavailable)
                    };
                }
                #[cfg(unix)]
                let is_directory =
                    node_kind_from_mode(metadata_or_stat.st_mode)? == NodeKind::Directory;
                #[cfg(not(unix))]
                let is_directory = metadata_or_stat.is_dir();
                if is_directory {
                    pending.push(child);
                }
            }
        }
        Err(ServerError::InvalidNode)
    }

    fn reserve_node(&self) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), ServerError> {
        let session = self
            .node_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServerError::TooManyNodes)?;
        let global = match self.export.shared.node_permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if !self.evict_unreferenced_global_node()? {
                    return Err(ServerError::TooManyNodes);
                }
                self.export
                    .shared
                    .node_permits
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ServerError::TooManyNodes)?
            }
        };
        Ok((session, global))
    }

    fn evict_unreferenced_global_node(&self) -> Result<bool, ServerError> {
        let mut nodes = lock_mutex(&self.export.shared.nodes)?;
        let candidate = nodes.by_node.iter().find_map(|(node, known)| {
            (*node != ROOT_NODE && known.session_references == 0).then_some((*node, known.identity))
        });
        let Some((node, identity)) = candidate else {
            return Ok(false);
        };
        nodes.by_node.remove(&node);
        nodes.by_identity.remove(&identity);
        Ok(true)
    }

    fn remember(&self, identity: FileIdentity, path: PathBuf) -> Result<NodeId, ServerError> {
        let existing = {
            let mut nodes = lock_mutex(&self.export.shared.nodes)?;
            if let Some(node) = nodes.by_identity.get(&identity).copied() {
                if let Some(known) = nodes.by_node.get_mut(&node) {
                    known.paths.insert(path.clone());
                }
                Some(node)
            } else {
                None
            }
        };
        if let Some(node) = existing {
            self.track_node(node)?;
            return Ok(node);
        }
        let (session, global) = self.reserve_node()?;
        self.remember_reserved(identity, path, session, global)
    }

    fn remember_reserved(
        &self,
        identity: FileIdentity,
        path: PathBuf,
        session_permit: OwnedSemaphorePermit,
        global_permit: OwnedSemaphorePermit,
    ) -> Result<NodeId, ServerError> {
        let mut nodes = lock_mutex(&self.export.shared.nodes)?;
        if let Some(node) = nodes.by_identity.get(&identity).copied() {
            if let Some(known) = nodes.by_node.get_mut(&node) {
                known.paths.insert(path);
            }
            drop(nodes);
            drop(global_permit);
            self.track_node_with_permit(node, session_permit)?;
            return Ok(node);
        }
        let node = stable_node_id(self.export.shared.node_key, identity);
        if node == ROOT_NODE
            || nodes
                .by_node
                .get(&node)
                .is_some_and(|known| known.identity != identity)
        {
            return Err(ServerError::StateUnavailable);
        }
        nodes.by_identity.insert(identity, node);
        nodes.by_node.insert(
            node,
            KnownNode {
                identity,
                paths: BTreeSet::from([path]),
                session_references: 0,
                _global_permit: Some(global_permit),
            },
        );
        drop(nodes);
        self.track_node_with_permit(node, session_permit)?;
        Ok(node)
    }

    fn track_node(&self, node: NodeId) -> Result<(), ServerError> {
        if lock_mutex(&self.known_nodes)?.contains_key(&node) {
            return Ok(());
        }
        let permit = self
            .node_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServerError::TooManyNodes)?;
        self.track_node_with_permit(node, permit)
    }

    fn track_node_with_permit(
        &self,
        node: NodeId,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), ServerError> {
        let mut known = lock_mutex(&self.known_nodes)?;
        if let std::collections::hash_map::Entry::Vacant(entry) = known.entry(node) {
            let mut nodes = lock_mutex(&self.export.shared.nodes)?;
            let shared = nodes
                .by_node
                .get_mut(&node)
                .ok_or(ServerError::InvalidNode)?;
            shared.session_references = shared
                .session_references
                .checked_add(1)
                .ok_or(ServerError::StateUnavailable)?;
            entry.insert(Some(permit));
        } else {
            drop(permit);
        }
        Ok(())
    }

    fn release_node_reference(&self, node: NodeId) -> Result<(), ServerError> {
        if node == ROOT_NODE {
            return Ok(());
        }
        let mut nodes = lock_mutex(&self.export.shared.nodes)?;
        match nodes.by_node.get_mut(&node) {
            Some(known) if known.session_references > 0 => {
                known.session_references -= 1;
            }
            Some(_) => return Err(ServerError::StateUnavailable),
            // Deleted nodes are removed from the registry immediately, so a
            // later kernel forget for one is already satisfied.
            None => return Ok(()),
        }
        Ok(())
    }

    fn remove_registry_path(&self, path: &Path, descendants: bool) -> Result<(), ServerError> {
        let mut nodes = lock_mutex(&self.export.shared.nodes)?;
        let mut empty = Vec::new();
        for (node, known) in &mut nodes.by_node {
            if *node == ROOT_NODE {
                continue;
            }
            known.paths.retain(|candidate| {
                if descendants {
                    !is_path_or_descendant(candidate, path)
                } else {
                    candidate != path
                }
            });
            if known.paths.is_empty() {
                empty.push((*node, known.identity));
            }
        }
        for (node, identity) in empty {
            nodes.by_node.remove(&node);
            nodes.by_identity.remove(&identity);
        }
        Ok(())
    }

    fn update_registry_after_rename(
        &self,
        old: &Path,
        new: &Path,
        mode: RenameMode,
        source_identity: FileIdentity,
        destination_identity: Option<FileIdentity>,
    ) -> Result<(), ServerError> {
        if destination_identity == Some(source_identity) {
            return Ok(());
        }
        let mut nodes = lock_mutex(&self.export.shared.nodes)?;
        if mode == RenameMode::Replace {
            for known in nodes.by_node.values_mut() {
                if known.identity != source_identity {
                    known
                        .paths
                        .retain(|candidate| !is_path_or_descendant(candidate, new));
                }
            }
        }
        for known in nodes.by_node.values_mut() {
            let updated = known
                .paths
                .iter()
                .map(|path| match mode {
                    RenameMode::Exchange if is_path_or_descendant(path, old) => {
                        replace_prefix(path, old, new)
                    }
                    RenameMode::Exchange if is_path_or_descendant(path, new) => {
                        replace_prefix(path, new, old)
                    }
                    _ if is_path_or_descendant(path, old) => replace_prefix(path, old, new),
                    _ => path.clone(),
                })
                .collect();
            known.paths = updated;
        }
        let empty: Vec<_> = nodes
            .by_node
            .iter()
            .filter_map(|(node, known)| {
                (*node != ROOT_NODE && known.paths.is_empty()).then_some((*node, known.identity))
            })
            .collect();
        for (node, identity) in empty {
            nodes.by_node.remove(&node);
            nodes.by_identity.remove(&identity);
        }
        Ok(())
    }

    fn remove_locks(&self, remove: impl Fn(&LockRecord) -> bool) -> Result<(), ServerError> {
        let mut locks = lock_mutex(&self.export.shared.locks)?;
        let before = locks.len();
        locks.retain(|record| !remove(record));
        let changed = before != locks.len();
        drop(locks);
        if changed {
            self.export.shared.lock_notify.notify_waiters();
        }
        Ok(())
    }

    #[cfg(unix)]
    fn open_snapshot(
        &self,
        snapshot: &NodeSnapshot,
        flags: OFlags,
        expected: Option<NodeKind>,
    ) -> Result<(StdFile, PathBuf, std::fs::Metadata), ServerError> {
        let mut first_error = None;
        for path in &snapshot.paths {
            let (file, opened_path, metadata) =
                match open_relative(&self.export.shared.root_file, path, flags) {
                    Ok(opened) => opened,
                    Err(ServerError::NotFound | ServerError::NotDirectory) => continue,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        continue;
                    }
                };
            if identity(&metadata) != snapshot.identity {
                continue;
            }
            if let Some(expected) = expected
                && node_kind(&metadata) != expected
            {
                return Err(match expected {
                    NodeKind::Directory => ServerError::NotDirectory,
                    NodeKind::File => ServerError::IsDirectory,
                    NodeKind::Symlink => {
                        ServerError::InvalidRequest("node is not a symbolic link".into())
                    }
                    NodeKind::NamedPipe
                    | NodeKind::CharacterDevice
                    | NodeKind::BlockDevice
                    | NodeKind::Socket => {
                        ServerError::InvalidRequest("node has a different special-file type".into())
                    }
                });
            }
            return Ok((file, opened_path, metadata));
        }
        Err(first_error.unwrap_or(ServerError::NotFound))
    }

    #[cfg(unix)]
    fn open_xattr_file(&self, snapshot: &NodeSnapshot) -> Result<StdFile, ServerError> {
        let kind = self
            .path_stat(snapshot)?
            .map(|stat| node_kind_from_mode(stat.st_mode))
            .transpose()?
            .ok_or(ServerError::NotFound)?;
        if kind == NodeKind::Symlink {
            // Descriptor-relative xattr syscalls cannot address a symlink
            // itself portably without falling back to a racy absolute path.
            return Err(ServerError::NotSupported);
        }
        let flags = if kind == NodeKind::Directory {
            OFlags::RDONLY | OFlags::DIRECTORY
        } else {
            OFlags::RDONLY | OFlags::NONBLOCK
        };
        self.open_snapshot(snapshot, flags, None)
            .map(|(file, _, _)| file)
    }

    fn resource_fork_path(&self, node: NodeId) -> Option<PathBuf> {
        self.export
            .shared
            .persistence
            .as_ref()
            .map(|state| state.resource_forks.join(format!("{}.fork", node.0)))
    }

    fn enrich_resource_fork_revision(&self, node: NodeId, metadata: &mut Metadata) {
        enrich_resource_fork_revision_at(
            self.export
                .shared
                .persistence
                .as_ref()
                .map(|state| state.resource_forks.as_path()),
            node,
            metadata,
        );
    }

    #[cfg(unix)]
    fn read_backup_time(&self, snapshot: &NodeSnapshot) -> Result<Option<u64>, ServerError> {
        let file = match self.open_xattr_file(snapshot) {
            Ok(file) => file,
            Err(ServerError::NotSupported) => return Ok(None),
            Err(error) => return Err(error),
        };
        match read_fd_xattr(&file, OsStr::from_bytes(BACKUP_TIME_XATTR), 8) {
            Ok(bytes) if bytes.len() == 8 => {
                Ok(Some(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]))))
            }
            Ok(_) => Err(ServerError::StateUnavailable),
            Err(ServerError::NoAttribute) => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(unix)]
    fn path_stat(&self, snapshot: &NodeSnapshot) -> Result<Option<rustix::fs::Stat>, ServerError> {
        if snapshot
            .paths
            .iter()
            .any(|path| path.as_os_str().is_empty())
        {
            let stat =
                rustix::fs::fstat(&self.export.shared.root_file).map_err(std::io::Error::from)?;
            return Ok((identity_from_stat(&stat) == snapshot.identity).then_some(stat));
        }
        let mut first_error = None;
        for path in &snapshot.paths {
            let stat = match secure_lstat(&self.export.shared.root_file, path) {
                Ok(stat) => stat,
                Err(ServerError::NotFound | ServerError::NotDirectory) => continue,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    continue;
                }
            };
            if identity_from_stat(&stat) != snapshot.identity {
                continue;
            }
            return Ok(Some(stat));
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(None)
    }

    #[cfg(unix)]
    fn set_path_attributes(
        &self,
        snapshot: &NodeSnapshot,
        mode: Option<u32>,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), ServerError> {
        let timestamps = rustix::fs::Timestamps {
            last_access: rustix_timestamp(accessed)?,
            last_modification: rustix_timestamp(modified)?,
        };
        for path in &snapshot.paths {
            let Ok(stat) = secure_lstat(&self.export.shared.root_file, path) else {
                continue;
            };
            if identity_from_stat(&stat) != snapshot.identity {
                continue;
            }
            if mode.is_some() && node_kind_from_mode(stat.st_mode)? == NodeKind::Symlink {
                return Err(ServerError::NotSupported);
            }
            let (parent, name) = open_parent(&self.export.shared.root_file, path)?;
            if let Some(mode) = mode {
                rustix::fs::chmodat(
                    &parent,
                    &name,
                    Mode::from_raw_mode((mode & 0o7777) as rustix::fs::RawMode),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(std::io::Error::from)?;
            }
            if accessed.is_some() || modified.is_some() {
                rustix::fs::utimensat(&parent, &name, &timestamps, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(std::io::Error::from)?;
            }
            return Ok(());
        }
        Err(ServerError::NotFound)
    }

    #[cfg(not(unix))]
    fn absolute_snapshot_path(&self, snapshot: &NodeSnapshot) -> Result<PathBuf, ServerError> {
        snapshot
            .paths
            .first()
            .map(|path| self.export.shared.root.join(path))
            .ok_or(ServerError::NotFound)
    }
}

impl Drop for ExportSession {
    fn drop(&mut self) {
        let _ = self.cleanup_locks();
        let nodes = self
            .known_nodes
            .get_mut()
            .map(|known| {
                known
                    .drain()
                    .filter_map(|(node, _)| (node != ROOT_NODE).then_some(node))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for node in nodes {
            let _ = self.release_node_reference(node);
        }
    }
}

fn random_node_key() -> [u8; 32] {
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

fn stable_node_id(key: [u8; 32], identity: FileIdentity) -> NodeId {
    let mut hasher = Sha256::new();
    hasher.update(b"quickfs stable node v1");
    hasher.update(key);
    hasher.update(identity.device.to_le_bytes());
    hasher.update(identity.inode.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap_or([0; 16]);
    // Mark the opaque value as an RFC 4122 variant/version-5 UUID for sane
    // diagnostics; its uniqueness still comes from the keyed SHA-256 digest.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    NodeId(Uuid::from_bytes(bytes))
}

fn export_marker(root: &Path, root_identity: FileIdentity) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"quickfs export identity v1");
    #[cfg(unix)]
    hasher.update(root.as_os_str().as_bytes());
    #[cfg(not(unix))]
    hasher.update(root.to_string_lossy().as_bytes());
    hasher.update(root_identity.device.to_le_bytes());
    hasher.update(root_identity.inode.to_le_bytes());
    hex::encode(hasher.finalize())
}

fn decode_node_key(encoded: &str) -> Result<[u8; 32], ServerError> {
    let decoded = hex::decode(encoded).map_err(|_| ServerError::StateUnavailable)?;
    decoded
        .try_into()
        .map_err(|_| ServerError::StateUnavailable)
}

fn load_or_create_export_record(
    path: &Path,
    marker: &str,
) -> Result<PersistentExportRecord, ServerError> {
    match read_export_record(path) {
        Ok(bytes) => {
            if bytes.len() > 64 * 1024 {
                return Err(ServerError::StateUnavailable);
            }
            let record: PersistentExportRecord =
                serde_json::from_slice(&bytes).map_err(|_| ServerError::StateUnavailable)?;
            if record.version != 1 || record.marker != marker {
                return Err(ServerError::StateUnavailable);
            }
            decode_node_key(&record.node_key)?;
            Ok(record)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let record = PersistentExportRecord {
                version: 1,
                marker: marker.to_owned(),
                epoch: Uuid::new_v4(),
                node_key: hex::encode(random_node_key()),
                volume_name: "quicKFS".into(),
            };
            save_export_record(path, &record)?;
            Ok(record)
        }
        Err(error) => Err(error.into()),
    }
}

fn read_export_record(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(unix)]
    let mut file = StdFile::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    #[cfg(not(unix))]
    let mut file = StdFile::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid export state file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    #[cfg(unix)]
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    #[cfg(not(unix))]
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(bytes)
}

fn save_export_record(path: &Path, record: &PersistentExportRecord) -> Result<(), ServerError> {
    let bytes = serde_json::to_vec(record).map_err(|_| ServerError::StateUnavailable)?;
    let parent = path.parent().ok_or(ServerError::StateUnavailable)?;
    let temporary = parent.join(format!(".quickfs-export-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<(), ServerError> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        StdFile::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn prepare_resource_fork_directory(state_file: &Path) -> Result<PathBuf, ServerError> {
    let parent = state_file.parent().ok_or(ServerError::StateUnavailable)?;
    let path = parent.join("filesystem-resource-forks");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(ServerError::StateUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                std::fs::DirBuilder::new().mode(0o700).create(&path)?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir(&path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn validate_directory_view_options(options: DirectoryViewOptions) -> Result<(), ServerError> {
    if options.inline_xattr_size > MAX_DIRECTORY_INLINE_XATTR_SIZE
        || options.inline_xattr_total_size > MAX_DIRECTORY_INLINE_XATTR_TOTAL_SIZE
        || (!options.include_xattrs
            && (options.inline_xattr_size != 0 || options.inline_xattr_total_size != 0))
    {
        return Err(ServerError::InvalidRequest(
            "directory xattr projection exceeds the protocol budget".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_directory_scan(
    root: &StdFile,
    snapshot: &NodeSnapshot,
) -> Result<PreparedDirectoryScan, ServerError> {
    let (directory, relative, before) = open_node_snapshot(
        root,
        snapshot,
        OFlags::RDONLY | OFlags::DIRECTORY,
        Some(NodeKind::Directory),
    )?;
    let stream = rustix::fs::Dir::read_from(&directory).map_err(std::io::Error::from)?;
    let mut names = Vec::new();
    for entry in stream {
        let entry = entry.map_err(std::io::Error::from)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        // The directory view is streamed in frame-sized chunks, so the response
        // no longer has to fit one MAX_FRAME_SIZE frame. Bound only the total
        // entry count to keep a pathological directory from allocating without
        // limit; realistically the per-connection node ceiling is hit first.
        if names.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(ServerError::DirectoryTooLarge);
        }
        names.push(bytes.to_vec());
    }
    Ok(PreparedDirectoryScan {
        directory: Arc::new(directory),
        relative,
        before,
        names,
    })
}

#[cfg(unix)]
fn scan_directory_entry(
    directory: &StdFile,
    parent: &Path,
    name: Vec<u8>,
    node_key: [u8; 32],
    resource_forks: Option<&Path>,
    options: DirectoryViewOptions,
    remaining_inline: &AtomicUsize,
) -> Result<ScannedDirectoryEntry, ServerError> {
    let host_name = OsStr::from_bytes(&name);
    let stat = rustix::fs::statat(directory, host_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(std::io::Error::from)?;
    let identity = identity_from_stat(&stat);
    let node = stable_node_id(node_key, identity);
    let kind = node_kind_from_mode(stat.st_mode)?;
    let mut metadata = to_metadata_from_stat(node, &stat)?;
    let base_revision = metadata.revision;
    let xattrs = if options.include_xattrs && kind != NodeKind::Symlink {
        match open_directory_entry_for_xattrs(directory, host_name, kind) {
            Ok(file) => {
                metadata.backup_unix_ms = read_fd_backup_time(&file).ok().flatten();
                let snapshot = xattr_snapshot_for_open_file(
                    &file,
                    node,
                    resource_forks,
                    options.inline_xattr_size as usize,
                    remaining_inline,
                )
                .ok();
                if revision(&file.metadata()?) != base_revision {
                    return Err(ServerError::Conflict);
                }
                snapshot
            }
            Err(_) => None,
        }
    } else {
        None
    };
    enrich_resource_fork_revision_at(resource_forks, node, &mut metadata);
    Ok(ScannedDirectoryEntry {
        identity,
        relative: parent.join(host_name),
        entry: DirectoryEntry {
            node,
            name: Name::new(name),
            kind,
            metadata,
        },
        xattrs,
    })
}

#[cfg(unix)]
fn open_directory_entry_for_xattrs(
    directory: &StdFile,
    name: &OsStr,
    kind: NodeKind,
) -> Result<StdFile, ServerError> {
    let flags = if kind == NodeKind::Directory {
        OFlags::RDONLY | OFlags::DIRECTORY
    } else {
        OFlags::RDONLY | OFlags::NONBLOCK
    };
    rustix::fs::openat(
        directory,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(StdFile::from)
    .map_err(|error| std::io::Error::from(error).into())
}

#[cfg(unix)]
fn xattr_snapshot_for_open_file(
    file: &StdFile,
    node: NodeId,
    resource_forks: Option<&Path>,
    inline_maximum: usize,
    remaining_inline: &AtomicUsize,
) -> Result<XattrSnapshot, ServerError> {
    let mut names = list_fd_xattrs(file)?;
    let resource_fork = resource_forks.map(|root| root.join(format!("{}.fork", node.0)));
    if resource_fork.as_ref().is_some_and(|path| path.is_file())
        && !names.iter().any(is_resource_fork)
    {
        names.push(Name::from("com.apple.ResourceFork"));
        names.sort();
    }
    let mut inline_values = Vec::new();
    if inline_maximum > 0 {
        for name in &names {
            let value = if is_resource_fork(name) {
                resource_fork.as_ref().and_then(|path| {
                    read_resource_fork_range(path, 0, inline_maximum as u64)
                        .ok()
                        .and_then(|(total, value)| {
                            (total as usize <= inline_maximum).then_some(value)
                        })
                })
            } else {
                logical_xattr_name(name).ok().and_then(|host_name| {
                    read_fd_xattr_bounded(file, &host_name, inline_maximum)
                        .ok()
                        .flatten()
                })
            };
            if let Some(value) = value
                && claim_inline_budget(remaining_inline, value.len())
            {
                inline_values.push(InlineXattr {
                    name: name.clone(),
                    value,
                });
            }
        }
    }
    Ok(XattrSnapshot {
        names,
        inline_values,
    })
}

#[cfg(unix)]
fn read_fd_xattr_bounded(
    file: &StdFile,
    name: &OsStr,
    maximum: usize,
) -> Result<Option<Vec<u8>>, ServerError> {
    if maximum == 0 {
        return Ok(None);
    }
    let mut capacity = 256usize.min(maximum).max(1);
    loop {
        let mut buffer = vec![0_u8; capacity];
        match rustix::fs::fgetxattr(file, name, &mut buffer) {
            Ok(length) => {
                buffer.truncate(length);
                return Ok(Some(buffer));
            }
            Err(rustix::io::Errno::RANGE) if capacity < maximum => {
                capacity = capacity.saturating_mul(2).min(maximum);
            }
            Err(rustix::io::Errno::RANGE) => return Ok(None),
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
}

fn claim_inline_budget(remaining: &AtomicUsize, amount: usize) -> bool {
    remaining
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |available| {
            available.checked_sub(amount)
        })
        .is_ok()
}

fn enrich_resource_fork_revision_at(
    resource_forks: Option<&Path>,
    node: NodeId,
    metadata: &mut Metadata,
) {
    let Some(path) = resource_forks.map(|root| root.join(format!("{}.fork", node.0))) else {
        return;
    };
    let Ok(sidecar) = std::fs::metadata(path) else {
        return;
    };
    let mut hasher = Sha256::new();
    hasher.update(b"quickfs resource fork revision v1");
    hasher.update(metadata.revision.to_le_bytes());
    hasher.update(sidecar.len().to_le_bytes());
    if let Ok(modified) = sidecar.modified()
        && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
    {
        hasher.update(duration.as_nanos().to_le_bytes());
    }
    let digest = hasher.finalize();
    metadata.revision = u64::from_le_bytes(digest[..8].try_into().unwrap_or([0; 8]));
}

#[cfg(unix)]
fn open_node_snapshot(
    root: &StdFile,
    snapshot: &NodeSnapshot,
    flags: OFlags,
    expected: Option<NodeKind>,
) -> Result<(StdFile, PathBuf, std::fs::Metadata), ServerError> {
    let mut first_error = None;
    for path in &snapshot.paths {
        let (file, opened_path, metadata) = match open_relative(root, path, flags) {
            Ok(opened) => opened,
            Err(ServerError::NotFound | ServerError::NotDirectory) => continue,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
        };
        if identity(&metadata) != snapshot.identity {
            continue;
        }
        if let Some(expected) = expected
            && node_kind(&metadata) != expected
        {
            return Err(match expected {
                NodeKind::Directory => ServerError::NotDirectory,
                NodeKind::File => ServerError::IsDirectory,
                NodeKind::Symlink => {
                    ServerError::InvalidRequest("node is not a symbolic link".into())
                }
                NodeKind::NamedPipe
                | NodeKind::CharacterDevice
                | NodeKind::BlockDevice
                | NodeKind::Socket => {
                    ServerError::InvalidRequest("node has a different special-file type".into())
                }
            });
        }
        return Ok((file, opened_path, metadata));
    }
    Err(first_error.unwrap_or(ServerError::NotFound))
}

fn validate_limits(limits: &Limits) -> Result<(), ServerError> {
    if limits.max_read_size == 0 || limits.max_write_size == 0 {
        return Err(ServerError::InvalidRequest(
            "maximum read and write sizes must be greater than zero".into(),
        ));
    }
    if limits.max_open_handles == 0 || limits.max_open_handles > Semaphore::MAX_PERMITS {
        return Err(ServerError::InvalidRequest(
            "maximum open handles must fit the runtime semaphore limit".into(),
        ));
    }
    if limits.max_known_nodes == 0 || limits.max_known_nodes > Semaphore::MAX_PERMITS {
        return Err(ServerError::InvalidRequest(
            "maximum known nodes must fit the runtime semaphore limit".into(),
        ));
    }
    if limits.max_total_known_nodes == 0
        || limits.max_total_known_nodes > Semaphore::MAX_PERMITS
        || limits.max_known_nodes > limits.max_total_known_nodes
    {
        return Err(ServerError::InvalidRequest(
            "total known-node capacity must cover one connection and fit the runtime limit".into(),
        ));
    }
    if limits.max_directory_entry_tasks == 0
        || limits.max_directory_entry_tasks > Semaphore::MAX_PERMITS
    {
        return Err(ServerError::InvalidRequest(
            "maximum directory entry tasks must fit the runtime semaphore limit".into(),
        ));
    }
    Ok(())
}

fn validate_open_options(options: FileOpenOptions, writable: bool) -> Result<(), ServerError> {
    if (options.truncate || options.append) && !options.access.can_write() {
        return Err(ServerError::InvalidRequest(
            "truncate and append require write access".into(),
        ));
    }
    if options.access.can_write() && !writable {
        return Err(ServerError::ReadOnly);
    }
    Ok(())
}

fn validate_name(name: &[u8]) -> Result<(), ServerError> {
    validate_filename(name).map_err(|error| ServerError::InvalidRequest(error.to_string()))
}

fn validate_xattr_name(name: &Name) -> Result<(), ServerError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 || bytes.contains(&0) {
        Err(ServerError::InvalidRequest(
            "invalid extended-attribute name".into(),
        ))
    } else {
        Ok(())
    }
}

fn is_resource_fork(name: &Name) -> bool {
    name.as_bytes() == b"com.apple.ResourceFork"
}

fn open_resource_fork(path: &Path, write: bool, create: bool) -> Result<StdFile, ServerError> {
    #[cfg(unix)]
    {
        let mut flags = if write { OFlags::RDWR } else { OFlags::RDONLY };
        flags |= OFlags::CLOEXEC | OFlags::NOFOLLOW;
        if create {
            flags |= OFlags::CREATE;
        }
        let file = rustix::fs::open(path, flags, Mode::from_raw_mode(0o600))
            .map(StdFile::from)
            .map_err(std::io::Error::from)?;
        if !file.metadata()?.is_file() {
            return Err(ServerError::StateUnavailable);
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let mut options = OpenOptions::new();
        options.read(true).write(write).create(create);
        Ok(options.open(path)?)
    }
}

fn read_resource_fork_range(
    path: &Path,
    offset: u64,
    length: u64,
) -> Result<(u64, Vec<u8>), ServerError> {
    let file = match open_resource_fork(path, false, false) {
        Ok(file) => file,
        Err(ServerError::NotFound) => return Err(ServerError::NoAttribute),
        Err(error) => return Err(error),
    };
    let total = file.metadata()?.len();
    let amount = length.min(total.saturating_sub(offset));
    let mut data = vec![
        0_u8;
        usize::try_from(amount).map_err(|_| ServerError::InvalidRequest(
            "resource-fork range does not fit memory".into()
        ))?
    ];
    read_all_at(&file, offset, &mut data)?;
    Ok((total, data))
}

fn write_resource_fork(
    path: &Path,
    value: &[u8],
    mode: XattrSetMode,
    position: u32,
) -> Result<(), ServerError> {
    let exists = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => return Err(ServerError::StateUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    match (mode, exists) {
        (XattrSetMode::Create, true) => return Err(ServerError::AlreadyExists),
        (XattrSetMode::Replace, false) => return Err(ServerError::NoAttribute),
        _ => {}
    }
    let end = u64::from(position)
        .checked_add(value.len() as u64)
        .ok_or_else(|| ServerError::InvalidRequest("resource-fork range overflow".into()))?;
    if end > MAX_XATTR_SIZE as u64 {
        return Err(ServerError::InvalidRequest(
            "resource fork is too large".into(),
        ));
    }
    let file = open_resource_fork(path, true, !exists)?;
    if position == 0 {
        file.set_len(0)?;
    }
    write_all_at(&file, u64::from(position), value, false)?;
    Ok(())
}

fn remove_resource_fork(path: &Path) -> Result<(), ServerError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => std::fs::remove_file(path).map_err(ServerError::from),
        Ok(_) => Err(ServerError::StateUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(ServerError::NoAttribute),
        Err(error) => Err(error.into()),
    }
}

fn exchange_resource_forks(left: &Path, right: &Path) -> Result<(), ServerError> {
    let left_value = optional_resource_fork(left)?;
    let right_value = optional_resource_fork(right)?;
    replace_resource_fork(left, right_value.as_deref())?;
    if let Err(error) = replace_resource_fork(right, left_value.as_deref()) {
        // Best-effort rollback keeps a failed exchange from silently leaving
        // only one side changed.
        let _ = replace_resource_fork(left, left_value.as_deref());
        return Err(error);
    }
    Ok(())
}

fn optional_resource_fork(path: &Path) -> Result<Option<Vec<u8>>, ServerError> {
    match read_resource_fork_range(path, 0, MAX_XATTR_SIZE as u64) {
        Ok((_, value)) => Ok(Some(value)),
        Err(ServerError::NoAttribute) => Ok(None),
        Err(error) => Err(error),
    }
}

fn replace_resource_fork(path: &Path, value: Option<&[u8]>) -> Result<(), ServerError> {
    if let Some(value) = value {
        write_resource_fork(path, value, XattrSetMode::Upsert, 0)
    } else {
        match remove_resource_fork(path) {
            Ok(()) | Err(ServerError::NoAttribute) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(unix)]
fn logical_xattr_name(name: &Name) -> Result<OsString, ServerError> {
    validate_xattr_name(name)?;
    #[cfg(target_vendor = "apple")]
    {
        Ok(OsStr::from_bytes(name.as_bytes()).to_os_string())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        const PREFIX: &[u8] = b"user.quickfs.mac.";
        let bytes = name.as_bytes();
        if bytes.starts_with(b"user.") && !bytes.starts_with(b"user.quickfs.") {
            return Ok(OsStr::from_bytes(bytes).to_os_string());
        }
        let mut encoded = PREFIX.to_vec();
        encoded.extend_from_slice(URL_SAFE_NO_PAD.encode(bytes).as_bytes());
        if encoded.len() > 255 {
            return Err(ServerError::InvalidRequest(
                "extended-attribute name is too large for the server filesystem".into(),
            ));
        }
        Ok(OsStr::from_bytes(&encoded).to_os_string())
    }
}

#[cfg(unix)]
fn host_xattr_name(name: &[u8]) -> Option<Name> {
    if name == BACKUP_TIME_XATTR {
        return None;
    }
    #[cfg(target_vendor = "apple")]
    {
        Some(Name::new(name.to_vec()))
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        const PREFIX: &[u8] = b"user.quickfs.mac.";
        if let Some(encoded) = name.strip_prefix(PREFIX) {
            return URL_SAFE_NO_PAD.decode(encoded).ok().map(Name::new);
        }
        name.starts_with(b"user.").then(|| Name::new(name.to_vec()))
    }
}

#[cfg(unix)]
fn read_fd_xattr(file: &StdFile, name: &OsStr, maximum: usize) -> Result<Vec<u8>, ServerError> {
    let mut capacity = 256usize.min(maximum.max(1));
    loop {
        let mut buffer = vec![0_u8; capacity];
        match rustix::fs::fgetxattr(file, name, &mut buffer) {
            Ok(length) => {
                buffer.truncate(length);
                return Ok(buffer);
            }
            Err(rustix::io::Errno::RANGE) if capacity < maximum => {
                capacity = capacity.saturating_mul(2).min(maximum);
            }
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
}

#[cfg(unix)]
fn list_fd_xattrs(file: &StdFile) -> Result<Vec<Name>, ServerError> {
    let mut capacity = 1024usize;
    let bytes = loop {
        let mut buffer = vec![0_u8; capacity];
        match rustix::fs::flistxattr(file, &mut buffer) {
            Ok(length) => {
                buffer.truncate(length);
                break buffer;
            }
            Err(rustix::io::Errno::RANGE) if capacity < MAX_FRAME_SIZE => {
                capacity = capacity.saturating_mul(2).min(MAX_FRAME_SIZE);
            }
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    };
    let mut names = bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .filter_map(host_xattr_name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

#[cfg(unix)]
fn write_backup_time(file: &StdFile, value: Option<u64>) -> Result<(), ServerError> {
    if let Some(value) = value {
        rustix::fs::fsetxattr(
            file,
            OsStr::from_bytes(BACKUP_TIME_XATTR),
            &value.to_le_bytes(),
            XattrFlags::empty(),
        )
        .map_err(std::io::Error::from)?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_fd_backup_time(file: &StdFile) -> Result<Option<u64>, ServerError> {
    match read_fd_xattr(file, OsStr::from_bytes(BACKUP_TIME_XATTR), 8) {
        Ok(bytes) if bytes.len() == 8 => {
            Ok(Some(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]))))
        }
        Ok(_) => Err(ServerError::StateUnavailable),
        Err(ServerError::NoAttribute) => Ok(None),
        Err(error) => Err(error),
    }
}

fn validate_symlink_target(target: &[u8]) -> Result<(), ServerError> {
    if target.is_empty() || target.len() > MAX_SYMLINK_TARGET_SIZE || target.contains(&0) {
        return Err(ServerError::InvalidRequest(
            "invalid symbolic-link target".into(),
        ));
    }
    #[cfg(unix)]
    {
        let path = Path::new(OsStr::from_bytes(target));
        let mut normal = false;
        for component in path.components() {
            match component {
                std::path::Component::Normal(_) => normal = true,
                std::path::Component::CurDir => {}
                _ => return Err(ServerError::PermissionDenied),
            }
        }
        if !normal {
            return Err(ServerError::InvalidRequest(
                "symbolic-link target must name an in-export object".into(),
            ));
        }
    }
    Ok(())
}

fn validate_lock(lock: FileLock, allow_unlock: bool) -> Result<(), ServerError> {
    if lock.start > lock.end || (!allow_unlock && lock.kind == LockKind::Unlock) {
        return Err(ServerError::InvalidRequest("invalid advisory lock".into()));
    }
    Ok(())
}

fn validate_lock_access(opened: &OpenFile, kind: LockKind) -> Result<(), ServerError> {
    match kind {
        LockKind::Read if !opened.access.can_read() => Err(ServerError::PermissionDenied),
        LockKind::Write if !opened.access.can_write() => Err(ServerError::PermissionDenied),
        _ => Ok(()),
    }
}

fn first_conflict(
    locks: &[LockRecord],
    identity: FileIdentity,
    session: Uuid,
    requested: FileLock,
) -> Option<&LockRecord> {
    locks
        .iter()
        .filter(|record| {
            record.identity == identity
                && !(record.session == session && record.lock.owner == requested.owner)
                && ranges_overlap(record.lock, requested)
                && (record.lock.kind == LockKind::Write || requested.kind == LockKind::Write)
        })
        .min_by_key(|record| record.lock.start)
}

fn replace_owner_range(
    locks: &mut Vec<LockRecord>,
    identity: FileIdentity,
    session: Uuid,
    requested: FileLock,
    insert: bool,
) -> Result<(), ServerError> {
    let mut next = Vec::with_capacity(locks.len().saturating_add(2));
    for record in locks.iter() {
        if record.identity != identity
            || record.session != session
            || record.lock.owner != requested.owner
            || !ranges_overlap(record.lock, requested)
        {
            next.push(record.clone());
            continue;
        }
        if record.lock.start < requested.start {
            let mut left = record.clone();
            left.lock.end = requested.start - 1;
            next.push(left);
        }
        if record.lock.end > requested.end {
            let mut right = record.clone();
            right.lock.start = requested.end.saturating_add(1);
            next.push(right);
        }
    }
    if insert {
        next.push(LockRecord {
            identity,
            session,
            lock: requested,
        });
    }
    merge_owner_locks(&mut next);
    if next.len() > MAX_LOCK_RECORDS {
        return Err(ServerError::TooManyLocks);
    }
    *locks = next;
    Ok(())
}

fn merge_owner_locks(locks: &mut Vec<LockRecord>) {
    locks.sort_by_key(|record| {
        (
            record.identity.device,
            record.identity.inode,
            record.session,
            record.lock.owner,
            record.lock.kind as u8,
            record.lock.start,
        )
    });
    let mut merged: Vec<LockRecord> = Vec::with_capacity(locks.len());
    for record in locks.drain(..) {
        if let Some(previous) = merged.last_mut()
            && previous.identity == record.identity
            && previous.session == record.session
            && previous.lock.owner == record.lock.owner
            && previous.lock.kind == record.lock.kind
            && previous.lock.pid == record.lock.pid
            && previous.lock.end.saturating_add(1) >= record.lock.start
        {
            previous.lock.end = previous.lock.end.max(record.lock.end);
        } else {
            merged.push(record);
        }
    }
    *locks = merged;
}

fn ranges_overlap(left: FileLock, right: FileLock) -> bool {
    left.start <= right.end && right.start <= left.end
}

fn is_path_or_descendant(candidate: &Path, prefix: &Path) -> bool {
    candidate == prefix || candidate.strip_prefix(prefix).is_ok()
}

fn replace_prefix(path: &Path, old: &Path, new: &Path) -> PathBuf {
    path.strip_prefix(old)
        .map(|suffix| new.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ServerError> {
    mutex.lock().map_err(|_| ServerError::StateUnavailable)
}

async fn blocking<T, F>(operation: F) -> Result<T, ServerError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ServerError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| ServerError::TaskFailed)?
}

fn system_time_from_millis(value: u64) -> Result<SystemTime, ServerError> {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(value))
        .ok_or_else(|| ServerError::InvalidRequest("timestamp is too large".into()))
}

#[cfg(unix)]
fn rustix_timestamp(time: Option<SystemTime>) -> Result<rustix::fs::Timespec, ServerError> {
    let Some(time) = time else {
        return Ok(rustix::fs::Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        });
    };
    let duration = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ServerError::InvalidRequest("timestamp predates the Unix epoch".into()))?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| ServerError::InvalidRequest("timestamp is too large".into()))?;
    Ok(rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: duration.subsec_nanos().into(),
    })
}

fn apply_file_changes(
    file: &StdFile,
    changes: AttributeChanges,
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
) -> Result<std::fs::Metadata, ServerError> {
    if let Some(size) = changes.size {
        file.set_len(size)?;
    }
    if let Some(mode) = changes.mode {
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))?;
        #[cfg(not(unix))]
        {
            let _ = mode;
            return Err(ServerError::NotSupported);
        }
    }
    if accessed.is_some() || modified.is_some() {
        let mut times = FileTimes::new();
        if let Some(accessed) = accessed {
            times = times.set_accessed(accessed);
        }
        if let Some(modified) = modified {
            times = times.set_modified(modified);
        }
        file.set_times(times)?;
    }
    Ok(file.metadata()?)
}

fn revision(metadata: &std::fs::Metadata) -> u64 {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
        hasher.update(metadata.len().to_le_bytes());
        hasher.update(metadata.mtime().to_le_bytes());
        hasher.update(metadata.mtime_nsec().to_le_bytes());
        hasher.update(metadata.ctime().to_le_bytes());
        hasher.update(metadata.ctime_nsec().to_le_bytes());
        hasher.update(metadata.mode().to_le_bytes());
        hasher.update(metadata.nlink().to_le_bytes());
        hasher.update(metadata.rdev().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(metadata.len().to_le_bytes());
        if let Ok(modified) = metadata.modified()
            && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
        {
            hasher.update(duration.as_nanos().to_le_bytes());
        }
    }
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().unwrap_or([0; 8]))
}

fn to_metadata(node: NodeId, metadata: &std::fs::Metadata) -> Metadata {
    Metadata {
        node,
        kind: node_kind(metadata),
        size: metadata.len(),
        mode: metadata_mode(metadata),
        allocated_blocks: metadata_allocated_blocks(metadata),
        revision: revision(metadata),
        accessed_unix_ms: metadata
            .accessed()
            .ok()
            .and_then(system_time_millis)
            .unwrap_or(0),
        modified_unix_ms: metadata
            .modified()
            .ok()
            .and_then(system_time_millis)
            .unwrap_or(0),
        created_unix_ms: metadata.created().ok().and_then(system_time_millis),
        backup_unix_ms: None,
        link_count: metadata_link_count(metadata),
        device_major: metadata_device(metadata).0,
        device_minor: metadata_device(metadata).1,
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn to_metadata_from_stat(node: NodeId, stat: &rustix::fs::Stat) -> Result<Metadata, ServerError> {
    let accessed_unix_ms =
        unix_timestamp_millis(stat.st_atime as i128, stat.st_atime_nsec as i128).unwrap_or(0);
    let modified_unix_ms =
        unix_timestamp_millis(stat.st_mtime as i128, stat.st_mtime_nsec as i128).unwrap_or(0);
    #[cfg(target_vendor = "apple")]
    let created_unix_ms =
        unix_timestamp_millis(stat.st_birthtime as i128, stat.st_birthtime_nsec as i128);
    #[cfg(not(target_vendor = "apple"))]
    let created_unix_ms = None;
    Ok(Metadata {
        node,
        kind: node_kind_from_mode(stat.st_mode)?,
        size: (stat.st_size as i128).max(0) as u64,
        mode: (stat.st_mode as u32) & 0o7777,
        allocated_blocks: (stat.st_blocks as i128).max(0) as u64,
        revision: revision_from_stat(stat),
        accessed_unix_ms,
        modified_unix_ms,
        created_unix_ms,
        backup_unix_ms: None,
        link_count: u32::try_from((stat.st_nlink as i128).max(0)).unwrap_or(u32::MAX),
        device_major: rustix::fs::major(stat.st_rdev),
        device_minor: rustix::fs::minor(stat.st_rdev),
    })
}

#[cfg(unix)]
fn unix_timestamp_millis(seconds: i128, nanoseconds: i128) -> Option<u64> {
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return None;
    }
    let seconds = u64::try_from(seconds).ok()?;
    let milliseconds = u64::try_from(nanoseconds / 1_000_000).ok()?;
    Some(seconds.saturating_mul(1_000).saturating_add(milliseconds))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn revision_from_stat(stat: &rustix::fs::Stat) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update((stat.st_dev as u64).to_le_bytes());
    hasher.update((stat.st_ino as u64).to_le_bytes());
    hasher.update(((stat.st_size as i128).max(0) as u64).to_le_bytes());
    hasher.update((stat.st_mtime as i64).to_le_bytes());
    hasher.update((stat.st_mtime_nsec as i64).to_le_bytes());
    hasher.update((stat.st_ctime as i64).to_le_bytes());
    hasher.update((stat.st_ctime_nsec as i64).to_le_bytes());
    hasher.update((stat.st_mode as u32).to_le_bytes());
    hasher.update(((stat.st_nlink as i128).max(0) as u64).to_le_bytes());
    hasher.update(((stat.st_rdev as i128).max(0) as u64).to_le_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().unwrap_or([0; 8]))
}

fn system_time_millis(time: SystemTime) -> Option<u64> {
    let millis = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    Some(u64::try_from(millis).unwrap_or(u64::MAX))
}

#[cfg(unix)]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    match (metadata.is_dir(), metadata.permissions().readonly()) {
        (true, true) => 0o555,
        (true, false) => 0o777,
        (false, true) => 0o444,
        (false, false) => 0o666,
    }
}

#[cfg(unix)]
fn metadata_allocated_blocks(metadata: &std::fs::Metadata) -> u64 {
    metadata.blocks()
}

#[cfg(unix)]
fn metadata_link_count(metadata: &std::fs::Metadata) -> u32 {
    u32::try_from(metadata.nlink()).unwrap_or(u32::MAX)
}

#[cfg(not(unix))]
fn metadata_link_count(_metadata: &std::fs::Metadata) -> u32 {
    1
}

#[cfg(unix)]
fn metadata_device(metadata: &std::fs::Metadata) -> (u32, u32) {
    let device = rustix::fs::Dev::try_from(metadata.rdev()).unwrap_or_default();
    (rustix::fs::major(device), rustix::fs::minor(device))
}

#[cfg(not(unix))]
fn metadata_device(_metadata: &std::fs::Metadata) -> (u32, u32) {
    (0, 0)
}

#[cfg(not(unix))]
fn metadata_allocated_blocks(metadata: &std::fs::Metadata) -> u64 {
    metadata.len().div_ceil(512)
}

fn node_kind(metadata: &std::fs::Metadata) -> NodeKind {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        NodeKind::Directory
    } else if file_type.is_file() {
        NodeKind::File
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else if file_type.is_fifo() {
        NodeKind::NamedPipe
    } else if file_type.is_char_device() {
        NodeKind::CharacterDevice
    } else if file_type.is_block_device() {
        NodeKind::BlockDevice
    } else if file_type.is_socket() {
        NodeKind::Socket
    } else {
        NodeKind::Symlink
    }
}

fn read_consistent(
    file: &StdFile,
    offset: u64,
    length: u64,
) -> Result<(u64, Vec<u8>), ServerError> {
    let before = file.metadata()?;
    let before_revision = revision(&before);
    let amount = length.min(before.len().saturating_sub(offset));
    let size = usize::try_from(amount)
        .map_err(|_| ServerError::InvalidRequest("range does not fit memory".into()))?;
    let mut data = vec![0; size];
    #[cfg(unix)]
    {
        let mut consumed = 0usize;
        while consumed < data.len() {
            let read = file.read_at(
                &mut data[consumed..],
                offset.saturating_add(consumed as u64),
            )?;
            if read == 0 {
                data.truncate(consumed);
                break;
            }
            consumed = consumed.saturating_add(read);
        }
    }
    #[cfg(not(unix))]
    {
        let mut cloned = file.try_clone()?;
        cloned.seek(SeekFrom::Start(offset))?;
        let mut consumed = 0usize;
        while consumed < data.len() {
            let read = cloned.read(&mut data[consumed..])?;
            if read == 0 {
                data.truncate(consumed);
                break;
            }
            consumed = consumed.saturating_add(read);
        }
    }
    let after = file.metadata()?;
    let after_revision = revision(&after);
    if before_revision != after_revision {
        return Err(ServerError::Conflict);
    }
    Ok((after_revision, data))
}

#[cfg(unix)]
fn write_all_at(file: &StdFile, offset: u64, data: &[u8], append: bool) -> Result<(), ServerError> {
    if append {
        let mut view = file;
        view.write_all(data)?;
        return Ok(());
    }
    let mut consumed = 0usize;
    while consumed < data.len() {
        let written = file.write_at(&data[consumed..], offset.saturating_add(consumed as u64))?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "file write returned zero bytes",
            )
            .into());
        }
        consumed = consumed.saturating_add(written);
    }
    Ok(())
}

fn copy_range_server(
    input: &StdFile,
    input_offset: u64,
    output: &StdFile,
    output_offset: u64,
    length: u64,
) -> Result<u64, ServerError> {
    let available = input.metadata()?.len().saturating_sub(input_offset);
    let amount = length.min(available);
    if amount == 0 {
        return Ok(0);
    }

    #[cfg(target_os = "linux")]
    if input_offset == 0
        && output_offset == 0
        && amount == input.metadata()?.len()
        && output.metadata()?.len() == 0
        && rustix::fs::ioctl_ficlone(output, input).is_ok()
    {
        return Ok(amount);
    }

    // A same-file overlapping copy uses bounded, directional buffering so it
    // has memmove semantics without allocating the entire requested range.
    if identity(&input.metadata()?) == identity(&output.metadata()?)
        && input_offset < output_offset.saturating_add(amount)
        && output_offset < input_offset.saturating_add(amount)
    {
        copy_range_buffered_overlap(input, input_offset, output, output_offset, amount)?;
        return Ok(amount);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut source = input_offset;
        let mut destination = output_offset;
        let mut copied = 0_u64;
        while copied < amount {
            let chunk = usize::try_from((amount - copied).min(usize::MAX as u64))
                .map_err(|_| ServerError::InvalidRequest("copy range is too large".into()))?;
            match rustix::fs::copy_file_range(
                input,
                Some(&mut source),
                output,
                Some(&mut destination),
                chunk,
            ) {
                Ok(0) => break,
                Ok(written) => copied = copied.saturating_add(written as u64),
                Err(error)
                    if matches!(
                        error,
                        rustix::io::Errno::XDEV
                            | rustix::io::Errno::INVAL
                            | rustix::io::Errno::NOSYS
                            | rustix::io::Errno::OPNOTSUPP
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(std::io::Error::from(error).into()),
            }
        }
        if copied == amount {
            return Ok(copied);
        }
        let remaining = amount - copied;
        copy_range_buffered(input, source, output, destination, remaining)?;
        return Ok(amount);
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        copy_range_buffered(input, input_offset, output, output_offset, amount)?;
        Ok(amount)
    }
}

fn copy_range_buffered_overlap(
    input: &StdFile,
    input_offset: u64,
    output: &StdFile,
    output_offset: u64,
    length: u64,
) -> Result<(), ServerError> {
    const CHUNK: u64 = 1024 * 1024;
    if output_offset <= input_offset {
        return copy_range_buffered(input, input_offset, output, output_offset, length);
    }
    let mut remaining = length;
    let mut buffer = vec![0_u8; CHUNK as usize];
    while remaining > 0 {
        let amount = remaining.min(CHUNK);
        let source = input_offset + remaining - amount;
        let destination = output_offset + remaining - amount;
        let amount = usize::try_from(amount)
            .map_err(|_| ServerError::InvalidRequest("copy range is too large".into()))?;
        read_all_at(input, source, &mut buffer[..amount])?;
        write_all_at(output, destination, &buffer[..amount], false)?;
        remaining -= amount as u64;
    }
    Ok(())
}

fn copy_range_buffered(
    input: &StdFile,
    mut input_offset: u64,
    output: &StdFile,
    mut output_offset: u64,
    mut length: u64,
) -> Result<(), ServerError> {
    const CHUNK: usize = 1024 * 1024;
    let mut buffer = vec![0_u8; CHUNK];
    while length > 0 {
        let amount = usize::try_from(length.min(CHUNK as u64)).unwrap_or(CHUNK);
        read_all_at(input, input_offset, &mut buffer[..amount])?;
        write_all_at(output, output_offset, &buffer[..amount], false)?;
        input_offset += amount as u64;
        output_offset += amount as u64;
        length -= amount as u64;
    }
    Ok(())
}

fn read_all_at(file: &StdFile, offset: u64, data: &mut [u8]) -> Result<(), ServerError> {
    #[cfg(unix)]
    {
        let mut consumed = 0usize;
        while consumed < data.len() {
            let read = file.read_at(&mut data[consumed..], offset + consumed as u64)?;
            if read == 0 {
                return Err(ServerError::Conflict);
            }
            consumed += read;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, offset, data);
        Err(ServerError::NotSupported)
    }
}

#[cfg(unix)]
fn exchange_file_contents(
    left: &StdFile,
    right: &StdFile,
    temporary: &StdFile,
) -> Result<(), ServerError> {
    let left_size = left.metadata()?.len();
    let right_size = right.metadata()?.len();
    temporary.set_len(0)?;
    copy_range_in_chunks(left, temporary, left_size)?;
    left.set_len(0)?;
    copy_range_in_chunks(right, left, right_size)?;
    right.set_len(0)?;
    copy_range_in_chunks(temporary, right, left_size)?;

    let resource_name = logical_xattr_name(&Name::from("com.apple.ResourceFork"))?;
    let left_resource = match read_fd_xattr(left, &resource_name, MAX_XATTR_SIZE) {
        Ok(value) => Some(value),
        Err(ServerError::NoAttribute) => None,
        Err(error) => return Err(error),
    };
    let right_resource = match read_fd_xattr(right, &resource_name, MAX_XATTR_SIZE) {
        Ok(value) => Some(value),
        Err(ServerError::NoAttribute) => None,
        Err(error) => return Err(error),
    };
    replace_optional_xattr(left, &resource_name, right_resource.as_deref())?;
    replace_optional_xattr(right, &resource_name, left_resource.as_deref())?;
    left.sync_all()?;
    right.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn copy_range_in_chunks(
    input: &StdFile,
    output: &StdFile,
    mut length: u64,
) -> Result<(), ServerError> {
    let mut offset = 0_u64;
    while length > 0 {
        let chunk = length.min(8 * 1024 * 1024);
        let copied = copy_range_server(input, offset, output, offset, chunk)?;
        if copied == 0 {
            return Err(ServerError::Conflict);
        }
        offset += copied;
        length -= copied;
    }
    Ok(())
}

#[cfg(unix)]
fn replace_optional_xattr(
    file: &StdFile,
    name: &OsStr,
    value: Option<&[u8]>,
) -> Result<(), ServerError> {
    if let Some(value) = value {
        rustix::fs::fsetxattr(file, name, value, XattrFlags::empty())
            .map_err(std::io::Error::from)?;
    } else {
        match rustix::fs::fremovexattr(file, name) {
            Ok(()) => {}
            Err(error) => {
                let mapped = ServerError::from(std::io::Error::from(error));
                if !matches!(mapped, ServerError::NoAttribute) {
                    return Err(mapped);
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_root(path: &Path) -> Result<StdFile, ServerError> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(StdFile::from(fd))
}

#[cfg(unix)]
fn open_flags(access: FileAccess, append: bool) -> OFlags {
    let mut flags = match access {
        FileAccess::ReadOnly => OFlags::RDONLY,
        FileAccess::WriteOnly => OFlags::WRONLY,
        FileAccess::ReadWrite => OFlags::RDWR,
    };
    if append {
        flags |= OFlags::APPEND;
    }
    flags
}

#[cfg(unix)]
fn open_directory(root: &StdFile, path: &Path) -> Result<StdFile, ServerError> {
    let mut current = StdFile::from(rustix::io::dup(root).map_err(std::io::Error::from)?);
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(ServerError::PermissionDenied);
        };
        let fd = rustix::fs::openat(
            &current,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        current = StdFile::from(fd);
    }
    Ok(current)
}

#[cfg(unix)]
fn open_parent(root: &StdFile, path: &Path) -> Result<(StdFile, OsString), ServerError> {
    let name = path
        .file_name()
        .ok_or_else(|| ServerError::InvalidRequest("root has no parent entry".into()))?
        .to_os_string();
    let parent = open_directory(root, path.parent().unwrap_or_else(|| Path::new("")))?;
    Ok((parent, name))
}

#[cfg(unix)]
fn open_relative(
    root: &StdFile,
    path: &Path,
    flags: OFlags,
) -> Result<(StdFile, PathBuf, std::fs::Metadata), ServerError> {
    if path.as_os_str().is_empty() {
        let file = StdFile::from(rustix::io::dup(root).map_err(std::io::Error::from)?);
        let metadata = file.metadata()?;
        return Ok((file, PathBuf::new(), metadata));
    }
    let (parent, name) = open_parent(root, path)?;
    let fd = rustix::fs::openat(
        &parent,
        &name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let file = StdFile::from(fd);
    let metadata = file.metadata()?;
    Ok((file, path.to_path_buf(), metadata))
}

#[cfg(unix)]
fn secure_lstat(root: &StdFile, path: &Path) -> Result<rustix::fs::Stat, ServerError> {
    if path.as_os_str().is_empty() {
        return rustix::fs::fstat(root)
            .map_err(std::io::Error::from)
            .map_err(ServerError::from);
    }
    let (parent, name) = open_parent(root, path)?;
    Ok(
        rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)?,
    )
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn identity_from_stat(stat: &rustix::fs::Stat) -> FileIdentity {
    // rustix exposes platform-native dev_t/ino_t widths. The widening casts
    // are necessary on some Unix targets and no-ops on 64-bit macOS/Linux.
    FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    }
}

#[cfg(unix)]
fn node_kind_from_mode(mode: rustix::fs::RawMode) -> Result<NodeKind, ServerError> {
    match FileType::from_raw_mode(mode) {
        FileType::RegularFile => Ok(NodeKind::File),
        FileType::Directory => Ok(NodeKind::Directory),
        FileType::Symlink => Ok(NodeKind::Symlink),
        FileType::Fifo => Ok(NodeKind::NamedPipe),
        FileType::CharacterDevice => Ok(NodeKind::CharacterDevice),
        FileType::BlockDevice => Ok(NodeKind::BlockDevice),
        FileType::Socket => Ok(NodeKind::Socket),
        _ => Err(ServerError::NotSupported),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn writable_options() -> FileOpenOptions {
        FileOpenOptions {
            access: FileAccess::ReadWrite,
            truncate: false,
            append: false,
        }
    }

    #[tokio::test]
    async fn read_boundaries_and_invalid_handle() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("x"), b"abcdef").unwrap();
        let export = Export::new(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session();
        let node = session.list(ROOT_NODE).await.unwrap()[0].node;
        let (handle, _, _) = session
            .open(node, FileOpenOptions::READ_ONLY)
            .await
            .unwrap();
        assert_eq!(session.read(handle, 4, 9).await.unwrap().1, b"ef");
        assert!(matches!(
            session.read(FileHandle(Uuid::nil()), 0, 1).await,
            Err(ServerError::InvalidHandle)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn positioned_reads_and_writes_can_share_one_handle_concurrently() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("media"), b"abcdefgh").unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let node = session.list(ROOT_NODE).await.unwrap()[0].node;
        let handle = session.open(node, writable_options()).await.unwrap().0;
        let opened = session.open_handle(handle).unwrap();
        let _shared_gate = opened.operation_gate.read().await;

        let read = tokio::time::timeout(Duration::from_secs(1), session.read(handle, 2, 3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.1, b"cde");
        tokio::time::timeout(Duration::from_secs(1), session.write(handle, 5, b"XYZ"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("media")).unwrap(),
            b"abcdeXYZ"
        );
    }

    #[tokio::test]
    async fn writable_export_creates_writes_truncates_and_syncs() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let (created, handle, _, _) = session
            .create_file(ROOT_NODE, "media.bin", 0o640, writable_options())
            .await
            .unwrap();
        assert_eq!(created.kind, NodeKind::File);
        assert_eq!(session.write(handle, 4, b"frame").await.unwrap().0, 5);
        assert_eq!(
            session.read(handle, 0, 16).await.unwrap().1,
            b"\0\0\0\0frame"
        );
        session.sync(handle, false).await.unwrap();
        let changed = session
            .set_attributes(
                created.node,
                Some(handle),
                AttributeChanges {
                    size: Some(3),
                    mode: None,
                    accessed_unix_ms: None,
                    modified_unix_ms: None,
                    backup_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(changed.size, 3);
        session.close(handle).unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("media.bin")).unwrap(),
            b"\0\0\0"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_metadata_attributes_and_directory_sync_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let capabilities = session.capabilities();
        assert!(capabilities.supports_directory_sync);
        assert!(capabilities.supports_preallocation);

        let (created, handle, _, _) = session
            .create_file(ROOT_NODE, "metadata.bin", 0o640, writable_options())
            .await
            .unwrap();
        session.write(handle, 0, b"allocated data").await.unwrap();
        let accessed_unix_ms = 1_700_000_000_123;
        let modified_unix_ms = 1_700_000_100_456;
        let changed = session
            .set_attributes(
                created.node,
                Some(handle),
                AttributeChanges {
                    size: None,
                    mode: Some(0o604),
                    accessed_unix_ms: Some(accessed_unix_ms),
                    modified_unix_ms: Some(modified_unix_ms),
                    backup_unix_ms: None,
                },
            )
            .await
            .unwrap();
        let on_disk = std::fs::metadata(directory.path().join("metadata.bin")).unwrap();
        assert_eq!(changed.mode, 0o604);
        assert_eq!(changed.mode, on_disk.mode() & 0o7777);
        assert_eq!(changed.allocated_blocks, on_disk.blocks());
        assert!(changed.accessed_unix_ms.abs_diff(accessed_unix_ms) <= 1_000);
        assert!(changed.modified_unix_ms.abs_diff(modified_unix_ms) <= 1_000);
        assert_eq!(
            changed.created_unix_ms,
            on_disk.created().ok().and_then(system_time_millis)
        );
        assert_eq!(session.metadata(created.node).await.unwrap(), changed);

        session.sync(handle, false).await.unwrap();
        session.sync_directory(ROOT_NODE).await.unwrap();
        session.close(handle).unwrap();

        let link = session
            .create_symlink(ROOT_NODE, "metadata-link", b"metadata.bin")
            .await
            .unwrap();
        let changed_link = session
            .set_attributes(
                link.node,
                None,
                AttributeChanges {
                    size: None,
                    mode: None,
                    accessed_unix_ms: Some(accessed_unix_ms),
                    modified_unix_ms: Some(modified_unix_ms),
                    backup_unix_ms: None,
                },
            )
            .await
            .unwrap();
        assert!(changed_link.accessed_unix_ms.abs_diff(accessed_unix_ms) <= 1_000);
        assert!(changed_link.modified_unix_ms.abs_diff(modified_unix_ms) <= 1_000);
        assert!(matches!(
            session
                .set_attributes(
                    link.node,
                    None,
                    AttributeChanges {
                        size: None,
                        mode: Some(0o777),
                        accessed_unix_ms: None,
                        modified_unix_ms: None,
                        backup_unix_ms: None,
                    },
                )
                .await,
            Err(ServerError::NotSupported)
        ));
    }

    #[tokio::test]
    async fn append_allocation_stats_and_external_revisions_work() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let (created, handle, opened_revision, _) = session
            .create_file(ROOT_NODE, "scratch", 0o640, writable_options())
            .await
            .unwrap();
        session.write(handle, 0, b"A").await.unwrap();
        session.close(handle).unwrap();
        let append_options = FileOpenOptions {
            access: FileAccess::WriteOnly,
            truncate: false,
            append: true,
        };
        let append_handle = session.open(created.node, append_options).await.unwrap().0;
        let second_append_handle = session.open(created.node, append_options).await.unwrap().0;
        let first_append = session.open_handle(append_handle).unwrap();
        let held_append = first_append.append_operation.lock().await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                session.write(second_append_handle, 0, b"B"),
            )
            .await
            .is_err()
        );
        drop(held_append);
        session.write(second_append_handle, 0, b"B").await.unwrap();
        session.close(second_append_handle).unwrap();
        let (allocated_revision, allocated_size) =
            session.allocate(append_handle, 0, 4_096).await.unwrap();
        assert!(allocated_size >= 4_096);
        assert_ne!(allocated_revision, opened_revision);
        session.close(append_handle).unwrap();
        assert_eq!(
            &std::fs::read(directory.path().join("scratch")).unwrap()[..2],
            b"AB"
        );
        let stats = session.stat_filesystem().await.unwrap();
        assert!(stats.block_size > 0);

        let read_handle = session
            .open(created.node, FileOpenOptions::READ_ONLY)
            .await
            .unwrap()
            .0;
        let before_external = session.read(read_handle, 0, 2).await.unwrap().0;
        std::fs::write(directory.path().join("scratch"), b"changed").unwrap();
        let after_external = session.read(read_handle, 0, 7).await.unwrap().0;
        assert_ne!(before_external, after_external);
    }

    #[tokio::test]
    async fn read_only_export_rejects_mutation() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("x"), b"data").unwrap();
        let export = Export::new(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session();
        assert!(matches!(
            session
                .create_file(ROOT_NODE, "new", 0o600, writable_options())
                .await,
            Err(ServerError::ReadOnly)
        ));
        let node = session.list(ROOT_NODE).await.unwrap()[0].node;
        assert!(matches!(
            session.open(node, writable_options()).await,
            Err(ServerError::ReadOnly)
        ));
    }

    #[tokio::test]
    async fn session_write_grants_are_isolated_and_revocable() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("existing"), b"data").unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let read_only = export.session();
        let writable = export.session_with_writes(true);
        assert!(export.capabilities().writable);
        assert!(!read_only.capabilities().writable);
        assert!(writable.capabilities().writable);
        assert!(matches!(
            read_only
                .create_file(ROOT_NODE, "denied", 0o600, writable_options())
                .await,
            Err(ServerError::ReadOnly)
        ));

        read_only.set_write_authorized(true);
        let node = read_only.list(ROOT_NODE).await.unwrap()[0].node;
        let handle = read_only.open(node, writable_options()).await.unwrap().0;
        read_only.write(handle, 0, b"A").await.unwrap();
        read_only.set_write_authorized(false);
        assert!(!read_only.capabilities().writable);
        assert!(matches!(
            read_only.write(handle, 1, b"B").await,
            Err(ServerError::ReadOnly)
        ));
        assert!(matches!(
            read_only
                .set_lock(
                    handle,
                    FileLock {
                        owner: 7,
                        start: 0,
                        end: u64::MAX,
                        kind: LockKind::Write,
                        pid: 42,
                    },
                    false,
                )
                .await,
            Err(ServerError::ReadOnly)
        ));
        assert_eq!(read_only.read(handle, 0, 4).await.unwrap().1, b"Aata");
        assert!(writable.capabilities().writable);
    }

    #[tokio::test]
    async fn node_ids_survive_reconnect_rename_and_hardlinks() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("original"), b"data").unwrap();
        std::fs::hard_link(
            directory.path().join("original"),
            directory.path().join("alias"),
        )
        .unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let first = export.session_with_writes(true);
        let entries = first.list(ROOT_NODE).await.unwrap();
        assert_eq!(entries[0].node, entries[1].node);
        let stable = entries[0].node;
        first
            .rename_node(
                ROOT_NODE,
                "original",
                ROOT_NODE,
                "renamed",
                RenameMode::Replace,
            )
            .await
            .unwrap();
        let second = export.session();
        assert_eq!(second.metadata(stable).await.unwrap().node, stable);
        assert!(
            second
                .list(ROOT_NODE)
                .await
                .unwrap()
                .iter()
                .filter(|entry| entry.node == stable)
                .count()
                >= 2
        );
    }

    #[tokio::test]
    async fn directories_symlinks_removal_and_real_revisions_work() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let (before, _) = session.list_with_revision(ROOT_NODE).await.unwrap();
        let child = session
            .create_directory(ROOT_NODE, "folder", 0o750)
            .await
            .unwrap();
        let link = session
            .create_symlink(ROOT_NODE, "folder-link", b"folder")
            .await
            .unwrap();
        assert_eq!(session.read_link(link.node).await.unwrap(), b"folder");
        let (after, entries) = session.list_with_revision(ROOT_NODE).await.unwrap();
        assert_ne!(before, after);
        assert_eq!(entries.len(), 2);
        session
            .remove_node(ROOT_NODE, "folder-link", false)
            .await
            .unwrap();
        session
            .remove_node(ROOT_NODE, "folder", true)
            .await
            .unwrap();
        assert!(matches!(
            session.metadata(child.node).await,
            Err(ServerError::InvalidNode)
        ));
    }

    #[tokio::test]
    async fn renaming_a_directory_preserves_descendant_node_ids() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let folder = session
            .create_directory(ROOT_NODE, "before", 0o750)
            .await
            .unwrap();
        let (child, child_handle, _, _) = session
            .create_file(folder.node, "clip.mov", 0o640, writable_options())
            .await
            .unwrap();
        session.close(child_handle).unwrap();
        session
            .rename_node(
                ROOT_NODE,
                "before",
                ROOT_NODE,
                "after",
                RenameMode::NoReplace,
            )
            .await
            .unwrap();

        assert_eq!(
            session.metadata(folder.node).await.unwrap().node,
            folder.node
        );
        assert_eq!(session.metadata(child.node).await.unwrap().node, child.node);
        assert!(directory.path().join("after/clip.mov").is_file());
    }

    #[tokio::test]
    async fn rename_modes_preserve_identity_and_enforce_no_replace() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("left"), b"L").unwrap();
        std::fs::write(directory.path().join("right"), b"R").unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let entries = session.list(ROOT_NODE).await.unwrap();
        let left = entries
            .iter()
            .find(|entry| entry.name == Name::from("left"))
            .unwrap()
            .node;
        let right = entries
            .iter()
            .find(|entry| entry.name == Name::from("right"))
            .unwrap()
            .node;
        assert!(matches!(
            session
                .rename_node(ROOT_NODE, "left", ROOT_NODE, "right", RenameMode::NoReplace,)
                .await,
            Err(ServerError::AlreadyExists)
        ));
        session
            .rename_node(ROOT_NODE, "left", ROOT_NODE, "right", RenameMode::Exchange)
            .await
            .unwrap();
        let entries = session.list(ROOT_NODE).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name == Name::from("right"))
                .unwrap()
                .node,
            left
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.name == Name::from("left"))
                .unwrap()
                .node,
            right
        );
    }

    #[tokio::test]
    async fn byte_range_locks_conflict_split_and_cleanup_across_sessions() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("x"), b"abcdef").unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let first = export.session_with_writes(true);
        let second = export.session_with_writes(true);
        let node = first.list(ROOT_NODE).await.unwrap()[0].node;
        let first_handle = first.open(node, writable_options()).await.unwrap().0;
        let second_handle = second.open(node, writable_options()).await.unwrap().0;
        let write_lock = FileLock {
            owner: 1,
            start: 10,
            end: 30,
            kind: LockKind::Write,
            pid: 10,
        };
        first
            .set_lock(first_handle, write_lock, false)
            .await
            .unwrap();
        assert!(matches!(
            second.set_lock(second_handle, write_lock, false).await,
            Err(ServerError::WouldBlock)
        ));
        first
            .set_lock(
                first_handle,
                FileLock {
                    kind: LockKind::Unlock,
                    start: 15,
                    end: 20,
                    ..write_lock
                },
                false,
            )
            .await
            .unwrap();
        assert!(
            second
                .set_lock(
                    second_handle,
                    FileLock {
                        start: 15,
                        end: 20,
                        ..write_lock
                    },
                    false,
                )
                .await
                .is_ok()
        );
        assert!(matches!(
            second
                .set_lock(
                    second_handle,
                    FileLock {
                        start: 10,
                        end: 14,
                        ..write_lock
                    },
                    false,
                )
                .await,
            Err(ServerError::WouldBlock)
        ));
        first.cleanup_locks().unwrap();
        assert!(
            second
                .set_lock(
                    second_handle,
                    FileLock {
                        start: 10,
                        end: 14,
                        ..write_lock
                    },
                    false,
                )
                .await
                .is_ok()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mutations_reject_symlinked_parent_escape() {
        use std::os::unix::fs::symlink;
        let export_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), export_root.path().join("escape")).unwrap();
        let export = Export::new_writable(export_root.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let escape = session
            .list(ROOT_NODE)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == Name::from("escape"))
            .unwrap();
        assert!(matches!(
            session
                .create_file(escape.node, "outside", 0o600, writable_options())
                .await,
            Err(ServerError::NotFound | ServerError::NotDirectory)
        ));
        assert!(!outside.path().join("outside").exists());
    }

    #[tokio::test]
    async fn handles_remain_session_scoped_and_globally_bounded() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("x"), b"abcdef").unwrap();
        let limits = Limits {
            max_open_handles: 1,
            ..Limits::default()
        };
        let export = Export::new(directory.path(), limits).await.unwrap();
        let first = export.session();
        let second = export.session();
        let node = first.list(ROOT_NODE).await.unwrap()[0].node;
        let handle = first
            .open(node, FileOpenOptions::READ_ONLY)
            .await
            .unwrap()
            .0;
        assert!(matches!(
            second.open(node, FileOpenOptions::READ_ONLY).await,
            Err(ServerError::TooManyHandles)
        ));
        assert!(matches!(
            second.read(handle, 0, 1).await,
            Err(ServerError::InvalidHandle)
        ));
        drop(first);
        assert!(second.open(node, FileOpenOptions::READ_ONLY).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn xattrs_resource_forks_and_backup_time_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let export_root = root.path().join("export");
        std::fs::create_dir(&export_root).unwrap();
        let export = Export::new_persistent_with_writes(
            &export_root,
            root.path().join("export-state.json"),
            Limits::default(),
            true,
        )
        .await
        .unwrap();
        let session = export.session_with_writes(true);
        let (created, handle, _, _) = session
            .create_file(ROOT_NODE, "tagged", 0o600, writable_options())
            .await
            .unwrap();
        let tag = Name::from("com.quickfs.test-tag");
        session
            .set_xattr(
                created.node,
                &tag,
                b"abcdef".to_vec(),
                XattrSetMode::Create,
                0,
            )
            .await
            .unwrap();
        assert!(matches!(
            session
                .set_xattr(
                    created.node,
                    &tag,
                    b"again".to_vec(),
                    XattrSetMode::Create,
                    0,
                )
                .await,
            Err(ServerError::AlreadyExists)
        ));
        session
            .set_xattr(created.node, &tag, b"ZZ".to_vec(), XattrSetMode::Replace, 2)
            .await
            .unwrap();
        assert_eq!(
            session.get_xattr(created.node, &tag, 1, 4).await.unwrap(),
            (6, b"bZZe".to_vec())
        );
        assert!(
            session
                .list_xattrs(created.node)
                .await
                .unwrap()
                .contains(&tag)
        );

        let fork = Name::from("com.apple.ResourceFork");
        session
            .set_xattr(
                created.node,
                &fork,
                b"fork-".to_vec(),
                XattrSetMode::Create,
                0,
            )
            .await
            .unwrap();
        session
            .set_xattr(
                created.node,
                &fork,
                b"data".to_vec(),
                XattrSetMode::Replace,
                5,
            )
            .await
            .unwrap();
        assert_eq!(
            session.get_xattr(created.node, &fork, 0, 64).await.unwrap(),
            (9, b"fork-data".to_vec())
        );

        let backup = 1_700_123_456_789;
        let changed = session
            .set_attributes(
                created.node,
                Some(handle),
                AttributeChanges {
                    size: None,
                    mode: None,
                    accessed_unix_ms: None,
                    modified_unix_ms: None,
                    backup_unix_ms: Some(backup),
                },
            )
            .await
            .unwrap();
        assert_eq!(changed.backup_unix_ms, Some(backup));
        assert_eq!(
            session.metadata(created.node).await.unwrap().backup_unix_ms,
            Some(backup)
        );

        session.remove_xattr(created.node, &tag).await.unwrap();
        assert!(matches!(
            session.get_xattr(created.node, &tag, 0, 1).await,
            Err(ServerError::NoAttribute)
        ));
        session.remove_xattr(created.node, &fork).await.unwrap();
        assert!(matches!(
            session.get_xattr(created.node, &fork, 0, 1).await,
            Err(ServerError::NoAttribute)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enriched_directory_view_batches_metadata_and_bounded_xattrs() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let folder = session
            .create_directory(ROOT_NODE, "folder", 0o750)
            .await
            .unwrap();
        let (file, handle, _, _) = session
            .create_file(ROOT_NODE, "clip.mov", 0o640, writable_options())
            .await
            .unwrap();
        session.close(handle).unwrap();
        let small_name = Name::from("user.DOSATTRIB");
        let large_name = Name::from("com.apple.metadata:_kMDItemUserTags");
        session
            .set_xattr(
                file.node,
                &small_name,
                b"0x20".to_vec(),
                XattrSetMode::Upsert,
                0,
            )
            .await
            .unwrap();
        session
            .set_xattr(
                file.node,
                &large_name,
                vec![0x5a; MAX_DIRECTORY_INLINE_XATTR_SIZE as usize + 1],
                XattrSetMode::Upsert,
                0,
            )
            .await
            .unwrap();

        let view = session
            .directory_view(ROOT_NODE, DirectoryViewOptions::NATIVE)
            .await
            .unwrap();
        assert_eq!(view.directory.node, ROOT_NODE);
        assert_eq!(view.parent.node, ROOT_NODE);
        let file = view
            .entries
            .iter()
            .find(|entry| entry.entry.node == file.node)
            .unwrap();
        let xattrs = file.xattrs.as_ref().unwrap();
        assert!(xattrs.names.contains(&small_name));
        assert!(xattrs.names.contains(&large_name));
        assert_eq!(
            xattrs
                .inline_values
                .iter()
                .find(|value| value.name == small_name)
                .unwrap()
                .value,
            b"0x20"
        );
        assert!(
            xattrs
                .inline_values
                .iter()
                .all(|value| value.name != large_name)
        );

        let nested = session
            .directory_view(folder.node, DirectoryViewOptions::NATIVE)
            .await
            .unwrap();
        assert_eq!(nested.directory.node, folder.node);
        assert_eq!(nested.parent.node, ROOT_NODE);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directory_view_beyond_one_frame_projects_every_entry() {
        // A directory whose enriched view serializes past MAX_FRAME_SIZE must no
        // longer be rejected with DirectoryTooLarge: the daemon streams it in
        // chunks, so the server layer simply projects all entries.
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        const ENTRIES: usize = 12_000;
        for index in 0..ENTRIES {
            std::fs::write(directory.path().join(format!("entry_{index:08}.dat")), []).unwrap();
        }
        let view = session
            .directory_view(ROOT_NODE, DirectoryViewOptions::NATIVE)
            .await
            .unwrap();
        assert_eq!(view.entries.len(), ENTRIES);
        // Confirm the projection genuinely exceeds a single frame, so this test
        // actually exercises the path that used to fail.
        assert!(
            encoded_len(&Response::DirectoryView(view)).unwrap() > MAX_FRAME_SIZE,
            "test directory should exceed one frame to be meaningful"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hardlinks_copy_seek_ioctl_bmap_and_exchangedata_work() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let (left, left_handle, _, _) = session
            .create_file(ROOT_NODE, "left", 0o600, writable_options())
            .await
            .unwrap();
        session.write(left_handle, 0, b"abcdefgh").await.unwrap();
        let linked = session
            .create_hard_link(left.node, ROOT_NODE, "left-link")
            .await
            .unwrap();
        assert_eq!(linked.node, left.node);
        assert_eq!(linked.link_count, 2);

        let (right, right_handle, _, _) = session
            .create_file(ROOT_NODE, "right", 0o600, writable_options())
            .await
            .unwrap();
        session.write(right_handle, 0, b"1234").await.unwrap();
        let copied = session
            .copy_file_range(left_handle, 2, right_handle, 1, 4)
            .await
            .unwrap();
        assert_eq!(copied.0, 4);
        assert_eq!(session.read(right_handle, 0, 16).await.unwrap().1, b"1cdef");
        assert_eq!(
            session
                .seek_file(left_handle, 0, SeekWhence::Data)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            session
                .seek_file(left_handle, 0, SeekWhence::Hole)
                .await
                .unwrap(),
            8
        );
        assert_eq!(
            session
                .safe_ioctl(left_handle, SafeIoctl::BytesAvailable)
                .unwrap(),
            8
        );
        assert_eq!(session.map_block(left.node, 4096, 0).await.unwrap(), 0);

        session
            .exchange_data(ROOT_NODE, "left", ROOT_NODE, "right", 0)
            .await
            .unwrap();
        assert_eq!(session.read(left_handle, 0, 16).await.unwrap().1, b"1cdef");
        assert_eq!(
            session.read(right_handle, 0, 16).await.unwrap().1,
            b"abcdefgh"
        );
        assert_eq!(session.metadata(right.node).await.unwrap().node, right.node);
    }

    #[tokio::test]
    async fn persistent_epoch_node_ids_and_volume_name_survive_export_restart() {
        let root = tempfile::tempdir().unwrap();
        let export_root = root.path().join("export");
        std::fs::create_dir(&export_root).unwrap();
        std::fs::write(export_root.join("clip.mov"), b"media").unwrap();
        let state = root.path().join("export-state.json");

        let first =
            Export::new_persistent_with_writes(&export_root, &state, Limits::default(), true)
                .await
                .unwrap();
        let first_session = first.session_with_writes(true);
        let node = first_session.list(ROOT_NODE).await.unwrap()[0].node;
        let epoch = first_session.capabilities().server_epoch;
        first_session
            .set_volume_name(&Name::from("Editing Media"))
            .unwrap();
        drop(first_session);
        drop(first);

        let second =
            Export::new_persistent_with_writes(&export_root, &state, Limits::default(), true)
                .await
                .unwrap();
        let second_session = second.session();
        let capabilities = second_session.capabilities();
        assert_eq!(capabilities.server_epoch, epoch);
        assert_eq!(capabilities.volume_name, "Editing Media");
        assert!(capabilities.persistent_node_ids);
        assert!(capabilities.restart_lock_replay);
        assert_eq!(second_session.metadata(node).await.unwrap().node, node);
    }

    #[tokio::test]
    async fn server_side_copy_is_not_limited_by_wire_payload_sizes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source"), b"0123456789abcdef").unwrap();
        std::fs::write(directory.path().join("destination"), b"").unwrap();
        let limits = Limits {
            max_read_size: 4,
            max_write_size: 4,
            ..Limits::default()
        };
        let export = Export::new_writable(directory.path(), limits)
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let entries = session.list(ROOT_NODE).await.unwrap();
        let source = entries
            .iter()
            .find(|entry| entry.name == Name::from("source"))
            .unwrap()
            .node;
        let destination = entries
            .iter()
            .find(|entry| entry.name == Name::from("destination"))
            .unwrap()
            .node;
        let source = session
            .open(source, FileOpenOptions::READ_ONLY)
            .await
            .unwrap()
            .0;
        let destination = session
            .open(destination, writable_options())
            .await
            .unwrap()
            .0;
        assert_eq!(
            session
                .copy_file_range(source, 0, destination, 0, 16)
                .await
                .unwrap()
                .0,
            16
        );
        assert_eq!(
            std::fs::read(directory.path().join("destination")).unwrap(),
            b"0123456789abcdef"
        );
    }

    #[tokio::test]
    async fn forgotten_nodes_release_server_registry_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let export_root = directory.path().join("export");
        std::fs::create_dir(&export_root).unwrap();
        std::fs::write(export_root.join("old"), b"old").unwrap();
        let limits = Limits {
            max_known_nodes: 2,
            max_total_known_nodes: 2,
            ..Limits::default()
        };
        let export = Export::new(&export_root, limits).await.unwrap();
        let session = export.session();
        let old = session.list(ROOT_NODE).await.unwrap()[0].node;
        session.forget_nodes(&[old, old]).unwrap();
        assert!(
            export
                .shared
                .nodes
                .lock()
                .unwrap()
                .by_node
                .contains_key(&old)
        );
        std::fs::rename(
            export_root.join("old"),
            directory.path().join("old-still-alive"),
        )
        .unwrap();
        std::fs::write(export_root.join("new"), b"new").unwrap();
        let names = session
            .list(ROOT_NODE)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&Name::from("new")));
        assert!(
            !export
                .shared
                .nodes
                .lock()
                .unwrap()
                .by_node
                .contains_key(&old)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn special_fifo_and_socket_nodes_are_created_with_mknod() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let fifo = session
            .create_special_node(ROOT_NODE, "fifo", SpecialNodeKind::NamedPipe, 0o600, 0, 0)
            .await
            .unwrap();
        assert_eq!(fifo.kind, NodeKind::NamedPipe);
        let socket = session
            .create_special_node(ROOT_NODE, "socket", SpecialNodeKind::Socket, 0o600, 0, 0)
            .await
            .unwrap();
        assert_eq!(socket.kind, NodeKind::Socket);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn non_utf8_names_round_trip_losslessly() {
        let directory = tempfile::tempdir().unwrap();
        let export = Export::new_writable(directory.path(), Limits::default())
            .await
            .unwrap();
        let session = export.session_with_writes(true);
        let raw = b"clip-\xff.mov";
        let created = session
            .create_directory(ROOT_NODE, raw, 0o700)
            .await
            .unwrap();
        let entry = session
            .list(ROOT_NODE)
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.node == created.node)
            .unwrap();
        assert_eq!(entry.name.as_bytes(), raw);
        session.remove_node(ROOT_NODE, raw, true).await.unwrap();
    }
}
