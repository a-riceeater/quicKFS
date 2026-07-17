# Troubleshooting

## The client cannot connect

Confirm the daemon is running and that the client address matches `--bind`:

```sh
RUST_LOG=debug cargo run -p quickfs-server-daemon -- serve ...
cargo run -p quickfs-client-cli -- --server 127.0.0.1:4433 ... ping
```

QUIC uses UDP. Ensure UDP port 4433 is allowed by host and network firewalls. A successful TCP connection test does not verify QUIC reachability.

## The server identity is missing

Initialize the state directory once, then use the same directory when serving:

```sh
quickfs-server-daemon init --state-dir .quickfs --server-name localhost
quickfs-server-daemon serve --state-dir .quickfs ...
```

Do not repeatedly initialize or delete this directory. Doing so changes server identity and invalidates every client pin.

## The client says the server is not paired

Create a fresh pairing on the server:

```sh
quickfs-server-daemon pair create --state-dir .quickfs
```

Then pair from the client with the printed ID and enter the code at the prompt. Confirm that `--server` and `--server-name` are identical during pairing and later use because the trust record is keyed by both.

## Pairing fails

Pairing codes expire after five minutes by default and are single-use. Create a new pairing rather than reusing an old record. Check that the client received the full grouped code through a trusted channel and that both machines have reasonable clocks.

Do not send a username or password until pairing succeeds. The temporary certificate-accepting mode is used internally only by the `pair` command.

## Pinned server identity changed

An unexpected identity can indicate a man-in-the-middle attack, the wrong address, lost server state, or deliberate key replacement. Investigate before proceeding. If the administrator deliberately replaced the identity, explicitly remove the old pin and pair again:

```sh
quickfs-client-cli --server <ADDRESS> --server-name <NAME> forget
quickfs-client-cli --server <ADDRESS> --server-name <NAME> pair --pairing-id <NEW_ID>
```

There is no automatic insecure override.

## Username/password authentication fails

Verify the username and add it if necessary:

```sh
quickfs-server-daemon user add --state-dir .quickfs alice
quickfs-client-cli --username alice ping
```

After five failed attempts on one connection, that connection rejects further attempts. The CLI creates a new connection per invocation. Passwords are case-sensitive and are never intentionally logged. Do not include passwords, pairing codes, private state, or trust databases in issue reports.

## A path is not found

Remote paths start at the configured export root, not at the server filesystem root. If the server uses:

```text
--export-root /srv/project-share
```

then local `/srv/project-share/example.txt` is remote `/example.txt`.

The path resolver walks directory listings and is case-sensitive when the server filesystem is case-sensitive. Parent traversal such as `..` is not a supported remote navigation mechanism.

## Permission denied while listing

The daemon process needs operating-system permission to traverse directories and read files. A symlink resolving outside the export root is intentionally rejected as a confinement violation.

Check permissions without changing them broadly:

```sh
ls -ld /srv/project-share
ls -l /srv/project-share/example.txt
```

## A ranged read fails

The default maximum request is 8 MiB (`8388608` bytes). Use a smaller `--length` or deliberately adjust the server's `--max-read-size`.

Offsets and lengths are unsigned byte counts. Reads beyond EOF return fewer bytes rather than padding the response.

## Address already in use

Another process may already own UDP port 4433. Stop the other server or select another port on both sides:

```sh
quickfs-server-daemon serve --bind 127.0.0.1:4443 ...
quickfs-client-cli --server 127.0.0.1:4443 ... ping
```

## macFUSE will not mount

Native mounting is not implemented in the current repository. `clients/macos/filesystem-macfuse` is an adapter skeleton only. Use `quickfs-client-cli` to exercise the server without macFUSE.

## Build or test failures

### `feature edition2024 is required`

The project uses the stable Rust 2024 edition and requires Rust/Cargo 1.85 or newer. This error commonly occurs when Linux invokes an older Cargo package supplied by the distribution, such as Cargo 1.75.

Do not add `cargo-features = ["edition2024"]` to the manifests. That suggestion applies to Cargo versions from before the edition was stabilized and would not make the project build correctly on stable Cargo 1.75.

Install or update the rustup-managed stable toolchain:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
rustup show active-toolchain
rustc --version
cargo --version
cargo build --workspace
```

If `cargo --version` still reports the old version:

```sh
command -v cargo
```

Ensure `$HOME/.cargo/bin` precedes `/usr/bin` in `PATH`, then open a new shell or source `$HOME/.cargo/env` again. The repository's `rust-toolchain.toml` is honored only when Cargo is launched through rustup's proxy.

### Other build failures

Verify that rustup is honoring the repository toolchain:

```sh
rustup show active-toolchain
rustc --version
```

Then run individual checks to isolate the failure:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

When reporting a problem, include the failing command, operating system, Rust version, and sanitized logs.
