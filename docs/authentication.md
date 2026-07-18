# Authentication and server trust

quicKFS separates server trust from user login. Unmanaged clients can use pairing-assisted trust on first use; managed deployments can instead use the operating-system trust policy, a deployed private-CA bundle, or a centrally distributed exact certificate pin. Every mode is followed by username/password authentication.

QUIC always uses TLS, so every QuickFS server needs a certificate and private
key and the client must authenticate the server during connection setup. QUIC
does **not** require a QuickFS pairing code: pairing is one way to bootstrap
trust in a self-signed certificate. Public PKI, an enterprise CA, or a managed
pin satisfies the same server-authentication requirement without per-user
administrator contact. See [RFC 9001 §4.4](https://www.rfc-editor.org/rfc/rfc9001.html#section-4.4).

## Security roles

- The server's persistent TLS identity authenticates the server to clients.
- A trust policy authenticates the server certificate before any password is requested or sent.
- Pairing and managed-pin modes store an exact certificate SHA-256 fingerprint.
- Public/enterprise PKI modes validate the certificate chain, validity, key usage, and `--server-name`.
- Username/password authentication identifies the user on every later connection.

These are separate. Establishing server trust does not log a user in, and a correct password is never sent until the selected trust policy has authenticated the server.

## Server trust modes

Choose one client trust mode for a deployment:

| Mode | Client option or command | Bootstrap and rotation behavior |
| --- | --- | --- |
| Pairing-assisted exact pin | Default; run `pair` once | Administrator transfers a one-time code to each unmanaged client trust store. Certificate changes require `forget` and a new pairing. |
| Managed exact pin | `trust import --sha256 …` or `--certificate …` | MDM/configuration management distributes the expected leaf fingerprint or certificate. Changes require a coordinated pin replacement. |
| Enterprise private CA | `--ca-cert <ROOTS.pem>` | The deployed CA bundle and DNS/IP name authenticate the server. New leaf certificates under the same CA/name rotate without client re-pairing. |
| Operating-system/public PKI | `--trust-system-roots` | The platform trust policy—including roots installed through MDM or Group Policy—and DNS/IP name authenticate the server. Normal CA renewal works without re-pairing. |

`--ca-cert` and `--trust-system-roots` are mutually exclusive. If neither is supplied, authenticated commands require an exact pin in the client trust database. CA modes deliberately do not fall back to a pin or pairing when validation fails.

## First connection with pairing

```text
Administrator                 Client                         Server
      |                          |                              |
      | create one-time pairing |                              |
      | ID and secret code      |                              |
      |------------------------->|                              |
      |                          |-- temporary TLS connection ->|
      |                          |<- certificate ----------------|
      |                          |-- nonce + client proof ------->|
      |                          |<- server proof ----------------|
      |                          |                              |
      |                          | Verify proof binds:          |
      |                          | - out-of-band code           |
      |                          | - presented certificate      |
      |                          | - fresh client nonce         |
      |                          |                              |
      |                          | Pin certificate fingerprint  |
```

The temporary pairing connection accepts the presented certificate before it is trusted, but its API is restricted to the pairing request and cannot send a username, password, or filesystem request. The client first sends a domain-separated HMAC-SHA-256 proof of code possession over the pairing ID, presented certificate fingerprint, and a fresh nonce. The server verifies that proof before consuming the record, then returns a separately domain-separated server proof over the same values. A wrong code does not consume the record, and a man-in-the-middle presenting a different certificate cannot produce or relay a matching proof.

Pairing codes contain 160 random bits, expire (five minutes by default, one hour maximum), and are deleted after a successful proof. The pairing ID identifies the server-side record but is not sufficient without the code. Transfer both through a trusted channel. Prefer entering the code at the hidden prompt instead of placing it in command-line history.

This is pairing-assisted TOFU, not plain TOFU: the out-of-band secret authenticates the first trust decision. There is currently no QR renderer; the ID and grouped code are textual.

## Later connections and managed PKI

```text
Client                                              Server
  |                                                    |
  |--- trust-policy TLS preflight -------------------->|
  |<-- persistent server certificate ------------------|
  | Verify exact pin OR certificate chain + name        |
  |--- close preflight -------------------------------->|
  |                                                    |
  | Prompt for password locally                         |
  |                                                    |
  |--- fresh QUIC/TLS connection --------------------->|
  |<-- persistent server certificate ------------------|
  | Apply the same trust policy again                    |
  |                                                    |
  |--- Authenticate { username, password } ------------>|
  |<-- AuthenticateAck or Unauthenticated -------------|
  |                                                    |
  |--- filesystem requests after authentication ------>|
```

The CLI completes a trust-policy TLS preflight before requesting a password. Because an interactive prompt can outlast a QUIC idle timeout, it then opens a fresh connection and applies the same in-memory trust policy again before transmitting the credential. An invalid certificate is therefore rejected before credentials are requested or transmitted.

Exact-pin modes accept only the configured leaf fingerprint. Pairing never replaces an existing pin, and managed import also refuses silent replacement. CA modes use normal X.509 validation and can therefore accept a renewed leaf certificate when it remains valid for the requested server name and chains to an authorized root.

## Server identity files

`quickfs-server-daemon init` creates persistent state:

```text
.quickfs/
├── server.crt          initial certificate or certificate chain
├── server.key          initial secret private key
├── users.json          usernames and Argon2id password hashes
├── pairings/           short-lived one-time pairing records
├── active-identity     optional atomic generation selector
└── identities/         validated renewal/replacement generations
```

By default, `init --server-name` generates a self-signed identity. Enterprise deployments can instead initialize from a PEM leaf-plus-intermediate chain and matching unencrypted PEM key using `init --certificate … --private-key …`. The inputs are validated as a usable QUIC/TLS identity and copied into protected server state.

`identity install` performs the same validation for renewal, writes a new private generation, and atomically changes `active-identity`; restart the daemon to present it. Existing generations remain protected for administrative recovery. CA-trusting clients accept a normal renewal automatically when its name and chain remain valid. Exact-pin clients require coordinated trust replacement.

The certificate and private key remain on the server. Pairing/managed-pin clients store only leaf fingerprints; CA clients use centrally provisioned roots. Back up the state directory securely. Anyone obtaining an active private key can impersonate that server to clients whose trust policy accepts its certificate.

State files are created with mode `0600` and state directories with mode `0700` on Unix. The daemon rejects symlinked, non-regular, incorrectly owned, or group/other-accessible state, including the identity certificate whose integrity must also be protected. It also refuses to start when the state and export directories overlap in either direction, which prevents exposing the private key, password database, or pairing records to authenticated filesystem clients.

## Password storage and login limits

Passwords are entered through a hidden terminal prompt and must contain 12–1024 bytes. The server stores salted PHC-format Argon2id hashes, never plaintext passwords. Verification runs in a globally bounded blocking worker pool so memory-hard hashing does not block or exhaust the async network runtime. Accounts can be enabled, disabled, deleted, and assigned a new password. Changes affect new logins; already authenticated connections are not revoked.

Each connection serializes login attempts and permits at most five failures, including concurrent streams. Across reconnects, each source IP is limited to 30 attempts per rolling minute by default, and at most four Argon2 jobs run globally by default. Both limits are configurable. Distributed guessing and account-specific lockout remain outside this prototype. Authentication state lasts only for the current QUIC connection.

The client trust database is stored under `.quickfs-client/trusted-servers.json` by default, with directory mode `0700` and file mode `0600` on Unix. The client rejects symlinked, incorrectly owned, or overly permissive trust state. It keys pins by server address and server name.

## Commands

Initialize server identity:

```sh
quickfs-server-daemon init --state-dir .quickfs --server-name files.example.net
```

Initialize with an enterprise/public-CA identity instead:

```sh
quickfs-server-daemon init --state-dir .quickfs \
  --certificate server-fullchain.pem --private-key server-key.pem
```

Install a renewed identity and restart the daemon:

```sh
quickfs-server-daemon identity install --state-dir .quickfs \
  --certificate renewed-fullchain.pem --private-key renewed-key.pem
```

Add a user:

```sh
quickfs-server-daemon user add --state-dir .quickfs alice
```

Manage the account lifecycle:

```sh
quickfs-server-daemon user password --state-dir .quickfs alice
quickfs-server-daemon user disable --state-dir .quickfs alice
quickfs-server-daemon user enable --state-dir .quickfs alice
quickfs-server-daemon user delete --state-dir .quickfs alice
```

Create pairing material while the server is running or stopped:

```sh
quickfs-server-daemon pair create --state-dir .quickfs --expires-seconds 300
```

Pair the client; omit `--code` to use the hidden prompt:

```sh
quickfs-client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  pair --pairing-id <PAIRING_ID>
```

Authenticate and use the filesystem:

```sh
quickfs-client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  --username alice list /
```

Use a deployed private CA or operating-system trust policy without pairing:

```sh
quickfs-client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  --ca-cert /etc/quickfs/organization-roots.pem --username alice list /

quickfs-client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  --trust-system-roots --username alice list /
```

Import a centrally distributed exact pin without contacting the server:

```sh
quickfs-client-cli --server 192.0.2.10:4433 --server-name files.example.net \
  trust import --sha256 <64_HEX_DIGITS>
```

Explicitly remove a pin before a deliberate identity change:

```sh
quickfs-client-cli --server 192.0.2.10:4433 --server-name files.example.net forget
```

There is no automatic certificate replacement. This prevents an unexpected key change from silently becoming trusted.

## Current limitations

- All authenticated users currently receive the same export access; per-user authorization is not implemented.
- Account recovery and immediate revocation of existing sessions are not implemented.
- Pairing codes are high-entropy text rather than short numeric codes; this avoids offline guessing weaknesses without introducing an unaudited PAKE implementation.
- QR pairing is not implemented.
- Exact pins do not yet support an overlapping old/new pin window; coordinate `forget` plus managed import/pairing with an identity change. CA modes support ordinary leaf renewal.
- Source-based throttling does not stop distributed password guessing, and secure platform keychain storage is not implemented.
- Self-signed identities require pairing or managed pins. PKI modes require administrators to secure CA issuance, root distribution, server names, and revocation policy.

Treat this as a substantial development authentication foundation, not a completed production identity system. See the [threat model](threat-model.md) and [security policy](../SECURITY.md).
