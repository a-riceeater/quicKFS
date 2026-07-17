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
    sync::Mutex,
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
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::Io(_) => ErrorCode::Internal,
        };
        ProtocolError {
            code,
            message: self.to_string(),
        }
    }
}
struct OpenFile {
    file: Mutex<File>,
    revision: u64,
    size: u64,
}
#[derive(Clone)]
pub struct Export {
    root: Arc<PathBuf>,
    nodes: Arc<DashMap<NodeId, PathBuf>>,
    handles: Arc<DashMap<FileHandle, Arc<OpenFile>>>,
    limits: Limits,
}
impl Export {
    pub async fn new(root: impl AsRef<Path>, limits: Limits) -> Result<Self, ServerError> {
        let root = fs::canonicalize(root).await?;
        if !fs::metadata(&root).await?.is_dir() {
            return Err(ServerError::InvalidRequest(
                "export root is not a directory".into(),
            ));
        }
        let nodes = DashMap::new();
        nodes.insert(ROOT_NODE, root.clone());
        Ok(Self {
            root: Arc::new(root),
            nodes: Arc::new(nodes),
            handles: Arc::new(DashMap::new()),
            limits,
        })
    }
    fn id_for(&self, path: &Path) -> NodeId {
        let relative = path
            .strip_prefix(self.root.as_ref())
            .unwrap_or(Path::new(""));
        let digest = Sha256::digest(relative.as_os_str().as_encoded_bytes());
        let mut b = [0; 16];
        b.copy_from_slice(&digest[..16]);
        NodeId(Uuid::from_bytes(b))
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
        if !canonical.starts_with(self.root.as_ref()) {
            return Err(ServerError::PermissionDenied);
        }
        Ok(canonical)
    }
    fn path(&self, node: NodeId) -> Result<PathBuf, ServerError> {
        self.nodes
            .get(&node)
            .map(|v| v.clone())
            .ok_or(ServerError::InvalidNode)
    }
    pub async fn metadata(&self, node: NodeId) -> Result<Metadata, ServerError> {
        let p = self.path(node)?;
        let m = fs::symlink_metadata(p).await?;
        Ok(to_metadata(node, &m))
    }
    pub async fn list(&self, node: NodeId) -> Result<Vec<DirectoryEntry>, ServerError> {
        let p = self.path(node)?;
        let mut rd = fs::read_dir(&p).await?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name();
            let child = self.safe_child(&p, &name).await?;
            let id = self.id_for(&child);
            self.nodes.insert(id, child);
            let ft = entry.file_type().await?;
            out.push(DirectoryEntry {
                node: id,
                name: name.to_string_lossy().into_owned(),
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
        if self.handles.len() >= self.limits.max_open_handles {
            return Err(ServerError::TooManyHandles);
        }
        let p = self.path(node)?;
        let file = File::open(p).await?;
        let m = file.metadata().await?;
        if !m.is_file() {
            return Err(ServerError::InvalidRequest("node is not a file".into()));
        }
        let revision = revision(&m);
        let handle = FileHandle(Uuid::new_v4());
        self.handles.insert(
            handle,
            Arc::new(OpenFile {
                file: Mutex::new(file),
                revision,
                size: m.len(),
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
        validate_range(offset, length, self.limits.max_read_size)
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
        let e = Export::new(d.path(), Limits::default()).await.unwrap();
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
        let e = Export::new(d.path(), Limits::default()).await.unwrap();
        assert!(matches!(
            e.list(ROOT_NODE).await,
            Err(ServerError::PermissionDenied)
        ));
    }
}
