# Usage and command reference

The current tools are a server daemon, a diagnostic client, a native macFUSE mount, and a native Windows mount. Run each command with `--help` for generated help.

## Configuration file

The Linux daemon reads a TOML configuration. The system path is
`/etc/quickfs/quickfs.toml`; development invocations name another with
`--config`:

```sh
quickfs-server-daemon serve --config ./quickfs.toml
```

`serve` uses `/etc/quickfs/quickfs.toml` when it exists and no `--config` is
given, and runs from command-line options alone when it does not. A file named
explicitly with `--config` or `QUICKFS_CONFIG` must exist.

### Precedence

```text
explicit CLI option > environment variable > TOML value > built-in default
```

No `serve` option carries a Clap default, so leaving one out really does defer
to the file and then to the built-in default rather than pinning it to a value
nobody chose. `--allow-writes` alone means true and `--allow-writes=false` turns
it off again, so the command line can override a configured `allow_writes` in
either direction.

`config print-effective` shows exactly what an invocation resolves to.

### Schema

Unknown keys and unknown sections are **errors**. A mistyped `allow_writess`
fails validation rather than silently leaving the built-in default in place.
Paths in the file must be absolute: a relative path in a system service resolves
against whatever working directory the supervisor happened to use. Values passed
on the command line may still be relative.

```toml
[server]
bind = "0.0.0.0:4433"
export_root = "/srv/quickfs/export"
state_dir = "/var/lib/quickfs"
allow_writes = false

[limits]
max_read_size = 16777216
max_write_size = 8388608
max_open_handles = 1024
max_known_nodes_per_connection = 131072
max_total_known_nodes = 524288
max_directory_entry_tasks = 64
request_timeout_ms = 120000
max_concurrent_requests = 128
max_concurrent_requests_per_connection = 64
max_in_flight_read_bytes = 134217728
max_in_flight_write_bytes = 67108864
max_in_flight_write_bytes_per_connection = 16777216
max_concurrent_connections = 256
max_concurrent_auth = 4
auth_attempts_per_minute = 30

[logging]
filter = "info"
```

Every key is optional and every value shown is the built-in default, except
`server.export_root`, which has none and must come from the file, `--export-root`
or `QUICKFS_EXPORT_ROOT`. The meaning of each limit is the same as the matching
`serve` option in the table below. `packaging/quickfs.toml.example` is the same
schema with a paragraph of rationale per key.

`logging.filter` sets the level when `RUST_LOG` is unset; an explicit `RUST_LOG`
wins, so an operator can debug a running service without editing the file. An
unparseable filter fails validation instead of being silently discarded.

The configuration holds deployment paths and policy only. It never holds
passwords, pairing codes, private keys, or certificate material — those live in
the protected state directory.

### `config validate`

```sh
quickfs-server-daemon config validate --config /etc/quickfs/quickfs.toml
quickfs-server-daemon config validate --config /etc/quickfs/quickfs.toml --require-identity
```

Parses the whole file, rejects unknown fields, checks the bind address syntax,
requires an absolute export root that exists and is a directory, rejects any
nesting between the state and export directories in either direction, rejects
zero or contradictory limits, validates the log filter, and checks state
permissions when the state directory exists. It mutates nothing and prints no
secrets. A nonzero exit means the configuration is unusable.

`--require-identity` additionally demands initialized server state. The systemd
unit passes it from `ExecStartPre`, so a service whose identity was never
created fails with that message instead of a TLS load error.

### `config print-effective`

```sh
quickfs-server-daemon config print-effective --config /etc/quickfs/quickfs.toml
```

Prints the fully resolved configuration as TOML that could be written back,
after the same validation. It mutates nothing, exits nonzero for an invalid
configuration, and redacts secret-bearing values. No configuration field carries
a secret today; the redaction path exists so one added later cannot leak through
output an administrator pastes into a bug report.

### Rejected configuration files

