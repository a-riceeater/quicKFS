# Setup guide

This guide creates a local server, user account, trusted client, and optionally writable mount. The local walkthrough uses pairing; a later section configures public/enterprise PKI or managed pins without per-client administrator contact. Linux is the deployment server target; the daemon also runs on macOS for development, without Linux-only special-node creation.

Deploying a real Linux server rather than building one? Skip to
[Production Linux installation](#production-linux-installation). That path uses
a released archive and needs no Rust or Cargo on the server.

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

These prerequisites are for building from source. Installing a released Linux
server needs none of them — see
[Production Linux installation](#production-linux-installation).

Install Git and Rust/Cargo 1.88 or newer. Rust 1.85 was the first stable release supporting Rust 2024; the current dependency graph sets the project's supported minimum at 1.88. The repository's `rust-toolchain.toml` selects current stable through rustup.

On Linux, distribution Cargo packages may be too old:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustc --version
cargo --version
```

Both versions must be at least 1.88. On Windows, install the Visual Studio Build Tools for the MSVC linker, and [WinFsp](https://winfsp.dev/rel/) to mount — see the [Windows client guide](windows.md) for what each WinFsp feature is needed for and how to build against an unpacked SDK when WinFsp cannot be installed. `command -v cargo` should normally report `$HOME/.cargo/bin/cargo`. Normal workspace builds and tests do not require an installed macFUSE runtime. Running the GUI, client CLI, or native mount on macOS does: each process checks at startup and stops with an installation link when it is absent. Install macFUSE from the [official website](https://macfuse.io/), install `pkgconf` for native development, and complete any system-extension approval requested by macOS:

```sh
brew install pkgconf
```

quicKFS does not install, approve, or update the system extension for you. The Linux server daemon and clients running on non-macOS platforms do not perform this check.

## Build

```sh
git clone https://github.com/quickfs/quickfs.git
cd quickfs
cargo build --workspace
```

The default debug binaries are `target/debug/quickfs-server-daemon` and `target/debug/quickfs-client-cli`. On a macFUSE-equipped Mac, build `target/debug/quickfs-mount` explicitly:

```sh
cargo build -p quickfs-filesystem-macfuse --features macfuse --bin quickfs-mount
```

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

Read access is the default. To authorize this account for writes, grant it explicitly:

```sh
cargo run -p quickfs-server-daemon -- user grant-write \
  --state-dir .quickfs \
  alice
```

## Start the server

```sh
RUST_LOG=info cargo run -p quickfs-server-daemon -- serve \
  --bind 127.0.0.1:4433 \
  --export-root ./shared \
  --state-dir .quickfs \
  --allow-writes
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
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 --server-name localhost \
  --username alice list /
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 --server-name localhost \
  --username alice stat /hello.txt
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 --server-name localhost \
  --username alice \
  read /hello.txt --offset 0 --length 4096
```

The address and server name default to local development values. The client refuses an unexpected server certificate before prompting for a password.

The account password is distinct from the one-time pairing code. It is checked on every new connection against the Argon2id hash stored by the server. Pairing codes are not reusable login credentials, and pairing never creates an account.

## Mount the export in Finder on macOS

After pairing and building `quickfs-mount`, create an empty mountpoint and start the foreground mount process with the same address, server name, state directory, and user used by the CLI:

```sh
mkdir -p "$HOME/Volumes/quickfs"
target/debug/quickfs-mount "$HOME/Volumes/quickfs" \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --state-dir .quickfs-client \
  --username alice
```

Enter the account password at the hidden prompt. The mount verifies the server before that prompt, reconnects and verifies it again before transmitting the password, then retains one shared Tokio runtime around an authenticated reconnecting filesystem and persistent read cache. Protocol v6 loads a Finder directory with one enriched request carrying directory/parent metadata, all child metadata, complete xattr names, and bounded small xattr values. Open `$HOME/Volumes/quickfs` in Finder. The volume is writable only when both the daemon and account gates above are enabled; otherwise it mounts read-only.

The v6 client negotiates the version-specific `quickfs/6` ALPN and cannot mount a v5 daemon. Upgrade/restart the server daemon and rebuild the CLI/mount from the same revision before testing the new directory pipeline. Certificate pins and user accounts remain valid when the server keeps the same state directory and TLS identity.

Keep the terminal process running. When finished, press Control+C there to unmount. A graceful macFUSE unmount gets three seconds; if it stalls (including after server loss), `quickfs-mount` attempts a forced local detach. An independent watchdog exits the mount process after eight seconds if the macOS unmount call itself wedges, which releases the mount even with no live server. You can also unmount from another terminal and allow `quickfs-mount` to exit:

```sh
umount "$HOME/Volumes/quickfs"
```

For enterprise CA or platform-root deployments, pass the same `--ca-cert` or `--trust-system-roots` option used by the diagnostic client. See the [usage reference](usage.md#macos-quickfs-mount) for all mount options.

## Mount the export in Explorer on Windows

Install [WinFsp](https://winfsp.dev/rel/) first — the **Core** feature to mount,
plus **Developer** if the same machine builds the client. Then pair and mount
with the same address, server name, state directory, and user the CLI uses:

```powershell
cargo build --release -p quickfs-windows-cli

target\release\quickfs.exe pair 127.0.0.1:4433 --pairing-id <id>
target\release\quickfs.exe mount 127.0.0.1:4433 Q: --username alice
```

Enter the account password at the hidden prompt. As on macOS, the mount verifies
the server before that prompt, reconnects and verifies it again before
transmitting the password, then retains one shared runtime around an
authenticated reconnecting filesystem and persistent read cache. Open `Q:` in
Explorer. The volume is writable only when both the daemon and account gates
above are enabled; otherwise it mounts read-only.

The trust database is shared with the macOS client, so a pin established by
either is understood by the other. On Windows it is protected by an owner-only
DACL rather than Unix modes, and a store another account could reach is refused
rather than read.

Keep the window running. When finished, press Control+C there, or run
`quickfs unmount Q:` from another window. For enterprise CA or platform-root
deployments, pass the same `--ca-cert` or `--trust-system-roots` option. See the
[usage reference](usage.md#windows-quickfs) for every option and the
[Windows client guide](windows.md) for the platform limitations.

## Production Linux installation

The walkthrough above builds from source, which is right for development. A
Linux server is deployed instead from a released archive: no Rust or Cargo on
the target machine, a TOML configuration, a dedicated system account, and a
hardened systemd service.

### Supported platforms

| | |
| --- | --- |
| Architectures | `x86_64` (also matched by `amd64`) and `aarch64` (also matched by `arm64`) |
| libc | glibc 2.35 or newer. The archives are built on Ubuntu 22.04. |
| Init | systemd, for the supplied unit. The daemon itself runs without one. |
| Not covered | musl distributions such as Alpine; build from source there. |

An unrecognized architecture is rejected rather than guessed at.

### Download, read, then run

There is deliberately no `curl … | sh` recipe. A pipe hands root to whatever the
server returned, with no chance to look at it first.

```sh
curl --proto '=https' --tlsv1.2 -fLO \
  https://github.com/quickfs/quickfs/releases/latest/download/quickfs-server-linux-x86_64.tar.gz
curl --proto '=https' --tlsv1.2 -fLO \
  https://github.com/quickfs/quickfs/releases/latest/download/quickfs-server-linux-x86_64.tar.gz.sha256

sha256sum -c quickfs-server-linux-x86_64.tar.gz.sha256
tar -xzf quickfs-server-linux-x86_64.tar.gz
cd quickfs-server-linux-x86_64
less install.sh
sudo ./install.sh
```

`install.sh` also fetches the release itself when no local archive is given:

```sh
sudo ./install.sh                 # latest release
sudo ./install.sh --version v0.2.0
sudo ./install.sh --no-start      # install and configure, do not start
sudo ./install.sh --archive ./quickfs-server-linux-x86_64.tar.gz
DESTDIR=/tmp/quickfs-package ./install.sh --no-systemd
```

It downloads over HTTPS only, refuses a redirect that leaves HTTPS, verifies the
SHA-256 checksum **before** extracting or executing anything, refuses archive
members that are absolute paths, contain `..`, or are symlinks, hardlinks,
devices, FIFOs or sockets, and fails closed on a partial download, a checksum
mismatch, an unexpected archive layout, an unsupported architecture, or an
invalid configuration.

Under `DESTDIR` the installer only stages files: it creates no user or group,
never contacts systemd, and writes nothing outside that directory.

### What the checksums prove

The `.sha256` file comes from the same GitHub release as the archive. It detects
a truncated or corrupted download and a mirror that altered the bytes. It does
**not** establish an independent trust chain: anyone able to replace the archive
in that release could replace the checksum beside it. Detached cryptographic
release signing is not yet set up for this project, and the installer's
verification step is structured so a signature check can be added in front of it
without changing anything else.

### Installed layout

| Path | Owner | Mode | Contents |
| --- | --- | --- | --- |
| `/usr/local/bin/quickfs-server-daemon` | `root:root` | `0755` | The daemon |
| `/etc/quickfs` | `root:quickfs` | `0750` | Configuration directory |
| `/etc/quickfs/quickfs.toml` | `root:quickfs` | `0640` | Your configuration |
| `/etc/quickfs/quickfs.toml.example` | `root:quickfs` | `0640` | Documented example, replaced on upgrade |
| `/var/lib/quickfs` | `quickfs:quickfs` | `0700` | Identity, accounts, pairing records, export epoch |
| `/etc/systemd/system/quickfs-server.service` | `root:root` | `0644` | The unit |

The daemon refuses to start when its private state is group- or
other-accessible, and refuses a configuration file that is a symbolic link or is
writable by group or other users.

### The service account

`quickfs` is a system user and group created with no password, no interactive
shell, no created home directory, and no supplementary groups. It should own the
private state directory and nothing else. Everything that account can read or
write is reachable by a compromise of the network-facing daemon, so giving it
ownership of unrelated data turns one service bug into access to that data.

### Granting the daemon access to an export

The installer never changes the ownership or permissions of your export, and
never recursively. Choose one of these deliberately:

```sh
# 1. A dedicated export owned by the service account.
sudo install -d -o quickfs -g quickfs -m 0750 /srv/quickfs/export

# 2. A dedicated export whose owning group is quickfs, with the setgid bit so
#    new files inherit the group.
sudo chgrp quickfs /srv/quickfs/export
sudo chmod 2750 /srv/quickfs/export

# 3. A POSIX ACL on existing data, leaving its ownership alone.
sudo setfacl -m u:quickfs:rx /srv/project-share
sudo setfacl -d -m u:quickfs:rx /srv/project-share     # for new entries

# 4. Membership of a storage group you chose on purpose.
sudo usermod -aG media quickfs
```

Option 4 gives the service everything that group can reach, so pick the group
knowing that. Do not run `chown -R quickfs` over a share other users or services
also own.

### Configure

```sh
sudoedit /etc/quickfs/quickfs.toml
quickfs-server-daemon config validate --config /etc/quickfs/quickfs.toml
quickfs-server-daemon config print-effective --config /etc/quickfs/quickfs.toml
```

`quickfs.toml.example` documents every key. Unknown keys are errors, so a
mistyped `allow_writess` fails validation instead of silently leaving writes
disabled in a file that appears to enable them. See the
[configuration reference](usage.md#configuration-file) for the schema and the
precedence rules.

### Initialize, add an account, start

```sh
sudo -u quickfs /usr/local/bin/quickfs-server-daemon init \
  --config /etc/quickfs/quickfs.toml \
  --server-name files.example.net

sudo -u quickfs /usr/local/bin/quickfs-server-daemon user add \
  --config /etc/quickfs/quickfs.toml alice

sudo systemctl enable --now quickfs-server
systemctl status quickfs-server
journalctl -u quickfs-server -f

sudo -u quickfs /usr/local/bin/quickfs-server-daemon pair create \
  --config /etc/quickfs/quickfs.toml
```

Run these as `quickfs`, not as root: state created by root would be owned by the
wrong account and the daemon would refuse it. `--config` supplies the state
directory so it does not have to be repeated; an explicit `--state-dir` still
wins.

The installer never asks for or creates the first account password, never
generates a pairing code on your behalf, and never leaves either in a log,
argument, environment variable or temporary file.

### Read-only by default, and how to enable writes

An export is read-only until **both** gates are open:

```sh
sudoedit /etc/quickfs/quickfs.toml      # allow_writes = true
sudo systemctl restart quickfs-server
sudo -u quickfs /usr/local/bin/quickfs-server-daemon user grant-write \
  --config /etc/quickfs/quickfs.toml alice
```

Neither the installer nor the unit enables writes.

### Firewall

QUIC is UDP. Allow the configured UDP port (`4433` by default) through the host
and network firewalls yourself:

```sh
sudo ufw allow 4433/udp                                    # ufw
sudo firewall-cmd --permanent --add-port=4433/udp && sudo firewall-cmd --reload
```

The installer never changes firewall rules. A successful TCP test does not prove
QUIC reachability.

### Managing the service

```sh
sudo systemctl start quickfs-server
sudo systemctl stop quickfs-server
sudo systemctl restart quickfs-server
sudo systemctl reload-or-restart quickfs-server
systemctl status quickfs-server
journalctl -u quickfs-server -f
journalctl -u quickfs-server --since '1 hour ago'
```

`ExecStartPre` runs `config validate --require-identity`, so the service refuses
to start — with the reason in the journal — when the configuration is missing or
invalid, the export root does not exist, state and export overlap, private state
permissions are unsafe, or the server identity was never initialized. The unit's
hardening choices, and the four directives deliberately left out, are documented
in [servers/linux/README.md](../servers/linux/README.md#systemd-hardening),
along with the override an expert needs for special-node creation and what that
override costs.

### Back up the state directory

`/var/lib/quickfs` is not reconstructible.

```sh
sudo systemctl stop quickfs-server
sudo tar -czf quickfs-state-$(date +%F).tar.gz -C /var/lib quickfs
sudo systemctl start quickfs-server
```

Store the archive as securely as the private key it contains. If the state
directory is lost:

- the **server identity** is gone, so every exact-pin client refuses to connect
  until it is paired again or given a new managed pin (CA-trusting clients
  recover once a new certificate for the same name is installed);
- the **export epoch and node-identity key** are gone, so every client treats
  the server as a different one, discards its cached metadata and byte ranges,
  and reconnects cold;
- **accounts and pairing records** are gone and must be recreated.

There is no recovery path for any of these.

### Upgrade

Run the newer installer. It replaces the binary and unit atomically, refreshes
`quickfs.toml.example`, validates the configuration with the new binary,
reloads systemd, and restarts only after validation succeeds. It preserves
`quickfs.toml`, `/var/lib/quickfs`, the certificate identity, accounts, pairing
records, and the server epoch. If the download, checksum, extraction,
installation, or validation fails, nothing is replaced and the running
installation keeps working.

Clients and the server must share the protocol **major** version, which keys the
ALPN. Same-major minor differences negotiate, so a `6.4` daemon serves a `6.0`
client. Coordinate a client and server upgrade only when the major changes.

### Uninstall

```sh
sudo ./uninstall.sh
```

It stops and disables the service, removes only the installed binary and unit,
and reloads systemd. It preserves `/etc/quickfs` (including your
`quickfs.toml`), `/var/lib/quickfs`, the `quickfs` user and group, and your
export root, which it never reads and never touches. Reinstalling later reuses
all of it, so clients do not need to be paired again.

There is no `--purge`. The script prints the exact manual commands for removing
configuration and state, and what that costs, so the decision and the paths are
both yours.

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
