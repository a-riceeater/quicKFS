# quicKFS

quicKFS is an experimental authenticated network filesystem designed for high-latency, intermittent, and long-distance connections. Today, a Linux server exports an existing directory while a macOS client mounts it through macFUSE, letting Finder, command-line tools, and applications such as media editors use ordinary filesystem operations instead of a separate transfer or synchronization workflow.

The transport is a purpose-built versioned protocol over QUIC/TLS. It supports random byte-range I/O, account-gated writes, reconnect with revision-safe handle recovery, persistent read-through caching, stable node identity across daemon restarts, and broad filesystem semantics. The protocol and client-core abstractions are platform-independent, and the long-term goal is first-class quicKFS clients and servers across macOS, Linux, Windows, and other platforms.

> **Project status:** quicKFS is a substantial prototype, not a production-audited filesystem. Do not expose it directly to the public Internet without an independent security and operational review.

## Why quicKFS?

quicKFS is built to make remote files feel responsive and dependable even when the network is far away, variable, or temporarily unavailable. Its protocol treats latency, concurrency, reconnect, caching, and filesystem consistency as one end-to-end design problem rather than separate layers.

The central goal is a short critical path from an application request to useful data. quicKFS returns related metadata together, transfers explicit bounded byte ranges with configurable read-ahead, keeps independent work concurrent, and carries the revision information needed to reuse cached data safely. This is especially valuable for large creative projects where an editor may inspect many files, seek repeatedly through a clip, and issue metadata and media reads at the same time.

| quicKFS design choice | Why it helps on a high-latency or lossy link |
| --- | --- |
| Enriched native directory views | One bounded directory request returns the directory/parent metadata, every child's name and metadata, complete xattr-name sets, and bounded small xattr values. Finder-style `lookup`, `getattr`, `listxattr`, negative xattr probes, and common small xattr reads then stay local. |
| Useful state in operation responses | Open/create replies include the handle, size, and revision; writes return the resulting size and revision; metadata mutations return updated metadata. The client does not need a follow-up round trip just to learn the state produced by its previous request. |
| Adaptive, explicit byte ranges | Applications can request only the portion of a large media file they need. Sequential/copy-sized reads use aligned blocks up to the 16 MiB default, while sub-1 MiB Finder/header probes use at most 1 MiB of read-ahead. Overlapping cache misses—including failures—are single-flighted and independent reads remain concurrent. |
| One independent QUIC stream per operation | A delayed or retransmitted file range does not impose transport-level head-of-line blocking on unrelated streams, so metadata and other reads can continue making progress. QUIC still applies connection congestion and flow control; this is isolation, not immunity to a bad link. |
| Revision-aware caching | Metadata, directory snapshots, and byte ranges are reused under an exact file revision. Repeated seeks and Finder metadata probes can become local cache hits without pretending stale bytes are current. |
| Server-side work | Range copy, reflink/copy-range acceleration, sparse seek, and filesystem metadata operations execute beside the backing storage rather than downloading data merely to upload it again. |
| Explicit reconnect invariants | A reconnect must reach the same server epoch, and an open file is recovered only at its exact prior revision. Safe reads may retry; ambiguous writes never run twice automatically. |
| FUSE-aware protocol shape | The macFUSE adapter and wire protocol are developed together, so common kernel request patterns can be translated into one semantic remote operation rather than a generic sequence of path-based calls. |
| Platform-independent core | Filesystem semantics live behind `RemoteFilesystem`, separate from macFUSE and QUIC plumbing. New native adapters can reuse the authenticated, reconnecting, cached client instead of rebuilding the protocol for each operating system. |

Together, these choices reduce **dependent** network round trips on the critical path. Publishing a directory uses one enriched request rather than a directory request followed by a request per entry/xattr, opening a clip returns the revision needed for later range validation, repeated metadata probes can be served from cache, and dozens of timeline reads can remain in flight independently. Server-side copies and sparse operations keep work close to the storage, while authenticated reconnect lets an unchanged working set recover without forcing applications to start over.

The same design is intended to grow beyond the current Linux-server/macOS-client pairing. Future Linux FUSE, Windows WinFsp, and other native adapters can share the protocol, authentication, revision model, cache, and reconnect implementation while translating each platform's filesystem API at the edge. The goal is one quicKFS network filesystem that feels native wherever a project is opened.

## What it supports

