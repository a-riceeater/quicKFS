// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use async_trait::async_trait;
use quickfs_auth::parse_pairing_code;
use quickfs_protocol::*;
use quickfs_transport_quic::{PairingClient, QuicClient, TransportError};
use std::sync::Arc;
use uuid::Uuid;

mod cached;
mod resilient;
mod trust;
pub use cached::{CachePolicy, CachedFilesystem, FilesystemCache};
pub use resilient::{
    AuthenticatedConnectionConfig, ReconnectPolicy, RemoteFilesystemConnector, ResilientFilesystem,
};
pub use trust::{ServerTrust, TrustStoreError, load_trusted_server_pin};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("server: {0:?}: {1}")]
    Server(ErrorCode, String),
    #[error("unexpected response")]
    UnexpectedResponse,
    #[error("single read exceeds the client safety limit of {0} bytes")]
    ReadTooLarge(u64),
    #[error("single write exceeds the client safety limit of {0} bytes")]
    WriteTooLarge(u64),
    #[error("remote data changed while an operation was in progress")]
    StaleRevision,
    #[error("the remote filesystem is offline")]
    Offline,
    #[error("the requested data is not present in the offline cache")]
    OfflineCacheMiss,
    #[error("a mutating request may have executed before the connection failed")]
    AmbiguousMutation,
}
pub type Result<T> = std::result::Result<T, ClientError>;
pub const MAX_CLIENT_READ_SIZE: u64 = 16 * 1024 * 1024;
pub const MAX_CLIENT_WRITE_SIZE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct DirectorySnapshot {
    pub revision: DirectoryRevision,
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenedFile {
    pub handle: FileHandle,
    pub revision: FileRevision,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeRead {
    pub revision: FileRevision,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct XattrRead {
    pub total_size: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteResult {
    pub written: u64,
    pub revision: FileRevision,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreatedFile {
    pub metadata: Metadata,
    pub opened: OpenedFile,
}

#[async_trait]
pub trait RemoteFilesystem: Send + Sync {
    async fn ping(&self, nonce: u64) -> Result<u64>;
    async fn capabilities(&self) -> Result<FilesystemCapabilities> {
        Err(unsupported("capabilities"))
    }
    async fn stat_filesystem(&self) -> Result<FilesystemStats> {
        Err(unsupported("filesystem statistics"))
    }
    async fn get_metadata(&self, node: NodeId) -> Result<Metadata>;
    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>>;
    async fn list_directory_snapshot(&self, node: NodeId) -> Result<DirectorySnapshot> {
        Ok(DirectorySnapshot {
            revision: 0,
            entries: self.list_directory(node).await?,
        })
    }
    async fn list_directory_view(
        &self,
        node: NodeId,
        _options: DirectoryViewOptions,
    ) -> Result<DirectoryView> {
        let (snapshot, directory) =
            tokio::try_join!(self.list_directory_snapshot(node), self.get_metadata(node))?;
        Ok(DirectoryView {
            revision: snapshot.revision,
            parent: directory.clone(),
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
    async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)>;
    async fn open_file_with_options(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> Result<OpenedFile> {
        if options == FileOpenOptions::READ_ONLY {
            let (handle, revision, size) = self.open_file(node).await?;
            Ok(OpenedFile {
                handle,
                revision,
                size,
            })
        } else {
            Err(unsupported("writable open"))
        }
    }
    async fn create_file(
        &self,
        _parent: NodeId,
        _name: Name,
        _mode: u32,
        _options: FileOpenOptions,
    ) -> Result<CreatedFile> {
        Err(unsupported("file creation"))
    }
    async fn create_directory(&self, _parent: NodeId, _name: Name, _mode: u32) -> Result<Metadata> {
        Err(unsupported("directory creation"))
    }
    async fn create_symlink(
        &self,
        _parent: NodeId,
        _name: Name,
        _target: Vec<u8>,
    ) -> Result<Metadata> {
        Err(unsupported("symbolic links"))
    }
    async fn remove_node(&self, _parent: NodeId, _name: Name, _directory: bool) -> Result<()> {
        Err(unsupported("node removal"))
    }
    async fn rename_node(
        &self,
        _parent: NodeId,
        _name: Name,
        _new_parent: NodeId,
        _new_name: Name,
        _mode: RenameMode,
    ) -> Result<()> {
        Err(unsupported("rename"))
    }
    async fn read_link(&self, _node: NodeId) -> Result<Vec<u8>> {
        Err(unsupported("readlink"))
    }
    async fn create_hard_link(
        &self,
        _node: NodeId,
        _new_parent: NodeId,
        _new_name: Name,
    ) -> Result<Metadata> {
        Err(unsupported("hard links"))
    }
    async fn create_special_node(
        &self,
        _parent: NodeId,
        _name: Name,
        _kind: SpecialNodeKind,
        _mode: u32,
        _device_major: u32,
        _device_minor: u32,
    ) -> Result<Metadata> {
        Err(unsupported("special nodes"))
    }
    async fn set_attributes(
        &self,
        _node: NodeId,
        _handle: Option<FileHandle>,
        _changes: AttributeChanges,
    ) -> Result<Metadata> {
        Err(unsupported("attribute changes"))
    }
    async fn read_range(&self, handle: FileHandle, offset: u64, length: u64) -> Result<Vec<u8>>;
    async fn read_range_versioned(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<RangeRead> {
        Ok(RangeRead {
            revision: 0,
            data: self.read_range(handle, offset, length).await?,
        })
    }
    async fn write_range(
        &self,
        _handle: FileHandle,
        _offset: u64,
        _data: &[u8],
    ) -> Result<WriteResult> {
        Err(unsupported("writes"))
    }
    async fn flush_file(&self, _handle: FileHandle, _lock_owner: Option<u64>) -> Result<()> {
        Err(unsupported("flush"))
    }
    async fn sync_file(&self, _handle: FileHandle, _data_only: bool) -> Result<()> {
        Err(unsupported("fsync"))
    }
    async fn sync_directory(&self, _node: NodeId) -> Result<()> {
        Err(unsupported("directory fsync"))
    }
    async fn allocate_file(
        &self,
        _handle: FileHandle,
        _offset: u64,
        _length: u64,
    ) -> Result<WriteResult> {
        Err(unsupported("preallocation"))
    }
    async fn get_xattr(
        &self,
        _node: NodeId,
        _name: Name,
        _offset: u64,
        _length: u64,
    ) -> Result<XattrRead> {
        Err(unsupported("extended attributes"))
    }
    async fn set_xattr(
        &self,
        _node: NodeId,
        _name: Name,
        _value: &[u8],
        _mode: XattrSetMode,
        _position: u32,
    ) -> Result<()> {
        Err(unsupported("extended attributes"))
    }
    async fn list_xattrs(&self, _node: NodeId) -> Result<Vec<Name>> {
        Err(unsupported("extended attributes"))
    }
    async fn remove_xattr(&self, _node: NodeId, _name: Name) -> Result<()> {
        Err(unsupported("extended attributes"))
    }
    async fn copy_file_range(
        &self,
        _input: FileHandle,
        _input_offset: u64,
        _output: FileHandle,
        _output_offset: u64,
        _length: u64,
    ) -> Result<WriteResult> {
        Err(unsupported("server-side copy"))
    }
    async fn seek_file(
        &self,
        _handle: FileHandle,
        _offset: u64,
        _whence: SeekWhence,
    ) -> Result<u64> {
        Err(unsupported("SEEK_DATA/SEEK_HOLE"))
    }
    async fn safe_ioctl(&self, _handle: FileHandle, _operation: SafeIoctl) -> Result<u64> {
        Err(unsupported("safe ioctl"))
    }
    async fn map_block(&self, _node: NodeId, _block_size: u32, _block: u64) -> Result<u64> {
        Err(unsupported("block mapping"))
    }
    async fn exchange_data(
        &self,
        _parent: NodeId,
        _name: Name,
        _new_parent: NodeId,
        _new_name: Name,
        _options: u64,
    ) -> Result<()> {
        Err(unsupported("exchangedata"))
    }
    async fn set_volume_name(&self, _name: Name) -> Result<()> {
        Err(unsupported("volume rename"))
    }
    /// Advisory, idempotent release of nodes whose final kernel lookup
    /// reference has been forgotten. Implementations without a node registry
    /// may safely leave this as a no-op.
    async fn forget_nodes(&self, _nodes: Vec<NodeId>) -> Result<()> {
        Ok(())
    }
    async fn get_lock(&self, _handle: FileHandle, _lock: FileLock) -> Result<Option<FileLock>> {
        Err(unsupported("byte-range locks"))
    }
    async fn set_lock(&self, _handle: FileHandle, _lock: FileLock, _wait: bool) -> Result<()> {
        Err(unsupported("byte-range locks"))
    }
    async fn close_file(&self, handle: FileHandle) -> Result<()>;
}

fn unsupported(operation: &str) -> ClientError {
    ClientError::Server(
        ErrorCode::NotSupported,
        format!("server does not support {operation}"),
    )
}
pub struct NetworkFilesystem {
    transport: Arc<QuicClient>,
}
impl NetworkFilesystem {
    pub async fn authenticate(
        transport: QuicClient,
        username: String,
        password: String,
    ) -> Result<Self> {
        let this = Self {
            transport: Arc::new(transport),
        };
        this.negotiate().await?;
        match this
            .request(Request::Authenticate {
                username,
                password: password.into(),
            })
            .await?
            .0
        {
            Response::AuthenticateAck => Ok(this),
            r => Err(response_error(r)),
        }
    }

    /// Exchange `Hello`/`HelloAck` before authenticating so each side learns the
    /// other's exact version. The major was already validated by [`request`]; the
    /// server's minor decides whether we may compress the frames we send it.
    /// Requests before this (only the `Hello` itself) go out uncompressed.
    async fn negotiate(&self) -> Result<()> {
        match self
            .request(Request::Hello {
                client_name: "quickfs-client".into(),
            })
            .await?
            .0
        {
            Response::HelloAck { version } => {
                self.transport
                    .set_compression(peer_supports_frame_compression(version));
                Ok(())
            }
            r => Err(response_error(r)),
        }
    }
    async fn request(
        &self,
        message: Request,
    ) -> Result<(Response, Option<quickfs_transport_quic::RecvStream>)> {
        let mut request = Envelope::new(message);
        let (mut send, mut recv) = self.transport.stream().await?;
        let write_result = self.transport.send_frame(&mut send, &request).await;
        request.message.clear_secrets();
        write_result?;
        send.finish().map_err(TransportError::Closed)?;
        let response: Envelope<Response> = self.transport.receive_frame(&mut recv).await?;
        if version_major(response.version) != PROTOCOL_MAJOR
            || response.request_id != request.request_id
        {
            return Err(ClientError::UnexpectedResponse);
        };
        Ok((response.message, Some(recv)))
    }

    /// Fetch an enriched directory view, reassembling a streamed response.
    ///
    /// A view that fit one frame arrives as a single `DirectoryView` (the
    /// pre-streaming fast path). A larger view arrives as `DirectoryViewStart`
    /// followed by `DirectoryViewChunk` frames and a `DirectoryViewEnd`, all on
    /// this request's stream; they are stitched back into one `DirectoryView`
    /// so every caller above the transport sees an identical result either way.
    async fn list_directory_view_streamed(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
    ) -> Result<DirectoryView> {
        let request = Envelope::new(Request::ListDirectoryView { node, options });
        let (mut send, mut recv) = self.transport.stream().await?;
        self.transport.send_frame(&mut send, &request).await?;
        send.finish().map_err(TransportError::Closed)?;

        let first: Envelope<Response> = self.transport.receive_frame(&mut recv).await?;
        if version_major(first.version) != PROTOCOL_MAJOR || first.request_id != request.request_id
        {
            return Err(ClientError::UnexpectedResponse);
        }
        let (revision, directory, parent, xattrs, entry_count) = match first.message {
            Response::DirectoryView(view) => return Ok(view),
            Response::DirectoryViewStart {
                revision,
                directory,
                parent,
                xattrs,
                entry_count,
            } => (revision, directory, parent, xattrs, entry_count),
            response => return Err(response_error(response)),
        };

        // entry_count is advisory; cap the pre-allocation and re-check the real
        // total as chunks arrive so a misbehaving server cannot force unbounded
        // buffering on the client.
        let capacity = entry_count.min(MAX_DIRECTORY_ENTRIES as u64) as usize;
        let mut entries: Vec<DirectoryViewEntry> = Vec::with_capacity(capacity);
        loop {
            let frame: Envelope<Response> = self.transport.receive_frame(&mut recv).await?;
            if version_major(frame.version) != PROTOCOL_MAJOR
                || frame.request_id != request.request_id
            {
                return Err(ClientError::UnexpectedResponse);
            }
            match frame.message {
                Response::DirectoryViewChunk { entries: chunk } => {
                    if entries.len().saturating_add(chunk.len()) > MAX_DIRECTORY_ENTRIES {
                        return Err(ClientError::UnexpectedResponse);
                    }
                    entries.extend(chunk);
                }
                Response::DirectoryViewEnd => break,
                response => return Err(response_error(response)),
            }
        }
        Ok(DirectoryView {
            revision,
            directory,
            parent,
            xattrs,
            entries,
        })
    }

    async fn request_with_data(
        &self,
        message: Request,
        data: &[u8],
    ) -> Result<(Response, Option<quickfs_transport_quic::RecvStream>)> {
        let request = Envelope::new(message);
        let (mut send, mut recv) = self.transport.stream().await?;
        self.transport.send_frame(&mut send, &request).await?;
        self.transport.send_all(&mut send, data).await?;
        send.finish().map_err(TransportError::Closed)?;
        let response: Envelope<Response> = self.transport.receive_frame(&mut recv).await?;
        if version_major(response.version) != PROTOCOL_MAJOR
            || response.request_id != request.request_id
        {
            return Err(ClientError::UnexpectedResponse);
        }
        Ok((response.message, Some(recv)))
    }
}

pub async fn verify_pairing(
    transport: &PairingClient,
    pairing_id: Uuid,
    pairing_code: &str,
) -> Result<[u8; 32]> {
    let secret = parse_pairing_code(pairing_code)
        .map_err(|error| ClientError::Server(ErrorCode::Unauthenticated, error.to_string()))?;
    let presented = transport.peer_certificate_fingerprint()?;
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|_| ClientError::UnexpectedResponse)?;
    let client_proof = secret
        .client_proof(pairing_id, &presented, &nonce)
        .map_err(|_| ClientError::UnexpectedResponse)?;
    match transport.pair(pairing_id, nonce, client_proof).await? {
        Response::PairingProof {
            certificate_fingerprint,
            proof,
        } => {
            if presented != certificate_fingerprint {
                return Err(ClientError::UnexpectedResponse);
            }
            if !secret.verify_server_proof(
                pairing_id,
                &certificate_fingerprint,
                &nonce,
                proof.as_bytes(),
            ) {
                return Err(ClientError::Server(
                    ErrorCode::Unauthenticated,
                    "pairing code did not authenticate this server".into(),
                ));
            }
            Ok(certificate_fingerprint)
        }
        other => Err(response_error(other)),
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
    async fn capabilities(&self) -> Result<FilesystemCapabilities> {
        match self.request(Request::GetCapabilities).await?.0 {
            Response::Capabilities(capabilities) => Ok(capabilities),
            response => Err(response_error(response)),
        }
    }
    async fn stat_filesystem(&self) -> Result<FilesystemStats> {
        match self.request(Request::StatFilesystem).await?.0 {
            Response::FilesystemStats(statistics) => Ok(statistics),
            response => Err(response_error(response)),
        }
    }
    async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
        match self.request(Request::GetMetadata { node }).await?.0 {
            Response::Metadata(v) => Ok(v),
            r => Err(response_error(r)),
        }
    }
    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        Ok(self.list_directory_snapshot(node).await?.entries)
    }
    async fn list_directory_snapshot(&self, node: NodeId) -> Result<DirectorySnapshot> {
        match self.request(Request::ListDirectory { node }).await?.0 {
            Response::DirectoryListing { revision, entries } => {
                Ok(DirectorySnapshot { revision, entries })
            }
            response => Err(response_error(response)),
        }
    }
    async fn list_directory_view(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
    ) -> Result<DirectoryView> {
        self.list_directory_view_streamed(node, options).await
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
        match self.request(Request::OpenFile { node, options }).await?.0 {
            Response::FileOpened {
                handle,
                revision,
                size,
            } => Ok(OpenedFile {
                handle,
                revision,
                size,
            }),
            response => Err(response_error(response)),
        }
    }
    async fn create_file(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<CreatedFile> {
        match self
            .request(Request::CreateFile {
                parent,
                name,
                mode,
                options,
            })
            .await?
            .0
        {
            Response::FileCreated {
                metadata,
                handle,
                revision,
                size,
            } => Ok(CreatedFile {
                metadata,
                opened: OpenedFile {
                    handle,
                    revision,
                    size,
                },
            }),
            response => Err(response_error(response)),
        }
    }
    async fn create_directory(&self, parent: NodeId, name: Name, mode: u32) -> Result<Metadata> {
        match self
            .request(Request::CreateDirectory { parent, name, mode })
            .await?
            .0
        {
            Response::NodeCreated(metadata) => Ok(metadata),
            response => Err(response_error(response)),
        }
    }
    async fn create_symlink(
        &self,
        parent: NodeId,
        name: Name,
        target: Vec<u8>,
    ) -> Result<Metadata> {
        match self
            .request(Request::CreateSymlink {
                parent,
                name,
                target,
            })
            .await?
            .0
        {
            Response::NodeCreated(metadata) => Ok(metadata),
            response => Err(response_error(response)),
        }
    }
    async fn remove_node(&self, parent: NodeId, name: Name, directory: bool) -> Result<()> {
        match self
            .request(Request::RemoveNode {
                parent,
                name,
                directory,
            })
            .await?
            .0
        {
            Response::NodeRemoved => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn rename_node(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    ) -> Result<()> {
        match self
            .request(Request::RenameNode {
                parent,
                name,
                new_parent,
                new_name,
                mode,
            })
            .await?
            .0
        {
            Response::NodeRenamed => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
        match self.request(Request::ReadLink { node }).await?.0 {
            Response::LinkTarget(target) => Ok(target),
            response => Err(response_error(response)),
        }
    }

    async fn create_hard_link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: Name,
    ) -> Result<Metadata> {
        match self
            .request(Request::CreateHardLink {
                node,
                new_parent,
                new_name,
            })
            .await?
            .0
        {
            Response::HardLinkCreated(metadata) => Ok(metadata),
            response => Err(response_error(response)),
        }
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
        match self
            .request(Request::CreateSpecialNode {
                parent,
                name,
                kind,
                mode,
                device_major,
                device_minor,
            })
            .await?
            .0
        {
            Response::NodeCreated(metadata) => Ok(metadata),
            response => Err(response_error(response)),
        }
    }
    async fn set_attributes(
        &self,
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> Result<Metadata> {
        match self
            .request(Request::SetAttributes {
                node,
                handle,
                changes,
            })
            .await?
            .0
        {
            Response::AttributesChanged(metadata) => Ok(metadata),
            response => Err(response_error(response)),
        }
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
        if length > MAX_CLIENT_READ_SIZE {
            return Err(ClientError::ReadTooLarge(MAX_CLIENT_READ_SIZE));
        }
        let (response, recv) = self
            .request(Request::ReadRange {
                handle,
                offset,
                length,
            })
            .await?;
        match response {
            Response::ReadData {
                revision,
                length: response_length,
            } => {
                if response_length > length || response_length > MAX_CLIENT_READ_SIZE {
                    return Err(ClientError::UnexpectedResponse);
                }
                let mut recv = recv.ok_or(ClientError::UnexpectedResponse)?;
                let size: usize = response_length
                    .try_into()
                    .map_err(|_| ClientError::UnexpectedResponse)?;
                let mut data = vec![0; size];
                self.transport.receive_exact(&mut recv, &mut data).await?;
                Ok(RangeRead { revision, data })
            }
            response => Err(response_error(response)),
        }
    }
    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<WriteResult> {
        let length = u64::try_from(data.len()).map_err(|_| ClientError::UnexpectedResponse)?;
        if length > MAX_CLIENT_WRITE_SIZE || offset.checked_add(length).is_none() {
            return Err(ClientError::WriteTooLarge(MAX_CLIENT_WRITE_SIZE));
        }
        match self
            .request_with_data(
                Request::WriteRange {
                    handle,
                    offset,
                    length,
                },
                data,
            )
            .await?
            .0
        {
            Response::WriteComplete {
                written,
                revision,
                size,
            } if written <= length => Ok(WriteResult {
                written,
                revision,
                size,
            }),
            Response::WriteComplete { .. } => Err(ClientError::UnexpectedResponse),
            response => Err(response_error(response)),
        }
    }
    async fn flush_file(&self, handle: FileHandle, lock_owner: Option<u64>) -> Result<()> {
        match self
            .request(Request::FlushFile { handle, lock_owner })
            .await?
            .0
        {
            Response::FileFlushed => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn sync_file(&self, handle: FileHandle, data_only: bool) -> Result<()> {
        match self
            .request(Request::SyncFile { handle, data_only })
            .await?
            .0
        {
            Response::FileSynced => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn sync_directory(&self, node: NodeId) -> Result<()> {
        match self.request(Request::SyncDirectory { node }).await?.0 {
            Response::DirectorySynced => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn allocate_file(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        match self
            .request(Request::AllocateFile {
                handle,
                offset,
                length,
            })
            .await?
            .0
        {
            Response::FileAllocated { revision, size } => Ok(WriteResult {
                written: 0,
                revision,
                size,
            }),
            response => Err(response_error(response)),
        }
    }

    async fn get_xattr(
        &self,
        node: NodeId,
        name: Name,
        offset: u64,
        length: u64,
    ) -> Result<XattrRead> {
        if length > MAX_CLIENT_READ_SIZE {
            return Err(ClientError::ReadTooLarge(length));
        }
        let (response, recv) = self
            .request(Request::GetXattr {
                node,
                name,
                offset,
                length,
            })
            .await?;
        match response {
            Response::XattrData { length, total_size } => {
                let size =
                    usize::try_from(length).map_err(|_| ClientError::ReadTooLarge(length))?;
                let mut data = vec![0_u8; size];
                let mut recv = recv.ok_or(ClientError::UnexpectedResponse)?;
                self.transport.receive_exact(&mut recv, &mut data).await?;
                Ok(XattrRead { total_size, data })
            }
            response => Err(response_error(response)),
        }
    }

    async fn set_xattr(
        &self,
        node: NodeId,
        name: Name,
        value: &[u8],
        mode: XattrSetMode,
        position: u32,
    ) -> Result<()> {
        let length =
            u64::try_from(value.len()).map_err(|_| ClientError::WriteTooLarge(u64::MAX))?;
        if length > MAX_CLIENT_WRITE_SIZE {
            return Err(ClientError::WriteTooLarge(length));
        }
        match self
            .request_with_data(
                Request::SetXattr {
                    node,
                    name,
                    mode,
                    position,
                    length,
                },
                value,
            )
            .await?
            .0
        {
            Response::XattrSet => Ok(()),
            response => Err(response_error(response)),
        }
    }

    async fn list_xattrs(&self, node: NodeId) -> Result<Vec<Name>> {
        match self.request(Request::ListXattrs { node }).await?.0 {
            Response::XattrNames(names) => Ok(names),
            response => Err(response_error(response)),
        }
    }

    async fn remove_xattr(&self, node: NodeId, name: Name) -> Result<()> {
        match self.request(Request::RemoveXattr { node, name }).await?.0 {
            Response::XattrRemoved => Ok(()),
            response => Err(response_error(response)),
        }
    }

    async fn copy_file_range(
        &self,
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        match self
            .request(Request::CopyFileRange {
                input,
                input_offset,
                output,
                output_offset,
                length,
            })
            .await?
            .0
        {
            Response::RangeCopied {
                copied,
                revision,
                size,
            } => Ok(WriteResult {
                written: copied,
                revision,
                size,
            }),
            response => Err(response_error(response)),
        }
    }

    async fn seek_file(&self, handle: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
        match self
            .request(Request::SeekFile {
                handle,
                offset,
                whence,
            })
            .await?
            .0
        {
            Response::FileSeeked { offset } => Ok(offset),
            response => Err(response_error(response)),
        }
    }

    async fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64> {
        match self
            .request(Request::SafeIoctl { handle, operation })
            .await?
            .0
        {
            Response::IoctlResult { value } => Ok(value),
            response => Err(response_error(response)),
        }
    }

    async fn map_block(&self, node: NodeId, block_size: u32, block: u64) -> Result<u64> {
        match self
            .request(Request::MapBlock {
                node,
                block_size,
                block,
            })
            .await?
            .0
        {
            Response::BlockMapped { block } => Ok(block),
            response => Err(response_error(response)),
        }
    }

    async fn exchange_data(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        options: u64,
    ) -> Result<()> {
        match self
            .request(Request::ExchangeData {
                parent,
                name,
                new_parent,
                new_name,
                options,
            })
            .await?
            .0
        {
            Response::DataExchanged => Ok(()),
            response => Err(response_error(response)),
        }
    }

    async fn set_volume_name(&self, name: Name) -> Result<()> {
        match self.request(Request::SetVolumeName { name }).await?.0 {
            Response::VolumeNameSet => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn forget_nodes(&self, nodes: Vec<NodeId>) -> Result<()> {
        match self.request(Request::ForgetNodes { nodes }).await?.0 {
            Response::NodesForgotten => Ok(()),
            response => Err(response_error(response)),
        }
    }
    async fn get_lock(&self, handle: FileHandle, lock: FileLock) -> Result<Option<FileLock>> {
        match self.request(Request::GetLock { handle, lock }).await?.0 {
            Response::LockStatus { conflict } => Ok(conflict),
            response => Err(response_error(response)),
        }
    }
    async fn set_lock(&self, handle: FileHandle, lock: FileLock, wait: bool) -> Result<()> {
        match self
            .request(Request::SetLock { handle, lock, wait })
            .await?
            .0
        {
            Response::LockUpdated => Ok(()),
            response => Err(response_error(response)),
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
            .find(|e| e.name.as_bytes() == part.as_bytes())
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
    async fn capabilities(&self) -> Result<FilesystemCapabilities> {
        self.wait().await;
        self.inner.capabilities().await
    }
    async fn stat_filesystem(&self) -> Result<FilesystemStats> {
        self.wait().await;
        self.inner.stat_filesystem().await
    }
    async fn get_metadata(&self, n: NodeId) -> Result<Metadata> {
        self.wait().await;
        self.inner.get_metadata(n).await
    }
    async fn list_directory(&self, n: NodeId) -> Result<Vec<DirectoryEntry>> {
        self.wait().await;
        self.inner.list_directory(n).await
    }
    async fn list_directory_snapshot(&self, n: NodeId) -> Result<DirectorySnapshot> {
        self.wait().await;
        self.inner.list_directory_snapshot(n).await
    }
    async fn open_file(&self, n: NodeId) -> Result<(FileHandle, u64, u64)> {
        self.wait().await;
        self.inner.open_file(n).await
    }
    async fn open_file_with_options(
        &self,
        n: NodeId,
        options: FileOpenOptions,
    ) -> Result<OpenedFile> {
        self.wait().await;
        self.inner.open_file_with_options(n, options).await
    }
    async fn create_file(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<CreatedFile> {
        self.wait().await;
        self.inner.create_file(parent, name, mode, options).await
    }
    async fn create_directory(&self, parent: NodeId, name: Name, mode: u32) -> Result<Metadata> {
        self.wait().await;
        self.inner.create_directory(parent, name, mode).await
    }
    async fn create_symlink(
        &self,
        parent: NodeId,
        name: Name,
        target: Vec<u8>,
    ) -> Result<Metadata> {
        self.wait().await;
        self.inner.create_symlink(parent, name, target).await
    }
    async fn create_hard_link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: Name,
    ) -> Result<Metadata> {
        self.wait().await;
        self.inner
            .create_hard_link(node, new_parent, new_name)
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
        self.wait().await;
        self.inner
            .create_special_node(parent, name, kind, mode, device_major, device_minor)
            .await
    }
    async fn remove_node(&self, parent: NodeId, name: Name, directory: bool) -> Result<()> {
        self.wait().await;
        self.inner.remove_node(parent, name, directory).await
    }
    async fn rename_node(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    ) -> Result<()> {
        self.wait().await;
        self.inner
            .rename_node(parent, name, new_parent, new_name, mode)
            .await
    }
    async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
        self.wait().await;
        self.inner.read_link(node).await
    }
    async fn set_attributes(
        &self,
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> Result<Metadata> {
        self.wait().await;
        self.inner.set_attributes(node, handle, changes).await
    }
    async fn read_range(&self, h: FileHandle, o: u64, l: u64) -> Result<Vec<u8>> {
        self.wait().await;
        self.inner.read_range(h, o, l).await
    }
    async fn read_range_versioned(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<RangeRead> {
        self.wait().await;
        self.inner
            .read_range_versioned(handle, offset, length)
            .await
    }
    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<WriteResult> {
        self.wait().await;
        self.inner.write_range(handle, offset, data).await
    }
    async fn flush_file(&self, handle: FileHandle, lock_owner: Option<u64>) -> Result<()> {
        self.wait().await;
        self.inner.flush_file(handle, lock_owner).await
    }
    async fn sync_file(&self, handle: FileHandle, data_only: bool) -> Result<()> {
        self.wait().await;
        self.inner.sync_file(handle, data_only).await
    }
    async fn sync_directory(&self, node: NodeId) -> Result<()> {
        self.wait().await;
        self.inner.sync_directory(node).await
    }
    async fn allocate_file(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        self.wait().await;
        self.inner.allocate_file(handle, offset, length).await
    }
    async fn get_xattr(
        &self,
        node: NodeId,
        name: Name,
        offset: u64,
        length: u64,
    ) -> Result<XattrRead> {
        self.wait().await;
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
        self.wait().await;
        self.inner
            .set_xattr(node, name, value, mode, position)
            .await
    }
    async fn list_xattrs(&self, node: NodeId) -> Result<Vec<Name>> {
        self.wait().await;
        self.inner.list_xattrs(node).await
    }
    async fn remove_xattr(&self, node: NodeId, name: Name) -> Result<()> {
        self.wait().await;
        self.inner.remove_xattr(node, name).await
    }
    async fn copy_file_range(
        &self,
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        self.wait().await;
        self.inner
            .copy_file_range(input, input_offset, output, output_offset, length)
            .await
    }
    async fn seek_file(&self, handle: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
        self.wait().await;
        self.inner.seek_file(handle, offset, whence).await
    }
    async fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64> {
        self.wait().await;
        self.inner.safe_ioctl(handle, operation).await
    }
    async fn map_block(&self, node: NodeId, block_size: u32, block: u64) -> Result<u64> {
        self.wait().await;
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
        self.wait().await;
        self.inner
            .exchange_data(parent, name, new_parent, new_name, options)
            .await
    }
    async fn set_volume_name(&self, name: Name) -> Result<()> {
        self.wait().await;
        self.inner.set_volume_name(name).await
    }
    async fn forget_nodes(&self, nodes: Vec<NodeId>) -> Result<()> {
        self.wait().await;
        self.inner.forget_nodes(nodes).await
    }
    async fn get_lock(&self, handle: FileHandle, lock: FileLock) -> Result<Option<FileLock>> {
        self.wait().await;
        self.inner.get_lock(handle, lock).await
    }
    async fn set_lock(&self, handle: FileHandle, lock: FileLock, wait: bool) -> Result<()> {
        self.wait().await;
        self.inner.set_lock(handle, lock, wait).await
    }
    async fn close_file(&self, h: FileHandle) -> Result<()> {
        self.wait().await;
        self.inner.close_file(h).await
    }
}
