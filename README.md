# quicKFS

quicKFS is an experimental authenticated network filesystem for high-latency and unstable links. It uses QUIC directly through Quinn. The current prototype provides a Linux server, a platform-independent Rust client/CLI, and a feature-gated read/write macFUSE mount for macOS.

```text
Finder → macFUSE adapter → client core → QUIC/TLS → Linux daemon → export directory
```

Implemented: mutual-proof pairing, managed pins/PKI, Argon2id users with per-account write grants, bounded authenticated sessions, protocol-v5 lossless names, persistent node identity, full ordinary read/write namespace operations, xattrs/resource forks, hardlinks and Linux special nodes, byte-range locks, server-side range copy, data/hole seek, exchange/backup/volume operations, authenticated reconnect, persistent offline read caching, and native macFUSE callbacks. The current macFUSE kernel ABI does not surface every optional protocol operation; see [filesystem semantics](docs/filesystem-semantics.md) for the exact platform boundaries. Offline writes and cold-start-offline mounts are intentionally not implemented; Windows/WinFsp remains future work.

On macOS, the desktop app and client commands require [macFUSE](https://macfuse.io/). They verify the installed runtime at process startup and stop with installation guidance when it is missing; server commands and non-macOS clients are unaffected.

## Build and run

Requires Rust 1.88 or newer. The repository uses Rust 2024 and its rustup toolchain file selects current stable Rust.

```sh
cargo build --workspace
cargo run -p quickfs-server-daemon -- init --state-dir .quickfs --server-name localhost
cargo run -p quickfs-server-daemon -- user add --state-dir .quickfs alice
cargo run -p quickfs-server-daemon -- serve --bind 127.0.0.1:4433 --export-root ./shared --state-dir .quickfs
```

Start with the [setup guide](docs/setup.md), which covers both initialize → user → serve → pair → login and enterprise CA/managed-pin deployment without per-client pairing. The [command reference](docs/usage.md), [authentication explanation](docs/authentication.md), and [documentation index](docs/README.md) provide the remaining protocol, security, troubleshooting, and development details. Contributors should also read [CONTRIBUTING.md](CONTRIBUTING.md).

> **Security warning:** authentication and per-account write authorization remain experimental. Do not expose the prototype to the public Internet without an independent review.

Licensed under Apache-2.0.
