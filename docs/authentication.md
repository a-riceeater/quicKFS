# Authentication and server trust

quicKFS uses pairing-assisted trust on first use followed by username/password authentication. Clients do not need a manually copied certificate.

## Security roles

- The server's persistent TLS identity authenticates the server to clients.
- A one-time pairing code authenticates the first presentation of that identity.
- The client pins the certificate SHA-256 fingerprint after pairing.
- Username/password authentication identifies the user on every later connection.

These are separate. Pairing does not log a user in, and a correct password is never sent until the client has verified the pinned server identity.

## First connection

```text
Administrator                 Client                         Server
      |                          |                              |
      | create one-time pairing |                              |
      | ID and secret code      |                              |
      |------------------------->|                              |
      |                          |-- temporary TLS connection ->|
      |                          |<- certificate + proof -------|
      |                          |                              |
      |                          | Verify proof binds:          |
      |                          | - out-of-band code           |
      |                          | - presented certificate      |
      |                          | - fresh client nonce         |
      |                          |                              |
      |                          | Pin certificate fingerprint  |
```

The temporary pairing connection accepts the presented certificate before it is trusted, but it is restricted by client behavior to the pairing request. The client does not send a username, password, or filesystem request on it. The server returns an HMAC-SHA-256 proof made with the high-entropy pairing secret over the presented certificate fingerprint and a fresh client nonce. A man-in-the-middle presenting a different certificate cannot produce a matching proof.

Pairing codes contain 160 random bits, expire (five minutes by default), and are deleted when used. The pairing ID identifies the server-side record but is not sufficient without the code. Transfer both through a trusted channel. Prefer entering the code at the hidden prompt instead of placing it in command-line history.

This is pairing-assisted TOFU, not plain TOFU: the out-of-band secret authenticates the first trust decision. There is currently no QR renderer; the ID and grouped code are textual.

## Later connections

```text
Client                                              Server
  |                                                    |
  |--- QUIC/TLS connection --------------------------->|
  |<-- persistent server certificate ------------------|
  | Verify exact pinned SHA-256 fingerprint             |
  |                                                    |
  |--- Authenticate { username, password } ------------>|
  |<-- AuthenticateAck or Unauthenticated -------------|
  |                                                    |
  |--- filesystem requests after authentication ------>|
```

An unexpected certificate is rejected before credentials are requested or transmitted. The user must explicitly run `forget` and complete a new pairing to accept a changed identity.

## Server identity files

`server-daemon init` creates persistent state:

```text
.quickfs/
├── server.crt       public certificate
├── server.key       secret private key
├── users.json       usernames and Argon2id password hashes
└── pairings/        short-lived one-time pairing records
```

The certificate and private key remain on the server. Clients store only the pinned certificate fingerprint. Back up the state directory securely: losing it changes the server identity and requires all clients to pair again. Anyone obtaining `server.key` can impersonate the server.

Files containing private state are created with mode `0600` on Unix. Directory permissions remain the administrator's responsibility.

## Password storage and login limits

Passwords are entered through a hidden terminal prompt and must contain at least 12 bytes. The server stores salted PHC-format Argon2id hashes, never plaintext passwords. Verification runs in a blocking worker so memory-hard hashing does not block the async network runtime. Accounts can be enabled, disabled, deleted, and assigned a new password. Changes affect new logins; already authenticated connections are not revoked.

Each connection permits at most five failed password attempts. Reconnecting resets that connection-local counter, so network-wide rate limiting and account lockout are still required before Internet exposure. Authentication state lasts only for the current QUIC connection.

The client trust database is stored under `.quickfs-client/trusted-servers.json` by default and is written with mode `0600` on Unix. It keys pins by server address and server name.

## Commands

Initialize server identity:

```sh
server-daemon init --state-dir .quickfs --server-name files.example.net
```

Add a user:

```sh
server-daemon user add --state-dir .quickfs alice
```

Manage the account lifecycle:

```sh
server-daemon user password --state-dir .quickfs alice
server-daemon user disable --state-dir .quickfs alice
server-daemon user enable --state-dir .quickfs alice
server-daemon user delete --state-dir .quickfs alice
```

Create pairing material while the server is running or stopped:

```sh
server-daemon pair create --state-dir .quickfs --expires-seconds 300
```

Pair the client; omit `--code` to use the hidden prompt:

```sh
client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  pair --pairing-id <PAIRING_ID>
```

Authenticate and use the filesystem:

```sh
client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  --username alice list /
```

Explicitly remove a pin before a deliberate identity change:

```sh
client-cli --server 192.0.2.10:4433 --server-name files.example.net forget
```

There is no automatic certificate replacement. This prevents an unexpected key change from silently becoming trusted.

## Current limitations

- All authenticated users currently receive the same export access; per-user authorization is not implemented.
- Account recovery and immediate revocation of existing sessions are not implemented.
- Pairing codes are high-entropy text rather than short numeric codes; this avoids offline guessing weaknesses without introducing an unaudited PAKE implementation.
- QR pairing is not implemented.
- Certificate/key rotation uses explicit forget-and-re-pair rather than a rotation statement signed by the old identity.
- Server-wide authentication rate limiting and secure platform keychain storage are not implemented.
- The generated certificate is self-signed and long-lived. Pinning, not public PKI validation, supplies server authentication.

Treat this as a substantial development authentication foundation, not a completed production identity system. See the [threat model](threat-model.md) and [security policy](../SECURITY.md).
