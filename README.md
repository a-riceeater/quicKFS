# quicKFS

quicKFS is an experimental authenticated network filesystem designed for high-latency, intermittent, and long-distance connections. A Linux server exports an existing directory, while a macOS client mounts it through macFUSE so Finder, command-line tools, and applications such as media editors can use ordinary filesystem operations instead of a separate transfer or synchronization workflow.

The transport is a purpose-built versioned protocol over QUIC/TLS. It supports random byte-range I/O, account-gated writes, reconnect with revision-safe handle recovery, persistent read-through caching, stable node identity across daemon restarts, and broad POSIX/macOS filesystem semantics.

> **Project status:** quicKFS is a substantial prototype, not a production-audited filesystem. Do not expose it directly to the public Internet without an independent security and operational review.

## Why quicKFS?

Traditional remote filesystems can be a poor fit for unstable or high-round-trip-time links, especially when an application repeatedly seeks through large media files. A workflow that needs several dependent network round trips to discover a file, fetch its metadata, open it, and read a small range multiplies the link latency at every step. Packet loss on a shared ordered transport can add another delay even when the lost bytes belong to an unrelated operation. Copying an entire project locally avoids those costs, but creates a separate synchronization and conflict problem.

quicKFS is designed to reduce that **latency amplification**, not merely to replace TCP with UDP:

| quicKFS design choice | Why it helps on a high-latency or lossy link |
| --- | --- |
| Metadata-bearing directory snapshots | One bounded directory request returns names, node IDs, types, sizes, modes, timestamps, revisions, link counts, and allocation data together. Finder-style lookup and listing do not need a network `getattr` request for every entry. |
| Useful state in operation responses | Open/create replies include the handle, size, and revision; writes return the resulting size and revision; metadata mutations return updated metadata. The client does not need a follow-up round trip just to learn the state produced by its previous request. |
| Large, explicit byte ranges | Applications can request only the portion of a large media file they need. Requests are split at the negotiated wire limit, while missing cache blocks and independent reads are fetched concurrently instead of serially. |
| One independent QUIC stream per operation | A delayed or retransmitted file range does not impose transport-level head-of-line blocking on unrelated streams, so metadata and other reads can continue making progress. QUIC still applies connection congestion and flow control; this is isolation, not immunity to a bad link. |
| Revision-aware caching | Metadata, directory snapshots, and byte ranges are reused under an exact file revision. Repeated seeks and Finder metadata probes can become local cache hits without pretending stale bytes are current. |
| Server-side work | Range copy, reflink/copy-range acceleration, sparse seek, and filesystem metadata operations execute beside the backing storage rather than downloading data merely to upload it again. |
| Explicit reconnect invariants | A reconnect must reach the same server epoch, and an open file is recovered only at its exact prior revision. Safe reads may retry; ambiguous writes never run twice automatically. |
| FUSE-aware protocol shape | The macFUSE adapter and wire protocol are developed together, so common kernel request patterns can be translated into one semantic remote operation rather than a generic sequence of path-based calls. |

This can make quicKFS a better fit than a conventional share for random-access creative workloads over a WAN, VPN, Wi-Fi, or other variable path. For example, listing a project directory transfers entry metadata once, opening a clip returns the revision needed for later range validation, and dozens of timeline reads can be in flight independently. The goal is fewer **dependent** round trips on the critical path, not simply fewer packets.

### How that comparison applies to SMB and other systems

