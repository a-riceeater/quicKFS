# quicKFS Linux server — install

This archive contains everything needed to install the quicKFS server daemon on
a Linux host. Rust and Cargo are **not** required on the target machine: the
binary is prebuilt.

```text
quickfs-server-daemon    the server binary
quickfs-server.service   systemd unit
quickfs.toml.example     fully commented example configuration
install.sh               installer
uninstall.sh             uninstaller
README-install.md        this file
LICENSE                  Apache-2.0
VERSION                  the Cargo package version this archive was built from
```

## Supported platforms

| | |
| --- | --- |
| Architectures | `x86_64` and `aarch64` |
| libc | glibc 2.35 or newer (built on Ubuntu 22.04) |
| Init | systemd, for the supplied unit. The binary itself runs without it. |
| Kernel | Any Linux with `openat2`-era descriptor-relative filesystem calls; the daemon is developed against current stable kernels. |

A musl distribution (Alpine) is not covered by these archives — build from
source there.

## Install

Download, **read the script**, then run it. There is deliberately no
`curl … | sh` recipe: a pipe hands root to whatever the server returned, with
no chance to look at it first.

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

Or let the installer fetch and verify the release itself:

```sh
sudo ./install.sh                 # latest release
sudo ./install.sh --version v0.2.0
sudo ./install.sh --no-start      # install and configure, do not start
sudo ./install.sh --archive ./quickfs-server-linux-x86_64.tar.gz
```

The installer downloads over HTTPS only, refuses a redirect that leaves HTTPS,
verifies the SHA-256 checksum **before** extracting or executing anything,
rejects archive members that are absolute, contain `..`, or are symlinks,
hardlinks, devices, FIFOs or sockets, and fails closed on a partial download.

**What the checksums do and do not prove.** The `.sha256` file is served from
the same GitHub release as the archive. It detects a truncated or corrupted
download and a mirror that altered the bytes; it does **not** establish an
independent trust chain, because anyone who could replace the archive in that
release could replace the checksum beside it. Detached cryptographic signing is
not yet set up for this project. The installer's verification step is written so
a signature check can be added in front of it later without changing anything
else.

## Staging without root

```sh
DESTDIR=/tmp/quickfs-package ./install.sh --no-systemd
```

Under `DESTDIR` the installer stages the file layout only. It creates no user or
group, never contacts systemd, never writes outside `DESTDIR`, and never runs
anything interactively.

## What gets installed

| Path | Owner | Mode |
| --- | --- | --- |
| `/usr/local/bin/quickfs-server-daemon` | `root:root` | `0755` |
| `/etc/quickfs` | `root:quickfs` | `0750` |
| `/etc/quickfs/quickfs.toml` | `root:quickfs` | `0640` |
| `/etc/quickfs/quickfs.toml.example` | `root:quickfs` | `0640` |
| `/var/lib/quickfs` | `quickfs:quickfs` | `0700` |
| `/etc/systemd/system/quickfs-server.service` | `root:root` | `0644` |

The `quickfs` user is a system account with no password, no interactive shell,
no created home directory, and no supplementary groups.

## First run

```sh
sudoedit /etc/quickfs/quickfs.toml
quickfs-server-daemon config validate --config /etc/quickfs/quickfs.toml

sudo -u quickfs /usr/local/bin/quickfs-server-daemon init \
  --config /etc/quickfs/quickfs.toml --server-name files.example.net
sudo -u quickfs /usr/local/bin/quickfs-server-daemon user add \
  --config /etc/quickfs/quickfs.toml alice

sudo systemctl enable --now quickfs-server
systemctl status quickfs-server
journalctl -u quickfs-server -f

sudo -u quickfs /usr/local/bin/quickfs-server-daemon pair create \
  --config /etc/quickfs/quickfs.toml
```

Writes are disabled. Enabling them takes both `allow_writes = true` in the
configuration and a per-account `user grant-write`.

QUIC is UDP. Allow the configured UDP port through your firewall yourself — the
installer never changes firewall rules.

**Do not `chown -R quickfs` your export.** Grant access deliberately instead:
make `quickfs` the owning group of a dedicated export, use an export owned by
`quickfs`, set a POSIX ACL, or add `quickfs` to a storage group you chose. The
installer never changes export ownership, recursively or otherwise.

## Upgrade

Run the newer installer. It replaces the binary and the unit atomically,
refreshes `quickfs.toml.example`, and preserves `quickfs.toml`,
`/var/lib/quickfs`, the certificate identity, accounts, pairing records, and the
server epoch. If the download, checksum, extraction, or validation fails,
nothing is replaced and the working installation keeps running.

Client and server must share the protocol **major** version. Upgrade clients and
server together only when the major changes; same-major minor differences
negotiate.

## Uninstall

```sh
sudo ./uninstall.sh
```

It stops and disables the service, removes only the binary and the unit, and
reloads systemd. It preserves `/etc/quickfs`, `/var/lib/quickfs`, the `quickfs`
user and group, and the export root. There is no `--purge`; the script prints
the exact manual commands and what deleting state costs.

## Security status

quicKFS is an experimental prototype without an independent security audit. Read
`SECURITY.md`, `docs/threat-model.md`, and `docs/authentication.md` before
exposing it beyond a trusted network.
