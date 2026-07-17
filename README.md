# quicKFS

quicKFS is an experimental read-only network filesystem foundation for high-latency and unstable links. It uses QUIC directly through Quinn. The current prototype provides a Linux server, a platform-independent Rust client, and a macOS CLI; native mounting is not implemented yet.

```text
macFUSE adapter (planned) → client core → QUIC/TLS → Linux daemon → export directory
```

Implemented: authenticated sessions with a development token, metadata, directory listings, file open/close, arbitrary bounded ranged reads, opaque node IDs, and an in-memory cache interface. Planned: native macFUSE callbacks, reconnect/retry policy, persistent caching, writes, and Windows/WinFsp support.

## Build and run

Requires Rust 1.85 or newer (Rust 2024 edition). The repository's rustup toolchain file selects current stable Rust.

```sh
cargo build --workspace
./scripts/dev-cert.sh
cargo run -p quickfs-server-daemon -- serve --bind 127.0.0.1:4433 --export-root ./shared --cert ./certs/server.crt --key ./certs/server.key --token development-token
cargo run -p quickfs-client-cli -- --server 127.0.0.1:4433 --cert ./certs/server.crt --token development-token list /
```

Start with the [setup guide](docs/setup.md), [command reference](docs/usage.md), and [authentication explanation](docs/authentication.md). The [documentation index](docs/README.md) links architecture, development, protocol, troubleshooting, security, and roadmap material. Contributors should also read [CONTRIBUTING.md](CONTRIBUTING.md).

> **Security warning:** this is an experimental prototype. The development token scheme is not production authentication. Do not expose it to the public Internet.

Licensed under Apache-2.0.