| Area | Operations and behavior |
| --- | --- |
| File I/O | Create, open, positioned and append reads/writes, truncate, sparse files, preallocation, `mmap` through the kernel page cache, flush, `fsync`, and `fdatasync`. |
| Namespace | Directory create/remove, unlink, symlink/readlink, hardlinks, replace/no-replace rename, and atomic rename swapping. |
| Metadata | `getattr`/`setattr`, modes, timestamps, link counts, allocation data, statfs, macOS backup time, and volume renaming. |
| Application metadata | Extended attributes, Finder tags, quarantine metadata, custom xattrs, and persisted `com.apple.ResourceFork` sidecars. |
| Media-oriented access | Concurrent random byte ranges, requests larger than one wire chunk, `SEEK_DATA`/`SEEK_HOLE`, server-side range copy, safe `FIONREAD`, poll readiness, and logical `bmap`. |
| Coordination | Cross-client POSIX byte-range locks, directory sync, stable node IDs, revision checking, and reconnect-time handle/lock recovery. |
| Lifecycle | Enriched directory views, `readdirplus` callback support, inode `forget`/`batch_forget` eviction, and server-independent local unmount. |
| Linux server features | FIFO, socket, character-device, and block-device creation through `mknod`, subject to normal host privileges. |

The protocol also represents filenames and xattr names as arbitrary bytes. Linux preserves non-UTF-8 names end to end; modern macOS can reject invalid UTF-8 path components before macFUSE delivers them to the adapter.

## Architecture

```text
Finder / application / CLI
          │
          ▼
macFUSE adapter ── stable inode + handle translation
          │         one shared Tokio runtime
          ▼
cached RemoteFilesystem
          │
          ▼
resilient authenticated client
          │
          ▼
QUIC/TLS 1.3  ── quickfs/8 ── independent request streams
          │
          ▼
Linux server daemon
          │
          ▼
descriptor-confined export directory
```

The platform adapter depends on the `RemoteFilesystem` interface rather than transport details. The same client pipeline used by the diagnostic CLI is wrapped by reconnect and cache layers and retained by the macFUSE mount for its entire lifetime. One multithread Tokio runtime services asynchronous work from synchronous FUSE callbacks.

The main components are:

- `crates/protocol`: version 6 requests, responses, capabilities, metadata, and limits;
- `crates/transport-quic`: Quinn endpoints, TLS trust policies, framing, timeouts, and peer identity;
- `crates/client-core`: network, resilient, cached, and delayed/test filesystem implementations;
- `crates/cache`: private persistent metadata, directory, symlink, statfs, and revision-keyed range cache;
- `crates/server-core`: confined filesystem operations, stable identity, revisions, xattrs, copies, locks, and persistence;
- `servers/linux/server-daemon`: authentication, authorization, QUIC dispatch, limits, and administration;
- `clients/macos/filesystem-macfuse`: native macFUSE callbacks and the `quickfs-mount` executable;
- `clients/macos/client-cli`: trust, authentication, administration, and direct protocol diagnostics.

See [Architecture](ARCHITECTURE.md) for the crate and trust boundaries.

## How protocol version 6 works

### Transport and framing

QUIC/TLS negotiates the version-specific ALPN identifier `quickfs/8`. Each filesystem operation normally uses an independent bidirectional QUIC stream on one authenticated connection, allowing concurrent reads and unrelated metadata requests to progress independently. Control frames carry an optional per-frame zstd compression flag (see [docs/protocol.md](docs/protocol.md)). The client sends a 10-second keepalive, both peers allow five minutes of idle time, and the connection/stream flow-control windows accommodate several concurrent maximum-size reads. Protocol versions are intentionally incompatible, so upgrade the server and clients together.

Control messages are Postcard-encoded and length-prefixed with a 1 MiB frame limit. Bulk read, write, and xattr data follows its control frame as an explicitly sized raw body rather than being embedded in serialization. The default negotiated read limit is 16 MiB, matching the maximum macFUSE read; writes remain limited to 8 MiB. Larger direct adapter operations are split while preserving one expected revision.

`ListDirectoryView` is the native-mount projection. In one request it returns the current and parent directory metadata, metadata for every child, complete xattr names where supported, and values up to 4 KiB under a 256 KiB response-wide inline budget. Large values such as resource forks remain explicit ranged xattr reads. The server performs child stat/xattr work concurrently under `--max-directory-entry-tasks` and verifies the directory revision before replying; the adapter single-flights concurrent cold loads and answers subsequent Finder callbacks from that response.

