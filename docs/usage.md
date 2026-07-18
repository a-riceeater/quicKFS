# Usage and command reference

The current tools are a server daemon and diagnostic read-only client. Run each command with `--help` for generated help.

## Server administration

### `init`

Creates a persistent self-signed server identity, empty user database, and pairing directory:

```sh
quickfs-server-daemon init \
  --state-dir /var/lib/quickfs \
  --server-name files.example.net
```

`--state-dir` defaults to `.quickfs`. Supply `--server-name` more than once when the identity needs multiple names. Initialization refuses to overwrite existing identity files.

To use a certificate issued by a public or enterprise CA, replace `--server-name` with both external identity inputs:

```sh
quickfs-server-daemon init \
  --state-dir /var/lib/quickfs \
  --certificate server-fullchain.pem \
  --private-key server-key.pem
```

The certificate file must contain the leaf first and then any intermediates. The unencrypted PEM key must match the leaf. QuickFS parses the chain, verifies the leaf/key match and TLS usability, and copies both into private server state. The client performs lifetime, requested-name, and CA-chain validation when it connects. Protect and retire the source key file separately; copying it does not change the source file's permissions or lifecycle.

### `user add`

```sh
quickfs-server-daemon user add --state-dir /var/lib/quickfs alice
```

Prompts twice for a password of 12–1024 bytes and stores only a salted Argon2id hash. Usernames are 1–64 ASCII letters, digits, `.`, `_`, or `-`. Duplicate users are rejected.

Other lifecycle commands are:

```sh
quickfs-server-daemon user password --state-dir /var/lib/quickfs alice
quickfs-server-daemon user disable --state-dir /var/lib/quickfs alice
quickfs-server-daemon user enable --state-dir /var/lib/quickfs alice
quickfs-server-daemon user delete --state-dir /var/lib/quickfs alice
```

Password changes prompt twice. Disable and delete prevent new logins but do not terminate connections that are already authenticated.

### `pair create`

```sh
quickfs-server-daemon pair create \
  --state-dir /var/lib/quickfs \
  --expires-seconds 300
```

Creates a single-use pairing ID and 160-bit secret. The default lifetime is five minutes and the maximum is one hour. The running server reads pairing records from the state directory, so it does not need a restart.

### `identity install`

```sh
quickfs-server-daemon identity install \
  --state-dir /var/lib/quickfs \
  --certificate renewed-fullchain.pem \
  --private-key renewed-key.pem
```

Validates the replacement chain/key, writes a new protected identity generation, and atomically selects it. Restart the daemon to load it. CA-trusting clients can accept ordinary leaf renewal; exact-pin clients require a coordinated pin update or new pairing.

## Server `serve`

```sh
quickfs-server-daemon serve [OPTIONS] --export-root <PATH>
```

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--bind <ADDRESS>` | `QUICKFS_BIND` | `0.0.0.0:4433` | UDP address on which QUIC listens. |
| `--export-root <PATH>` | `QUICKFS_EXPORT_ROOT` | Required | Directory exposed as remote `/`. |
| `--state-dir <PATH>` | `QUICKFS_STATE_DIR` | `.quickfs` | Identity, accounts, and pairing state. |
| `--max-read-size <BYTES>` | — | `8388608` | Largest permitted ranged read. |
| `--max-open-handles <COUNT>` | — | `1024` | Maximum tracked open files. |
| `--max-known-nodes-per-connection <COUNT>` | — | `8192` | Maximum node IDs retained by one connection, including the root. |
| `--max-total-known-nodes <COUNT>` | — | `65536` | Global budget for retained non-root node IDs across connections. |
| `--request-timeout-ms <MS>` | — | `30000` | Full request timeout, including frame I/O and filesystem work. |
| `--max-concurrent-requests <COUNT>` | — | `128` | Global request concurrency bound. |
| `--max-in-flight-read-bytes <BYTES>` | — | `67108864` | Global memory budget reserved by concurrent raw reads. |
| `--max-concurrent-connections <COUNT>` | — | `256` | Global accepted-connection bound. |
| `--max-concurrent-auth <COUNT>` | — | `4` | Global Argon2 worker bound. |
| `--auth-attempts-per-minute <COUNT>` | — | `30` | Rolling login-attempt limit per source IP (maximum `1000`). |

Example:

```sh
RUST_LOG=info quickfs-server-daemon serve \
  --bind 0.0.0.0:4433 \
  --export-root /srv/project-share \
  --state-dir /var/lib/quickfs
```

## Client options

```text
quickfs-client-cli [OPTIONS] <COMMAND>
```

On macOS, every client subcommand first verifies that the standard macFUSE filesystem bundle is installed. If it is missing, the command exits before pairing, trust, authentication, or filesystem work begins and prints the [official installation URL](https://macfuse.io/). Help and version output remain available, and the runtime preflight is macOS-only.

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--server <ADDRESS>` | `QUICKFS_SERVER` | `127.0.0.1:4433` | Server UDP address. |
| `--server-name <NAME>` | — | `localhost` | Logical identity associated with the pin. |
| `--state-dir <PATH>` | `QUICKFS_CLIENT_STATE_DIR` | `.quickfs-client` | Client trust database directory. |
| `--username <NAME>` | `QUICKFS_USERNAME` | — | Account for authenticated commands. |
| `--timeout-ms <MS>` | — | `30000` | Connection, stream-open, frame, and response-data timeout. |
| `--trust-system-roots` | `QUICKFS_TRUST_SYSTEM_ROOTS` | Off | Validate with the operating-system public/managed trust policy and `--server-name`. |
| `--ca-cert <PEM>` | `QUICKFS_CA_CERT` | — | Validate with a deployed private-CA bundle and `--server-name`. |

