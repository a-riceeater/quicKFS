# c.md — Finder "Loading…"/disconnect investigation (2026-07-20)

Working notes for the live bug: **opening a quicKFS volume in Finder stalls, shows an
empty "Loading…" window, never populates, and the volume eventually disconnects — while
every terminal operation (`ls`, `cp`, `cat`, `stat`, `xattr`, …) works perfectly.**

## ✅ RESOLVED (2026-07-20, post-reboot validation)

**Root cause:** a macOS volume-registration race, not a filesystem bug. At mount time
`coreservicesd` builds its per-volume FileIDTree via Carbon FSRef/LaunchServices
probes; on a high-latency mount each probe costs a network round trip, registration
races an internal timeout, and once it loses, coreservicesd caches a permanent error
for that mounted volume — Finder then gets EIO (error -5 / -36) forever while direct
syscalls keep working. Fixes: mount-time root prewarm + statfs cache (registration
probes are served locally), a post-mount registration health check with bounded
automatic remount retries, READDIR/AppleDouble round-trip reductions, and the
vendored-fuser macFUSE capability-dialect corrections (whitelist verified against
macFUSE headers; RENAME_SWAP/EXCL restored).

**Validation on the real RAID mount** (fresh reboot, clean §6 protocol, first-ever
pass after 10+ historical failures):
- `open ~/QFS-RAID` → exit 0 (LaunchServices healthy); registration succeeded on the
  first attempt (no health-check remount during a 75 s stability window).
- Finder root window populated all 137 items in <15 s, stable at +75 s.
- Subfolders: `open` exit 0; media folder `100MSDCF` populated all 265 items in <30 s
  (empty-folder probe on `100GOPRO` correctly showed 0 items).
- No disconnect: mount survived ~6 min of Finder thumbnail load across 3 windows.

**Quality gates:** `cargo fmt --check` clean, `cargo clippy -p
quickfs-filesystem-macfuse --features macfuse --all-targets` clean, crate tests
21/21 pass. Vendored fuser is not a workspace member (`-p fuser` unavailable); it is
compile-checked via clippy as a path dep and covered by the adapter tests + live run.

Everything below is the investigation history that led here.

---

## 1. The user-visible failure, precisely

- `open <any folder on the volume>` (LaunchServices — the same path Finder/double-click
  uses) fails in ~1 s with `_LSOpenURLsWithCompletionHandler() failed with error 5`.
  Via `NSWorkspace.open` the error is `NSCocoaErrorDomain 256` wrapping
  `NSOSStatusErrorDomain code=5` (**EIO**) from `_LSOpenStuffCallLocal` (line 4224).
- A Finder window opened via AppleScript *does* appear, but its `target` is
  unresolvable (`-2763`), it shows 0 items forever ("Loading…"), and Finder never sends
  a single `READDIR` for it.
- Meanwhile the same volume serves a heavy background crawl fine (Spotlight/mds:
  thousands of LOOKUPs, 69 READDIRs, 459 OPENs, READs — all succeed).
- `ApplicationsStorageExtension` (macOS storage management) was found spinning at ~46%
  CPU statfs-polling the volume — a victim/amplifier of the same breakage, not a cause.
- The "disconnect" the user sees was not directly reproduced today (test mounts stayed
  up for hours). Best theory: Finder-side retry storms + macFUSE `daemon_timeout`
  ejection, or the user unmounting/rebooting; revisit once the LS failure is fixed.

## 2. Facts established by experiment (each reproduced)

1. **Every relevant syscall succeeds when issued directly** from a probe process on the
   broken volume: `statfs`, `getattrlist` (volume caps/name/UUID/CRTIME/BKUPTIME/
   USERACCESS), `getattrlistbulk` (all 207 root entries), `open`, `O_EVTONLY`, `mmap`,
   `read`, `listxattr/getxattr`, `fcntl(F_GETPATH)`, `realpath`. The EIO is generated
   inside LaunchServices' own volume handling, not returned by any syscall we could
   observe (we cannot trace `lsd` without root).
2. **Not the network/RAID**: an isolated loopback daemon mount fails identically.
3. **Not a recent regression**: the previous commit's client fails identically.
4. **Not quicKFS's filesystem code**: a 40-line hello-world filesystem written against
   the *vendored `fuser` crate* reproduces the LS failure; the *same* hello-world
   against macFUSE's own libfuse (C, `/usr/local/lib/libfuse`) has **never failed once**
   (~12/12 trials across every era of the session).
