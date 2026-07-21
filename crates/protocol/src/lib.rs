// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Protocol **major** version. A major bump is a genuine flag day: the wire
/// contract is incompatible, the ALPN identifier changes, and peers refuse to
/// talk across majors. Everything additive rides a **minor** bump instead
/// (see [`PROTOCOL_MINOR`]).
pub const PROTOCOL_MAJOR: u16 = 6;

/// Protocol **minor** version. Minors are backward-negotiated within a major:
/// two peers use the intersection of what they both speak (see
/// [`peer_supports_frame_compression`]). A newer minor may add optional,
/// individually gated capabilities, so a newer client and an older same-major
/// daemon still interoperate — the newer side simply does not emit a feature
/// the older side did not advertise. Bumping the minor is **not** a flag day.
///
/// - `6.0` baseline enriched-directory-view protocol.
/// - `6.1` streamed directory-view pagination (`DirectoryViewStart`/`Chunk`/`End`).
/// - `6.3` per-frame compression ([`MINOR_FRAME_COMPRESSION`]).
pub const PROTOCOL_MINOR: u16 = 3;

/// The wire version word: major in the high byte, minor in the low byte. Carried
/// in every [`Envelope`] and echoed in `HelloAck` so each side learns the other's
/// exact version. Kept a single `u16` so the frame layout is unchanged.
pub const PROTOCOL_VERSION: u16 = make_version(PROTOCOL_MAJOR, PROTOCOL_MINOR);

/// ALPN identifier. Keyed to the **major only**, so every `6.x` peer negotiates
/// the same QUIC connection and minor differences are resolved in the
/// application handshake rather than by refusing to connect.
pub const ALPN_PROTOCOL: &[u8] = b"quickfs/6";

/// Compose a wire version word from a major/minor pair.
pub const fn make_version(major: u16, minor: u16) -> u16 {
    (major << 8) | (minor & 0x00ff)
}

/// Major component of a wire version word.
pub const fn version_major(version: u16) -> u16 {
    version >> 8
}

/// Minor component of a wire version word.
pub const fn version_minor(version: u16) -> u16 {
    version & 0x00ff
}

/// Minor version in which per-frame compression was introduced. A peer only
/// *emits* a compressed frame to a partner whose advertised version is at least
/// this, so an older same-major peer that cannot decompress never receives one.
/// Decoding a compressed frame is always supported regardless of the partner's
/// version — the compressed flag is self-describing — so this gates the sender,
/// not the receiver.
pub const MINOR_FRAME_COMPRESSION: u16 = 3;

/// Whether it is safe to send `peer` a compressed frame: same major and a minor
/// at least [`MINOR_FRAME_COMPRESSION`]. Both sides use this — the server against
/// each request's advertised version, the client against the version it learned
/// from `HelloAck`.
pub fn peer_supports_frame_compression(peer_version: u16) -> bool {
    version_major(peer_version) == PROTOCOL_MAJOR
        && version_minor(peer_version) >= MINOR_FRAME_COMPRESSION
}

pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// High bit of the four-byte frame length prefix, set when the framed body is
/// zstd-compressed. The remaining 31 bits carry the on-wire body length, which
/// is always at most [`MAX_FRAME_SIZE`] (2^20), so a real length can never reach
/// bit 31 and the flag never collides with one. A frame whose body does not set
/// this bit is a raw postcard encoding, exactly as in protocol versions before
/// compression existed.
pub const FRAME_COMPRESSED_FLAG: u32 = 1 << 31;

/// Payloads smaller than this are sent uncompressed. On already-tiny control
/// messages (pings, single-node metadata, acks) the codec byte and CPU cost
/// outweigh any saving, and such frames often do not shrink at all. The frames
/// the roadmap targets — enriched directory views and their streamed chunks,
/// with their repeated names and xattr keys — are far above this threshold.
pub const FRAME_COMPRESSION_THRESHOLD: usize = 1024;

/// zstd level used for frame compression. Level 3 (the zstd default) is the
/// throughput/ratio sweet spot for the highly repetitive directory-view and
/// metadata payloads this shrinks; higher levels cost noticeably more CPU for
/// little extra ratio on this kind of data.
pub const FRAME_COMPRESSION_LEVEL: i32 = 3;
pub const MAX_DIRECTORY_INLINE_XATTR_SIZE: u32 = 4 * 1024;
pub const MAX_DIRECTORY_INLINE_XATTR_TOTAL_SIZE: u32 = 256 * 1024;

