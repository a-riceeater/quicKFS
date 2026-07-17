# Protocol version 1

QUIC/TLS carries Postcard-encoded, four-byte big-endian length-prefixed control frames (maximum 1 MiB). Every envelope has a protocol version and request UUID. Independent operations use independent bidirectional streams on one session connection.

Requests: `Hello`, `Authenticate`, `GetMetadata`, `ListDirectory`, `OpenFile`, `ReadRange`, `CloseFile`, and `Ping`. Responses: `HelloAck`, `AuthenticateAck`, `Metadata`, `DirectoryListing`, `FileOpened`, `ReadData`, `FileClosed`, `Pong`, and `Error`.

Except for hello, authentication, and ping, operations require successful authentication. Nodes and handles are opaque UUIDs. `ReadRange` supplies handle, offset, and length. `ReadData` supplies actual length and file revision, followed immediately by that many raw stream bytes. Reads are bounded and may be shorter at EOF. Checksums and notifications are reserved for later versions.

