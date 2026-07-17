# Setup guide

This guide creates a local Linux or macOS development session. The server is intended for Linux deployment, but the Rust daemon can also be used on macOS for development.

## Prerequisites

Install:

- Git;
- Rust 1.85 or newer, including Cargo, rustfmt, and Clippy;
- OpenSSL, used by the development certificate script.

Rust 1.85 is the minimum because it is the first stable release supporting the Rust 2024 edition. The repository's `rust-toolchain.toml` selects current stable Rust and installs rustfmt and Clippy automatically through rustup.

On Linux, distribution packages can provide an older compiler even on a current operating system. The recommended installation is rustup:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install stable --component rustfmt --component clippy
```

If rustup is already installed, update stable and let the repository override select it:

```sh
rustup update stable
rustup show active-toolchain
```

Verify the tools:

```sh
rustc --version
cargo --version
openssl version
```

Both `rustc` and `cargo` must report version 1.85 or newer. If Cargo still reports an older distribution version, verify which executable the shell finds:

```sh
command -v cargo
```

With rustup installed and its environment loaded, this normally reports `$HOME/.cargo/bin/cargo`.

macFUSE is not needed for the CLI workflow. Native mounting is not implemented yet, so installing macFUSE does not enable a mount command in the current version.

## Clone and build

```sh
git clone https://github.com/quickfs/quickfs.git
cd quickfs
cargo build --workspace
```

To build optimized binaries:

```sh
cargo build --release -p quickfs-server-daemon -p quickfs-client-cli
```

The resulting programs are:

```text
target/release/quickfs-server-daemon
target/release/quickfs-client-cli
```

## Create a local export

The server exposes one directory as its root. Create a development fixture:

```sh
mkdir -p shared/examples
printf 'Hello from quicKFS\n' > shared/hello.txt
printf 'Nested file\n' > shared/examples/nested.txt
```

Only files beneath this directory are available. The wire protocol does not expose the server's absolute paths.

## Generate a development certificate

From the repository root:

```sh
./scripts/dev-cert.sh
```

This creates:

```text
certs/server.crt
certs/server.key
```

The certificate is valid for `localhost` and `127.0.0.1` for 30 days. It is for local development only. The private key is ignored by Git and should not be shared.

The client needs a copy of `server.crt`, which is public, but must never receive `server.key`, which is secret. The certificate authenticates the server to the client; the development token separately authenticates the client to the server. See [Authentication and server trust](authentication.md) for the complete flow.

If the certificate expires or the hostname changes, regenerate it. The current script always creates a localhost certificate; certificates for remote hosts must contain the actual DNS name or IP address in their subject alternative names.

## Start the server

Run this in the first terminal:

```sh
RUST_LOG=info cargo run -p quickfs-server-daemon -- serve \
  --bind 127.0.0.1:4433 \
  --export-root ./shared \
  --cert ./certs/server.crt \
  --key ./certs/server.key \
  --token development-token
```

Keep this terminal open. Stop the server with Ctrl+C. On Unix, SIGTERM also initiates graceful shutdown.

## Connect with the client

In a second terminal, verify the session:

```sh
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --cert ./certs/server.crt \
  --token development-token \
  ping
```

Expected output:

```text
pong 42
```

Browse and read the fixture:

```sh
cargo run -p quickfs-client-cli -- \
  --cert ./certs/server.crt --token development-token list /

cargo run -p quickfs-client-cli -- \
  --cert ./certs/server.crt --token development-token stat /hello.txt

cargo run -p quickfs-client-cli -- \
  --cert ./certs/server.crt --token development-token \
  read /hello.txt --offset 0 --length 4096
```

The client defaults to `127.0.0.1:4433` and TLS server name `localhost`, so those options can be omitted for local use.

## Verify the development environment

Run all local quality checks:

```sh
./scripts/check.sh
```

This checks formatting, runs strict Clippy, executes workspace tests, and builds the Rust documentation.

Continue with the [usage and command reference](usage.md) for remote-server examples and every supported option.
