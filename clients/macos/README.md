# macOS client

`client-cli` exercises the server without mounting. `filesystem-macfuse` provides the `quickfs-mount` executable and native read/write callbacks for ordinary namespace operations, random I/O, xattrs/resource forks, hardlinks/special nodes, locks, server-side copy, data/hole seek, exchange/backup/volume operations, and inode eviction. It bridges callbacks through one retained multi-thread Tokio runtime and an authenticated reconnecting `RemoteFilesystem`, with a persistent read-through cache for safe offline reads. Some optional callbacks are unavailable when the installed macFUSE/macOS ABI does not advertise them; the exact boundaries are documented in [filesystem semantics](../../docs/filesystem-semantics.md).

Every macOS client process performs a lightweight runtime preflight against the installed macFUSE filesystem bundle. `quickfs-client-cli` and `quickfs-mount` stop with the official installation URL when it is absent. The GUI performs the same check once during startup and replaces the pairing flow with a blocking installation screen. The result is intentionally not persisted: checking two local filesystem entries on each launch is cheap and avoids stale state after macFUSE is installed or removed. Download macFUSE from the [official website](https://macfuse.io/).

## Desktop GUI

`client-gui` is the Tauri 2 and Svelte desktop client. Its Rust process depends directly on `quickfs-client-core`; it does not parse or wrap CLI output. The initial shell uses a compact macOS utility-window layout with grouped input for the existing 27-character high-entropy code, a macFUSE startup gate, and System/Light/Dark appearance modes. Network pairing and the post-pairing browser are the next implementation slice.

Install Node.js 20 or later, then run the native development application:

```sh
cd clients/macos/client-gui
npm install
npm run tauri -- dev
```

Run the frontend checks without opening a native window:

```sh
npm run check
npm run build
```

The UI uses the macOS system font stack rather than redistributing an Apple font file. Pairing-code characters use the built-in monospaced stack for unambiguous case-sensitive entry.

## Native mount

Install macFUSE 4 or newer from the [official website](https://macfuse.io/) and install `pkgconf` separately, then pair the CLI (or provision CA trust), build, and mount:

```sh
brew install pkgconf
cargo build -p quickfs-filesystem-macfuse --features macfuse --bin quickfs-mount
mkdir -p "$HOME/Volumes/quickfs"
target/debug/quickfs-mount "$HOME/Volumes/quickfs" \
  --server 127.0.0.1:4433 \
  --server-name localhost \
  --username alice
```

Keep the foreground process running. Open the mountpoint in Finder and press Control+C for a graceful unmount, or run `umount "$HOME/Volumes/quickfs"` from another terminal. The project never installs or approves the macFUSE system extension itself.
