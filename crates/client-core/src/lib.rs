// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use async_trait::async_trait;
use quickfs_protocol::*;
use quickfs_transport_quic::{QuicClient, TransportError, read_frame, write_frame};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("server: {0:?}: {1}")]
    Server(ErrorCode, String),
    #[error("unexpected response")]
    UnexpectedResponse,
}
pub type Result<T> = std::result::Result<T, ClientError>;
#[async_trait]
pub trait RemoteFilesystem: Send + Sync {
    async fn ping(&self, nonce: u64) -> Result<u64>;
    async fn get_metadata(&self, node: NodeId) -> Result<Metadata>;
    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>>;
    async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)>;
    async fn read_range(&self, handle: FileHandle, offset: u64, length: u64) -> Result<Vec<u8>>;
    async fn close_file(&self, handle: FileHandle) -> Result<()>;
}
pub struct NetworkFilesystem {
    transport: Arc<QuicClient>,
}
impl NetworkFilesystem {
    pub async fn authenticate(transport: QuicClient, token: String) -> Result<Self> {
        let this = Self {
            transport: Arc::new(transport),
        };
        match this.request(Request::Authenticate { token }).await?.0 {
            Response::AuthenticateAck => Ok(this),
            r => Err(response_error(r)),
        }
    }
    async fn request(
        &self,
        message: Request,
    ) -> Result<(Response, Option<quickfs_transport_quic::RecvStream>)> {
        let request = Envelope::new(message);
        let (mut send, mut recv) = self.transport.stream().await?;
        write_frame(&mut send, &request).await?;
        send.finish().map_err(TransportError::Closed)?;
        let response: Envelope<Response> = read_frame(&mut recv).await?;
        if response.request_id != request.request_id {
            return Err(ClientError::UnexpectedResponse);
        };
        Ok((response.message, Some(recv)))
    }
}
fn response_error(r: Response) -> ClientError {
    if let Response::Error(e) = r {
        ClientError::Server(e.code, e.message)
    } else {
        ClientError::UnexpectedResponse
    }
}
#[async_trait]
impl RemoteFilesystem for NetworkFilesystem {
    async fn ping(&self, nonce: u64) -> Result<u64> {
        match self.request(Request::Ping { nonce }).await?.0 {
            Response::Pong { nonce } => Ok(nonce),
            r => Err(response_error(r)),
        }
    }
    async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
        match self.request(Request::GetMetadata { node }).await?.0 {
            Response::Metadata(v) => Ok(v),
            r => Err(response_error(r)),
        }
    }
    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        match self.request(Request::ListDirectory { node }).await?.0 {
            Response::DirectoryListing { entries, .. } => Ok(entries),
            r => Err(response_error(r)),
        }
    }
    async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)> {
        match self.request(Request::OpenFile { node }).await?.0 {
            Response::FileOpened {
                handle,
                revision,
                size,
            } => Ok((handle, revision, size)),
            r => Err(response_error(r)),
        }
    }
    async fn read_range(&self, handle: FileHandle, offset: u64, length: u64) -> Result<Vec<u8>> {
        let (response, recv) = self
            .request(Request::ReadRange {
                handle,
                offset,
                length,
            })
            .await?;
        match response {
            Response::ReadData { length, .. } => {
                let mut recv = recv.ok_or(ClientError::UnexpectedResponse)?;
                let size: usize = length
                    .try_into()
                    .map_err(|_| ClientError::UnexpectedResponse)?;
                let mut data = vec![0; size];
                recv.read_exact(&mut data)
                    .await
                    .map_err(TransportError::Read)?;
                Ok(data)
            }
            r => Err(response_error(r)),
        }
    }
    async fn close_file(&self, handle: FileHandle) -> Result<()> {
        match self.request(Request::CloseFile { handle }).await?.0 {
            Response::FileClosed => Ok(()),
            r => Err(response_error(r)),
        }
    }
}
pub async fn resolve_path(fs: &dyn RemoteFilesystem, path: &str) -> Result<NodeId> {
    let mut node = ROOT_NODE;
    for part in path.split('/').filter(|v| !v.is_empty()) {
        let entries = fs.list_directory(node).await?;
        node = entries
            .into_iter()
            .find(|e| e.name == part)
            .ok_or_else(|| ClientError::Server(ErrorCode::NotFound, part.into()))?
            .node;
    }
    Ok(node)
}

pub struct DelayedFilesystem<T> {
    inner: T,
    delay: std::time::Duration,
}
impl<T> DelayedFilesystem<T> {
    pub fn new(inner: T, delay: std::time::Duration) -> Self {
        Self { inner, delay }
    }
    async fn wait(&self) {
        tokio::time::sleep(self.delay).await
    }
}
#[async_trait]
impl<T: RemoteFilesystem> RemoteFilesystem for DelayedFilesystem<T> {
    async fn ping(&self, n: u64) -> Result<u64> {
        self.wait().await;
        self.inner.ping(n).await
    }
    async fn get_metadata(&self, n: NodeId) -> Result<Metadata> {
        self.wait().await;
        self.inner.get_metadata(n).await
    }
    async fn list_directory(&self, n: NodeId) -> Result<Vec<DirectoryEntry>> {
        self.wait().await;
        self.inner.list_directory(n).await
    }
    async fn open_file(&self, n: NodeId) -> Result<(FileHandle, u64, u64)> {
        self.wait().await;
        self.inner.open_file(n).await
    }
    async fn read_range(&self, h: FileHandle, o: u64, l: u64) -> Result<Vec<u8>> {
        self.wait().await;
        self.inner.read_range(h, o, l).await
    }
    async fn close_file(&self, h: FileHandle) -> Result<()> {
        self.wait().await;
        self.inner.close_file(h).await
    }
}
