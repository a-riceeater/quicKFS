# Setup guide

This guide creates a local server, user account, trusted client, and read-only session. The local walkthrough uses pairing; a later section configures public/enterprise PKI or managed pins without per-client administrator contact. Linux is the initial server target; the daemon also runs on macOS for development.

## How pairing and login fit together

For the default unmanaged-client flow, pairing and login are separate operations:

1. The administrator initializes one persistent server TLS identity and creates a user account.
2. For each new client installation, the administrator creates a short-lived, one-time pairing ID and code.
3. The client uses that code to authenticate the certificate presented by the server, then stores only its SHA-256 fingerprint. Pairing does not send a username or password and does not log the user in.
4. On later commands, the client verifies the stored fingerprint before prompting for a password. It verifies the pin again on a fresh connection after the prompt, then sends the username and password over that pinned TLS connection.
5. Successful login authorizes filesystem operations for that QUIC connection only. A later CLI invocation logs in again but does not pair again.

The state created by these operations is deliberately split:

| Location | Contents | Keep it? |
| --- | --- | --- |
| Server `--state-dir` | TLS certificate/private key, Argon2id user hashes, unused pairing records | Persist and back up securely; never export or copy it to clients. |
| Client `--state-dir` | Exact server pins established through pairing or managed import | Persist when using exact pins; CA modes use their deployed roots instead. |
| Export root | Files visible to every enabled authenticated user | Keep disjoint from the server state directory. |

Use the same client `--server` and `--server-name` values during pairing and later commands. For local development their defaults are `127.0.0.1:4433` and `localhost`. For a remote setup, repeat both values on every client command; `QUICKFS_SERVER` can supply the address, while `--server-name` remains explicit. Persist the client state directory rather than pairing anew for each invocation. In CA modes, `--server-name` must match a DNS name or IP address in the certificate.

## Prerequisites

Install Git and Rust/Cargo 1.85 or newer. Rust 1.85 is the first stable release supporting Rust 2024. The repository's `rust-toolchain.toml` selects current stable through rustup.

On Linux, distribution Cargo packages may be too old:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustc --version
cargo --version
```

Both versions must be at least 1.85. `command -v cargo` should normally report `$HOME/.cargo/bin/cargo`. macFUSE is not required; native mounting is not implemented yet.

## Build

```sh
git clone https://github.com/quickfs/quickfs.git
cd quickfs
cargo build --workspace
```

The debug binaries are `target/debug/quickfs-server-daemon` and `target/debug/quickfs-client-cli`.

## Create an export

```sh
mkdir -p shared/examples
printf 'Hello from quicKFS\n' > shared/hello.txt
printf 'Nested file\n' > shared/examples/nested.txt
```

The server exposes this directory as remote `/`. It does not create a missing export automatically.

## Initialize persistent server identity

```sh
cargo run -p quickfs-server-daemon -- init \
  --state-dir .quickfs \
  --server-name localhost
```

This creates a self-signed TLS identity, an empty user database, and pairing storage. On Unix, private directories use mode `0700` and private files use `0600`. It replaces the old manual `dev-cert.sh` workflow for normal use. Do not copy the private server state to clients, and keep the state and export directories in disjoint directory trees.

For a remote server, choose its stable DNS name or IP address during initialization:

```sh
quickfs-server-daemon init \
  --state-dir /var/lib/quickfs \
  --server-name files.example.net
```

## Add a user

```sh
cargo run -p quickfs-server-daemon -- user add \
  --state-dir .quickfs \
  alice
```

Enter and confirm a password of at least 12 bytes at the hidden prompts. The server stores a salted Argon2id hash.

## Start the server

```sh
RUST_LOG=info cargo run -p quickfs-server-daemon -- serve \
  --bind 127.0.0.1:4433 \
  --export-root ./shared \
  --state-dir .quickfs
```

Use `--bind 0.0.0.0:4433` to accept remote connections, and allow UDP port 4433 through the relevant firewall. Keep the server running. Ctrl+C and SIGTERM initiate graceful shutdown.

## Create a one-time pairing

In another server terminal:

```sh
cargo run -p quickfs-server-daemon -- pair create \
  --state-dir .quickfs \
  --expires-seconds 300
