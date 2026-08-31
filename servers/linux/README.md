# Linux server

The Linux daemon is the deployment target. It installs from a signed-by-checksum
release archive with no Rust toolchain on the machine, reads a TOML
configuration, and runs as a hardened systemd service under a dedicated
`quickfs` system account.

## Install

```sh
curl --proto '=https' --tlsv1.2 -fLO \
  https://github.com/a-riceeater/quicKFS/releases/latest/download/quickfs-server-linux-x86_64.tar.gz
curl --proto '=https' --tlsv1.2 -fLO \
  https://github.com/a-riceeater/quicKFS/releases/latest/download/quickfs-server-linux-x86_64.tar.gz.sha256

sha256sum -c quickfs-server-linux-x86_64.tar.gz.sha256
tar -xzf quickfs-server-linux-x86_64.tar.gz
cd quickfs-server-linux-x86_64
less install.sh          # read it before running it as root
sudo ./install.sh
```

Never pipe an installer into a shell. `sudo ./install.sh` with no local archive
downloads and verifies the latest release itself; `--version v0.2.0` pins a
release, `--archive <PATH>` uses a local one, `--no-start` stops before
enabling the service, and `DESTDIR=… ./install.sh --no-systemd` stages the
layout without touching the system. Supported architectures are `x86_64` and
`aarch64` on glibc 2.35 or newer.

