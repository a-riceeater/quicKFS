# Troubleshooting

## The client cannot connect

Confirm the daemon is running and that the client address matches `--bind`:

```sh
RUST_LOG=debug cargo run -p quickfs-server-daemon -- serve ...
cargo run -p quickfs-client-cli -- \
  --server 127.0.0.1:4433 --server-name localhost \
  --username alice ping
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
quickfs-client-cli \
  --server <ADDRESS> --server-name <NAME> \
  --username alice ping
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

## Finder or `ls` takes many seconds to show a directory

Protocol v6 uses one `ListDirectoryView` request for a native directory. That response includes the directory/parent metadata, all child metadata, complete xattr names, and bounded small xattr values. macFUSE then serves Finder's `lookup`, `getattr`, `listxattr`, negative xattr probes, and common small xattr reads locally. There should not be a second wave of one network xattr request per child.

First verify the server daemon and mount were rebuilt from the same v6 revision. A v6 binary negotiates `quickfs/6`; it does not silently fall back to the old v5 pipeline. Restarting only the client is insufficient because the enriched response and parallel scan are implemented by the server.

Compare a direct list with the mount using the exact same endpoint, server name, state directory, and account:

```sh
time target/debug/quickfs-client-cli \
  --server 192.0.2.10:4433 \
  --server-name files.example.net \
  --state-dir .quickfs-client \
  --username alice list /

time ls -la "$HOME/Volumes/quickfs"
```

If both are slow on the first access, the backing filesystem is spending time in directory enumeration/child metadata I/O. v6 schedules that work concurrently beside the storage; `--max-directory-entry-tasks` defaults to `64` and is a bounded tuning knob for the server. Raising it can hurt a saturated rotational array, so measure before changing it. If the direct v6 list is fast but the cold mount is slow, confirm the mount is actually talking to the upgraded daemon and capture its foreground diagnostics; increasing `--callback-timeout-ms` only hides the symptom.

Directory views and all metadata/xattr state discovered inside them share the same 30-second lifetime. Repeated access in that window should therefore be local; child metadata must not expire after one second while the names remain cached. Concurrent cold callbacks for the same directory are single-flighted. If a first `ls` completes but an immediate second `ls` stalls for roughly 30 seconds, rebuild the mount from the current source: that pattern identifies the old mismatched child-metadata TTL, which caused a per-child `GetMetadata` wave and let the directory view expire during the wave.

After create, content/attribute/xattr change, link, rename/move, exchange, or remove, the adapter invalidates every directory view embedding the affected node. Native directory opens do not request `FOPEN_CACHE_DIR`, so a completed namespace mutation is visible to the next open/list rather than hidden behind a second kernel cache. Do not add synchronous fuser invalidation notifications inside these callbacks; macFUSE's one receive loop can deadlock while delivering them.

The client sends a transport keepalive every 10 seconds and permits five idle minutes. The mount allows 60 seconds per transport phase and 120 seconds for the complete callback; the daemon's default full-request deadline is 120 seconds. A stall at an older 10/30/45-second boundary indicates an old binary or an explicitly overridden timeout.

## A ranged read fails

The default maximum read request is 16 MiB (`16777216` bytes); writes remain 8 MiB. Use a smaller `--length` or deliberately adjust the server's `--max-read-size`.

The persistent cache has a bounded 256 MiB process-local hot range tier. Sequential/copy-sized reads use aligned blocks up to the configured 16 MiB default; a request below 1 MiB uses at most a 1 MiB aligned block. Concurrent reads of one aligned block share its persistent lookup or remote fetch—including a failed fetch—so one cold-disk timeout does not become a sequence of follower reconnects. Subsequent small slices should not reread or SHA-256-check the whole block. If repeated reads remain CPU-bound, confirm `quickfs-mount` was rebuilt from the same source revision and that Cargo selected the optimized `sha2` assembly feature.

Offsets and lengths are unsigned byte counts. Reads beyond EOF return fewer bytes rather than padding the response.

## A copy reports `fcopyfile failed: Network is down` or the destination is missing

Current macFUSE releases may implement an ordinary copy with ranged reads and writes rather than the optional native `copy_file_range` callback. `ENETDOWN` means the resilient read exhausted its authenticated reconnect path and the requested revision was not already cached; it is not a special `.RW2` or file-size rejection. Keep the mount in the foreground and inspect its first transport error, then compare a direct read using the same endpoint, name, state directory, and account.

Header probes below 1 MiB no longer expand into a cold 16 MiB request, while copy-sized reads retain the larger aligned blocks. The 60-second transport/120-second callback defaults cover a cold large read without letting one failure trigger serial follower reconnects. The server's request deadline defaults to 120 seconds as well; if it is explicitly lower than the client phase timeout, raise it or remove the override.

Root and subdirectory entries use the same read path. A root-only case where a mutation succeeds but a fresh listing omits the destination indicates a stale directory projection, not a successful no-op copy. Rebuild both peers, remount, and verify that the mount no longer requests `FOPEN_CACHE_DIR`; current mutation handling invalidates every active and persistent directory snapshot containing the affected node, including both parents of a move/rename and all hardlink parents.

For a non-destructive read check, copy to a local directory and compare bytes:

```sh
cp -p "$HOME/Volumes/quickfs/root-file.RW2" /tmp/root-file.RW2
cmp "$HOME/Volumes/quickfs/root-file.RW2" /tmp/root-file.RW2
cp -p "$HOME/Volumes/quickfs/subdir/large-file.RW2" /tmp/large-file.RW2
cmp "$HOME/Volumes/quickfs/subdir/large-file.RW2" /tmp/large-file.RW2
```

Test creates, copies into the mount, rename/move, replace, and remove only against a disposable writable export. A production/read-only server must never be used for mutation diagnostics.

## Address already in use

Another process may already own UDP port 4433. Stop the other server or select another port on both sides:

```sh
quickfs-server-daemon serve --bind 127.0.0.1:4443 ...
quickfs-client-cli \
  --server 127.0.0.1:4443 --server-name <NAME> \
  --username alice ping
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