/// Upper bound on the number of children a single directory may project in one
/// `ListDirectoryView`/`ListDirectory` response. A directory view is streamed
/// across as many `DirectoryViewChunk` frames as needed (see
/// [`DIRECTORY_VIEW_CHUNK_BUDGET`]), so this is no longer a `MAX_FRAME_SIZE`
/// limit — it is a memory/DoS backstop that bounds how large the fully
/// materialized entry set can grow on either peer. In practice the server's
/// per-connection node-registry ceiling is reached first; this constant only
/// rejects pathological directories before they can allocate without bound.
pub const MAX_DIRECTORY_ENTRIES: usize = 1024 * 1024;

/// Per-frame byte budget for the entries carried by one `DirectoryViewChunk`.
/// Set below [`MAX_FRAME_SIZE`] to leave headroom for the response envelope,
/// the enum discriminant, and the entry-vector length prefix so that a chunk
/// filled up to this budget always encodes within `MAX_FRAME_SIZE`.
pub const DIRECTORY_VIEW_CHUNK_BUDGET: usize = MAX_FRAME_SIZE - 16 * 1024;
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
    /// Return the complete information needed to publish one native directory
    /// view without a metadata/xattr request per child.
    ListDirectoryView {
        node: NodeId,
        options: DirectoryViewOptions,
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
    DirectoryView(DirectoryView),
    /// Header of a directory view too large to fit one `DirectoryView` frame.
    /// Followed on the same stream by one or more `DirectoryViewChunk` frames
    /// and terminated by `DirectoryViewEnd`. Carries every field of
    /// `DirectoryView` except the entries, which arrive in the chunks.
    DirectoryViewStart {
        revision: DirectoryRevision,
        directory: Metadata,
        parent: Metadata,
        xattrs: Option<XattrSnapshot>,
        /// Total number of entries that the following chunks will carry, so the
        /// receiver can pre-size its buffer. Advisory: the receiver still
        /// enforces `MAX_DIRECTORY_ENTRIES` against the entries it actually
        /// receives rather than trusting this count.
        entry_count: u64,
    },
    /// One ordered batch of entries belonging to a streamed directory view.
    DirectoryViewChunk {
        entries: Vec<DirectoryViewEntry>,
    },
    /// Terminates a streamed directory view begun with `DirectoryViewStart`.
    DirectoryViewEnd,
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

/// Controls the bounded optional data attached to an enriched directory
/// response. Xattr names are either complete or omitted for a node; values
/// larger than `inline_xattr_size` remain available through `GetXattr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryViewOptions {
    pub include_xattrs: bool,
    pub inline_xattr_size: u32,
    pub inline_xattr_total_size: u32,
}

impl DirectoryViewOptions {
    pub const NATIVE: Self = Self {
        include_xattrs: true,
        inline_xattr_size: MAX_DIRECTORY_INLINE_XATTR_SIZE,
        inline_xattr_total_size: MAX_DIRECTORY_INLINE_XATTR_TOTAL_SIZE,
    };

    pub const METADATA_ONLY: Self = Self {
        include_xattrs: false,
        inline_xattr_size: 0,
        inline_xattr_total_size: 0,
    };
}

impl Default for DirectoryViewOptions {
    fn default() -> Self {
        Self::METADATA_ONLY
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InlineXattr {
    pub name: Name,
    pub value: Vec<u8>,
}

/// A complete xattr name snapshot plus the small values the server could fit
/// within the request's explicit budgets.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct XattrSnapshot {
    pub names: Vec<Name>,
    pub inline_values: Vec<InlineXattr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryViewEntry {
    pub entry: DirectoryEntry,
    /// `None` means xattrs were not requested or are unsupported for this node.
    /// `Some` always contains the complete name set.
    pub xattrs: Option<XattrSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryView {
    pub revision: DirectoryRevision,
    pub directory: Metadata,
    pub parent: Metadata,
    pub xattrs: Option<XattrSnapshot>,
    pub entries: Vec<DirectoryViewEntry>,
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
    #[error("frame decompression failed")]
    Decompress,
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let out = postcard::to_allocvec(value)?;
    if out.len() > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(out.len()));
    }
    Ok(out)
}
/// Length in bytes of the postcard encoding of `value`, without imposing the
/// [`MAX_FRAME_SIZE`] cap that [`encode`] enforces. Used by the directory-view
/// chunker to measure individual entries while packing frames.
pub fn encoded_len<T: Serialize>(value: &T) -> Result<usize, CodecError> {
    Ok(postcard::to_allocvec(value)?.len())
}
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(bytes.len()));
    }
    Ok(postcard::from_bytes(bytes)?)
}