Peers exchange capability information after authentication. Optional operations such as special nodes, server-side copy, data/hole seek, exchange, volume metadata, and persistent restart behavior are used only when the server advertises them.

### Nodes, handles, and revisions

Wire paths are not server filesystem paths. Clients walk directories from an opaque root node and send opaque node UUIDs plus individual lossless name components. The server derives node IDs from keyed backing device/inode identity; the key and export epoch are stored outside the export, so node IDs survive renames, hardlinks, reconnects, and a complete daemon restart.

Open handles are connection-local. Files and directories carry revisions that act as change indicators. Multi-chunk reads require one consistent revision, and reconnect reopens an old logical handle only when the same server epoch, node, and exact revision are still present. This prevents a recovered application read from combining bytes from two versions of a file.

### Reads, writes, and server-side work

Reads and writes are positioned and bounded. Independent reads can run concurrently; mutations on one logical handle are ordered, and append writes are serialized for the backing inode. The daemon enforces request, connection, concurrency, handle, node-registry, authentication-work, and in-flight byte budgets.

Whole-file and range-copy requests keep data on the server when possible. A Linux server attempts reflink cloning, then `copy_file_range`, then a bounded buffered fallback. Same-file overlapping copies have memmove-style semantics.

If transport fails during a read-only operation, the resilient client can reconnect and retry it. If transport fails during a mutation, its result may be ambiguous—the server might have applied it before the response was lost—so quicKFS reports the ambiguity instead of blindly performing the mutation twice.

### Reconnect, locks, and caching

Reconnect is bounded, single-flight, and authenticated with the original trust policy. A replacement connection must report the same persisted server epoch. Revision-matched handles are reopened and a surviving client's active advisory locks are replayed. Locks are not immortal server records: closing the handle or losing the owning client releases them.

The persistent cache is namespaced by authenticated certificate fingerprint, server epoch, and username. Entries are revision-keyed, integrity-checked, owner-private, atomically written, and LRU-evicted under a configured byte budget. A bounded 256 MiB process-local range tier keeps hot blocks out of the disk/hash path, and concurrent cold readers share one disk verification or remote fetch—including its failure—for an aligned block. Mutations invalidate the node plus every directory snapshot that embeds that node's metadata, covering rename/move, links, content, attributes, and xattrs without allowing an old enriched snapshot to resurrect a stale revision. After an online mount disconnects, already cached metadata and byte ranges can remain readable.

The cache never authorizes a cold-start offline mount and never queues writes. Without the server, a client cannot confirm current identity, epoch, account status, permissions, locks, quotas, or competing namespace changes. See [Caching and offline semantics](docs/caching.md) for the consistency rationale.

## Security and authentication pipeline

quicKFS separates **server trust** from **user authentication**. A password is never requested or transmitted until the client has authenticated the server certificate using one explicitly selected trust mode.

| Trust mode | Bootstrap |
| --- | --- |
| Pairing-assisted exact pin | A one-time 160-bit pairing code authenticates the first certificate fingerprint. |
| Managed exact pin | An administrator or device-management system imports the expected SHA-256 fingerprint/certificate. |
| Private CA | `--ca-cert` validates the certificate chain and `--server-name`. |
| System/public PKI | `--trust-system-roots` uses the operating-system trust policy and `--server-name`. |

An authenticated connection follows this pipeline:

1. The server loads its persistent TLS identity and protected user database.
2. The client performs a TLS preflight using the selected exact-pin or CA policy.
3. Only after successful server verification does the client prompt locally for the password.
4. The client opens a fresh QUIC connection and applies the same trust policy again.
5. It sends the username and password over that authenticated TLS connection.
6. The server verifies the salted Argon2id password hash in a bounded blocking worker pool.
7. Successful authentication enables filesystem requests for that connection.
8. A mutation is authorized only when both the daemon was started with `--allow-writes` and that account has an explicit write grant.

Pairing uses a restricted connection that cannot carry credentials or filesystem operations. Domain-separated HMAC-SHA-256 proofs bind the one-time secret, pairing ID, fresh nonce, and exact certificate fingerprint in both directions before the client stores a pin. Pairing records expire, are single-use after success, and a new certificate never silently replaces an existing pin.

On the server, filesystem access is descriptor-relative beneath a retained export-root descriptor. Parent components are reopened with no-follow checks, symlink escape is rejected, and clients never send host paths. Private identity/account/export-state directories require owner-only permissions, reject symlinked or unsafe files, and must not overlap the exported directory.

