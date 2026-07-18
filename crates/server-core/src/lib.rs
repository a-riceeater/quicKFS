// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use dashmap::DashMap;
use quickfs_common::{Limits, validate_range};
use quickfs_protocol::*;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
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
    #[error("directory listing exceeds the control-frame limit")]
    DirectoryTooLarge,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
impl ServerError {
    pub fn protocol(&self) -> ProtocolError {
        let code = match self {
            Self::NotFound => ErrorCode::NotFound,
            Self::PermissionDenied => ErrorCode::PermissionDenied,
            Self::InvalidNode => ErrorCode::InvalidNode,
            Self::InvalidHandle => ErrorCode::InvalidHandle,
            Self::TooManyHandles => ErrorCode::TooLarge,
            Self::TooManyNodes => ErrorCode::TooLarge,
            Self::DirectoryTooLarge => ErrorCode::TooLarge,
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::Io(_) => ErrorCode::Internal,
        };
        ProtocolError {
            code,
            message: if matches!(self, Self::Io(_)) {
                "internal server error".into()
            } else {
                self.to_string()
            },
        }
    }
}
struct OpenFile {
    file: Mutex<File>,
    revision: u64,
    size: u64,
    _permit: OwnedSemaphorePermit,
}
struct KnownNode {
    path: PathBuf,
    _session_permit: Option<OwnedSemaphorePermit>,
    _global_permit: Option<OwnedSemaphorePermit>,
}
#[derive(Clone)]
pub struct Export {
    root: Arc<PathBuf>,
    limits: Limits,
    handle_permits: Arc<Semaphore>,
    node_permits: Arc<Semaphore>,
}

/// Per-connection filesystem capabilities. Dropping a session closes every
/// handle opened by that connection and makes its node identifiers unusable by
/// other connections.
pub struct ExportSession {
    export: Export,
    nodes: DashMap<NodeId, KnownNode>,
    node_permits: Arc<Semaphore>,
    handles: DashMap<FileHandle, Arc<OpenFile>>,
}
impl Export {
    pub async fn new(root: impl AsRef<Path>, limits: Limits) -> Result<Self, ServerError> {
        if limits.max_open_handles > Semaphore::MAX_PERMITS {
            return Err(ServerError::InvalidRequest(
                "maximum open handles exceeds the runtime limit".into(),
            ));
        }
        if limits.max_known_nodes == 0 || limits.max_known_nodes > Semaphore::MAX_PERMITS {
            return Err(ServerError::InvalidRequest(
                "maximum known nodes must be within the runtime semaphore limit".into(),
            ));
        }
        if limits.max_total_known_nodes == 0
            || limits.max_total_known_nodes > Semaphore::MAX_PERMITS
            || limits.max_known_nodes.saturating_sub(1) > limits.max_total_known_nodes
        {
            return Err(ServerError::InvalidRequest(
                "total known-node capacity must cover one connection and fit the runtime limit"
                    .into(),
            ));
        }
        let root = fs::canonicalize(root).await?;
        if !fs::metadata(&root).await?.is_dir() {
            return Err(ServerError::InvalidRequest(
                "export root is not a directory".into(),
            ));
        }
        Ok(Self {
            root: Arc::new(root),
            handle_permits: Arc::new(Semaphore::new(limits.max_open_handles)),
            node_permits: Arc::new(Semaphore::new(limits.max_total_known_nodes)),
            limits,
        })
    }

    pub fn session(&self) -> ExportSession {
        let nodes = DashMap::new();
        nodes.insert(
            ROOT_NODE,
            KnownNode {
                path: self.root.as_ref().clone(),
                _session_permit: None,
                _global_permit: None,
            },
        );
        ExportSession {
            node_permits: Arc::new(Semaphore::new(self.limits.max_known_nodes - 1)),
            export: self.clone(),
            nodes,
            handles: DashMap::new(),
        }
    }
}

