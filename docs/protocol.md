# Protocol version 6

QUIC/TLS uses the `quickfs/6` ALPN identifier. Control messages are Postcard-encoded envelopes with a protocol version and request UUID, framed by a four-byte big-endian length with a 1 MiB maximum. Independent operations use independent bidirectional streams on one authenticated connection. The client sends a 10-second transport keepalive, both endpoints allow five minutes of idle time, the server permits 256 concurrent bidirectional streams, and 32 MiB stream/128 MiB connection receive windows allow several large reads to progress without flow-control serialization. Versions are deliberately incompatible rather than silently negotiating different filesystem semantics; deploy the v6 daemon before or together with v6 clients.

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

The Linux server performs child stat/xattr work concurrently beside the export under the global `--max-directory-entry-tasks` bound. It rechecks the directory revision before sending the result. If optional xattr data would make the 1 MiB control frame too large, inline values are removed first and xattr snapshots are omitted if necessary; entry metadata remains authoritative. Large xattrs, including resource forks, continue through bounded `GetXattr` ranges.

The macFUSE adapter single-flights concurrent cold requests for the same directory, populates inode, metadata, complete xattr-name, xattr-size, and inline-value caches from the response, and then answers Finder's `readdir`, `lookup`, `getattr`, `listxattr`, negative xattr probes, and common small `getxattr` calls locally. `ListDirectory` remains the lighter diagnostic/path-resolution request used by the CLI.

## Raw I/O and server-side work

`ReadRange` and `GetXattr` return a control response followed by exactly the advertised raw byte count. `WriteRange` and `SetXattr` send a bounded raw body after their request frame. The default read range is 16 MiB and the default write range is 8 MiB. In-flight read byte permits queue under the overall request timeout instead of failing a valid burst merely because another read is active.

The mount aligns read-through cache fills to the negotiated range limit. Concurrent overlapping misses for one revision share one persistent lookup or remote fetch, and a normal maximum-size macFUSE read fits in one v6 read request. A bounded process-local hot range tier serves repeated small slices without rereading and verifying the same persistent block. Server-side copy carries no raw body and may cover a larger range so the server can use reflink/copy-range acceleration without downloading and re-uploading bytes.

## Reconnect and mutation safety

Mutations are not blindly retried after a transport failure because their outcome may be ambiguous. Safe reads and lookups may reconnect and retry. A logical open handle is reopened only when the persistent server epoch and exact file revision still match; the surviving client then replays its advisory lock history.

See [Authentication and server trust](authentication.md) and [Filesystem semantics](filesystem-semantics.md).