Press Control+C in the mount terminal to unmount, or unmount from another terminal:

```sh
umount "$HOME/Volumes/quickfs"
```

Unmount is local and does not require a live server. If macFUSE's graceful unmount does not finish within three seconds, `quickfs-mount` runs a forced local detach. A separate eight-second watchdog exits the process even if the operating-system unmount call wedges; closing the macFUSE process releases the volume. If an older/stale mount process predates that fix, force it manually on macOS:

```sh
diskutil unmount force "$HOME/Volumes/quickfs"
```

The mount performs bounded single-flight reconnect and accepts only the same persisted server epoch. Revision-matched handles are reopened and their locks replayed; an ambiguous mutation is never retried. Previously cached reads can continue after a disconnect, but cache misses and all mutations fail closed. A mount cannot cold-start while the server is offline. If reconnect reports an epoch/revision conflict, restore the intended server state or unmount/remount rather than bypassing the stale-state check. Use `quickfs-client-cli` to isolate trust/authentication or remote filesystem problems without creating a mount.

If the GUI or a client command says macFUSE is missing even after installation, verify that `/Library/Filesystems/macfuse.fs/Contents/Info.plist` exists, complete any approval or restart requested by the installer, and launch quicKFS again. The preflight is repeated on every process launch, so there is no cached result to clear.

## An optional filesystem operation returns `ENOTSUP`

QuickFS negotiates optional operations at both the network and macFUSE boundaries. The current macFUSE 5.3 kernel backend on macOS 15.1 does not advertise native `copy_file_range`, `readdirplus`, or `exchangedata` messages. Normal application copies still work through ranged reads/writes, directory listings already arrive from the server with metadata, and atomic rename swapping is supported. [macFUSE dropped the distinct `exchangedata(2)` volume capability on macOS 11](https://macfuse.github.io/2020/10/30/macfuse-4.0.0.html), so that syscall cannot reach QuickFS on modern macOS even though the protocol and adapter callback exist. `SEEK_DATA`/`SEEK_HOLE` is handled through Darwin's seek ioctls and is supported.

[The FSKit backend requires macOS 15.4 or later](https://github.com/macfuse/macfuse/wiki/FUSE-Backends). On an earlier release, continue using the kernel backend; selecting `--macfuse-backend fskit` will fail before mounting. Invalid UTF-8 path components are also rejected by modern macOS before a FUSE filesystem receives them. These host boundaries are not fixed by retrying or weakening mount options.

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