/// Encode `value` and wrap it as an on-wire frame: return the four-byte length
/// prefix word and the body bytes to write after it. Compression is attempted
/// only when `compress` is set — the caller passes the negotiated decision (does
/// the partner advertise [`MINOR_FRAME_COMPRESSION`]?) so a peer that cannot
/// decompress never receives a compressed frame. Even when `compress` is set the
/// body is zstd-compressed only if that actually shrinks a payload at or above
/// [`FRAME_COMPRESSION_THRESHOLD`]; otherwise the raw postcard encoding is sent,
/// so an incompressible or tiny frame is never larger. When compressed, the
/// prefix has [`FRAME_COMPRESSED_FLAG`] set and its low 31 bits hold the
/// compressed body length; when not, the prefix is simply the postcard length.
/// The postcard payload is still bounded by [`MAX_FRAME_SIZE`] via [`encode`], so
/// packing decisions (directory-view chunking) stay measured against the
/// uncompressed size — compression happens *after* the fit.
pub fn encode_frame<T: Serialize>(value: &T, compress: bool) -> Result<(u32, Vec<u8>), CodecError> {
    let payload = encode(value)?;
    if compress {
        Ok(frame_body(payload))
    } else {
        // `payload.len()` is bounded by `MAX_FRAME_SIZE` (2^20), so it fits in the
        // low 31 bits and never sets `FRAME_COMPRESSED_FLAG`.
        Ok((payload.len() as u32, payload))
    }
}

fn frame_body(mut payload: Vec<u8>) -> (u32, Vec<u8>) {
    if payload.len() >= FRAME_COMPRESSION_THRESHOLD
        && let Ok(compressed) = zstd::bulk::compress(&payload, FRAME_COMPRESSION_LEVEL)
        && compressed.len() < payload.len()
    {
        // The uncompressed payload may carry redactable material (e.g. a pairing
        // proof); clear it since it is not the buffer returned.
        payload.zeroize();
        return (compressed.len() as u32 | FRAME_COMPRESSED_FLAG, compressed);
    }
    // `payload.len()` is bounded by `MAX_FRAME_SIZE` (2^20), so it fits in the
    // low 31 bits and never sets `FRAME_COMPRESSED_FLAG`.
    (payload.len() as u32, payload)
}

/// Split a received four-byte frame length prefix into `(compressed, body_len)`.
/// Rejects a declared body length beyond [`MAX_FRAME_SIZE`] before any allocation.
pub fn parse_frame_header(prefix: u32) -> Result<(bool, usize), CodecError> {
    let compressed = prefix & FRAME_COMPRESSED_FLAG != 0;
    let length = (prefix & !FRAME_COMPRESSED_FLAG) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(CodecError::TooLarge(length));
    }
    Ok((compressed, length))
}