quicKFS is not universally better than SMB. Modern SMB 2/3 is a mature protocol with [compound requests](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/fa6687f5-99d4-4c9b-ba2e-a770310225e0), [metadata-rich directory information classes](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/29dfcc9b-3aec-406b-abb5-0b4fe96712e2), request credits, leases, [durable-handle reconnect](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/75364667-3a93-4e2c-b771-592d8d5e876d), encryption, [Multichannel/RDMA](https://learn.microsoft.com/en-us/windows-server/storage/storage-spaces/manage-smb-multichannel), and [SMB over QUIC](https://learn.microsoft.com/en-us/windows-server/storage/file-server/smb-over-quic) on supported Windows deployments. A good SMB implementation can pipeline or combine work and will usually be the stronger choice on a reliable LAN or in an Active Directory environment because it has years of interoperability, ACL, policy, failover, monitoring, and vendor support.

The difference is focus and end-to-end control—not that SMB is incapable of batching or using QUIC. SMB must implement broad Windows and cross-platform semantics through general-purpose clients, and its WAN behavior depends on the client, dialect, credits, leases, transport, server, and mount settings in use. quicKFS owns both the macFUSE translation and its narrower wire contract, always returns the revision/coherency information required by its client, and maps each operation to an independent QUIC stream by default. [QUIC avoids head-of-line blocking between independent streams](https://www.rfc-editor.org/rfc/rfc9000.html#section-13). SMB over QUIC can provide the same underlying transport class, but retains SMB's protocol shape and currently targets supported Windows client/server combinations; quicKFS instead targets a Linux export and native macOS mount without requiring an SMB/Active Directory deployment.

[NFSv4.1](https://www.rfc-editor.org/rfc/rfc8881.html) likewise has compound operations, sessions, delegations, and parallel-storage support, and is a much more mature Unix filesystem protocol. quicKFS's narrower advantage is the combination of an application-aware macFUSE adapter, authenticated QUIC transport, revision-keyed ranges, and fail-closed reconnect/cache behavior. Compared with SSHFS/SFTP-style mounts, quicKFS adds filesystem-specific revisions, metadata snapshots, persistent read caching, server-side copy, lock replay, and independent QUIC request streams. Compared with rsync, cloud-drive, or object-storage synchronization, it exposes one authoritative live namespace and does not require whole files or a second mutable project tree to be synchronized later.

The tradeoff is maturity and scope: quicKFS does not yet provide SMB's enterprise integration, broad client ecosystem, per-user share roots/ACL model, clustered failover, or production audit history. It is intended to behave like a latency-conscious network filesystem, not a bidirectional sync product. Cold-start offline mounts, queued offline writes, automatic merging, and conflict reconciliation are deliberately not implemented.

## What it supports

| Area | Operations and behavior |
| --- | --- |
| File I/O | Create, open, positioned and append reads/writes, truncate, sparse files, preallocation, `mmap` through the kernel page cache, flush, `fsync`, and `fdatasync`. |
| Namespace | Directory create/remove, unlink, symlink/readlink, hardlinks, replace/no-replace rename, and atomic rename swapping. |
| Metadata | `getattr`/`setattr`, modes, timestamps, link counts, allocation data, statfs, macOS backup time, and volume renaming. |
| Application metadata | Extended attributes, Finder tags, quarantine metadata, custom xattrs, and persisted `com.apple.ResourceFork` sidecars. |
| Media-oriented access | Concurrent random byte ranges, requests larger than one wire chunk, `SEEK_DATA`/`SEEK_HOLE`, server-side range copy, safe `FIONREAD`, poll readiness, and logical `bmap`. |
| Coordination | Cross-client POSIX byte-range locks, directory sync, stable node IDs, revision checking, and reconnect-time handle/lock recovery. |
| Lifecycle | Metadata-bearing directory snapshots, `readdirplus` callback support, and inode `forget`/`batch_forget` eviction. |
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
QUIC/TLS 1.3  ── quickfs/5 ── independent request streams
          │
          ▼
Linux server daemon
          │
          ▼
descriptor-confined export directory
```

The platform adapter depends on the `RemoteFilesystem` interface rather than transport details. The same client pipeline used by the diagnostic CLI is wrapped by reconnect and cache layers and retained by the macFUSE mount for its entire lifetime. One multithread Tokio runtime services asynchronous work from synchronous FUSE callbacks.

The main components are:

- `crates/protocol`: version 5 requests, responses, capabilities, metadata, and limits;
- `crates/transport-quic`: Quinn endpoints, TLS trust policies, framing, timeouts, and peer identity;
- `crates/client-core`: network, resilient, cached, and delayed/test filesystem implementations;
- `crates/cache`: private persistent metadata, directory, symlink, statfs, and revision-keyed range cache;
- `crates/server-core`: confined filesystem operations, stable identity, revisions, xattrs, copies, locks, and persistence;
- `servers/linux/server-daemon`: authentication, authorization, QUIC dispatch, limits, and administration;
- `clients/macos/filesystem-macfuse`: native macFUSE callbacks and the `quickfs-mount` executable;
- `clients/macos/client-cli`: trust, authentication, administration, and direct protocol diagnostics.

See [Architecture](ARCHITECTURE.md) for the crate and trust boundaries.

## How protocol version 5 works

### Transport and framing

QUIC/TLS negotiates the version-specific ALPN identifier `quickfs/5`. Each filesystem operation normally uses an independent bidirectional QUIC stream on one authenticated connection, allowing concurrent reads and unrelated metadata requests to progress independently.

Control messages are Postcard-encoded and length-prefixed with a 1 MiB frame limit. Bulk read, write, and xattr data follows its control frame as an explicitly sized raw body rather than being embedded in serialization. The default negotiated read/write request limit is 8 MiB; the macFUSE adapter splits larger kernel requests while preserving one expected revision.

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

The persistent cache is namespaced by authenticated certificate fingerprint, server epoch, and username. Entries are revision-keyed, integrity-checked, owner-private, atomically written, and LRU-evicted under a configured byte budget. After an online mount disconnects, already cached metadata and byte ranges can remain readable.

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
  --state-dir .quickfs-client \
  --username alice list /

target/debug/quickfs-client-cli \
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

The mount runs in the foreground. Unmount it from another terminal with:

```sh
diskutil unmount ./mountpoint
```

For remote deployment, enterprise certificates, managed pins, cache tuning, server limits, and every command option, use the [setup guide](docs/setup.md) and [usage reference](docs/usage.md).

## macOS and macFUSE boundaries

Ordinary Finder and application operations—including random I/O, create/delete, xattrs/resource forks, hardlinks, locks, sparse seek, volume rename, and backup time—work through the native mount. Some optional operations depend on what the installed macFUSE/macOS ABI advertises:

- the tested macFUSE 5.3 kernel backend does not dispatch native `copy_file_range` or `readdirplus`, although ordinary copies work through ranged I/O and remote directory snapshots already include metadata;
- macFUSE dropped the distinct `exchangedata(2)` capability on macOS 11, while atomic rename swapping remains available;
- invalid UTF-8 path components may be rejected by macOS before reaching the adapter;
- special nodes require a Linux backing daemon and its normal host privileges.

See [Filesystem semantics](docs/filesystem-semantics.md) for the precise behavior and current host limitations.

## Documentation

- [Setup guide](docs/setup.md)
- [Usage and command reference](docs/usage.md)
- [Protocol version 5](docs/protocol.md)
- [Authentication and server trust](docs/authentication.md)
- [Filesystem semantics](docs/filesystem-semantics.md)
- [Caching and offline behavior](docs/caching.md)
- [Threat model](docs/threat-model.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Development and quality checks](docs/development.md)
- [Roadmap](docs/roadmap.md)

Contributors should also read [CONTRIBUTING.md](CONTRIBUTING.md). quicKFS is licensed under Apache-2.0.
