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
- **Telemetry**: link figures come from QUIC's own counters. While a server is mounted the helper writes them every second to `--stats-file` (in `Application Support/quicKFS/telemetry`); otherwise the app samples its browsing session. The helper also gets `--exit-with-parent`, so a crashed app never leaves an orphaned mount.
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
| `QUICKFS_FUSE_DEBUG=1` | Console: Trace, then Remount |
| (none) | Overview: live throughput, latency, loss, totals; Console: event stream, QUIC counters, helper invocation |
| (none) | Settings: offline cache size and Clear (only while nothing is mounted) |

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

### Signing

`Signing.xcconfig` defaults to ad-hoc signing, which needs no Apple account but gives every build a new identity, so macOS asks again for Keychain access after each rebuild. To sign with your own certificate instead, create the gitignored `Local.xcconfig` beside it:

```text
DEVELOPMENT_TEAM = ABCDE12345
CODE_SIGN_IDENTITY = Apple Development
```

then run `xcodegen generate`. `security find-identity -v -p codesigning` lists your identities, and the team ID is the certificate's OU (`security find-certificate -c "Apple Development" -p | openssl x509 -noout -subject`). With a stable certificate, choose Always Allow the next time the Keychain asks and rebuilds stop prompting. `scripts/release.sh` uses the same settings unless `SIGN_IDENTITY`/`TEAM_ID` are given.

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