5. **The real quicKFS mount has never passed the LS probe** (~10+ attempts, including
   after the fixes below, after 90 s of quiet, on a fresh mountpoint).
6. **The fuser-based hello fs is nondeterministic** (~40–60% pass) and — critically —
   its pass/fail flips were shown to be **uncorrelated with every fs-side knob we
   varied** (see §4) and instead **correlate with environmental churn**: failures
   cluster right after rapid mount/unmount cycles and while stale Finder windows on
   broken volumes exist; passes follow quiet periods. Closing 14 stray broken-volume
   Finder windows flipped a failing series to 6/6 passes. macOS (lsd/Finder) evidently
   caches per-volume failure state for a while.
   **Methodology lesson: a single `open` probe is meaningless. Only trust results
   preceded by ≥60–90 s of no mount churn, no broken windows, and paired with an
   interleaved control.**

## 3. Real defects found and FIXED in the working tree (uncommitted)

These are genuine correctness bugs, worth keeping regardless; they are **not yet proven
to fix the Finder symptom** (the real mount still fails the LS probe after them).

1. **fuser negotiates FUSE INIT capabilities in the wrong dialect** —
   macFUSE's kernel capability word is *not* Linux's. macFUSE 5.3 offers
   `0xEF800008` (bits 3, 23–27, 29–31). Bits 29/30/31 are macFUSE's
   CASE_INSENSITIVE/VOL_RENAME/XTIMES, but fuser *also* interprets the same bits as
   Linux SETXATTR_EXT/FUSE_INIT_EXT/INIT_RESERVED, and interprets kernel bit 30
   (VOL_RENAME) as "extended init" — reading a `flags2` field that doesn't exist in
   macFUSE's init struct. The old INIT reply advertised bits 25/26/27 back to the
   kernel (25/26 added deliberately by our adapter believing they are macFUSE's
   RENAME_SWAP/RENAME_EXCL — see §5 Q3). Apple's libfuse replies `0x10` only.
   *Fixed:* `vendor/fuser/src/ll/flags/init_flags.rs` adds a documented
   `MACFUSE_KNOWN = 0xF800_007F` whitelist (shared legacy bits 0–6 + macFUSE bits
   27–31); `vendor/fuser/src/ll/request.rs` masks the macOS INIT reply with it and
   cfg-gates the `FUSE_INIT_EXT`/`flags2` parsing to non-macOS;
   `vendor/fuser/src/lib.rs` stops stripping bit 30 from reported capabilities on
   macOS.
2. **`subtype=quickfs` mount option removed on macOS**
   (`clients/macos/filesystem-macfuse/src/native.rs`): `subtype=` is a Linux mtab
   concept macFUSE's mount helper does not define; passing undefined options to the
   helper is the same class of dialect divergence.
3. **Field diagnosability**: `QUICKFS_FUSE_DEBUG=1` now streams every FUSE request the
   kernel dispatches (+ every error reply) to stderr
   (`clients/macos/filesystem-macfuse/src/main.rs` env-gated logger,
   `vendor/fuser/src/reply.rs` errno logging, `log` dep added to the crate). This is
   how all traces below were captured; keep it.
4. Related guard already present and confirmed important: `daemon_timeout` is clamped
   to macFUSE's max of 600 s (`native.rs::macfuse_daemon_timeout_secs`). In three
   trials, `daemon_timeout=660` (over the max) poisoned LS deterministically — though
   with the churn caveat of §2.6, treat as "probably real, keep the clamp".

## 4. Theories tested and ELIMINATED (with the churn caveat)

- **INIT reply flag content beyond the dialect fix** — clamp/bit-flip experiments were
  eventually shown to send *byte-identical* replies in most variants (the requested set
  only ever contained bits 29–31), so earlier "bit 26 is poison" bisection results were
  churn noise. With genuinely different flag sets (0x0 / single bits / 0xE0000000) the
  outcomes flapped independent of the value.
- **Mount options individually** (`default_permissions`, `noexec`, `noatime`,
  `subtype`, `auto_xattr`, `daemon_timeout=600`, `nodev`) — each passed alone;
  combination failures didn't replicate consistently → churn noise.