For the system-service path the daemon refuses a configuration file that is a
symbolic link, is not a regular file, is writable by group or other users, or
sits in a directory other users can write without the sticky bit. Anything able
to rewrite the configuration can point `export_root` at `/` or move `state_dir`
somewhere it controls, so this is a privilege boundary. The file is stat'ed,
opened, and re-stat'ed through the open descriptor with device and inode
compared, which closes the ordinary check/use race between those steps.

## Server administration

### `init`

Creates a persistent self-signed server identity, empty user database, and pairing directory:

```sh
quickfs-server-daemon init \
  --state-dir /var/lib/quickfs \
  --server-name files.example.net
```

`--state-dir` defaults to `.quickfs`. Supply `--server-name` more than once when the identity needs multiple names. Initialization refuses to overwrite existing identity files.

`init`, `user`, `pair`, and `identity` all accept `--config <PATH>` instead, so
a deployment does not have to repeat its state directory:

```sh
sudo -u quickfs quickfs-server-daemon init \
  --config /etc/quickfs/quickfs.toml --server-name files.example.net
```

An explicit `--state-dir` (or `QUICKFS_STATE_DIR`) still overrides the
configured value, which is what a staging or recovery directory needs. Run these
as the service account, not as root: state created by root would be owned by the
wrong user and the daemon would refuse it.

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
quickfs-server-daemon user grant-write --state-dir /var/lib/quickfs alice
quickfs-server-daemon user revoke-write --state-dir /var/lib/quickfs alice
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
quickfs-server-daemon serve --config /etc/quickfs/quickfs.toml
quickfs-server-daemon serve [OPTIONS] --export-root <PATH>
```

The "Default" column is the built-in default, used when neither the command
line, the environment, nor the configuration file supplies a value. The TOML key
column gives where the same setting lives in `quickfs.toml`.

| Option | Environment variable | TOML key | Default | Purpose |
| --- | --- | --- | --- | --- |
| `--config <PATH>` | `QUICKFS_CONFIG` | — | `/etc/quickfs/quickfs.toml` if present | Configuration file. An explicitly named file must exist. |
| `--bind <ADDRESS>` | `QUICKFS_BIND` | `server.bind` | `0.0.0.0:4433` | UDP address on which QUIC listens. |
| `--export-root <PATH>` | `QUICKFS_EXPORT_ROOT` | `server.export_root` | Required | Directory exposed as remote `/`. Must exist and be a directory; absolute in a configuration file. |
| `--state-dir <PATH>` | `QUICKFS_STATE_DIR` | `server.state_dir` | `.quickfs` | Identity, accounts, and pairing state. Must not nest with the export root in either direction. |
| `--allow-writes[=<BOOL>]` | `QUICKFS_ALLOW_WRITES` | `server.allow_writes` | `false` | Export-wide write gate; an account write grant is also required. |
| `--max-read-size <BYTES>` | — | `limits.max_read_size` | `16777216` | Largest permitted ranged read. |
| `--max-write-size <BYTES>` | — | `limits.max_write_size` | `8388608` | Largest raw write/xattr payload. |
| `--max-open-handles <COUNT>` | — | `limits.max_open_handles` | `1024` | Maximum tracked open files. |
| `--max-known-nodes-per-connection <COUNT>` | — | `limits.max_known_nodes_per_connection` | `131072` | Live working set of inodes one connection may reference, including the root. Not a file count. |
| `--max-total-known-nodes <COUNT>` | — | `limits.max_total_known_nodes` | `524288` | Global budget for retained non-root node IDs across connections. |
| `--max-directory-entry-tasks <COUNT>` | — | `limits.max_directory_entry_tasks` | `64` | Global worker bound for concurrent child metadata/xattr discovery. |
| `--request-timeout-ms <MS>` | — | `limits.request_timeout_ms` | `120000` | Full request timeout, including cold backing-store work and raw payload transfer. Keep it comfortably above the mount transport-phase timeout. |
| `--max-concurrent-requests <COUNT>` | — | `limits.max_concurrent_requests` | `128` | Global request concurrency bound. |
| `--max-concurrent-requests-per-connection <COUNT>` | — | `limits.max_concurrent_requests_per_connection` | `64` | Request tasks one connection may occupy, so one client cannot hold every global slot. |
| `--max-in-flight-read-bytes <BYTES>` | — | `limits.max_in_flight_read_bytes` | `134217728` | Global memory budget reserved by concurrent raw reads. |
| `--max-in-flight-write-bytes <BYTES>` | — | `limits.max_in_flight_write_bytes` | `67108864` | Global raw-write memory budget. |
| `--max-in-flight-write-bytes-per-connection <BYTES>` | — | `limits.max_in_flight_write_bytes_per_connection` | `16777216` | Per-connection raw-write memory budget. |
| `--max-concurrent-connections <COUNT>` | — | `limits.max_concurrent_connections` | `256` | Global accepted-connection bound. |
| `--max-concurrent-auth <COUNT>` | — | `limits.max_concurrent_auth` | `4` | Global Argon2 worker bound. |
| `--auth-attempts-per-minute <COUNT>` | — | `limits.auth_attempts_per_minute` | `30` | Rolling login-attempt limit per source IP (maximum `1000`). |
| — | `RUST_LOG` | `logging.filter` | `info` in the packaged example | Tracing filter. `RUST_LOG` overrides the configured value. |

Example, from a configuration file:

```sh
quickfs-server-daemon serve --config /etc/quickfs/quickfs.toml
```

Example, from the command line during development:

```sh
RUST_LOG=info quickfs-server-daemon serve \
  --bind 0.0.0.0:4433 \
  --export-root /srv/project-share \
  --state-dir /var/lib/quickfs \
  --allow-writes
