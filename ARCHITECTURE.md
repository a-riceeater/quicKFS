# Architecture

The protocol crate owns versioned wire types. `transport-quic` owns Quinn and TLS. `client-core` exposes `RemoteFilesystem`; platform adapters depend only on this interface. `server-core` owns filesystem policy and has no Quinn dependency. The Linux daemon binds transport to request dispatch.

Each operation uses an independent bidirectional stream on one long-lived QUIC connection. Control messages are length-prefixed Postcard frames. A read response frame states its byte count and revision; raw bytes immediately follow, avoiding embedding file data in serialization.

The export root is canonicalized at startup. Clients see stable opaque IDs derived from export-relative paths and never submit server paths. Discovered children are canonicalized and must remain beneath the root, preventing traversal and symlink escape. Open handles are random, session-visible capabilities with configured bounds.

TLS protects transport. A one-time high-entropy pairing code authenticates and pins the server certificate fingerprint on first contact. Later connections require the pin before username/password authentication; password records use Argon2id. Per-user authorization and production identity recovery remain incomplete.

The macOS adapter boundary owns the synchronous-callback/async-runtime bridge. It will use one runtime and connection manager, never a runtime per callback. Windows support is expected to use WinFsp later.

Metadata, directory, and revision-keyed range cache traits currently have an in-memory implementation. Persistent SQLite metadata and disk block storage are future work. A future write path will require leases/revisions, conflict semantics, durability, and explicit protocol negotiation; none are implemented.