- **xattr reply style** — libfuse emulates `com.apple.FinderInfo` (32 zero bytes) when
  the fs has no getxattr; fuser replies ENOSYS; forcing ENOATTR instead: 3 pass /
  3 fail → not the discriminator.
- **volname** (`quicKFS` vs fresh names) — no correlation.
- **fsname / bare `mntfromname`** (`quickfs` vs `name@macfuseN`) — libfuse with
  `fsname=quickfs` (bare fromname) still passed.
- **`noexec`** removal — no effect.
- **`/.vol`/`fsgetpath` (file-ID resolution)** — unsupported (ENOTSUP) on *healthy*
  libfuse macFUSE volumes too → normal for macFUSE, not our bug.
- **DiskArbitration registration** — DADisk exists (kind `macfuse`, network, no BSD
  device) identically for broken fuser mounts and working libfuse mounts.
- **Volume attributes** — name/UUID/caps/CRTIME/statfs values all healthy and sane on
  the broken mounts.
- **FUSE_EXPORT_SUPPORT** — advertising it does not enable volfs and does not help.
- **GETXTIMES / ACCESS** — the kernel never even sends them in the failing window.
- **Adapter callback errors** — none; the only error replies during the failing probe
  are benign `ENOENT` for `._` AppleDouble sidecar lookups (auto_xattr).

## 5. Open questions / remaining suspects

