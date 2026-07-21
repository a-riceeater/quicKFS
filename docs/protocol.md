# Protocol version 6.3

QUIC/TLS uses the `quickfs/6` ALPN identifier. Control messages are Postcard-encoded envelopes with a protocol version and request UUID, framed by a four-byte big-endian length prefix. Independent operations use independent bidirectional streams on one authenticated connection. The client sends a 10-second transport keepalive, both endpoints allow five minutes of idle time, the server permits 256 concurrent bidirectional streams, and 32 MiB stream/128 MiB connection receive windows allow several large reads to progress without flow-control serialization.

## Versioning and negotiation

The wire version is a **major.minor** pair packed into a single `u16` (major in the high byte, minor in the low byte). Compatibility is **per major**: the ALPN identifier is keyed to the major alone (`quickfs/6`), so every `6.x` peer establishes one QUIC connection instead of failing to connect, and the per-request/per-response check accepts any same-major minor and hard-rejects only a different major. A different major is a genuine flag day (incompatible wire contract, different ALPN); everything additive is a **minor** bump and is *not*.

Minors are backward-negotiated. `Hello`/`HelloAck` carry each side's exact version: the client sends `Hello` before authenticating and learns the server's version from `HelloAck`, and the server reads the client's version off every request envelope (so it needs no per-connection state). A minor may add optional capabilities that each side **only emits when the partner advertises support** for them, so a newer client and an older same-major daemon still interoperate — the newer side simply withholds what the older side cannot parse. Minor history within major 6:

- **6.0** — baseline enriched-directory-view protocol.
- **6.1** — streamed directory-view pagination (`DirectoryViewStart`/`Chunk`/`End`; see [Enriched directory views](#enriched-directory-views)).
- **6.3** — per-frame compression (see [Frame compression](#frame-compression)).

## Frame compression

The four-byte length prefix carries a compression flag in its high bit (`FRAME_COMPRESSED_FLAG`). When set, the framed body is [zstd](https://facebook.github.io/zstd/)-compressed and the low 31 bits hold its on-wire length; when clear, the body is the raw Postcard encoding. Because a real body length never exceeds `MAX_FRAME_SIZE` (1 MiB = 2^20), it can never reach bit 31, so the flag never collides with a length.

Compression is **negotiated then opportunistic**. A peer emits a compressed frame only when the partner's advertised minor is at least `MINOR_FRAME_COMPRESSION` (6.3) — so an older `6.x` peer that cannot decompress never receives one — *and* the payload is at least `FRAME_COMPRESSION_THRESHOLD` (1 KiB) *and* the compressed form is actually smaller. Tiny control messages (pings, single-node metadata, acks) and incompressible bodies are therefore sent uncompressed and never enlarged. Decoding a compressed frame is always supported regardless of negotiation — the flag is self-describing — so the gate governs the *sender*, not the receiver. zstd level 3 is used, the throughput/ratio sweet spot for these payloads. Enriched directory views are the target: on a realistic media-library directory their repeated names and xattr keys compress roughly **7×** (≈86% fewer bytes on the wire) even with high-entropy per-child sizes and timestamps, which directly speeds cold crawls over a WAN.

Compression happens *after* Postcard encoding and *after* directory-view chunk packing, so the 1 MiB frame limit and the chunk budget are still measured against the uncompressed size (compress *then* fit) and paging behavior is unchanged. On receipt, a compressed body is decompressed with its output bounded to `MAX_FRAME_SIZE`, so a hostile peer cannot force an unbounded allocation from a tiny frame (decompression-bomb guard). Raw I/O bodies (`ReadRange`/`GetXattr`/`WriteRange` payloads, below) are not control frames and are never compressed at this layer — file bytes carry their own representation and are typically already-compressed media.

Pairing and login remain separated from filesystem access. `Pair` proves possession of a one-time secret while binding the pairing ID, fresh nonce, and exact TLS certificate fingerprint. `Authenticate` is sent only after the selected pin/CA/system trust policy authenticates TLS. Every filesystem request requires login, and every mutation additionally requires both daemon `--allow-writes` and the account's write grant.

Nodes and file handles are opaque UUIDs. `Name` is an arbitrary byte vector, so Unix filenames and xattr names are lossless rather than restricted to UTF-8. Persistent exports retain an epoch and secret node-key outside the export; clients reject a different epoch during reconnect. Handles are connection-local.

The request model covers capabilities/statfs, metadata and enriched directory views, open/create/read/write/flush/sync/close, directory and symlink operations, remove and all rename modes, hardlinks, special nodes, xattrs, preallocation, range copy, data/hole seek, safe ioctl, poll readiness, block mapping, data exchange, volume name, backup time, byte-range locks, and advisory batch node forget. Responses carry updated metadata/revisions where coherency requires them.

## Enriched directory views

`ListDirectoryView` is the native-filesystem projection. One request returns:

- the requested directory's metadata and directory revision;
- its parent metadata;
- every child's lossless name, stable node ID, type, and full metadata;
- a complete xattr-name set for each supported node; and
- small xattr values up to 4 KiB each, under a 256 KiB request-wide inline-value budget.

The Linux server performs child stat/xattr work concurrently beside the export under the global `--max-directory-entry-tasks` bound. It rechecks the directory revision before sending the result. Large xattrs, including resource forks, continue through bounded `GetXattr` ranges.

**Pagination (6.1).** A directory whose projection fits one 1 MiB control frame is returned as a single `DirectoryView`, unchanged from earlier versions. A larger projection is *streamed* over the request's own stream as a `DirectoryViewStart` header (revision, directory and parent metadata, the directory's own xattrs, and an advisory entry count), followed by one or more `DirectoryViewChunk` frames of entries packed just under the frame limit, and terminated by `DirectoryViewEnd`. The server holds one revision-consistent snapshot for the whole stream, so there is no cursor to expire and no cross-page skew; the client reassembles the frames into an identical `DirectoryView` before returning to any caller above the transport. A single entry too large for one chunk sheds its inline xattr values (still reachable via `GetXattr`) so every frame stays bounded. Both peers cap the fully materialized entry set at `MAX_DIRECTORY_ENTRIES` (1,048,576) as a memory backstop; in practice the per-connection node ceiling (below) is reached first. The lighter `ListDirectory` request is still single-frame and returns a clean `TooLarge` error (never a dropped connection) when a plain listing would exceed the frame.

The macFUSE adapter single-flights concurrent cold requests for the same directory, populates inode, metadata, complete xattr-name, xattr-size, and inline-value caches from the response, and then answers Finder's `readdir`, `lookup`, `getattr`, `listxattr`, negative xattr probes, and common small `getxattr` calls locally. Child metadata and the directory view share a 30-second lifetime; expiring children earlier would recreate the per-child metadata fan-out this request replaces. `ListDirectory` remains the lighter diagnostic/path-resolution request used by the CLI.

## Node identity and per-connection limits

A *node* is the server's opaque, stable UUID for one file or directory (§ above). The server keeps a **node registry** mapping each node to its backing file identity (device + inode) and known path(s). A node becomes *known* to a connection the moment the client references it — a `Lookup`, a `GetMetadata`, or, most significantly, an enriched `ListDirectoryView`, which registers **one node per child entry** in a single request.

Two bounds cap how many nodes may be known at once. Both are semaphore counters, so a high ceiling costs nothing until nodes are actually tracked:

- **`--max-known-nodes-per-connection`** — the ceiling on the *live working set* of one mounted client: how many distinct inodes it may reference simultaneously. This is the limit a client actually hits.
- **`--max-total-known-nodes`** — the ceiling across all connections. Must be at least one connection's worth.

This is **not** a limit on how many files an export may contain (that is unbounded) — it bounds the *live* set a single client holds at one instant. Its purpose is memory/DoS protection: without it, a buggy or hostile client could look up nodes without ever forgetting them and grow the registry until the daemon exhausts memory.

**Lifecycle.** Referencing a node takes one per-connection permit and one global permit. When the macOS kernel evicts an inode from its vnode cache it issues `Forget`; the adapter batches these into `ForgetNodes`, and the server releases the per-connection permit immediately. When a connection drops, its session releases every permit it held. Global registry entries whose reference count reaches zero are *retained as a cache* — so a reconnect keeps stable node IDs — and are evicted lazily (least-recently-found) only when the global pool is under pressure. So a client's working set falls right after a `Forget` or disconnect, while total daemon memory is a bounded cache that trends toward its high-water mark and is reclaimed on demand. Each tracked node costs roughly 400–500 bytes across the registry and per-connection maps.

**Why the default is 131072 / 524288.** macOS keeps a large kernel vnode cache (`kern.maxvnodes` is typically 100k–300k), so a Finder browse, Spotlight index, or recursive `find` over a large media library legitimately holds tens of thousands of live inodes on a single mount. The macFUSE mount also runs with `auto_xattr` (extended attributes stored in AppleDouble `._name` sidecars, required so `cp`/Finder can copy quarantined downloads — see [filesystem-semantics.md](filesystem-semantics.md)), which makes the kernel look up a `._name` node per file and roughly **doubles** the working set during a crawl. The historical `8192 / 65536` defaults were sized for small exports and starved these workloads: the client surfaces exhaustion as `opendir`/`readdir` failing with *"too many known nodes"* (delivered as `ErrorCode::TooLarge`, which the mount maps to `EFBIG` / "File too large").

**Suggested limits by environment.** The rule of thumb is ~0.5 KB per node — so `131072` per connection is ~64 MiB worst case (only if fully saturated; real usage tracks the live set and is usually far lower).

| Environment | `--max-known-nodes-per-connection` | `--max-total-known-nodes` | Notes |
| --- | --- | --- | --- |
| Small/personal export, light browsing | `65536` | `131072` | Lower RAM ceiling; fine if you never crawl huge trees. |
| **Media library, Finder + Spotlight, `auto_xattr` (default)** | **`131072`** | **`524288`** | Headroom for large crawls with sidecar doubling; ≈64 MiB / ≈256 MiB worst case. |
| Multi-user server, many concurrent mounts | `131072` | `per-conn × expected concurrent clients` | Size total to real concurrency and watch daemon RSS. |

Both are plain daemon flags, so a running server can be retuned by restarting it with new values — no client change and no protocol change. Since 6.1 the enriched directory view is streamed in frame-sized chunks (see [Enriched directory views](#enriched-directory-views)), so a single directory is no longer capped at what fits one 1 MiB control frame; the per-connection node ceiling above is the effective limit on how many children one view can register at once.

## Raw I/O and server-side work

`ReadRange` and `GetXattr` return a control response followed by exactly the advertised raw byte count. `WriteRange` and `SetXattr` send a bounded raw body after their request frame. The default read range is 16 MiB and the default write range is 8 MiB. In-flight read byte permits queue under the overall request timeout instead of failing a valid burst merely because another read is active.

The mount aligns sequential/copy-sized read-through cache fills to the configured block size, up to the negotiated 16 MiB range limit. Sub-1 MiB probes instead use at most a 1 MiB aligned fill so metadata/thumbnail inspection does not read 16 MiB from every touched file. Concurrent overlapping misses for one revision share one persistent lookup or remote fetch, including its failure, and a normal maximum-size macFUSE read fits in one v6 read request. A bounded process-local hot range tier serves repeated small slices without rereading and verifying the same persistent block. Server-side copy carries no raw body and may cover a larger range so the server can use reflink/copy-range acceleration without downloading and re-uploading bytes.

## Reconnect and mutation safety

Mutations are not blindly retried after a transport failure because their outcome may be ambiguous. Safe reads and lookups may reconnect and retry. A logical open handle is reopened only when the persistent server epoch and exact file revision still match; the surviving client then replays its advisory lock history.

See [Authentication and server trust](authentication.md) and [Filesystem semantics](filesystem-semantics.md).
