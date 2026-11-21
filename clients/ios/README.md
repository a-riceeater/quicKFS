# quicKFS for iPhone and iPad

The native iOS app: a SwiftUI client that pairs with servers, browses and transfers files, and previews them, streaming video and audio straight into the player. It matches the macOS app except for mounting, which iOS does not allow third-party apps to do for a network filesystem like this.

## Architecture

```text
SwiftUI app ── JSON over a C ABI ──────► quickfs-core-bridge (static library)
     │                                       └─ client-core: trust store, pairing,
     │                                          authentication, reconnect, file ops
     └── raw bytes, quickfs_bridge_stream_read ─► stream: CachedFilesystem
                                                  (read-ahead + bounded disk cache)
```

- **One core, two apps.** The project (`app/project.yml`) compiles the macOS app's platform-neutral `Core/` sources directly: the bridge client, models, Keychain, pairing, the server connection, the browser and transfer model, telemetry, the console log, and the preview loader. Everything under `app/quicKFS/` is iOS only: layout, previews, and the iOS app model. A fix to either half reaches both apps.
- **The bridge** is the same `quickfs-core-bridge` crate the Mac links, built for `aarch64-apple-ios` (devices) or `aarch64-apple-ios-sim` (Apple silicon simulators) by `app/scripts/build-core.sh`.
- **State**: the trust store and `servers.json` live in Application Support, passwords in the Keychain (this device only, after first unlock), the streaming cache in Caches, and downloads in Documents, which the Files app shows under On My iPhone › quicKFS.

## Streaming previews

A video does not need to be downloaded before it plays. The player is given a private URL scheme, so AVFoundation asks the app, not the network, for each byte range it wants: the header, the index (often at the end of the file), then the stretch after the playhead, and a new stretch on every seek. `MediaPlayback` answers those ranges from a *stream*, a file the bridge keeps open:

- `stream_open {session, node, cache_dir, cache_max_bytes}` returns `{stream, size, revision}`.
- `quickfs_bridge_stream_read(stream, offset, buffer, capacity, &length)` copies raw bytes into the caller's buffer. It returns null on success, or a JSON error reply. No hex, no JSON, no copy beyond the one into the buffer.
- `stream_close {stream}` releases the file.

Reads go through client-core's `CachedFilesystem`, the same layer the Mac's mount uses, so a player reading forward gets its adaptive read-ahead: the next blocks are already in flight before AVFoundation asks for them. Every block also lands in a persistent cache, so seeking back or watching again costs no network.

Resource use, per server session:

| What | Bound |
| --- | --- |
| Cache and read-ahead block | 2 MiB (the mount uses 16 MiB; a player cannot start before its first block arrives, so smaller blocks start sooner: about 0.5 s on a 4 MB/s relayed link) |
| Read-ahead window | grows to at most 64 MiB, with at most 32 MiB in flight on the wire |
| One read reply | at most 4 MiB; the player takes 1 MiB per response |
| Disk | the streaming cache budget per server, epoch, and account (2 GiB by default, set in Settings). It lives in Caches, which iOS may also clear when storage runs low |

Sessions for the same server, epoch, and account share one cache, because a cache directory admits one owner at a time. Clearing the cache is refused while a preview is open.

Images stream into memory the same way (up to 192 MiB) and decode at screen size through ImageIO, so a 100-megapixel photo never inflates to full resolution on a phone. Everything else, and any media AVFoundation cannot play (MKV, AVI, and WebM, for example), downloads to a private temporary file and opens in Quick Look, as on the Mac. The first 16 KiB are always one tap away as hex.

## What maps to what

| macOS app | iOS app |
| --- | --- |
| Sidebar of servers, detail tabs | Server list; tap a server to browse; Overview, Console, and Server Settings from a folder's More menu |
| Finder-style table, back and forward, path bar | Folders push and swipe back; the title menu jumps to any parent, goes to a typed path, or copies the path |
| Quick Look (whole file fetched first) | Preview: video and audio stream, images stream, other files fetched for Quick Look |
| Drag onto a folder to move | Move, then pick a folder |
| Upload from Finder or a panel | Upload Files (Files app) or Upload Photos; picked files are copied into a private staging folder first, since the core reads uploads by path |
| Download to a chosen folder | Download to Documents, visible in the Files app; open or share from Transfers |
| Inspector (⌘I) with chmod | Get Info, with the same permission grid |
| Transfer window | Transfers list and detail: speed chart, link and protocol counters, Copy Report, Resume |
| Overview, Console | The same cards and readouts, laid out for touch |
| Server settings | The same, without the mount section |
| Offline cache size and Clear | Settings: streaming cache size and Clear |
| Mount and eject | Not available on iOS |

## Building

Requirements: Xcode 26, Rust with the iOS targets, and [XcodeGen](https://github.com/yonaskolb/XcodeGen).

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
brew install xcodegen
cd clients/ios/app
xcodegen generate
open quicKFS-iOS.xcodeproj
```

The deployment target is iOS 18, so the app runs on simulators that ship with older Xcodes. On iOS 26 it uses Liquid Glass throughout; `Glass.swift` falls back to materials on iOS 18.

Simulator build from the command line:

```sh
xcodebuild -project quicKFS-iOS.xcodeproj -scheme quicKFS \
  -destination 'platform=iOS Simulator,name=iPhone 16' -derivedDataPath build/DerivedData build
```

### Signing

Simulator builds need no signing. To run on a device, create the gitignored `app/Local.xcconfig` with your team, then regenerate:

```text
DEVELOPMENT_TEAM = ABCDE12345
```

### Checking a build without touching

Debug builds read launch arguments that drive the app, so a simulator check needs no taps:

```sh
xcrun simctl launch booted org.quickfs.client.ios \
  -debugServer "10.0.0.5:4433|user|password|<sha256>" \
  -debugPreview /Movies/clip.mp4
```

`-debugServer` pins the fingerprint and saves the server; `-debugBrowse`, `-debugPreview`, `-debugDownload`, `-debugPanel overview|console|settings`, and `-debugTransfers` then act once it connects. `-showAddServer` opens the add flow.

## Limits

- No mounting, and so no Files app provider: iOS reaches the server only through this app.
- iOS suspends apps in the background. A transfer running then stalls and, when the app returns, reconnects and continues from the last committed byte, or stops as resumable if the retry window passed. Audio keeps playing with the screen locked.
- AVFoundation decides which media play. Anything else opens as a document.
