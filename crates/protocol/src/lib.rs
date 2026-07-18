// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const PROTOCOL_VERSION: u16 = 5;
pub const ALPN_PROTOCOL: &[u8] = b"quickfs/5";
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const ROOT_NODE: NodeId = NodeId(Uuid::from_u128(0));

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct RequestId(pub Uuid);
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct FileHandle(pub Uuid);
pub type FileRevision = u64;
pub type DirectoryRevision = u64;

/// A lossless Unix name. Unlike `String`, this can represent every filename
/// and extended-attribute byte sequence; a platform adapter may still reject
/// names that its host operating system cannot express.
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Name(Vec<u8>);

impl Name {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<u8>> for Name {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl From<String> for Name {
    fn from(value: String) -> Self {
        Self::new(value.into_bytes())
    }
}

impl From<&str> for Name {
    fn from(value: &str) -> Self {
        Self::new(value.as_bytes().to_vec())
    }
}

impl std::fmt::Debug for Name {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Name({:?})", String::from_utf8_lossy(&self.0))
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&String::from_utf8_lossy(&self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileAccess {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

impl FileAccess {
    pub fn can_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    pub fn can_write(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileOpenOptions {
    pub access: FileAccess,
    pub truncate: bool,
    pub append: bool,
}

impl FileOpenOptions {
    pub const READ_ONLY: Self = Self {
        access: FileAccess::ReadOnly,
        truncate: false,
        append: false,
    };
}

impl Default for FileOpenOptions {
    fn default() -> Self {
        Self::READ_ONLY
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RenameMode {
    Replace,
    NoReplace,
    Exchange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LockKind {
    Read,
    Write,
    Unlock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileLock {
    pub owner: u64,
    pub start: u64,
    /// Inclusive end offset. `u64::MAX` represents a lock through EOF.
    pub end: u64,
    pub kind: LockKind,
    pub pid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttributeChanges {
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub accessed_unix_ms: Option<u64>,
    pub modified_unix_ms: Option<u64>,
    pub backup_unix_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SpecialNodeKind {
    NamedPipe,
    CharacterDevice,
    BlockDevice,
    Socket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum XattrSetMode {
    Upsert,
    Create,
    Replace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SeekWhence {
    Data,
    Hole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SafeIoctl {
    /// The portable, read-only FIONREAD query.
    BytesAvailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FilesystemCapabilities {
    pub server_epoch: Uuid,
    pub writable: bool,
    pub supports_locks: bool,
    pub supports_atomic_rename: bool,
    pub supports_directory_sync: bool,
    pub supports_preallocation: bool,
    pub supports_symlinks: bool,
    pub supports_xattrs: bool,
    pub supports_hard_links: bool,
    pub supports_special_nodes: bool,
    pub supports_copy_file_range: bool,
    pub supports_seek_data_hole: bool,
    pub supports_safe_ioctl: bool,
    pub supports_poll: bool,
    pub supports_bmap: bool,
    pub supports_exchange_data: bool,
    pub supports_volume_rename: bool,
    pub supports_backup_time: bool,
    pub supports_readdirplus: bool,
    pub persistent_node_ids: bool,
    pub restart_lock_replay: bool,
    pub volume_name: String,
    pub max_read_size: u64,
    pub max_write_size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FilesystemStats {
    pub blocks: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,
    pub files: u64,
    pub files_free: u64,
    pub block_size: u32,
    pub name_length: u32,
    pub fragment_size: u32,
}

/// A wire-compatible UTF-8 string whose debug representation is redacted and
/// whose allocation is cleared when it is dropped.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

/// A fixed-size authentication proof whose debug representation is redacted
/// and whose bytes are cleared when it is dropped.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretProof([u8; 32]);

impl SecretProof {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for SecretProof {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for SecretProof {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretProof([REDACTED])")
    }
}

impl RequestId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u16,
    pub request_id: RequestId,
    pub message: T,
}
impl<T> Envelope<T> {
    pub fn new(message: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: RequestId::new(),
            message,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Hello {
        client_name: String,
    },
    Pair {
        pairing_id: Uuid,
        client_nonce: [u8; 32],
        client_proof: SecretProof,
    },
    Authenticate {
        username: String,
        password: SecretString,
    },
    GetCapabilities,
    StatFilesystem,
    GetMetadata {
        node: NodeId,
    },
    ListDirectory {
        node: NodeId,
    },
    OpenFile {
        node: NodeId,
        options: FileOpenOptions,
    },
    CreateFile {
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    },
    CreateDirectory {
        parent: NodeId,
        name: Name,
        mode: u32,
    },
    CreateSymlink {
        parent: NodeId,
        name: Name,
        target: Vec<u8>,
    },
    RemoveNode {
        parent: NodeId,
        name: Name,
        directory: bool,
    },
    RenameNode {
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    },
    CreateHardLink {
        node: NodeId,
        new_parent: NodeId,
        new_name: Name,
    },
    CreateSpecialNode {
        parent: NodeId,
        name: Name,
        kind: SpecialNodeKind,
        mode: u32,
        device_major: u32,
        device_minor: u32,
    },
    ReadLink {
        node: NodeId,
    },
    SetAttributes {
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    },
    ReadRange {
        handle: FileHandle,
        offset: u64,
        length: u64,
    },
    WriteRange {
        handle: FileHandle,
        offset: u64,
        length: u64,
    },
    FlushFile {
        handle: FileHandle,
        lock_owner: Option<u64>,
    },
    SyncFile {
        handle: FileHandle,
        data_only: bool,
    },
    SyncDirectory {
        node: NodeId,
    },
    AllocateFile {
        handle: FileHandle,
        offset: u64,
        length: u64,
    },
    GetXattr {
        node: NodeId,
        name: Name,
        offset: u64,
        length: u64,
    },
    SetXattr {
        node: NodeId,
        name: Name,
        mode: XattrSetMode,
        position: u32,
        length: u64,
    },
    ListXattrs {
        node: NodeId,
    },
    RemoveXattr {
        node: NodeId,
        name: Name,
    },
    CopyFileRange {
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    },
    SeekFile {
        handle: FileHandle,
        offset: u64,
        whence: SeekWhence,
    },
    SafeIoctl {
        handle: FileHandle,
        operation: SafeIoctl,
    },
    MapBlock {
        node: NodeId,
        block_size: u32,
        block: u64,
    },
    ExchangeData {
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        options: u64,
    },
    SetVolumeName {
        name: Name,
    },
    /// Release nodes for which the kernel has dropped its final lookup
    /// reference. This is advisory and idempotent: stable node IDs can be
    /// rediscovered if a later request names one again.
    ForgetNodes {
        nodes: Vec<NodeId>,
    },
    GetLock {
        handle: FileHandle,
        lock: FileLock,
    },
    SetLock {
        handle: FileHandle,
        lock: FileLock,
        wait: bool,
    },
    CloseFile {
        handle: FileHandle,
    },
    Ping {
        nonce: u64,
    },
}

impl Request {
    pub fn clear_secrets(&mut self) {
        match self {
            Self::Pair { client_proof, .. } => client_proof.zeroize(),
            Self::Authenticate { password, .. } => password.zeroize(),
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    HelloAck {
        version: u16,
    },
    AuthenticateAck,
    PairingProof {
        certificate_fingerprint: [u8; 32],
        proof: SecretProof,
    },
    Capabilities(FilesystemCapabilities),
    FilesystemStats(FilesystemStats),
    Metadata(Metadata),
    DirectoryListing {
        revision: DirectoryRevision,
        entries: Vec<DirectoryEntry>,
    },
    FileOpened {
        handle: FileHandle,
        revision: FileRevision,
        size: u64,
    },
    FileCreated {
        metadata: Metadata,
        handle: FileHandle,
        revision: FileRevision,
        size: u64,
    },
    NodeCreated(Metadata),
    HardLinkCreated(Metadata),
    NodeRemoved,
    NodeRenamed,
    LinkTarget(Vec<u8>),
    AttributesChanged(Metadata),
    ReadData {
        revision: FileRevision,
        length: u64,
    },
    WriteComplete {
        written: u64,
        revision: FileRevision,
        size: u64,
    },
    FileFlushed,
    FileSynced,
    DirectorySynced,
    FileAllocated {
        revision: FileRevision,
        size: u64,
    },
    XattrData {
        length: u64,
        total_size: u64,
    },
    XattrSet,
    XattrNames(Vec<Name>),
    XattrRemoved,
    RangeCopied {
        copied: u64,
        revision: FileRevision,
        size: u64,
    },
    FileSeeked {
        offset: u64,
    },
    IoctlResult {
        value: u64,
    },
    BlockMapped {
        block: u64,
    },
    DataExchanged,
    VolumeNameSet,
    NodesForgotten,
    LockStatus {
        conflict: Option<FileLock>,
    },
    LockUpdated,
    FileClosed,
    Pong {
        nonce: u64,
    },
    Error(ProtocolError),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub node: NodeId,
    pub kind: NodeKind,
    pub size: u64,
    /// Permission and special bits without the file-type bits.
    #[serde(default)]
    pub mode: u32,
    /// Number of allocated 512-byte blocks, when available.
    #[serde(default)]
    pub allocated_blocks: u64,
    pub revision: u64,
    #[serde(default)]
    pub accessed_unix_ms: u64,
    pub modified_unix_ms: u64,
    #[serde(default)]
    pub created_unix_ms: Option<u64>,
    #[serde(default)]
    pub backup_unix_ms: Option<u64>,
    #[serde(default = "default_link_count")]
    pub link_count: u32,
    #[serde(default)]
    pub device_major: u32,
    #[serde(default)]
    pub device_minor: u32,
}

const fn default_link_count() -> u32 {
    1
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub node: NodeId,
    pub name: Name,
    pub kind: NodeKind,
    pub metadata: Metadata,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
    NamedPipe,
    CharacterDevice,
    BlockDevice,
    Socket,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ErrorCode {
    Unauthenticated,
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    NotEmpty,
    NoAttribute,
    NoData,
    NotTty,
    ReadOnly,
    Conflict,
    WouldBlock,
    NoSpace,
    Busy,
    NotSupported,
    Offline,
    InvalidNode,
    InvalidHandle,
    InvalidRequest,
    UnsupportedVersion,
    TooLarge,
    Timeout,
    Internal,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let out = postcard::to_allocvec(value)?;
    if out.len() > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(out.len()));
    }
    Ok(out)
}
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(bytes.len()));
    }
    Ok(postcard::from_bytes(bytes)?)
}
pub fn decode_request(bytes: &[u8]) -> Result<Envelope<Request>, CodecError> {
    let msg: Envelope<Request> = decode(bytes)?;
    if msg.version != PROTOCOL_VERSION {
        return Err(CodecError::UnsupportedVersion(msg.version));
    }
    Ok(msg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn round_trip() {
        let m = Envelope::new(Request::Ping { nonce: 42 });
        let b = encode(&m).unwrap();
        assert_eq!(decode_request(&b).unwrap(), m);
    }
    #[test]
    fn rejects_version() {
        let mut m = Envelope::new(Request::Ping { nonce: 1 });
        m.version = 99;
        assert!(matches!(
            decode_request(&encode(&m).unwrap()),
            Err(CodecError::UnsupportedVersion(99))
        ));
    }
    #[test]
    fn rejects_bad_data() {
        assert!(decode_request(&[255, 1]).is_err());
    }
    #[test]
    fn rejects_oversize() {
        assert!(matches!(
            decode::<Request>(&vec![0; MAX_FRAME_SIZE + 1]),
            Err(CodecError::TooLarge(_))
        ));
    }

    #[test]
    fn authentication_debug_output_redacts_password() {
        let request = Request::Authenticate {
            username: "alice".into(),
            password: "correct horse battery staple".to_string().into(),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("correct horse"));
    }

    #[test]
    fn pairing_debug_output_redacts_proofs() {
        let request = Request::Pair {
            pairing_id: Uuid::nil(),
            client_nonce: [1; 32],
            client_proof: [2; 32].into(),
        };
        let output = format!("{request:?}");
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("2, 2, 2"));
    }
}