The full deployment walkthrough — export access, identity, accounts, pairing,
firewall, upgrades, uninstall — is in [docs/setup.md](../../docs/setup.md#production-linux-installation).

## Installed layout

| Path | Owner | Mode |
| --- | --- | --- |
| `/usr/local/bin/quickfs-server-daemon` | `root:root` | `0755` |
| `/etc/quickfs` | `root:quickfs` | `0750` |
| `/etc/quickfs/quickfs.toml` | `root:quickfs` | `0640` |
| `/etc/quickfs/quickfs.toml.example` | `root:quickfs` | `0640` |
| `/var/lib/quickfs` | `quickfs:quickfs` | `0700` |
| `/etc/systemd/system/quickfs-server.service` | `root:root` | `0644` |

`quickfs` is a system account with no password, no interactive shell, no created
home directory, and no supplementary groups. It should own the state directory
and nothing else: everything it can reach is reachable by a compromise of the
network-facing daemon.

## Configure

`/etc/quickfs/quickfs.toml` holds deployment paths, limits, and the log filter.
It never holds passwords, pairing codes, or private keys. `quickfs.toml.example`
beside it documents every key. Unknown keys are errors, not warnings.

```sh
sudoedit /etc/quickfs/quickfs.toml
quickfs-server-daemon config validate --config /etc/quickfs/quickfs.toml
quickfs-server-daemon config print-effective --config /etc/quickfs/quickfs.toml
```

Precedence is `explicit CLI option > environment variable > TOML value >
built-in default`. See [docs/usage.md](../../docs/usage.md#configuration-file)
for the schema.

## First run

```sh
sudo -u quickfs quickfs-server-daemon init \
  --config /etc/quickfs/quickfs.toml --server-name files.example.net
sudo -u quickfs quickfs-server-daemon user add \
  --config /etc/quickfs/quickfs.toml alice
sudo systemctl enable --now quickfs-server
sudo -u quickfs quickfs-server-daemon pair create \
  --config /etc/quickfs/quickfs.toml
```

Every administrative command accepts `--config` so the state directory does not
have to be repeated; an explicit `--state-dir` still overrides it.

For managed deployments, `init --certificate <FULLCHAIN.pem> --private-key
<KEY.pem>` imports an externally issued identity, and `identity install`
atomically selects a validated renewal for the next daemon start. Clients can
use a deployed private CA, their operating-system trust policy, or a centrally
imported exact pin instead of pairing. See the main setup and authentication
guides.

## Run without the service

```sh
RUST_LOG=info quickfs-server-daemon serve \
  --bind 0.0.0.0:4433 \
  --export-root /srv/project-share \
  --state-dir /var/lib/quickfs
```

Protocol v6 clients require a v6 daemon (`quickfs/6` ALPN); upgrade and restart
the server and clients together when the protocol **major** changes. Native
directory views discover child metadata/xattrs concurrently beside the export;
`limits.max_directory_entry_tasks` bounds that work globally and defaults to
`64`. Ctrl+C and SIGTERM initiate graceful shutdown.

## Operate the service

```sh
systemctl status quickfs-server
sudo systemctl restart quickfs-server
journalctl -u quickfs-server -f
```

`ExecStartPre` runs `config validate --require-identity`, so the service refuses
to start when the configuration is missing or invalid, the export root does not
exist, state and export overlap, private state permissions are unsafe, or the
server identity was never initialized. The failure is in the journal with the
specific reason.

## systemd hardening

The unit runs as `quickfs:quickfs` with `UMask=0077`, an empty
`CapabilityBoundingSet` and `AmbientCapabilities`, `NoNewPrivileges`,
`PrivateTmp`, `PrivateDevices`, `ProtectSystem=full`, the `ProtectKernel*` /
`ProtectControlGroups` / `ProtectClock` / `ProtectHostname` / `ProtectProc` set,
`RestrictSUIDSGID`, `RestrictRealtime`, `RestrictNamespaces`, `LockPersonality`,
`MemoryDenyWriteExecute`, `SystemCallArchitectures=native`,
`SystemCallFilter=@system-service`, and
`RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK`.

Four directives are deliberately **absent**, each because it would break a
legitimate export rather than protect one:

- `ProtectHome=` — an export may live under `/home`, and this would make it
  silently unreadable instead of failing loudly. Add it in a drop-in if your
  export is elsewhere.
- `ProtectSystem=strict` with `ReadWritePaths=` — the export root and state
  directory come from TOML, so the unit cannot know which paths to re-open.
  `full` already makes `/usr`, `/boot`, `/efi` and `/etc` read-only.
- `StateDirectory=quickfs` — `state_dir` is configurable, so letting systemd
  create and own a fixed `/var/lib/quickfs` would disagree with any other value.
- `PrivateUsers=true` — it blocks reading an export owned by other UIDs, which
  is the normal case.

The empty capability set is also what makes character- and block-device creation
through `mknod` unavailable in the default service: that needs `CAP_MKNOD`, and
a network-facing file server should not hold it. FIFOs and unix sockets need no
capability and keep working. An expert who genuinely needs device nodes can add
a deliberate override:

```sh
sudo systemctl edit quickfs-server
```

```ini
[Service]
CapabilityBoundingSet=CAP_MKNOD
AmbientCapabilities=CAP_MKNOD
```

That gives a remotely reachable daemon the ability to create device nodes
anywhere it can write, including inside the export. An authenticated client that
can create a writable device node in a directory another process trusts has a
privilege-escalation primitive. Do not enable it on a shared or
Internet-reachable server.

## Private state

The state directory contains the secret server identity, password hashes,
pairing records, and the export epoch and node-identity key. It and the export
root must be disjoint directory trees; on Unix the daemon requires mode `0700`
directories and `0600` private files and refuses to start otherwise. Restrict
access and back it up securely — losing it loses the server identity (every
exact-pin client must be paired again) and the export epoch (every client
discards its cache), with no recovery.

## Uninstall

```sh
sudo ./uninstall.sh
```

It stops and disables the service, removes only `/usr/local/bin/quickfs-server-daemon`
and `/etc/systemd/system/quickfs-server.service`, and reloads systemd. It
preserves `/etc/quickfs`, `/var/lib/quickfs`, the `quickfs` user and group, and
your export root, which it never reads. There is no `--purge`; the script prints
the manual commands and what deleting state costs.
