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

### `user add`

```sh
quickfs-server-daemon user add --state-dir /var/lib/quickfs alice
```

Prompts twice for a password of at least 12 bytes and stores only a salted Argon2id hash. Usernames are 1–64 ASCII letters, digits, `.`, `_`, or `-`. Duplicate users are rejected.

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

Creates a single-use pairing ID and 160-bit secret. The default lifetime is five minutes. The running server reads pairing records from the state directory, so it does not need a restart.

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
| `--request-timeout-ms <MS>` | — | `30000` | Configured timeout; full enforcement remains incomplete. |
| `--max-concurrent-requests <COUNT>` | — | `128` | Global request concurrency bound. |

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

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--server <ADDRESS>` | `QUICKFS_SERVER` | `127.0.0.1:4433` | Server UDP address. |
| `--server-name <NAME>` | — | `localhost` | Logical identity associated with the pin. |
| `--state-dir <PATH>` | `QUICKFS_CLIENT_STATE_DIR` | `.quickfs-client` | Client trust database directory. |
| `--username <NAME>` | `QUICKFS_USERNAME` | — | Account for authenticated commands. |
| `--timeout-ms <MS>` | — | `30000` | Connection and stream-open timeout. |

The client no longer accepts `--cert` or the shared `--token`. Pairing establishes trust, and passwords are entered through a hidden prompt.

### `pair`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  pair --pairing-id <PAIRING_ID>
```

The command prompts for the code and pins the authenticated certificate fingerprint. `--code` or `QUICKFS_PAIRING_CODE` exists for automation but can expose the secret through process configuration; interactive entry is preferred.

### `forget`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  forget
```

Removes exactly that address/name pin. Use it only for a deliberate server reset or identity rotation, then pair again. The client never replaces a changed identity silently.

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

## Logging and limitations

Use `RUST_LOG=info` or `RUST_LOG=quickfs_server_daemon=debug`. Logs must not contain passwords, pairing codes, or file contents.

The implementation remains experimental and read-only. Per-user export permissions, recovery and live-session revocation, server-wide login throttling, native mounting, reconnect/retry, and production deployment hardening are incomplete. Do not expose it directly to the public Internet.