The client no longer accepts `--cert` or the shared `--token`. Without a CA option it requires an exact pin established through pairing or managed import. Passwords are entered through a hidden prompt. `--trust-system-roots` and `--ca-cert` are mutually exclusive and apply to authenticated commands, not `pair`, `forget`, or `trust import`.

### `pair`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  pair --pairing-id <PAIRING_ID>
```

The command prompts for the code and pins the authenticated certificate fingerprint. A client proof is verified before the server consumes the pairing, so a wrong code can be corrected using the same pairing record. Pairing is refused when a pin already exists; use `forget` only after investigating and deliberately authorizing a server identity change. `--code` or `QUICKFS_PAIRING_CODE` exists for automation but can expose the secret through process configuration; interactive entry is preferred.

### `forget`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  forget
```

Removes exactly that address/name pin. Use it only for a deliberate server reset or identity rotation, then pair again. The client never replaces a changed identity silently.

### `trust import`

Provision an exact pin through an authenticated enterprise-management channel without contacting the server:

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  trust import --sha256 <64_HEX_DIGITS>

quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  trust import --certificate server-fullchain.pem
```

Colon-separated fingerprints are accepted. With a PEM chain, the first certificate is pinned. Import uses the same locked, private trust database as pairing and refuses to replace an existing address/name pin. Later authenticated commands use that pin when neither CA option is supplied.

### `ping`

```sh
quickfs-client-cli --username alice ping
```

Authenticates and prints `pong 42` on success.

### `list`

```sh
quickfs-client-cli --username alice list /
quickfs-client-cli --username alice list /examples
```

Paths are resolved client-side by walking opaque node IDs from directory listings.

### `stat`

```sh
quickfs-client-cli --username alice stat /hello.txt
```

Prints node ID, kind, size, revision, and modification time. Revisions are change indicators, not globally ordered versions.

### `read`

```sh
quickfs-client-cli --username alice \
  read /hello.txt --offset 0 --length 4096

quickfs-client-cli --username alice \
  read /archive.bin --offset 1048576 --length 65536 > block.bin
```

`--length` is required. Offset defaults to zero. Reads beyond EOF return available bytes, zero-length reads succeed, and output is raw file content.

## macOS `quickfs-mount`

The native mount is built separately because it links against the macFUSE 4 SDK:

```sh
cargo build -p quickfs-filesystem-macfuse --features macfuse --bin quickfs-mount
mkdir -p "$HOME/Volumes/quickfs"
target/debug/quickfs-mount "$HOME/Volumes/quickfs" \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --state-dir .quickfs-client \
  --username alice
```

The positional mountpoint must already be a directory. The process verifies the selected server trust policy before asking for the password, reconnects under that same policy, authenticates once, and keeps one `RemoteFilesystem` connection and one Tokio runtime for the mount lifetime. Leave it running while the volume is in use; Finder can browse directories and open/read files. Unmount from another terminal:

```sh
diskutil unmount "$HOME/Volumes/quickfs"
```

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--server <ADDRESS>` | `QUICKFS_SERVER` | `127.0.0.1:4433` | Server UDP address. |
| `--server-name <NAME>` | — | `localhost` | Logical TLS identity and exact-pin key. |
| `--state-dir <PATH>` | `QUICKFS_CLIENT_STATE_DIR` | `.quickfs-client` | Existing exact-pin trust database. |
| `--username <NAME>` | `QUICKFS_USERNAME` | Required | Account used to authenticate the retained session. |
| `--timeout-ms <MS>` | — | `30000` | Connection and transport-operation timeout. |
| `--callback-timeout-ms <MS>` | — | `30000` | Maximum time a macFUSE callback waits for its remote operation. |
| `--trust-system-roots` | `QUICKFS_TRUST_SYSTEM_ROOTS` | Off | Use the operating-system public/managed roots. |
| `--ca-cert <PEM>` | `QUICKFS_CA_CERT` | — | Use an explicit enterprise-CA bundle. |
| `--volume-name <NAME>` | — | `quicKFS` | Volume label shown by macOS. |

Without a CA option, the mount reads the same private exact-pin database created by `quickfs-client-cli pair` or `trust import`. `--trust-system-roots` and `--ca-cert` are mutually exclusive. The mount is intentionally read-only: directories are `0555`, files are `0444`, and macOS write/xattr sidecars are disabled.

## Logging and limitations

Use `RUST_LOG=info` or `RUST_LOG=quickfs_server_daemon=debug`. Logs must not contain passwords, pairing codes, or file contents.

The implementation remains experimental and read-only. Per-user export permissions, recovery and live-session revocation, distributed-login defense, reconnect/retry, installed-macFUSE integration testing, and production deployment hardening are incomplete. A lost QUIC session currently requires unmounting and starting `quickfs-mount` again. Do not expose it directly to the public Internet.
