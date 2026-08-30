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

Use `./scripts/test.sh` when only the workspace test suite is needed. Tests do not depend on a globally installed macFUSE extension. On a Mac without the SDK, `check.sh` uses fuser's compile-only `macos-no-mount` mode for Clippy so the complete callback and binary API still receive static analysis; an actual native build and mount require macFUSE 4.

On Windows, run `scripts\check.ps1` instead. It applies the same formatting and
Clippy gate to the whole workspace but tests only the client half: `server-core`
and the daemon are largely `cfg(unix)` and most of their tests report
`NotSupported` on Windows, so they are covered on Linux by CI and by
`check.sh`.

Building anything under `clients/windows` links against WinFsp's import library.
It is located through `WINFSP_LIB_DIR`, then `WINFSP_INSTALL_DIR`, then the
machine-wide installation registry key, so a machine where WinFsp was installed
without its Developer feature — or where it cannot be installed at all — can
still build against an SDK unpacked with
`msiexec /a winfsp-<version>.msi /qn TARGETDIR=C:\winfsp-sdk` by setting
`WINFSP_INSTALL_DIR=C:\winfsp-sdk\DYNAMIC`. Mounting still requires a real
installation, because the driver comes from the installer. See the
[Windows client guide](windows.md).

The Tauri desktop client has an additional frontend quality gate:

```sh
cd clients/macos/client-gui
npm install
npm run check
npm run build
cargo check -p quickfs-client-gui
```

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