1. **What syscall/return does `lsd` itself see?** Everything points at LS's volume
   bookkeeping, and we've been inferring it blind. `sudo fs_usage -w -f filesys | grep
   -E 'lsd|Finder'` (or `ktrace`) during one `open` failure would name the exact
   failing operation and errno in seconds. Needs the user (no passwordless sudo).
2. **Does libfuse perform post-mount device ioctls that the vendored shim skips?**
   The shim calls `fuse_mount()` + `fuse_chan_fd()` but never `fuse_new()`/loop, so any
   session-setup side effect in libfuse (osxfuse historically issued macFUSE device
   ioctls like `FUSEDEVIOCSETDAEMONDEAD`, "implemented-bits", etc. during session
   creation) never happens on our fd. If mount_macfuse/the kext gates "volume fully
   usable" on such a signal, LS would see a half-initialized volume — and libfuse's
   perfect 12/12 record vs fuser's flapping fits that. **Unexplored; strong lead.**
   How to check: osxfuse's libfuse source (`fuse_lowlevel.c`, `fuse_kern_chan.c`,
   `fuse_session_new`/`fuse_new` on the macFUSE fork) or `nm`/`strings` on
   `/usr/local/lib/libfuse.2.dylib` for `FUSEDEVIOC*` usage, then replicate the ioctls
   in the shim.
3. **What do macFUSE 5.3 kernel capability bits 23–26 actually mean?** The adapter's
   `init()` requests bits 25/26 as "RENAME_SWAP/RENAME_EXCL (macFUSE names for these
   bits)" — if that mapping is right, the new `MACFUSE_KNOWN` mask now suppresses
   advertising them and macOS swap-renames (`renamex_np(RENAME_SWAP)`, Finder atomic
   saves) may regress to non-atomic fallbacks on the mount. Verify against macFUSE
   headers (macFUSE.fs bundle / macFUSE SDK `fuse_kernel.h`) and widen the whitelist
   with the *verified* bits. Also re-run the media smoke test to confirm no rename
   regression.
4. **Is this Mac's LaunchServices state poisoned for quicKFS volumes specifically?**
   Weeks of broken mounts may have persisted bad per-volume records. Cheap checks:
   `lsregister -dump | grep -i quickfs` (look for volume entries), then
   `lsregister -kill -r -domain local -domain system -domain user`, or simply a
   **reboot**, then a single quiet-mount Finder test. If a reboot fixes Finder with the
   current fixed binary, the remaining "bug" is cached OS state, and the fuser-side
   dialect fixes + churn hygiene are the real story. **Do this before deeper code
   work — it's the cheapest decisive test.**
5. **INIT reply numeric fields** — fuser replies `max_background=16`,
   `congestion_threshold≈12`, `max_write=16 MiB`; libfuse replies `0/0/32 MiB`.
   Never isolated. Easy experiment: make fuser's INIT reply byte-identical to
   libfuse's and A/B with the quiet-oracle protocol.
6. **Why does the *real* quicKFS mount fail the probe even when hello-fuser passes?**
   Untested remaining constants: attr/entry TTL durations the adapter replies,
   generation numbers, `blksize` values, out-of-order async replies (adapter replies
   from a Tokio thread, hello replies inline), and the volume's actual content
   (`.DS_Store` exists on RAID root). Each is testable in the hello harness
   (add TTL/generation/async-reply knobs; drop a `.DS_Store`).

## 6. How to test reliably (hard-won protocol)

- Quiet ≥60–90 s with **zero** macFUSE mounts and **no stray Finder windows** before
  each trial; close windows with AppleScript after every trial
  (`tell application "Finder" to close every window` — careful, closes the user's too).
- One mount → settle ≥60 s → one probe → record → unmount → quiet again.
- Always interleave a known-good control (libfuse hello fs:
  `scratchpad/hellofs.c`, build:
  `cc -o hellofs hellofs.c -I/usr/local/include/fuse -D_FILE_OFFSET_BITS=64 -D_DARWIN_USE_64_BIT_INODE -L/usr/local/lib -lfuse`).
- Probes: `open <mountpoint>` (exit 0 = LS healthy) and, for the real Finder behavior,
  AppleScript-open + `count of items of front window` after a wait (populated window =
  真 success; Finder can lag minutes on a 76 ms link while crawling).
- The mount binary logs every kernel request with `QUICKFS_FUSE_DEBUG=1` (stderr).
- Mounting non-interactively needs `expect` (password prompt); scripts in
  `scratchpad/mount-*.exp`.
- RAID test creds: server `10.0.0.74:4433`, server-name `RAID`, user `chat` (read-only),
  mountpoint `~/QFS-RAID`, state dir `.quickfs-client` (already trusts the server).
- Disk space is chronically near-full on this Mac; build with
  `CARGO_PROFILE_DEV_DEBUG=0` and clean scratch `target/` dirs afterward.

## 7. Recommended plan from here (in order)

1. **Reboot (or `lsregister -kill …`), then one clean-protocol Finder test** of the
   fixed binary on RAID. If Finder now works: done with the client side — write it up,
   run the quality gates, commit; keep the churn-hygiene notes for future testing.
2. If still broken: **get one `sudo fs_usage` trace** of an `open` failure (user runs
   `sudo fs_usage -w -f filesys | egrep 'lsd|Finder|open'` in a second terminal while
   we trigger the probe). This turns the remaining mystery into a named syscall.
3. Chase §5.2 (libfuse post-mount ioctls) — diff libfuse's device-fd interaction with
   the shim's, replicate what's missing.
4. Verify §5.3 (bits 25/26 = rename caps?) against macFUSE headers and re-run the
   media smoke test for rename/atomic-save behavior.
5. Only then resume fine-grained INIT/reply-field bisection with the §6 protocol.
6. When Finder finally browses: watch for the *disconnect* half of the symptom
   (daemon_timeout ejection under Finder's statfs/thumbnail storms on the 76 ms link)
   before declaring victory, and re-run `scripts/media-workload-smoke.py` plus the
   crate's quality gates (`cargo fmt/clippy/test` — workspace gate needs disk).

## 8. Inventory of session artifacts (scratchpad, disposable)

`/private/tmp/claude-501/-Users-elijb-Developer-quicKFS/91b69de9-.../scratchpad/`:
`hellofs.c` (libfuse control), `hellofs2.c` (+xattr/access logging variants),
`hf-fixed/` (fuser hello fs vs the fixed vendor; env knobs `VOLNAME`, `QF_OPTS`,
`DT`, `XATTR_ENOATTR`), `attrprobe.c`, `lsprobe.py`, `voluuid.c`, `crtime.c`,
`fsgp.c` (fsgetpath), `daprobe.swift` (DiskArbitration), `wsopen.swift`
(NSWorkspace error surface), `interpose.c` (syscall interposer),
`mount-*.exp` (expect mount scripts), `raid-fixed.log` (full FUSE trace of the
fixed binary on RAID incl. the mds crawl), `wt-prev/` (worktree @ 87cd4cf5,
pristine fuser), `wt-dbg/` (worktree with experiment edits — superseded by the
main-tree fixes; safe to delete along with `local/` test daemon state).

Local test daemon for loopback repro: state in `scratchpad/local/` (user `alice`,
password `testpass123456`, port 14433).