```

The command prints a pairing ID and grouped 160-bit code. Transfer both to the client through a trusted channel. The default expiry is five minutes, the maximum is one hour, and the record is deleted only after a client successfully proves knowledge of the code. A mistyped but well-formed code does not consume it.

## Pair the client

```sh
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  pair --pairing-id <PAIRING_ID>
```

Enter the pairing code at the hidden prompt. Successful pairing stores the server certificate fingerprint in `.quickfs-client/trusted-servers.json`; no certificate file is copied.

Pair once for each client state directory. If the server later presents a different certificate, the client stops before requesting a password. Investigate the change; only use `forget` followed by a new pairing when the identity replacement was deliberate.

## Log in and use the client

```sh
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --username alice \
  ping
```

Enter the account password when prompted. Then browse and read:

```sh
cargo run -p quickfs-client-cli -- --username alice list /
cargo run -p quickfs-client-cli -- --username alice stat /hello.txt
cargo run -p quickfs-client-cli -- --username alice \
  read /hello.txt --offset 0 --length 4096
```

The address and server name default to local development values. The client refuses an unexpected server certificate before prompting for a password.

The account password is distinct from the one-time pairing code. It is checked on every new connection against the Argon2id hash stored by the server. Pairing codes are not reusable login credentials, and pairing never creates an account.

## Enterprise setup without per-client pairing

Large deployments should normally authenticate the server through centrally managed PKI. Obtain an unencrypted PEM private key and a PEM certificate file containing the leaf certificate first, followed by any intermediate certificates. The leaf must be valid for the stable DNS name or IP address clients pass as `--server-name`.

Initialize new server state directly from that identity:

```sh
quickfs-server-daemon init \
  --state-dir /var/lib/quickfs \
  --certificate /run/secrets/quickfs-fullchain.pem \
  --private-key /run/secrets/quickfs-key.pem
```

QuickFS parses the chain, verifies that the leaf and private key match and are
usable for QUIC/TLS, then copies them into protected state. Clients remain the
authority for certificate lifetime, name, and CA-chain validation. Continue
with `user add` and `serve` as above. The source key is outside QuickFS state:
protect its permissions and remove or expire the staging copy according to the
organization's secret-delivery policy.

Choose one client trust mode:

### Operating-system or public PKI

Use this when the issuing root is already trusted by the operating system,
including enterprise roots deployed through MDM or
[Windows Group Policy](https://learn.microsoft.com/en-us/windows-server/identity/ad-cs/distribute-certificates-group-policy):

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --trust-system-roots \
  --username alice ping
```

`QUICKFS_TRUST_SYSTEM_ROOTS=true` enables the same mode for managed configuration. No QuickFS pairing or client pin database is required.

### Explicit enterprise CA bundle

Use this when the organization distributes a dedicated root bundle without installing it into the platform store:

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --ca-cert /etc/quickfs/organization-roots.pem \
  --username alice ping
```

The bundle is loaded once before the password prompt and held in memory for the post-prompt connection. `QUICKFS_CA_CERT` can supply its path. QuickFS performs normal chain, validity, key-usage, and server-name validation; it does not fall back to pairing if validation fails.

### Centrally distributed exact pin

MDM or configuration management can provision the expected leaf certificate or its SHA-256 fingerprint without contacting the running server:

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  trust import --certificate files.example.net-fullchain.pem

quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  trust import --sha256 <64_HEX_DIGITS>
```

Only the first certificate in a PEM chain is pinned. Import refuses to replace an existing pin; distribute the input over an authenticated management channel. Later commands omit CA flags and use the managed pin from the client state directory.

## Renew or replace an enterprise identity

Validate and atomically select a new certificate generation:

```sh
quickfs-server-daemon identity install \
  --state-dir /var/lib/quickfs \
  --certificate /run/secrets/renewed-fullchain.pem \
  --private-key /run/secrets/renewed-key.pem
```

The currently running daemon keeps its loaded identity. Restart it to present the new generation. Clients using the system store or enterprise CA accept a normal renewal automatically when the new certificate remains valid for the same name and chains to an authorized root. Exact-pin deployments must coordinate `forget` plus managed import or a new pairing with the server change.

## Run development checks

```sh
./scripts/check.sh
```

See [usage](usage.md), [authentication](authentication.md), and [troubleshooting](troubleshooting.md) for more detail.
