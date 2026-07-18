# Protocol version 5

QUIC/TLS uses the `quickfs/5` ALPN identifier. Control messages are Postcard-encoded envelopes with a protocol version and request UUID, framed by a four-byte big-endian length with a 1 MiB maximum. Independent operations use independent bidirectional streams on one authenticated connection. Versions are deliberately incompatible rather than silently negotiating different filesystem semantics.

Pairing and login remain separated from filesystem access. `Pair` proves possession of a one-time secret while binding the pairing ID, fresh nonce, and exact TLS certificate fingerprint. `Authenticate` is sent only after the selected pin/CA/system trust policy authenticates TLS. Every filesystem request requires login, and every mutation additionally requires both daemon `--allow-writes` and the account's write grant.

Nodes and file handles are opaque UUIDs. `Name` is an arbitrary byte vector, so Unix filenames and xattr names are lossless rather than restricted to UTF-8. Persistent exports retain an epoch and secret node-key outside the export; clients reject a different epoch during reconnect. Handles are connection-local.

The request model covers capabilities/statfs, metadata and metadata-bearing directory snapshots, open/create/read/write/flush/sync/close, directory and symlink operations, remove and all rename modes, hardlinks, special nodes, xattrs, preallocation, range copy, data/hole seek, safe ioctl, poll readiness, block mapping, data exchange, volume name, backup time, byte-range locks, and advisory batch node forget. Responses carry updated metadata/revisions where coherency requires them.

`ReadRange` and `GetXattr` return a control response followed by exactly the advertised raw byte count. `WriteRange` and `SetXattr` send a bounded raw body after their request frame. Default raw read/write chunks are limited to 8 MiB and are guarded by per-request, per-connection, and global budgets. Server-side copy carries no raw body and may cover a larger range so the server can use reflink/copy-range acceleration.

Mutations are not blindly retried after a transport failure because their outcome may be ambiguous. Safe reads and lookups may reconnect and retry. A logical open handle is reopened only when the persistent server epoch and exact file revision still match; the surviving client then replays its advisory lock history.

See [Authentication and server trust](authentication.md) and [Filesystem semantics](filesystem-semantics.md).