impl ExportSession {
    fn id_for(&self, path: &Path) -> NodeId {
        let relative = path
            .strip_prefix(self.export.root.as_ref())
            .unwrap_or(Path::new(""));
        let digest = Sha256::digest(relative.as_os_str().as_encoded_bytes());
        let mut b = [0; 16];
        b.copy_from_slice(&digest[..16]);
        NodeId(Uuid::from_bytes(b))
    }
    fn remember(&self, path: PathBuf) -> Result<NodeId, ServerError> {
        use dashmap::mapref::entry::Entry;

        let id = self.id_for(&path);
        match self.nodes.entry(id) {
            Entry::Occupied(entry) if entry.get().path == path => Ok(id),
            Entry::Occupied(_) => Err(ServerError::InvalidNode),
            Entry::Vacant(entry) => {
                let session_permit = self
                    .node_permits
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ServerError::TooManyNodes)?;
                let global_permit = self
                    .export
                    .node_permits
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| ServerError::TooManyNodes)?;
                entry.insert(KnownNode {
                    path,
                    _session_permit: Some(session_permit),
                    _global_permit: Some(global_permit),
                });
                Ok(id)
            }
        }
    }
    async fn safe_child(
        &self,
        parent: &Path,
        name: &std::ffi::OsStr,
    ) -> Result<PathBuf, ServerError> {
        let candidate = parent.join(name);
        let canonical = fs::canonicalize(&candidate).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ServerError::NotFound
            } else {
                e.into()
            }
        })?;
        if !canonical.starts_with(self.export.root.as_ref()) {
            return Err(ServerError::PermissionDenied);
        }
        Ok(canonical)
    }
    async fn path(&self, node: NodeId) -> Result<PathBuf, ServerError> {
        let stored = self
            .nodes
            .get(&node)
            .map(|value| value.path.clone())
            .ok_or(ServerError::InvalidNode)?;
        let canonical = fs::canonicalize(stored).await?;
        if !canonical.starts_with(self.export.root.as_ref()) {
            return Err(ServerError::PermissionDenied);
        }
        Ok(canonical)
    }
    pub async fn metadata(&self, node: NodeId) -> Result<Metadata, ServerError> {
        let p = self.path(node).await?;
        let m = fs::symlink_metadata(p).await?;
        Ok(to_metadata(node, &m))
    }
    pub async fn list(&self, node: NodeId) -> Result<Vec<DirectoryEntry>, ServerError> {
        let p = self.path(node).await?;
        let mut rd = fs::read_dir(&p).await?;
        let mut out = Vec::new();
        let mut estimated_frame_size = 128usize;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name();
            let display_name = name.to_string_lossy().into_owned();
            estimated_frame_size = estimated_frame_size
                .checked_add(display_name.len().saturating_add(64))
                .ok_or(ServerError::DirectoryTooLarge)?;
            if estimated_frame_size > MAX_FRAME_SIZE {
                return Err(ServerError::DirectoryTooLarge);
            }
            let child = self.safe_child(&p, &name).await?;
            let id = self.remember(child)?;
            let ft = entry.file_type().await?;
            out.push(DirectoryEntry {
                node: id,
                name: display_name,
                kind: if ft.is_dir() {
                    NodeKind::Directory
                } else if ft.is_file() {
                    NodeKind::File
                } else {
                    NodeKind::Symlink
                },
            })
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
    pub async fn open(&self, node: NodeId) -> Result<(FileHandle, u64, u64), ServerError> {
        let permit = self
            .export
            .handle_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServerError::TooManyHandles)?;
        let p = self.path(node).await?;
        let file = File::open(&p).await?;
        let m = file.metadata().await?;
        if !m.is_file() {
            return Err(ServerError::InvalidRequest("node is not a file".into()));
        }
        let confirmed_path = self.path(node).await?;
        let confirmed_metadata = fs::metadata(confirmed_path).await?;
        if !same_file(&m, &confirmed_metadata) {
            return Err(ServerError::PermissionDenied);
        }
        let revision = revision(&m);
        let handle = FileHandle(Uuid::new_v4());
        self.handles.insert(
            handle,
            Arc::new(OpenFile {
                file: Mutex::new(file),
                revision,
                size: m.len(),
                _permit: permit,
            }),
        );
        Ok((handle, revision, m.len()))
    }
    pub async fn read(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<(u64, Vec<u8>), ServerError> {
        validate_range(offset, length, self.export.limits.max_read_size)
            .map_err(|e| ServerError::InvalidRequest(e.to_string()))?;
        let opened = self
            .handles
            .get(&handle)
            .map(|v| v.clone())
            .ok_or(ServerError::InvalidHandle)?;
        let available = opened.size.saturating_sub(offset);
        let amount = length.min(available);
        let size: usize = amount
            .try_into()
            .map_err(|_| ServerError::InvalidRequest("range does not fit memory".into()))?;
        let mut data = vec![0; size];
        let mut file = opened.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(&mut data).await?;
        Ok((opened.revision, data))
    }
    pub fn close(&self, handle: FileHandle) -> Result<(), ServerError> {
        self.handles
            .remove(&handle)
            .map(|_| ())
            .ok_or(ServerError::InvalidHandle)
    }
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    true
}
fn revision(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
        .map(|v| v.as_nanos() as u64)
        .unwrap_or(0)
        ^ m.len()
}
fn to_metadata(node: NodeId, m: &std::fs::Metadata) -> Metadata {
    Metadata {
        node,
        kind: if m.is_dir() {
            NodeKind::Directory
        } else if m.is_file() {
            NodeKind::File
        } else {
            NodeKind::Symlink
        },
        size: m.len(),
        revision: revision(m),
        modified_unix_ms: m
            .modified()
            .ok()
            .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
            .map(|v| v.as_millis() as u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn read_boundaries_and_invalid_handle() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"abcdef").unwrap();
        let export = Export::new(d.path(), Limits::default()).await.unwrap();
        let e = export.session();
        let list = e.list(ROOT_NODE).await.unwrap();
        let (h, _, _) = e.open(list[0].node).await.unwrap();
        assert_eq!(e.read(h, 4, 9).await.unwrap().1, b"ef");
        assert!(e.read(FileHandle(Uuid::nil()), 0, 1).await.is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        symlink("/etc", d.path().join("escape")).unwrap();
        let export = Export::new(d.path(), Limits::default()).await.unwrap();
        let e = export.session();
        assert!(matches!(
            e.list(ROOT_NODE).await,
            Err(ServerError::PermissionDenied)
        ));
    }

    #[tokio::test]
    async fn handles_are_session_scoped_and_globally_bounded() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"abcdef").unwrap();
        let limits = Limits {
            max_open_handles: 1,
            ..Limits::default()
        };
        let export = Export::new(d.path(), limits).await.unwrap();
        let first = export.session();
        let second = export.session();
        let first_node = first.list(ROOT_NODE).await.unwrap()[0].node;
        let second_node = second.list(ROOT_NODE).await.unwrap()[0].node;
        let (handle, _, _) = first.open(first_node).await.unwrap();

        assert!(matches!(
            second.open(second_node).await,
            Err(ServerError::TooManyHandles)
        ));
        assert!(matches!(
            second.read(handle, 0, 1).await,
            Err(ServerError::InvalidHandle)
        ));

        drop(first);
        assert!(second.open(second_node).await.is_ok());
    }

    #[tokio::test]
    async fn known_nodes_are_bounded_per_session() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"abcdef").unwrap();
        let limits = Limits {
            max_known_nodes: 1,
            ..Limits::default()
        };
        let export = Export::new(d.path(), limits).await.unwrap();
        let session = export.session();
        assert!(matches!(
            session.list(ROOT_NODE).await,
            Err(ServerError::TooManyNodes)
        ));
    }

    #[tokio::test]
    async fn known_nodes_are_globally_bounded_and_capacity_is_recovered() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"abcdef").unwrap();
        let limits = Limits {
            max_known_nodes: 2,
            max_total_known_nodes: 1,
            ..Limits::default()
        };
        let export = Export::new(d.path(), limits).await.unwrap();
        let first = export.session();
        let second = export.session();
        assert!(first.list(ROOT_NODE).await.is_ok());
        assert!(matches!(
            second.list(ROOT_NODE).await,
            Err(ServerError::TooManyNodes)
        ));
        drop(first);
        assert!(second.list(ROOT_NODE).await.is_ok());
    }
}
