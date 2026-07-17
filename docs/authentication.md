# Authentication and server trust

quicKFS currently uses two separate mechanisms:

1. TLS certificate verification authenticates the server to the client.
2. A shared development token authenticates the client to the server.

These mechanisms answer different questions. A valid token alone does not prove that the client reached the intended server, and a valid server certificate does not grant a client permission to browse files.

## Current connection flow

```text
Client                                              Server
  |                                                    |
  |--- QUIC/TLS connection request ------------------->|
  |<-- server certificate and TLS handshake -----------|
  |                                                    |
  | Verify certificate against --cert                  |
  | Verify --server-name appears in the certificate    |
  | Establish encrypted QUIC connection                |
  |                                                    |
  |--- Authenticate { development token } ------------>|
  |<-- AuthenticateAck or Unauthenticated -------------|
  |                                                    |
  |--- Filesystem requests after authentication ------>|
```

QUIC uses TLS 1.3 for encryption and peer authentication. The client must authenticate the server before trusting the connection. In the current prototype, the client creates a trust store containing the single certificate supplied through `--cert`.

The authentication token is sent only after the TLS-protected QUIC connection has been established. It must not be sent to a server whose certificate failed validation.

## Certificate and private key

Development certificate generation creates two files:

```text
certs/server.crt
certs/server.key
```

Their handling requirements differ:

| File | Purpose | Location | Secret? |
| --- | --- | --- | --- |
| `server.crt` | Contains the server identity and public key. Clients use it as an explicit trust anchor. | Server and every client that connects to it. | No. It may be distributed to clients. |
| `server.key` | Proves that the server controls the identity represented by the certificate. | Server only. | Yes. Never copy it to clients or commit it. |

The server loads both files:

```sh
quickfs-server-daemon serve \
  --cert ./certs/server.crt \
  --key ./certs/server.key \
  ...
```

The client receives only the public certificate:

```sh
quickfs-client-cli \
  --cert ./server.crt \
  --server-name localhost \
  ...
```

Possession of `server.crt` does not let somebody impersonate the server. Impersonation requires the corresponding private key. The certificate still needs an authenticated distribution channel: if an attacker can replace the certificate before it reaches the client, the client could be configured to trust the attacker instead.

## Certificate identity and server address

`--server` and `--server-name` serve different purposes:

- `--server` is the IP address and UDP port used to reach the machine.
- `--server-name` is the identity expected in the certificate.

For local development:

```sh
--server 127.0.0.1:4433 --server-name localhost
```

For a remote server, the certificate must contain the selected DNS name or IP address in its subject alternative names. A certificate created only for `localhost` does not correctly identify a server reached by its LAN address.

Normal quicKFS code does not offer an option to disable certificate verification.

## Development token

The server is started with one shared token:

```sh
--token development-token
```

The client supplies the same value:

```sh
--token development-token
```

After a successful `Authenticate` request, filesystem operations on that QUIC connection are allowed. An incorrect token produces an unauthenticated protocol error. Authentication state applies to the connection; opening a new connection requires authentication again.

The token is an experimental placeholder with important limitations:

- It represents all clients as the same identity.
- There are no usernames, per-user permissions, roles, expiration, revocation, or password changes.
- The server currently receives the original token and compares it directly.
- There is no credential database or password hashing scheme.
- A copied token grants the same access as the original token holder.

Do not reuse an account password as the development token. Use a randomly generated development value and protect it like a password. Avoid command-line tokens on shared systems because process listings and shell history may expose command arguments; environment variables reduce command-line exposure but are not a complete secret-management solution.

## What each failure means

| Failure | Meaning |
| --- | --- |
| Certificate is not trusted | The client cannot establish that the presented server belongs to an accepted trust anchor. |
| Server name mismatch | The certificate does not identify the name the client expected. |
| TLS handshake failure | Encryption parameters, certificate validation, keys, or protocol negotiation failed. |
| `Unauthenticated` after TLS connects | The server was authenticated, but the client token was absent or incorrect. |
| `PermissionDenied` after authentication | Authentication succeeded, but the requested filesystem operation was rejected. |

Do not work around certificate failures by disabling verification. Fix the certificate identity, trust source, clock, or selected server name.

## Current security boundary

The prototype assumes:

- clients obtained the correct public server certificate through a trusted channel;
- the server private key remains secret;
- the development token remains secret;
- the server host and exported files are trusted;
- the server is used on a private network rather than exposed directly to the public Internet.

The current system is suitable for development and protocol testing, not production identity or authorization. See the [threat model](threat-model.md) and [security policy](../SECURITY.md) for the broader limitations.

