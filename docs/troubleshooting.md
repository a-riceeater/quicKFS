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

## The client says no exact pin is configured

Create a fresh pairing on the server:

```sh
quickfs-server-daemon pair create --state-dir .quickfs
```

Then pair from the client with the printed ID and enter the code at the prompt. Confirm that `--server` and `--server-name` are identical during pairing and later use because the trust record is keyed by both. In a managed deployment, import the centrally authenticated pin instead, or explicitly select `--ca-cert`/`--trust-system-roots`; those modes do not require a local exact pin.

## Pairing fails

Pairing codes expire after five minutes by default and are single-use after a successful proof. A wrong code does not consume the record. Check that the client received the full grouped code through a trusted channel and that both machines have reasonable clocks.

Do not send a username or password until pairing succeeds. The temporary certificate-accepting mode is used internally only by the `pair` command.

## Pinned server identity changed

An unexpected identity can indicate a man-in-the-middle attack, the wrong address, lost server state, or deliberate key replacement. Investigate before proceeding. If the administrator deliberately replaced the identity, explicitly remove the old pin and pair again:

```sh
quickfs-client-cli --server <ADDRESS> --server-name <NAME> forget
quickfs-client-cli --server <ADDRESS> --server-name <NAME> pair --pairing-id <NEW_ID>
```

There is no automatic insecure override.

For a centrally distributed replacement pin, use `trust import --sha256 …` or
`trust import --certificate …` after `forget`. Distribute the new value and
authenticate it through the organization's management channel before removing
the old pin.

## Enterprise certificate validation fails

The value passed as `--server-name` must match a DNS name or IP subject
alternative name in the leaf certificate. Check that the server was given the
complete PEM chain with the leaf first, intermediates after it, and a matching
unencrypted PEM private key. With `--ca-cert`, confirm the configured bundle
contains the intended issuing trust anchor. With `--trust-system-roots`, confirm
the root is present in the operating-system trust policy. Certificate validity
also depends on reasonable server and client clocks.

CA modes never fall back to pairing or an old exact pin when validation fails.
Fix the certificate, name, chain, clock, or trust deployment instead of bypassing
the error.

After `quickfs-server-daemon identity install`, restart the daemon. The running
process retains the identity it loaded at startup; the next start atomically
selects the newly installed generation.

## Username/password authentication fails

Verify the username and add it if necessary:

```sh
quickfs-server-daemon user add --state-dir .quickfs alice
quickfs-client-cli --username alice ping
```

After five failed attempts on one connection, that connection rejects further attempts. Reconnects remain subject to the per-source rolling rate limit. Passwords are case-sensitive and are never intentionally logged. Do not include passwords, pairing codes, private state, or trust databases in issue reports.

## Private state permissions are unsafe

On Unix, the server requires state directories to be owned by its effective user with mode `0700` and private files with mode `0600`; the client applies the same ownership and mode policy to its trust database. Both reject symlinks for private state. Repair a state directory only after verifying that every path is the intended one:

```sh
chmod 700 .quickfs .quickfs/pairings .quickfs-client
chmod 600 .quickfs/server.crt .quickfs/server.key .quickfs/users.json .quickfs/pairings/*.json
chmod 600 .quickfs-client/trusted-servers.json
```

The server refuses any nesting between the export root and state directory in either direction. Move them into disjoint directory trees instead of weakening that check.

User administration, identity installation, and client trust updates use
create-once lock files to prevent concurrent writers from losing each other's
changes. If a process is killed during one of those updates, it can leave
`.quickfs/.users.lock`, `.quickfs/.identity.lock`, or
`.quickfs-client/.trusted-servers.lock` behind. First verify that no QuickFS
administration, identity-installation, or pairing command is still running,
then remove only the stale lock file. Never delete a lock held by a live
process.

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

The native target requires macFUSE 4 or newer and `pkgconf`; default workspace builds deliberately do not link them. Install macFUSE from the [official website](https://macfuse.io/), install `pkgconf` separately, then build the feature-gated binary:

```sh
brew install pkgconf
pkg-config --modversion fuse
cargo build -p quickfs-filesystem-macfuse --features macfuse --bin quickfs-mount
```

If Cargo reports that `fuse.pc` is missing, first confirm that the macFUSE installation completed and that `pkg-config --modversion fuse` works in the same shell. For a non-Homebrew installation, add the directory containing `fuse.pc` to `PKG_CONFIG_PATH`; do not point it at an untrusted SDK. Apple Silicon systems may require explicit approval of the third-party system extension and a restart before the first mount. Follow the macFUSE installer/system-settings instructions rather than weakening macOS security controls.

Use an existing empty directory as the mountpoint and keep the foreground process running. If authentication fails, verify the mount uses the same `--server`, `--server-name`, and `--state-dir` as the successful CLI command. The pin/CA policy is checked before the password is requested and again before it is sent.

Unmount before terminating the process:

```sh
diskutil unmount "$HOME/Volumes/quickfs"
```

There is no reconnect policy yet. If the QUIC session or server disappears, unmount the volume, restore connectivity, and start `quickfs-mount` again. After macFUSE is installed, use `quickfs-client-cli` to isolate trust/authentication or remote filesystem problems without creating a mount.

If the GUI or a client command says macFUSE is missing even after installation, verify that `/Library/Filesystems/macfuse.fs/Contents/Info.plist` exists, complete any approval or restart requested by the installer, and launch quicKFS again. The preflight is repeated on every process launch, so there is no cached result to clear.

## Build or test failures

### `feature edition2024 is required`

The project uses the stable Rust 2024 edition and requires Rust/Cargo 1.88 or newer. This error commonly occurs when Linux invokes an older Cargo package supplied by the distribution, such as Cargo 1.75.

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
