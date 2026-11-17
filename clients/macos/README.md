# macOS client

`client-cli` exercises the server without mounting. `filesystem-macfuse` provides the `quickfs-mount` executable and native read/write callbacks for ordinary namespace operations, random I/O, xattrs/resource forks, hardlinks/special nodes, locks, server-side copy, data/hole seek, exchange/backup/volume operations, and inode eviction. Protocol v6 loads a directory through one enriched request and serves Finder's follow-up metadata/xattr callbacks locally. It bridges callbacks through one retained multi-thread Tokio runtime and an authenticated reconnecting `RemoteFilesystem`, with a persistent read-through cache for safe offline reads, a bounded 256 MiB process-local hot range tier, and single-flight persistent/network block loads. Some optional callbacks are unavailable when the installed macFUSE/macOS ABI does not advertise them; the exact boundaries are documented in [filesystem semantics](../../docs/filesystem-semantics.md).

Every macOS client process performs a lightweight runtime preflight against the installed macFUSE filesystem bundle. `quickfs-client-cli` and `quickfs-mount` stop with the official installation URL when it is absent. The app performs the same check through the bridge at launch and shows an installation screen until macFUSE is present. The result is intentionally not persisted: checking two local filesystem entries on each launch is cheap and avoids stale state after macFUSE is installed or removed. Download macFUSE from the [official website](https://macfuse.io/).

## Desktop app

`app` is the native SwiftUI client for macOS 26 (Liquid Glass), generated with XcodeGen and updated through Sparkle. It links `core-bridge`, a static library over `quickfs-client-core`, so pairing, trust, authentication, browsing, and transfers run through the same code as the CLI rather than parsing CLI output, and it launches the bundled `quickfs-mount` helper for each mounted server. See [app/README.md](app/README.md) for the architecture, build, and release steps.

```sh
brew install xcodegen pkgconf
cd clients/macos/app
xcodegen generate
open quicKFS.xcodeproj
```

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

Keep the foreground process running. Open the mountpoint in Finder and press Control+C to unmount, or run `umount "$HOME/Volumes/quickfs"` from another terminal. If graceful macFUSE unmount stalls, forced local detach starts after three seconds; an independent watchdog exits the mount process after eight seconds even if the operating-system call wedges. It does not wait indefinitely for a dead server. The project never installs or approves the macFUSE system extension itself.