```

## Running as a systemd service

```sh
sudo systemctl enable --now quickfs-server
sudo systemctl start quickfs-server
sudo systemctl stop quickfs-server
sudo systemctl restart quickfs-server
systemctl status quickfs-server
journalctl -u quickfs-server -f
journalctl -u quickfs-server --since '1 hour ago'
```

The unit runs
`/usr/local/bin/quickfs-server-daemon serve --config /etc/quickfs/quickfs.toml`
as `quickfs:quickfs` with `UMask=0077`, logs to the journal, restarts on
unexpected failure with a bounded delay and start limit, and shuts down cleanly
on SIGTERM. `ExecStartPre` runs `config validate --require-identity`, so the
service refuses to start when the configuration is missing or invalid, the
export root does not exist, state and export overlap, private state permissions
are unsafe, or the identity was never initialized.

Special-node creation that needs elevated capabilities is unavailable in the
default service, because `CapabilityBoundingSet` is empty. The hardening
choices, the four directives deliberately omitted, and the drop-in an expert
would write to restore `CAP_MKNOD` — along with what that costs — are documented
in [servers/linux/README.md](../servers/linux/README.md#systemd-hardening).

Installation, upgrade, and uninstall are covered in the
[setup guide](setup.md#production-linux-installation).

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
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --username alice ping
```

Authenticates and prints `pong 42` on success.

### `list`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --username alice list /
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --username alice list /examples
```

Paths are resolved client-side by walking opaque node IDs from directory listings.

### `stat`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --username alice stat /hello.txt
```

Prints node ID, kind, size, revision, and modification time. Revisions are change indicators, not globally ordered versions.

### `read`