/// Decode a frame body produced by [`encode_frame`]. When `compressed`, the body
/// is zstd-decompressed with the output bounded to [`MAX_FRAME_SIZE`] so a
/// hostile peer cannot force an unbounded allocation from a tiny frame
/// (decompression-bomb guard), then postcard-decoded.
pub fn decode_frame<T: DeserializeOwned>(compressed: bool, body: &[u8]) -> Result<T, CodecError> {
    if compressed {
        let payload =
            zstd::bulk::decompress(body, MAX_FRAME_SIZE).map_err(|_| CodecError::Decompress)?;
        decode(&payload)
    } else {
        decode(body)
    }
}
pub fn decode_request(bytes: &[u8]) -> Result<Envelope<Request>, CodecError> {
    let msg: Envelope<Request> = decode(bytes)?;
    // Compatibility is per-major: a same-major peer on any minor is accepted and
    // its features are resolved by negotiation. Only a different major is a hard
    // rejection.
    if version_major(msg.version) != PROTOCOL_MAJOR {
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
    fn enriched_directory_request_round_trips_with_bounded_options() {
        let request = Envelope::new(Request::ListDirectoryView {
            node: ROOT_NODE,
            options: DirectoryViewOptions::NATIVE,
        });
        assert_eq!(decode_request(&encode(&request).unwrap()).unwrap(), request);
        assert_eq!(
            DirectoryViewOptions::NATIVE.inline_xattr_size,
            MAX_DIRECTORY_INLINE_XATTR_SIZE
        );
        assert_eq!(
            DirectoryViewOptions::NATIVE.inline_xattr_total_size,
            MAX_DIRECTORY_INLINE_XATTR_TOTAL_SIZE
        );
    }
    fn sample_metadata(seed: u128) -> Metadata {
        Metadata {
            node: NodeId(Uuid::from_u128(seed)),
            kind: NodeKind::File,
            size: 4096,
            mode: 0o644,
            allocated_blocks: 8,
            revision: 3,
            accessed_unix_ms: 1,
            modified_unix_ms: 2,
            created_unix_ms: Some(0),
            backup_unix_ms: None,
            link_count: 1,
            device_major: 0,
            device_minor: 0,
        }
    }

    fn sample_view_entry(index: usize) -> DirectoryViewEntry {
        let node = NodeId(Uuid::from_u128(index as u128));
        DirectoryViewEntry {
            entry: DirectoryEntry {
                node,
                name: format!("entry_{index:08}.dat").into(),
                kind: NodeKind::File,
                metadata: Metadata {
                    node,
                    ..sample_metadata(index as u128)
                },
            },
            xattrs: None,
        }
    }

    #[test]
    fn streamed_directory_view_frames_round_trip() {
        let start = Envelope::new(Response::DirectoryViewStart {
            revision: 42,
            directory: sample_metadata(1),
            parent: sample_metadata(2),
            xattrs: Some(XattrSnapshot {
                names: vec!["com.apple.FinderInfo".into()],
                inline_values: vec![InlineXattr {
                    name: "com.apple.FinderInfo".into(),
                    value: vec![7; 32],
                }],
            }),
            entry_count: 3,
        });
        assert_eq!(
            decode::<Envelope<Response>>(&encode(&start).unwrap()).unwrap(),
            start
        );

        let chunk = Envelope::new(Response::DirectoryViewChunk {
            entries: (0..3).map(sample_view_entry).collect(),
        });
        assert_eq!(
            decode::<Envelope<Response>>(&encode(&chunk).unwrap()).unwrap(),
            chunk
        );

        let end = Envelope::new(Response::DirectoryViewEnd);
        assert_eq!(
            decode::<Envelope<Response>>(&encode(&end).unwrap()).unwrap(),
            end
        );
    }

    #[test]
    fn chunk_filled_to_budget_encodes_within_frame_limit() {
        // Greedily pack entries the way the daemon does: never let the running
        // sum of individual entry sizes exceed DIRECTORY_VIEW_CHUNK_BUDGET.
        let mut entries = Vec::new();
        let mut packed = 0usize;
        for index in 0.. {
            let entry = sample_view_entry(index);
            let len = encoded_len(&entry).unwrap();
            if packed + len > DIRECTORY_VIEW_CHUNK_BUDGET {
                break;
            }
            packed += len;
            entries.push(entry);
        }
        assert!(!entries.is_empty());
        let frame = encode(&Envelope::new(Response::DirectoryViewChunk { entries }));
        // encode() rejects anything over MAX_FRAME_SIZE, so success proves the
        // budget headroom is sufficient for the envelope + framing overhead.
        assert!(frame.is_ok(), "budget-filled chunk exceeded MAX_FRAME_SIZE");
    }

    #[test]
    fn small_frame_is_left_uncompressed_and_round_trips() {
        // A tiny control message is below the threshold: the prefix must not set
        // the compressed flag and must decode identically to the pre-compression
        // framing (raw postcard).
        let message = Envelope::new(Response::Pong { nonce: 7 });
        let (prefix, body) = encode_frame(&message, true).unwrap();
        assert_eq!(prefix & FRAME_COMPRESSED_FLAG, 0);
        assert_eq!(body, encode(&message).unwrap());
        let (compressed, length) = parse_frame_header(prefix).unwrap();
        assert!(!compressed);
        assert_eq!(length, body.len());
        assert_eq!(
            decode_frame::<Envelope<Response>>(compressed, &body).unwrap(),
            message
        );
    }

    #[test]
    fn large_repetitive_frame_compresses_and_round_trips() {
        // A directory-view chunk of similar entries is exactly the highly
        // compressible payload this targets: the body must actually shrink, the
        // flag must be set, and it must reassemble bit-for-bit.
        let chunk = Envelope::new(Response::DirectoryViewChunk {
            entries: (0..2000).map(sample_view_entry).collect(),
        });
        let uncompressed = encode(&chunk).unwrap();
        let (prefix, body) = encode_frame(&chunk, true).unwrap();
        assert_ne!(prefix & FRAME_COMPRESSED_FLAG, 0);
        assert!(
            body.len() < uncompressed.len(),
            "compressed body ({}) was not smaller than raw ({})",
            body.len(),
            uncompressed.len()
        );
        let (compressed, length) = parse_frame_header(prefix).unwrap();
        assert!(compressed);
        assert_eq!(length, body.len());
        assert_eq!(
            decode_frame::<Envelope<Response>>(compressed, &body).unwrap(),
            chunk
        );
    }

    #[test]
    fn frame_header_rejects_oversized_length() {
        let prefix = (MAX_FRAME_SIZE as u32) + 1;
        assert!(matches!(
            parse_frame_header(prefix),
            Err(CodecError::TooLarge(_))
        ));
        // The flag bit must be masked off before the length check, so a
        // compressed frame at exactly the cap is still accepted.
        let (compressed, length) =
            parse_frame_header(MAX_FRAME_SIZE as u32 | FRAME_COMPRESSED_FLAG).unwrap();
        assert!(compressed);
        assert_eq!(length, MAX_FRAME_SIZE);
    }

    #[test]
    fn corrupt_compressed_body_fails_cleanly() {
        assert!(matches!(
            decode_frame::<Envelope<Response>>(true, &[0xff, 0x00, 0x13, 0x37]),
            Err(CodecError::Decompress)
        ));
    }

    #[test]
    fn compress_flag_gates_emission_but_not_decoding() {
        // With compress=false the sender must emit raw postcard even for a highly
        // compressible payload — this is how an older partner that cannot
        // decompress is protected. The receiver still round-trips it.
        let chunk = Envelope::new(Response::DirectoryViewChunk {
            entries: (0..2000).map(sample_view_entry).collect(),
        });
        let (prefix, body) = encode_frame(&chunk, false).unwrap();
        assert_eq!(prefix & FRAME_COMPRESSED_FLAG, 0);
        assert_eq!(body, encode(&chunk).unwrap());
        let (compressed, _) = parse_frame_header(prefix).unwrap();
        assert_eq!(
            decode_frame::<Envelope<Response>>(compressed, &body).unwrap(),
            chunk
        );
    }

    #[test]
    fn version_packing_and_compression_negotiation() {
        assert_eq!(version_major(PROTOCOL_VERSION), PROTOCOL_MAJOR);
        assert_eq!(version_minor(PROTOCOL_VERSION), PROTOCOL_MINOR);
        assert_eq!(make_version(6, 3), PROTOCOL_VERSION);
        // Same major, minor at/above the compression floor: compress. Below it or
        // a different major: do not.
        assert!(peer_supports_frame_compression(make_version(6, 3)));
        assert!(peer_supports_frame_compression(make_version(6, 9)));
        assert!(!peer_supports_frame_compression(make_version(6, 2)));
        assert!(!peer_supports_frame_compression(make_version(6, 0)));
        assert!(!peer_supports_frame_compression(make_version(7, 3)));
    }

    #[test]
    fn accepts_same_major_any_minor_but_rejects_other_major() {
        // A newer same-major minor is accepted (negotiation resolves features)…
        let mut newer = Envelope::new(Request::Ping { nonce: 1 });
        newer.version = make_version(PROTOCOL_MAJOR, 99);
        assert!(decode_request(&encode(&newer).unwrap()).is_ok());
        // …while a different major is a hard rejection.
        let mut other = Envelope::new(Request::Ping { nonce: 1 });
        other.version = make_version(7, 0);
        assert!(matches!(
            decode_request(&encode(&other).unwrap()),
            Err(CodecError::UnsupportedVersion(_))
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
