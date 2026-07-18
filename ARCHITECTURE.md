# Architecture

The protocol crate owns versioned wire types. `transport-quic` owns Quinn and TLS. `client-core` exposes `RemoteFilesystem`; platform adapters depend only on this interface. `server-core` owns filesystem policy and has no Quinn dependency. The Linux daemon binds transport to request dispatch.

Each operation uses an independent bidirectional stream on one long-lived QUIC connection. Control messages are length-prefixed Postcard frames. A read response frame states its byte count and revision; raw bytes immediately follow, avoiding embedding file data in serialization.

The export root is canonicalized at startup. Clients see stable opaque IDs derived from export-relative paths and never submit server paths. Discovered children are canonicalized and must remain beneath the root, preventing traversal and symlink escape. Known-node caches have both per-connection and server-wide budgets; open handles are random, session-visible capabilities with a global configured bound.

TLS protects transport and negotiates a version-specific ALPN. Server trust is selected explicitly: mutual role-separated pairing proofs, a centrally imported exact pin, a deployed private-CA bundle, or the operating-system public/managed trust policy. Exact-pin modes authenticate the leaf fingerprint; CA modes perform standard chain and server-name validation. Every mode authenticates the server before the password prompt and again on the post-prompt connection. Password records use Argon2id, and authentication work/source attempts are bounded. External certificate chains are supported; renewal is installed as a private generation selected by an atomic pointer. Per-user authorization, distributed-login defense, and production identity recovery remain incomplete.

The macOS adapter boundary owns the synchronous-callback/async-runtime bridge. It will use one runtime and connection manager, never a runtime per callback. Windows support is expected to use WinFsp later.

Metadata, directory, and revision-keyed range cache traits currently have an in-memory implementation. Persistent SQLite metadata and disk block storage are future work. A future write path will require leases/revisions, conflict semantics, durability, and explicit protocol negotiation; none are implemented.
