# quicKFS for macOS

The native macOS app: a SwiftUI client for macOS 26 that pairs with servers, browses and transfers files, and mounts exports in Finder. It does everything the CLI and `quickfs-mount` do, through the same Rust core.

## Architecture

```text
SwiftUI app ── JSON over a C ABI ──► quickfs-core-bridge (static library)
     │                                   └─ client-core: trust store, pairing,
     │                                      authentication, reconnect, file ops
     └── launches per server ──────────► Contents/Helpers/quickfs-mount
                                            └─ macFUSE session (one process each)
```

- **`core-bridge`** (`clients/macos/core-bridge`) exposes `quickfs_bridge_call(json) -> json`. It shares the CLI's trust store (`trusted-servers.json`) and runs pairing, managed pin import, authenticated browsing sessions with reconnect, and pipelined transfers on one Tokio runtime. Calls block, so the app makes them off the main thread; transfers return an id that the app polls for progress.
- **`quickfs-mount`** runs as a separate helper because a FUSE session owns its process: SIGINT starts the graceful unmount and a watchdog exits if the kernel wedges. The app passes the Keychain password on stdin (`--password-stdin`), watches for the ready line, and sends SIGINT to eject. Quitting the app ejects every mount first.
- **State**: the trust store lives in `~/Library/Application Support/quicKFS/state`, saved servers in `servers.json` next to it, the offline read cache in `~/Library/Caches/org.quickfs.client`, and passwords in the login Keychain. Mounts default to `~/Volumes/<server>`.

## What maps to what

| CLI / mount | App |
| --- | --- |
| `pair`, `trust import --sha256/--certificate`, `--ca-cert`, `--trust-system-roots` | Add Server: pairing code, fingerprint, certificate, enterprise CA, system trust |
| `forget` | Server settings: Forget Identity, then Pair |
| `ping`, `capabilities`, `filesystem-stats` | Overview: latency graph, capability tiles, storage ring |
| `list`, `stat` | Files browser and inspector (⌘I) |
| `read` | Quick Look (Space or double-click), first 1 MiB |
| `write --offset --sync` | Context menu: Overwrite from File |
| `create`, `mkdir`, `truncate`, `remove`, `rename` | New menu, Resize, Delete (recursive), Rename, drag onto a folder to move |
| (none) | Upload and download files or whole folders, chmod in the inspector |
| `quickfs-mount` flags | Server settings: mount point, volume name, backend, cache, read-ahead, timeouts, reconnect |

## Building

Requirements: Xcode 26, Rust (see `rust-toolchain.toml`), [XcodeGen](https://github.com/yonaskolb/XcodeGen), `pkgconf`, and macFUSE.

```sh
brew install xcodegen pkgconf
cd clients/macos/app
xcodegen generate
open quicKFS.xcodeproj
```

The `.xcodeproj` is generated from `project.yml` and is not committed. The **Build quicKFS core** phase runs `scripts/build-core.sh`, which builds the bridge library and the mount helper with cargo (debug or release to match the configuration), and **Embed mount helper** signs the helper into `Contents/Helpers`. Cargo runs in a clean environment targeting macOS 26; a later terminal build without that target recompiles the C crypto crates once.

Command-line build:

```sh
xcodebuild -project quicKFS.xcodeproj -scheme quicKFS -derivedDataPath build/DerivedData build
```

Debug builds are ad-hoc signed, so macOS treats each rebuild as a new app and asks once for Keychain access to a saved password.

## Releases and updates

Updates use [Sparkle](https://sparkle-project.org). The updater stays off until the build carries an EdDSA public key:

1. Run Sparkle's `generate_keys` once (it is in `build/DerivedData/SourcePackages/artifacts/sparkle/Sparkle/bin/` after a build). It stores the private key in your login Keychain and prints the public key.
2. Build and package:

   ```sh
   SIGN_IDENTITY="Developer ID Application: …" TEAM_ID=… \
   SPARKLE_PUBLIC_KEY=… NOTARY_PROFILE=… VERSION=0.2.0 BUILD=2 \
   scripts/release.sh
   ```

   This produces `build/release/quicKFS-<version>.zip` and a signed `appcast.xml`. Identity-signed builds get the hardened runtime and are notarized when `NOTARY_PROFILE` is set; the helper carries `disable-library-validation` so it can load macFUSE's `libfuse`.
3. Upload both files to the GitHub release that `SPARKLE_FEED_URL` in `project.yml` points at (`releases/latest/download/appcast.xml`).

## Debugging the bridge

`cargo run -p quickfs-core-bridge --example bridge-repl` reads one JSON request per line and prints each reply, which exercises exactly the app's code path:

```sh
echo '{"op":"resolve","host":"10.0.0.74"}' | cargo run -q -p quickfs-core-bridge --example bridge-repl
```

Debug builds also accept `-serverTab files|overview|activity|settings` and `-showAddServer` as launch arguments for UI checks.
