# Development

Follow the [setup guide](setup.md) for prerequisites and a working local server/client session. The [usage reference](usage.md) documents every current CLI option.

## Quality checks

Run the complete local gate before submitting changes:

```sh
./scripts/check.sh
```

It runs:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

Use `./scripts/test.sh` when only the workspace test suite is needed. Tests must not depend on a globally installed macFUSE extension.

## Focused commands

Run one crate's tests:

```sh
cargo test -p quickfs-protocol
cargo test -p quickfs-server-core
```

Run one named test with output:

```sh
cargo test -p quickfs-server-core rejects_symlink_escape -- --nocapture
```

Inspect the command interfaces directly:

```sh
cargo run -p quickfs-server-daemon -- serve --help
cargo run -p quickfs-client-cli -- --help
```

Enable server diagnostics with `RUST_LOG`, for example:

```sh
RUST_LOG=quickfs_server_daemon=debug cargo run -p quickfs-server-daemon -- serve ...
```

## Change boundaries

- Wire types and encoding belong in `crates/protocol`; update `docs/protocol.md` with every protocol change.
- Quinn and rustls details belong in `crates/transport-quic`.
- Filesystem policy and export-root confinement belong in `crates/server-core`.
- Platform-independent client behavior belongs in `crates/client-core`.
- macOS bindings and synchronous callback bridging belong under `clients/macos`.
- Use `thiserror` in libraries and reserve `anyhow` for binary entry points.
- Preserve `#![forbid(unsafe_code)]` unless a platform binding makes narrowly isolated unsafe code unavoidable.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for pull-request and protocol-change expectations.

