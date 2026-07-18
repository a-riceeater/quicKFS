# quicKFS

quicKFS is an experimental read-only network filesystem foundation for high-latency and unstable links. It uses QUIC directly through Quinn. The current prototype provides a Linux server, a platform-independent Rust client/CLI, and a feature-gated macFUSE mount for macOS.

```text
Finder → macFUSE adapter → client core → QUIC/TLS → Linux daemon → export directory
```

Implemented: mutual-proof pairing, managed exact pins, operating-system/public PKI and private-CA server validation, externally issued identity rotation, Argon2id-backed user accounts, throttled and resource-bounded authenticated sessions, metadata, directory listings, session-scoped file open/close, arbitrary bounded ranged reads, opaque node IDs, a read-only macFUSE adapter, and an in-memory cache interface. Planned: per-user export authorization, reconnect/retry policy, persistent caching, writes, and Windows/WinFsp support.

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

> **Security warning:** authentication remains experimental and authorization is not yet per-user. Do not expose the prototype to the public Internet.

Licensed under Apache-2.0.