```sh
quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --username alice \
  read /hello.txt --offset 0 --length 4096

quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --username alice \
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

The positional mountpoint must already be a directory. The process verifies the selected server trust policy before asking for the password, reconnects under that same policy, and keeps one shared Tokio runtime around an authenticated reconnecting `RemoteFilesystem`. Every operation through that resilient layer has one absolute budget set by `--callback-timeout-ms`; the budget includes its initial request, reconnect backoff and authentication, handle recovery, and any safe retry. It does not restart between phases. Protocol v6 publishes each directory from one enriched request containing directory/parent metadata, all child metadata, complete xattr names, and bounded small xattr values; Finder's follow-up probes are served locally. Finder and media applications can use ordinary read/write namespace operations, xattrs/resource forks, hardlinks, locks, random byte ranges, sparse seek, volume rename, and backup time. Server-side copy, `readdirplus`, and `exchangedata` callbacks are capability-gated because current macFUSE/macOS combinations do not necessarily expose those optional messages; see [filesystem semantics](filesystem-semantics.md). Press Control+C in the mount terminal to unmount. Graceful macFUSE unmount gets three seconds before forced local detach starts; an independent watchdog terminates the mount process at eight seconds if a macOS unmount syscall wedges, so server availability is not part of unmount:

```sh
umount "$HOME/Volumes/quickfs"
```

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `--server <ADDRESS>` | `QUICKFS_SERVER` | `127.0.0.1:4433` | Server UDP address. |
| `--server-name <NAME>` | — | `localhost` | Logical TLS identity and exact-pin key. |
| `--state-dir <PATH>` | `QUICKFS_CLIENT_STATE_DIR` | `.quickfs-client` | Existing exact-pin trust database. |
| `--username <NAME>` | `QUICKFS_USERNAME` | Required | Account used to authenticate the retained session. |
| `--cache-dir <PATH>` | `QUICKFS_CACHE_DIR` | Client state directory | Private persistent offline-read cache. |
| `--cache-max-bytes <BYTES>` | `QUICKFS_CACHE_MAX_BYTES` | `21474836480` | Hard payload budget for the persistent cache. |
| `--cache-block-kib <KiB>` | — | `16384` | Sequential/copy read-ahead block size, clamped to the negotiated limit. Requests below 1 MiB use at most 1 MiB read-ahead. |
| `--timeout-ms <MS>` | — | `60000` | Maximum idle wait for one connection/transport phase. |
| `--callback-timeout-ms <MS>` | — | `120000` | One hard end-to-end budget for the resilient remote operation, including reconnect, reauthentication, handle recovery, and safe retry. The callback bridge adds only a fixed 100 ms internal margin to deliver the typed result. |
| `--reconnect-attempts <COUNT>` | — | `3` | Bounded authenticated reconnect attempts. |
| `--trust-system-roots` | `QUICKFS_TRUST_SYSTEM_ROOTS` | Off | Use the operating-system public/managed roots. |
| `--ca-cert <PEM>` | `QUICKFS_CA_CERT` | — | Use an explicit enterprise-CA bundle. |
| `--volume-name <NAME>` | — | `quicKFS` | Volume label shown by macOS. |

Without a CA option, the mount reads the same private exact-pin database created by `quickfs-client-cli pair` or `trust import`. `--trust-system-roots` and `--ca-cert` are mutually exclusive. The mount reports read/write only when both server and account gates allow it. Cached data remains readable after a disconnect, but write opens/mutations fail closed and the server must be online to start a mount.

When the end-to-end budget expires, a read-only/idempotent operation returns a timeout (`ETIMEDOUT` at macFUSE). A mutation whose result may already have taken effect returns the existing ambiguous-mutation error (`EIO`) and is never replayed. The fixed 100 ms callback margin is not additional remote-work time; it prevents the callback guard from racing the resilient layer's typed error. Timing out one waiter does not cancel the shared bounded reconnect batch; that reconnect may still restore service for another operation with time left in its own budget.

## Windows `quickfs`

One executable covers the whole Windows lifecycle. It is built separately
because it links against the WinFsp import library:

```powershell
cargo build --release -p quickfs-windows-cli

