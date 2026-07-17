# Protocol version 2

QUIC/TLS carries Postcard-encoded, four-byte big-endian length-prefixed control frames (maximum 1 MiB). Every envelope has a protocol version and request UUID. Independent operations use independent bidirectional streams on one session connection.

Requests: `Hello`, `Pair`, `Authenticate`, `GetMetadata`, `ListDirectory`, `OpenFile`, `ReadRange`, `CloseFile`, and `Ping`. Responses: `HelloAck`, `PairingProof`, `AuthenticateAck`, `Metadata`, `DirectoryListing`, `FileOpened`, `ReadData`, `FileClosed`, `Pong`, and `Error`.

Except for hello, pairing, authentication, and ping, operations require successful authentication. Nodes and handles are opaque UUIDs. `ReadRange` supplies handle, offset, and length. `ReadData` supplies actual length and file revision, followed immediately by that many raw stream bytes. Reads are bounded and may be shorter at EOF. Checksums and notifications are reserved for later versions.

`Pair` carries a one-time pairing UUID and fresh 32-byte client nonce. `PairingProof` carries the presented certificate's SHA-256 fingerprint and HMAC-SHA-256 proof binding the pairing secret, fingerprint, and nonce. Pairing records are single-use. The client pins the fingerprint only after verifying both the proof and equality with the certificate presented by TLS.

`Authenticate` carries a username and password only on a connection whose certificate matches an existing pin. The server verifies a stored Argon2id password hash and returns `AuthenticateAck`; it never stores the plaintext password. The wire protocol does not yet express roles or per-user export permissions. See [Authentication and server trust](authentication.md).
