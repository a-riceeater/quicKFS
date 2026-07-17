# Setup guide

This guide creates a local server, user account, paired client, and read-only session. Linux is the initial server target; the daemon also runs on macOS for development.

## Prerequisites

Install Git and Rust/Cargo 1.85 or newer. Rust 1.85 is the first stable release supporting Rust 2024. The repository's `rust-toolchain.toml` selects current stable through rustup.

On Linux, distribution Cargo packages may be too old:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustc --version
cargo --version
```

Both versions must be at least 1.85. `command -v cargo` should normally report `$HOME/.cargo/bin/cargo`. macFUSE is not required; native mounting is not implemented yet.

## Build

```sh
git clone https://github.com/quickfs/quickfs.git
cd quickfs
cargo build --workspace
```

The debug binaries are `target/debug/quickfs-server-daemon` and `target/debug/quickfs-client-cli`.

## Create an export

```sh
mkdir -p shared/examples
printf 'Hello from quicKFS\n' > shared/hello.txt
printf 'Nested file\n' > shared/examples/nested.txt
```

The server exposes this directory as remote `/`. It does not create a missing export automatically.

## Initialize persistent server identity

```sh
cargo run -p quickfs-server-daemon -- init \
  --state-dir .quickfs \
  --server-name localhost
```

This creates a self-signed TLS identity, an empty user database, and pairing storage. It replaces the old manual `dev-cert.sh` workflow for normal use. Do not copy the private server state to clients.

For a remote server, choose its stable DNS name or IP address during initialization:

```sh
quickfs-server-daemon init \
  --state-dir /var/lib/quickfs \
  --server-name files.example.net
```

## Add a user

```sh
cargo run -p quickfs-server-daemon -- user add \
  --state-dir .quickfs \
  alice
```

Enter and confirm a password of at least 12 bytes at the hidden prompts. The server stores a salted Argon2id hash.

## Start the server

```sh
RUST_LOG=info cargo run -p quickfs-server-daemon -- serve \
  --bind 127.0.0.1:4433 \
  --export-root ./shared \
  --state-dir .quickfs
```

Use `--bind 0.0.0.0:4433` to accept remote connections, and allow UDP port 4433 through the relevant firewall. Keep the server running. Ctrl+C and SIGTERM initiate graceful shutdown.

## Create a one-time pairing

In another server terminal:

```sh
cargo run -p quickfs-server-daemon -- pair create \
  --state-dir .quickfs \
  --expires-seconds 300
```

The command prints a pairing ID and grouped high-entropy code. Transfer them to the client through a trusted channel. The code expires and is deleted after use.

## Pair the client

```sh
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  pair --pairing-id <PAIRING_ID>
```

Enter the pairing code at the hidden prompt. Successful pairing stores the server certificate fingerprint in `.quickfs-client/trusted-servers.json`; no certificate file is copied.

## Log in and use the client

```sh
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --username alice \
  ping
```

Enter the account password when prompted. Then browse and read:

```sh
cargo run -p quickfs-client-cli -- --username alice list /
cargo run -p quickfs-client-cli -- --username alice stat /hello.txt
cargo run -p quickfs-client-cli -- --username alice \
  read /hello.txt --offset 0 --length 4096
```

The address and server name default to local development values. The client refuses an unexpected server certificate before prompting for a password.

## Run development checks

```sh
./scripts/check.sh
```

See [usage](usage.md), [authentication](authentication.md), and [troubleshooting](troubleshooting.md) for more detail.