target\release\quickfs.exe pair 127.0.0.1:4433 --pairing-id <id>
target\release\quickfs.exe mount 127.0.0.1:4433 Q: --username alice
```

Both the server address and the mountpoint are positional. The mountpoint may
be a drive letter (`Q:`, `Q`, `Q:\`), an existing empty directory, or `*` for
the next free letter counting down from `Z:`. The mount runs in the foreground
and verifies the selected trust policy before the password prompt, reconnects
under that same policy, and retains one shared Tokio runtime around an
authenticated reconnecting `RemoteFilesystem` — exactly as the macOS mount does,
because it is the same code.

Unmount by pressing Control+C in the mount window, or from another window:

```powershell
quickfs unmount Q:
```

`unmount` reaches the mounting process through a session-local named event, so
it works only from the logon session that created the mount and says so when no
such mount exists. Ejecting the drive in Explorer does not unmount a WinFsp
volume.

| Option | Environment variable | Default | Purpose |
| --- | --- | --- | --- |
| `<SERVER>` | `QUICKFS_SERVER` | Required | Server UDP address, positional. |
| `<MOUNTPOINT>` | — | Required | Drive letter, directory, or `*`, positional. |
| `--server-name <NAME>` | — | `localhost` | Logical TLS identity and exact-pin key. |
| `--state-dir <PATH>` | `QUICKFS_CLIENT_STATE_DIR` | `.quickfs-client` | Exact-pin trust database, and the cache unless `--cache-dir` overrides it. |
| `--username <NAME>` | `QUICKFS_USERNAME` | Required | Account used to authenticate the retained session. |
| `--cache-dir <PATH>` | `QUICKFS_CACHE_DIR` | Client state directory | Private persistent offline-read cache. |
| `--cache-max-bytes <BYTES>` | `QUICKFS_CACHE_MAX_BYTES` | `21474836480` | Hard payload budget for the persistent cache. |
| `--cache-block-kib <KiB>` | — | `16384` | Sequential/copy read-ahead block size, clamped to the negotiated limit. |
| `--read-ahead-max-bytes <BYTES>` | `QUICKFS_READ_AHEAD_MAX_BYTES` | `67108864` | Speculative read-ahead ceiling; `0` disables it. |
| `--transport-timeout-ms <MS>` | — | `60000` | Maximum idle wait for one connection/transport phase. |
| `--callback-timeout-ms <MS>` | — | `120000` | One hard end-to-end budget per operation, including reconnect and safe retry. |
| `--reconnect-attempts <COUNT>` | — | `3` | Bounded authenticated reconnect attempts. |
| `--trust-system-roots` | `QUICKFS_TRUST_SYSTEM_ROOTS` | Off | Use the operating-system public/managed roots. |
| `--ca-cert <PEM>` | `QUICKFS_CA_CERT` | — | Use an explicit enterprise-CA bundle. |
| `--volume-name <NAME>` | — | `quicKFS` | Volume label shown in Explorer, at most 32 characters. |

The trust subcommands take the server address positionally and share the trust
database with the macOS client:

```powershell
quickfs trust import 127.0.0.1:4433 --sha256 A1:B2:...
quickfs trust import 127.0.0.1:4433 --certificate server.pem
quickfs trust list
quickfs forget 127.0.0.1:4433
```

`--trust-system-roots` and `--ca-cert` are mutually exclusive and apply only to
mounting; passing either to a pairing or trust command is refused rather than
silently ignored. Only `mount` requires WinFsp to be installed, so a machine can
be paired before the runtime is present.

Timeout behaviour is identical to the macOS mount: a read-only operation that
exhausts its budget returns a timeout status, and a mutation whose result may
already have taken effect returns an I/O error and is never replayed.

The Windows client does not provide the diagnostic file subcommands
(`list`, `stat`, `read`, `write`, and the rest) that `quickfs-client-cli` has on
macOS; use the mounted drive, or run the diagnostic CLI on another host. See the
[Windows client guide](windows.md) for the POSIX-to-Windows metadata mapping and
the platform limitations.

## Logging and limitations

Use `RUST_LOG=info` or `RUST_LOG=quickfs_server_daemon=debug`. Logs must not contain passwords, pairing codes, or file contents.

The implementation remains experimental. Per-user export roots, recovery and live-session revocation, distributed-login defense, cold-start offline mounting, offline mutation/conflict reconciliation, and production hardening remain incomplete. Do not expose it directly to the public Internet without an independent security and operational review.
