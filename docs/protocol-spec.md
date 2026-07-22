# quicKFS wire protocol — formal specification (v6.4)

This is the authoritative wire contract for quicKFS protocol major **6**, minor
**4**. It is the reference companion to [`protocol.md`](protocol.md) (the
narrative overview): where `protocol.md` explains *why*, this file pins down
*exactly what goes on the wire* — the transport, the frame layout, the version
word, every `Request` and `Response` variant, the error taxonomy, the capability
flags, and the rules a change must follow to be a compatible minor rather than a
flag-day major.

It is normative for anyone writing a second implementation or auditing the
existing one. The canonical source of truth is `crates/protocol/src/lib.rs`; this
document tracks it and every value here is named after the constant or type it
mirrors.

---

## 1. Transport

- **QUIC** (via `quinn`), TLS 1.3, ALPN **`quickfs/6`** (`ALPN_PROTOCOL`). The
  ALPN carries the protocol **major only** — see [§8](#8-versioning-and-negotiation).
- Server trust is established before any application data by one of: mutual
  role-separated pairing proof, centrally imported exact certificate pin, a
  deployed private-CA bundle, or the OS/public trust policy. See
  [`authentication.md`](authentication.md). This spec covers the framing and
  request model that ride *inside* the authenticated connection.
- **One operation per bidirectional stream.** Every request opens its own QUIC
  bidi stream, writes one request frame (finishing the send direction), and
  reads its response frame(s) back on the same stream. Streams are independent
  and may be in flight concurrently on the single long-lived connection;
  ordering between operations is not implied by the protocol.
- Client keep-alive interval **10 s**; both peers use a **5-minute** idle
  timeout. The server permits **256** concurrent bidirectional streams. Flow
  control: **32 MiB** stream receive window, **128 MiB** connection receive and
  send windows, so several maximum-size reads progress without serializing on
  flow control.

## 2. Frame layout

Every control message is a length-prefixed frame:

```
 ┌────────────────────────────┬─────────────────────────────────────┐
 │  4-byte big-endian prefix  │  body (`prefix & 0x7fff_ffff` bytes) │
 └────────────────────────────┴─────────────────────────────────────┘
```

- **Prefix** — a `u32`, big-endian on the wire. Its **high bit**
  (`FRAME_COMPRESSED_FLAG = 1 << 31`) is the compression flag. The low 31 bits
  are the body length in bytes.
- **Body length** is always ≤ `MAX_FRAME_SIZE` (**1 MiB = 2²⁰**). Because a real
  length never reaches bit 31, the flag can never collide with a length. A
  receiver **must** reject a declared length above `MAX_FRAME_SIZE` *before
  allocating* (`parse_frame_header` → `TooLarge`).
- **Body** — the message payload:
  - flag clear → raw **Postcard** encoding of the value;
  - flag set → **zstd** (`zstd::bulk`) compression of that same Postcard
    encoding. On receipt it is decompressed with the output bounded to
    `MAX_FRAME_SIZE` (decompression-bomb guard) and then Postcard-decoded.

The compression flag is **self-describing**: a receiver always honors it. Whether
a *sender* may set it is negotiated ([§8](#8-versioning-and-negotiation)).
Compression is applied only when negotiated **and** the body is ≥
`FRAME_COMPRESSION_THRESHOLD` (**1 KiB**) **and** the compressed form is strictly
smaller; otherwise the raw Postcard body is sent (a tiny or incompressible frame
is never enlarged). zstd level is `FRAME_COMPRESSION_LEVEL` (**3**).

### 2.1 Serialization

The body is **Postcard** (`postcard` 1.x), a compact, **non-self-describing**
binary format. This has a decisive consequence for evolution: struct fields are
written positionally with no names or count, and enum variants are identified by
their **declaration-order index** as a varint. There is no tolerance for a
missing or extra trailing struct field — `#[serde(default)]` does **not** rescue
a shortened struct on the wire (it only serves the self-describing JSON cache
manifests). The compatibility rules in [§8.2](#82-guideline-adding-a-version)
follow directly from this.

### 2.2 Envelope

Every request and every response body is an `Envelope<T>`:

| Field        | Type        | Notes |
|--------------|-------------|-------|
| `version`    | `u16`       | The sender's wire version word ([§8](#8-versioning-and-negotiation)). |
| `request_id` | `RequestId` | A UUID. A response echoes the request's `request_id`. |
| `message`    | `T`         | `Request` or `Response`. |

A receiver validates `version_major(version) == 6` and (for a response) that
`request_id` matches the outstanding request; otherwise the exchange is rejected.

### 2.3 Raw I/O bodies

`ReadRange`/`GetXattr` responses and `WriteRange`/`SetXattr` requests are a
control frame that **states a byte count**, immediately followed on the same
stream by exactly that many **raw** bytes (not a framed/Postcard value, never
compressed at this layer — file bytes carry their own representation and are
typically already-compressed media). Default read range **16 MiB**, default write
range **8 MiB** (`max_read_size`/`max_write_size` in the capabilities).

## 3. Primitive types

| Type            | Wire shape | Meaning |
|-----------------|-----------|---------|
| `RequestId`, `NodeId`, `FileHandle` | 16-byte UUID | Opaque identifiers. `ROOT_NODE` is the all-zero UUID. Handles are connection-local. |
| `Name`          | `Vec<u8>` | A lossless Unix name (filename or xattr name); arbitrary bytes, not restricted to UTF-8. |
| `FileRevision`, `DirectoryRevision` | `u64` | Monotonic version counters for coherence. |
| `SecretString`  | UTF-8 string | Redacted in debug, zeroized on drop. |
| `SecretProof`   | `[u8; 32]` | Redacted in debug, zeroized on drop. |
| timestamps      | `u64` / `Option<u64>` | Unix milliseconds. |

### 3.1 `Metadata`

The per-node attribute record. **Field order is the wire order** and must not
change (see [§8.2](#82-guideline-adding-a-version)):

| Field | Type | Notes |
|-------|------|-------|
| `node` | `NodeId` | |
| `kind` | `NodeKind` | `File`/`Directory`/`Symlink`/`NamedPipe`/`CharacterDevice`/`BlockDevice`/`Socket`. |
| `size` | `u64` | |
| `mode` | `u32` | Permission and special bits, without the type bits. |
| `allocated_blocks` | `u64` | 512-byte blocks, when available. |
| `revision` | `u64` | |
| `accessed_unix_ms` | `u64` | |
| `modified_unix_ms` | `u64` | |
| `created_unix_ms` | `Option<u64>` | |
| `backup_unix_ms` | `Option<u64>` | |
| `link_count` | `u32` | |
| `device_major`, `device_minor` | `u32`, `u32` | For special nodes. |

There is deliberately **no distinct inode-change (`ctime`) field**: adding one is
a positional struct change that Postcard cannot carry compatibly within a major
(see [§8.3](#83-known-non-additive-changes)).

## 4. Request messages

All requests except `Hello`, `Pair`, `Authenticate`, and `Ping` require a
completed `Authenticate`. Mutating requests additionally require both daemon
`--allow-writes` and the account's write grant.

### 4.1 Session and discovery

| Request | Fields | Response | Notes |
|---------|--------|----------|-------|
| `Hello` | `client_name: String` | `HelloAck { version }` | First message; version exchange. Pre-auth. |
| `Pair` | `pairing_id: Uuid`, `client_nonce: [u8;32]`, `client_proof: SecretProof` | `PairingProof { certificate_fingerprint, proof }` | One-time pairing. Pre-auth. |
| `Authenticate` | `username: String`, `password: SecretString` | `AuthenticateAck` | Sets the connection authenticated. Pre-auth. |
| `GetCapabilities` | — | `Capabilities(FilesystemCapabilities)` | |
| `StatFilesystem` | — | `FilesystemStats(...)` | |
| `Ping` | `nonce: u64` | `Pong { nonce }` | Liveness. Pre-auth. |

### 4.2 Metadata and directory reads

| Request | Fields | Response | Notes |
|---------|--------|----------|-------|
| `GetMetadata` | `node` | `Metadata(...)` | Single node. |
| `GetMetadataBatch` | `nodes: Vec<NodeId>` (≤ `MAX_METADATA_BATCH` = 4096) | `MetadataBatch { results }` | **Minor 6.4.** One positional `BatchedMetadata` per node; over-limit → `Error(TooLarge)`. See [§7](#7-metadata-batching-64). |
| `ListDirectory` | `node` | `DirectoryListing { revision, entries }` | Lightweight; single frame, returns `TooLarge` if it would exceed one frame. |
| `ListDirectoryView` | `node`, `options: DirectoryViewOptions` | `DirectoryView` **or** streamed `DirectoryViewStart`/`Chunk`/`End` | Enriched projection; see [§6](#6-enriched-directory-views). |

### 4.3 File I/O

| Request | Fields | Response |
|---------|--------|----------|
| `OpenFile` | `node`, `options: FileOpenOptions` | `FileOpened { handle, revision, size }` |
| `ReadRange` | `handle`, `offset`, `length` | `ReadData { revision, length }` + raw bytes |
| `WriteRange` | `handle`, `offset`, `length` + raw bytes | `WriteComplete { written, revision, size }` |
| `FlushFile` | `handle`, `lock_owner: Option<u64>` | `FileFlushed` |
| `SyncFile` | `handle`, `data_only: bool` | `FileSynced` |
| `AllocateFile` | `handle`, `offset`, `length` | `FileAllocated { revision, size }` |
| `CopyFileRange` | `input`, `input_offset`, `output`, `output_offset`, `length` | `RangeCopied { copied, revision, size }` |
| `SeekFile` | `handle`, `offset`, `whence: SeekWhence` (`Data`/`Hole`) | `FileSeeked { offset }` |
| `SafeIoctl` | `handle`, `operation: SafeIoctl` (`BytesAvailable`) | `IoctlResult { value }` |
| `MapBlock` | `node`, `block_size`, `block` | `BlockMapped { block }` |
| `CloseFile` | `handle` | `FileClosed` |

### 4.4 Namespace mutations

| Request | Fields | Response |
|---------|--------|----------|
| `CreateFile` | `parent`, `name`, `mode`, `options` | `FileCreated { metadata, handle, revision, size }` |
| `CreateDirectory` | `parent`, `name`, `mode` | `NodeCreated(Metadata)` |
| `CreateSymlink` | `parent`, `name`, `target: Vec<u8>` | `NodeCreated(Metadata)` |
| `CreateSpecialNode` | `parent`, `name`, `kind`, `mode`, `device_major`, `device_minor` | `NodeCreated(Metadata)` |
| `CreateHardLink` | `node`, `new_parent`, `new_name` | `HardLinkCreated(Metadata)` |
| `RemoveNode` | `parent`, `name`, `directory: bool` | `NodeRemoved` |
| `RenameNode` | `parent`, `name`, `new_parent`, `new_name`, `mode: RenameMode` (`Replace`/`NoReplace`/`Exchange`) | `NodeRenamed` |
| `ExchangeData` | `parent`, `name`, `new_parent`, `new_name`, `options: u64` | `DataExchanged` |
| `ReadLink` | `node` | `LinkTarget(Vec<u8>)` |
| `SetAttributes` | `node`, `handle: Option<FileHandle>`, `changes: AttributeChanges` | `AttributesChanged(Metadata)` |
| `SyncDirectory` | `node` | `DirectorySynced` |
| `SetVolumeName` | `name` | `VolumeNameSet` |

`AttributeChanges` carries `Option`s for `size`, `mode`, `accessed_unix_ms`,
`modified_unix_ms`, `backup_unix_ms`.

### 4.5 Extended attributes

| Request | Fields | Response |
|---------|--------|----------|
| `GetXattr` | `node`, `name`, `offset`, `length` | `XattrData { length, total_size }` + raw bytes |
| `SetXattr` | `node`, `name`, `mode: XattrSetMode` (`Upsert`/`Create`/`Replace`), `position`, `length` + raw bytes | `XattrSet` |
| `ListXattrs` | `node` | `XattrNames(Vec<Name>)` |
| `RemoveXattr` | `node`, `name` | `XattrRemoved` |

### 4.6 Locks and node lifecycle

| Request | Fields | Response |
|---------|--------|----------|
| `GetLock` | `handle`, `lock: FileLock` | `LockStatus { conflict: Option<FileLock> }` |
| `SetLock` | `handle`, `lock: FileLock`, `wait: bool` | `LockUpdated` (or `Error(WouldBlock)`) |
| `ForgetNodes` | `nodes: Vec<NodeId>` | `NodesForgotten` | Advisory, idempotent per-connection node release. |

`FileLock` = `{ owner: u64, start: u64, end: u64 (inclusive; u64::MAX = EOF), kind: LockKind (Read/Write/Unlock), pid: u32 }`.

## 5. Response messages

Every response is one of the variants named in the tables above, plus:

- `Error(ProtocolError { code: ErrorCode, message: String })` — any request may
  answer with this instead of its success variant.
- The streamed directory-view triple `DirectoryViewStart` / `DirectoryViewChunk`
  / `DirectoryViewEnd` ([§6](#6-enriched-directory-views)).
- `MetadataBatch { results: Vec<BatchedMetadata> }` ([§7](#7-metadata-batching-64)).

### 5.1 Error taxonomy (`ErrorCode`)

`Unauthenticated`, `NotFound`, `PermissionDenied`, `AlreadyExists`,
`NotDirectory`, `IsDirectory`, `NotEmpty`, `NoAttribute`, `NoData`, `NotTty`,
`ReadOnly`, `Conflict`, `WouldBlock`, `NoSpace`, `Busy`, `NotSupported`,
`Offline`, `InvalidNode`, `InvalidHandle`, `InvalidRequest`,
`UnsupportedVersion`, `TooLarge`, `Timeout`, `Internal`.

These map to the platform adapter's native errno (e.g. `NotFound` → `ENOENT`,
`TooLarge` → `EFBIG`, `Conflict`/`InvalidNode` → `ESTALE`). `UnsupportedVersion`
is returned (uncompressed) when a peer's **major** differs from 6.

## 6. Enriched directory views

`ListDirectoryView` returns, for one directory: its own and its parent's
`Metadata`, the directory revision, and for every child its lossless name, stable
`NodeId`, kind, and full `Metadata`; optionally (per `DirectoryViewOptions`) a
complete xattr-name set per node and small inline xattr values (≤ 4 KiB each,
`MAX_DIRECTORY_INLINE_XATTR_SIZE`, under a 256 KiB request-wide budget,
`MAX_DIRECTORY_INLINE_XATTR_TOTAL_SIZE`).

**Single-frame:** a view that fits one `MAX_FRAME_SIZE` frame is one
`DirectoryView` value.

**Streamed (minor 6.1):** a larger view is `DirectoryViewStart { revision,
directory, parent, xattrs, entry_count }`, then one or more
`DirectoryViewChunk { entries }` each packed just under
`DIRECTORY_VIEW_CHUNK_BUDGET`, then `DirectoryViewEnd`, all on the request's own
stream from one revision-consistent snapshot. The client reassembles them into a
single `DirectoryView`, so callers above the transport see one result either way.
Both peers cap the materialized entry set at `MAX_DIRECTORY_ENTRIES`
(**1,048,576**). `entry_count` is advisory; the receiver still enforces the cap
against what it actually receives.

## 7. Metadata batching (6.4)

`GetMetadataBatch { nodes }` resolves up to `MAX_METADATA_BATCH` (**4096**) nodes
in one round trip. The reply `MetadataBatch { results }` holds one entry per
requested node, **in request order**:

```
enum BatchedMetadata { Found(Metadata), Failed(ErrorCode) }
```

A per-node failure is reported in its own slot, so a single missing node never
fails the batch. An over-limit request (more than 4096 nodes) is rejected whole
with `Error(TooLarge)`. The 4096 cap guarantees the reply fits one control frame
(no streaming path) and bounds server-side fan-out. The request is read-only and
idempotent — safe to reconnect and retry. See [§8.1](#81-capability-negotiation)
for how emission is gated.

## 8. Versioning and negotiation

### 8.1 The version word and capability negotiation

The wire version is a **major.minor** pair packed into a single `u16`: major in
the high byte, minor in the low byte (`make_version`, `version_major`,
`version_minor`). Current value **6.4** (`PROTOCOL_MAJOR = 6`,
`PROTOCOL_MINOR = 4`).

Compatibility is **per major**:

- The **ALPN is keyed to the major only** (`quickfs/6`), so every `6.x` peer
  establishes one QUIC connection rather than failing to connect.
- The per-request / per-response check **accepts any same-major minor** and
  **hard-rejects only a different major** (`UnsupportedVersion`,
  `decode_request`).

Each side learns the other's **exact** version: the client sends `Hello` before
authenticating and reads the server's version from `HelloAck`; the server reads
the client's version off **every** request envelope, so it holds no
per-connection version state.

A minor may add **optional capabilities**, each gated by a
`peer_supports_<feature>(version)` predicate that is true only for a same-major
peer at or above the feature's floor minor. **A peer emits the feature only when
the partner advertises support**, so a newer peer and an older same-major peer
still interoperate — the newer side simply withholds what the older side cannot
parse. Current capabilities:

| Capability | Floor | Predicate | Gates |
|------------|-------|-----------|-------|
| Frame compression | 6.3 (`MINOR_FRAME_COMPRESSION`) | `peer_supports_frame_compression` | whether a **sender** may set the compression flag (decoding is always supported). |
| Metadata batching | 6.4 (`MINOR_METADATA_BATCH`) | `peer_supports_metadata_batch` | whether the **client** may send `GetMetadataBatch` (else it falls back to per-node `GetMetadata`). |

Minor history: **6.0** baseline enriched views · **6.1** streamed directory-view
pagination · **6.3** frame compression · **6.4** metadata batching. (6.2 was
skipped.)

### 8.2 Guideline: adding a version

Because Postcard is positional and non-self-describing ([§2.1](#21-serialization)),
compatibility hinges on never changing how an *existing* value decodes. The rule
set that keeps a change a **minor** (no flag day):

1. **Add, never reshape.** New `Request`/`Response` variants must be **appended
   at the end** of their enum so every existing variant keeps its discriminant
   index. **Never** insert a variant mid-enum, reorder variants, or remove one.
2. **Do not change any existing struct's fields** — not their number, order, or
   type. Adding a trailing field to a struct like `Metadata` is **not** additive
   on the wire even with `#[serde(default)]`: Postcard has no field count, so a
   reader with the extra field either hits `DeserializeUnexpectedEnd` or steals
   bytes from the following value. A new field means a **new** variant/struct
   carrying it, gated by negotiation — or a major bump.
3. **Gate emission on negotiation.** A new message or a new frame-level behavior
   is emitted only when `peer_supports_<feature>(peer_version)` holds, so an
   older same-major peer never receives (and never has to decode) it. Add the
   `MINOR_<FEATURE>` floor constant and the predicate alongside the change.
4. **Self-describing frame flags** (like `FRAME_COMPRESSED_FLAG`) may be *decoded*
   unconditionally, but whether they are *set* is still negotiated per rule 3.
5. **Bump `PROTOCOL_MINOR`** and record the new minor in the history above,
   `protocol.md`, and this file.
6. **Prove backward compatibility** for all four peer combinations
   (old/new client × old/new server); the in-repo tests do this for compression
   (`directory_view_compression_is_negotiated_by_minor_version`) and batching
   (`metadata_batch_negotiation_gates_on_minor`, the client fallback path).

Following these, an additive feature ships to a subset of peers with **no
flag day** — a `6.(n+1)` daemon serves a `6.0` client and a `6.(n+1)` client
talks to a `6.n` daemon.

A change that cannot obey rules 1–2 (a reshaped struct, a removed/renamed
variant, an incompatible field-type change, or a semantic change to an existing
message) is a **major bump**: increment `PROTOCOL_MAJOR`, which changes the ALPN
to `quickfs/<major>` and is a genuine flag day where peers refuse to talk across
majors. There is currently **no** intermediate "breaking but same-ALPN" tier; a
per-major capability bitset (which would allow one) is noted as future work in
the roadmap.

> Terminology note. Informally the scheme is sometimes written `v6.x.y` with `x`
> a "major bump" and `y` a "minor bump" under the fixed `6` epoch. As
> implemented, the `6` **is** the major (the ALPN/flag-day axis) and there are
> two levels, not three: additive changes are **minor** bumps (`y`, negotiated,
> what almost everything is), and a breaking change is a **major** bump (a new
> ALPN). The optional middle "sub-major" tier does not exist until the
> capability-bitset work lands.

### 8.3 Known non-additive changes

Deferred precisely because they cannot be compatible minors under
[§8.2](#82-guideline-adding-a-version):

- **Distinct `ctime` on `Metadata`.** A new positional field on a struct that is
  embedded in ~a dozen responses; not carriable within major 6 without either a
  parallel negotiated `Metadata`-carrying message or a major bump.
- **Server-initiated messages / server push.** The model is strictly
  request/response; a push channel (dedicated uni stream or datagram plus a
  client dispatcher) is a transport-shaped change, not an enum append.

## 9. Reconnect and mutation safety

A mutation is **not** blindly retried after a transport failure — its outcome may
be ambiguous (`AmbiguousMutation`). Safe reads and lookups (including
`GetMetadataBatch`) may reconnect and retry. A logical open handle is reopened
only when the persistent server epoch **and** the exact file revision still
match; the surviving client then replays its advisory lock history. Handles are
connection-local; node IDs are stable across reconnect via the persisted
epoch/node-key.

---

*This document mirrors `crates/protocol/src/lib.rs` at protocol 6.4. When the
code and this file disagree, the code is authoritative — and the discrepancy is a
bug in this file.*