Login attempts and Argon2 work are bounded, raw bodies are length-checked before allocation, and every request requires an authenticated connection. Current limitations include shared export roots, no immediate revocation of an already authenticated session, no account recovery, limited distributed-guessing defense, and no independent formal audit. Read the full [authentication design](docs/authentication.md), [threat model](docs/threat-model.md), and [security policy](SECURITY.md) before deployment.

## Quick start

Rust/Cargo 1.88 or newer is required. A native macOS mount also requires [macFUSE](https://macfuse.io/) and `pkgconf`.

Build the workspace and create an export:

```sh
cargo build --workspace
mkdir -p shared mountpoint
printf 'Hello from quicKFS\n' > shared/hello.txt
```

Initialize a local identity, add an account, grant it write access, and create a one-time pairing:

```sh
target/debug/quickfs-server-daemon init \
  --state-dir .quickfs --server-name localhost
target/debug/quickfs-server-daemon user add \
  --state-dir .quickfs alice
target/debug/quickfs-server-daemon user grant-write \
  --state-dir .quickfs alice
target/debug/quickfs-server-daemon pair create \
  --state-dir .quickfs
```

Start the server in one terminal:

```sh
target/debug/quickfs-server-daemon serve \
  --bind 127.0.0.1:4433 \
  --export-root ./shared \
  --state-dir .quickfs \
  --allow-writes
```

On the client, pair using the printed pairing ID and enter the code at the hidden prompt:

```sh
target/debug/quickfs-client-cli \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --state-dir .quickfs-client \
  pair --pairing-id <PAIRING_ID>
```

Confirm direct protocol access:

```sh
target/debug/quickfs-client-cli \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --state-dir .quickfs-client \
  --username alice list /

target/debug/quickfs-client-cli \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --state-dir .quickfs-client \
  --username alice read /hello.txt --offset 0 --length 4096
```

On macOS, build and start the mount:

```sh
cargo build -p quickfs-filesystem-macfuse \
  --features macfuse --bin quickfs-mount

target/debug/quickfs-mount ./mountpoint \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --state-dir .quickfs-client \
  --username alice
```

The mount runs in the foreground. Press Control+C in that terminal to unmount. macFUSE gets three seconds for a graceful unmount before forced local detach begins. An independent process watchdog exits after eight seconds if a macOS unmount syscall itself wedges—for example because the server disappeared—so closing the mount never waits indefinitely for the network. You can also unmount it from another terminal with:

```sh
umount ./mountpoint
```

For remote deployment, enterprise certificates, managed pins, cache tuning, server limits, and every command option, use the [setup guide](docs/setup.md) and [usage reference](docs/usage.md).

## macOS and macFUSE boundaries

Ordinary Finder and application operations—including random I/O, create/delete, xattrs/resource forks, hardlinks, locks, sparse seek, volume rename, and backup time—work through the native mount. Some optional operations depend on what the installed macFUSE/macOS ABI advertises:

The adapter retains one enriched directory snapshot for Finder's callback burst and invalidates every affected local view after create, write, attribute/xattr change, link, rename/move, exchange, or remove. It deliberately does not request macFUSE's additional `FOPEN_CACHE_DIR` pin: that kernel cache can outlive a mutation. It also avoids synchronous kernel invalidation notifications from inside callbacks because macFUSE's single receive loop can deadlock on them.

- the tested macFUSE 5.3 kernel backend does not dispatch native `copy_file_range` or `readdirplus`, although ordinary copies work through ranged I/O and the ordinary `readdir` path already consumes enriched remote directory views;
- macFUSE dropped the distinct `exchangedata(2)` capability on macOS 11, while atomic rename swapping remains available;
- invalid UTF-8 path components may be rejected by macOS before reaching the adapter;
- special nodes require a Linux backing daemon and its normal host privileges.

See [Filesystem semantics](docs/filesystem-semantics.md) for the precise behavior and current host limitations.

## Documentation

- [Setup guide](docs/setup.md)
- [Usage and command reference](docs/usage.md)
- [Protocol version 6](docs/protocol.md)
- [Authentication and server trust](docs/authentication.md)
- [Filesystem semantics](docs/filesystem-semantics.md)
- [Caching and offline behavior](docs/caching.md)
- [Threat model](docs/threat-model.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Development and quality checks](docs/development.md)
- [Roadmap](docs/roadmap.md)

Contributors should also read [CONTRIBUTING.md](CONTRIBUTING.md). quicKFS is licensed under Apache-2.0.
