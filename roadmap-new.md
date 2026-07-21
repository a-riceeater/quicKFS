# e.md — Outstanding engineering work

A thorough, code-grounded backlog of what quicKFS still needs. This is the
deep companion to the terse `docs/roadmap.md`; where they overlap, this file
is the detailed version.

**Scope:** everything *except* new OS/platform ports. The repo already has
`clients/{linux,macos,windows}` and `servers/{linux,macos,windows}`
scaffolding; completing Windows (WinFsp) or a Linux FUSE client is explicitly
**out of scope for this document** (see §13). Everything here improves the
existing macOS/macFUSE client + Linux-daemon system.

Priority tags: **[P0]** correctness / users hit it today · **[P1]** important,
not yet painful · **[P2]** hardening / nice-to-have. "Where" points at the
code that would change.

---

## 0. Transport reality check: it is *not* HTTP/2, and HTTP/3 is not the upgrade

This needs stating clearly because it changes the whole transport backlog.

- **There is no HTTP anywhere in quicKFS.** No `hyper`, `reqwest`, `h2`, `h3`,
  or `http` crate is a dependency. The wire protocol is a **custom binary RPC
  (Postcard-serialized `Request`/`Response`/`Envelope`) sent directly over raw
  QUIC bidirectional streams** via `quinn`, negotiated with ALPN `quickfs/6`.
  See `crates/transport-quic/src/lib.rs` (`open_bi()`, `Envelope`) and
  `crates/protocol/src/lib.rs` (`PROTOCOL_VERSION`, `ALPN_PROTOCOL`).
- **HTTP/2 runs over TCP**, and this project uses no TCP for the filesystem
  path — so there is nothing "HTTP/2" to move off of.
- **HTTP/3 *is* HTTP semantics layered on top of QUIC.** quicKFS is already on
  QUIC — the exact transport HTTP/3 uses. Adopting HTTP/3 would mean wrapping
  our lean binary RPC in HTTP request/response framing (HPACK/QPACK headers,
  method/status/pseudo-headers, HTTP-level flow control), which **adds
  overhead and indirection** for zero functional gain in a private
  point-to-point filesystem protocol. It is a lateral-to-downward move here,
  not an upgrade.

**When HTTP/3 or WebTransport *would* be worth it** — capture the real need
behind the question rather than the framing:
- A **browser client** (WASM). Browsers cannot open raw QUIC, but they can
  speak **WebTransport** (HTTP/3-based). If a web UI ever needs to talk to the
  daemon directly, a WebTransport ingress is the standard path. **[P2]**
- **Proxy / CDN / corporate-middlebox traversal.** Raw QUIC on `:4433` can be
  blocked or not proxied; HTTP/3 rides standard infrastructure. Only relevant
  if deployment expands beyond LAN/direct WAN. **[P2]**
- **Standard tooling / interop** (curl, load balancers, observability). Only
  if third parties must speak the protocol. **[P2]**

So the actionable transport work below is about **using QUIC better**, not
about HTTP.

---

## 1. Transport & connection resilience

- **[P0] Root-cause the connection wedge.** Under sustained load a QUIC
  connection has been observed to wedge and compound reconnect/retry timeouts
  into a multi-minute outage for in-flight requests (seen once during mmap
  validation). Speculative fetches now deadline at 120 s so the fetch-slot
  pool recovers, but the **demand read path still waits out the full reconnect
  ladder**. Suspects to isolate: client keep-alive (`CLIENT_KEEP_ALIVE_INTERVAL
  = 10 s`) vs server idle policy (`FILESYSTEM_IDLE_TIMEOUT_MILLIS = 5 min`) vs
  Wi-Fi/NIC power-save vs quinn congestion state after loss.
  *Where:* `crates/transport-quic/src/lib.rs`, `crates/client-core/src/resilient.rs`.
- **[P1] Demand path should not block on the full reconnect ladder.** Reconnect
  is single-flight with `initial_backoff 100 ms → maximum_backoff 2 s`, but a
  wedged (not-yet-dead) connection can keep demand reads hanging. Add a hard
  per-request ceiling that abandons a wedged connection and forces a fresh one,
  rather than waiting for QUIC's own idle timeout to notice.
  *Where:* `ResilientFilesystem` in `resilient.rs`.
- **[P1] Single connection = single congestion controller.** Commit
  `be60fc63` is described as "parallel connections," but the client actually
  multiplexes every request as a separate **bidi stream over one `Connection`**
  (`open_bi()`), sharing one CUBIC controller and one path. Evaluate whether
  genuinely parallel *connections* (or QUIC connection migration / multipath)
  help WAN throughput and failover, and whether the extra congestion-control
  independence is worth the server-side accounting.
  *Where:* `transport-quic` `QuicClient`, server accept loop.
- **[P2] 0-RTT / session resumption.** Every mount pays a fresh QUIC+TLS
  handshake and (server-side) Argon2. TLS session resumption / 0-RTT for
  reconnects would cut reconnect latency — but needs replay-safety analysis
  (0-RTT data is replayable; only idempotent ops may ride it).
- **[P2] Congestion control & pacing review.** Transport uses quinn defaults.
  Confirm the controller and pacer suit large sequential media reads on a
  jittery WAN (observed cold ceilings swinging 30–65 Mbit/s); consider BBR if
  quinn exposes it.
- **[P2] QUIC DATAGRAM frames for unreliable/small signals.** Pings,
  keep-alives, and future server-push invalidations don't need stream
  reliability/ordering; datagrams would avoid stream setup cost.

---

## 2. Protocol surface & framing

- **[DONE — protocol v7] Directory pagination.** `MAX_FRAME_SIZE = 1 MiB` used
  to cap one response frame, so a single directory with more than ~10k entries
  could not be projected in one `DirectoryView` even after inline
  xattrs/snapshots were stripped (`fit_directory_view_response` → `TooLarge` →
  client `EFBIG`). Real media libraries hit this. **Resolved** with multi-frame
  streaming over the request's own stream (chosen over cursor tokens to keep one
  revision-consistent snapshot and avoid server-side cursor state): a view too
  large for one frame is sent as `DirectoryViewStart` + N `DirectoryViewChunk` +
  `DirectoryViewEnd`; the client reassembles it transparently so layers above
  the transport are unchanged. Scan cap changed from a frame estimate to
  `MAX_DIRECTORY_ENTRIES`; legacy `ListDirectory` now returns a clean `TooLarge`
  instead of dropping the connection. *Where landed:* `crates/protocol`
  (variants + `DIRECTORY_VIEW_CHUNK_BUDGET`), `crates/server-core` scan gate,
  daemon `write_directory_view`/`stream_directory_view`, client-core
  `list_directory_view_streamed`.
- **[P1] No server-initiated messages.** The protocol is strictly
  request/response — the server cannot push. This blocks **cross-client cache
  invalidation** (§4) and lease/notify features. Add a server-push channel
  (dedicated uni stream or datagram) with a client dispatcher.
- **[P1] Protocol versioning is all-or-nothing.** `decode` rejects any frame
  whose `version != PROTOCOL_VERSION` (hard equality). There is no negotiated
  version *range* or capability handshake, so **every protocol bump is a
  flag-day** requiring client and server to update in lockstep — painful for a
  separately-deployed daemon (the RAID box runs old code after a client
  rebuild). Add min/max version negotiation in `Hello`/`HelloAck` and a
  compatibility window. *Where:* `protocol/src/lib.rs` version check,
  `Request::Hello`/`Response::HelloAck`.
- **[P1] Missing `ctime` / inode-change timestamp.** "The protocol does not yet
  carry a distinct inode-change timestamp" (`native.rs:1862`) — the adapter
  can't report an accurate `st_ctime`. Add it to `Metadata`.
- **[P2] Batch / compound requests.** `readdirplus`-style enrichment already
  folds stat into readdir, but multi-node metadata fetches, `ForgetNodes`
  batching aside, and speculative opens still cost one RTT each. A compound
  request (pipeline N ops in one frame) would cut RTTs on cold crawls over
  WAN.
- **[P2] Frame compression.** Directory views and metadata are highly
  compressible (repeated names, xattr keys). Optional per-frame compression
  (e.g., zstd) would shrink the 1 MiB-bound directory frames and speed cold
  crawls — interacts with §2 pagination (compress *then* fit).
- **[P2] Formal protocol specification.** `docs/protocol.md` exists but a
  versioned wire spec (frame layout, every `Request`/`Response` variant,
  error taxonomy, capability flags) would make the flag-day/negotiation work
  and any third-party client safe.

---

## 3. Caching & read path

- **[P1] Cross-client cache coherence (server-push invalidation).** Local
  post-mutation projections are invalidated coherently, but a *second* client
  mutating a file does not notify the first — the first serves stale cached
  blocks until its revision check happens to catch it. Depends on §2
  server-push. *Where:* `crates/cache`, `client-core/src/cached.rs`,
  server registry.
- **[P1] Random-read acceleration gap.** The prefetcher only accelerates
  *sequential* access; purely random/seeky reads stay one-request-per-read and
  RTT-bound, and with `-o noreadahead` the kernel won't help either. This is
  the intended trade, but a smarter policy (small speculative window on
  detected strided access; larger cache-block reuse) could help media apps
  that seek. *Where:* `SequentialPrefetcher` in `cached.rs`.
- **[P1] >1 MiB demand-miss waits a whole block.** A cold demand read above
  1 MiB fetches the entire `--cache-block-kib` block before returning, so a
  `dd bs=4m`-style reader on a slow link stalls on a full block. Media players
  (≤1 MiB reads) never hit this, but it caps large-block cold throughput.
  Consider sub-block streaming return.
- **[P2] Cache eviction / GC policy.** Mount cache budget defaults to 20 GiB;
  document and tune the eviction policy, verify GC correctness against the
  content-addressed store (history of collapse-era wrong bytes — see the cache
  writeups), and add a cache-integrity self-check.
- **[P2] Prefetcher cross-handle / connection-warmth awareness.** Windows are
  per-handle and reset each open; the first file after a fresh mount ramps
  from cold while the connection is also cold. A connection-level warmth signal
  could let a new handle start with a larger initial window.

---

## 4. Write path & consistency

- **[P1] Random small-write coalescing gap.** Contiguous writes coalesce into
  one 8 MiB `WriteRange`, but a purely random small-write pattern (no
  contiguity) still issues one request per 16 KiB `FUSE_WRITE` chunk — the
  write mirror of the random-read gap. *Where:* macFUSE write path in
  `native.rs`, `PendingWrite` buffering.
- **[P1] Cross-descriptor write visibility.** Coalescing is per handle; two
  descriptors writing the same file only see each other's buffered bytes after
  a flush/close. Standard write-behind semantics, but document it and consider
  a shared per-inode write buffer if an app needs it.
- **[P2] No write-back cache.** macFUSE 5.3 doesn't advertise
  `FUSE_WRITEBACK_CACHE`, so the kernel can't batch writes for us; userspace
  coalescing compensates. Re-evaluate if a newer macFUSE/FSKit backend gains
  it.
- **[P2] Revision/ESTALE edge cases.** Read-only handles adopt newer revisions
  (monotonic); writable handles fail closed. The torn-read guard only holds
  within one read callback. Broaden test coverage for concurrent
  writer-elsewhere + long-reader-here, and for xattr-vs-setattr revision
  semantics (xattr writes don't bump revision; setattr does — verified
  empirically, not spec'd).

---

## 5. Offline / durability (intentionally deferred — documented requirements)

Cold-start offline mounting and offline **writes** are deliberately
unsupported today. Before they can be, the design must include (per
`docs/roadmap.md`, expanded):

- **[P2]** An authenticated, durable local **mutation journal** (crash-atomic).
- **[P2]** **Version preconditions** on replay (compare-and-swap against server
  revision) so an offline edit can't silently clobber a newer server state.
- **[P2]** **Lock/permission revalidation** on reconnect (the account may have
  lost write grant while offline).
- **[P2]** An explicit **conflict policy** and **user-visible reconciliation**
  (not silent last-writer-wins).
- **[P2]** Cold-start mount from cache while the server is unreachable
  (authenticated cache trust without a live handshake).

These are a coherent feature, not incremental patches — keep them grouped.

---

## 6. Security & authorization

- **[P0] Independent protocol/security review.** The auth/pairing/trust stack
  is self-audited only. An external review of the QUIC/TLS trust modes,
  pairing proofs, Argon2 parameters, and request bounds is a release gate.
- **[P1] Immediate live-session revocation.** Revoking an account or its
  write grant does not currently tear down an already-authenticated live
  session. Add push-based session kill / capability revalidation. Depends on
  §2 server-push. *Where:* `crates/server-core`, session/capability tracking.
- **[P1] Per-user export roots.** All accounts see one export tree; there is no
  per-user root confinement. Needed before multi-tenant use.
- **[P1] Audit logging.** No structured audit trail of auth attempts, grants,
  mutations. Add append-only audit logging (ties into §10 observability).
- **[P2] Distributed-login / brute-force defense.** Per-source throttling and
  global Argon2/connection bounds exist; evaluate coordinated multi-source
  login abuse and add stronger global defense.
- **[P2] Certificate/pin rotation.** Signed, overlapping exact-pin rotation and
  platform keychain integration (macOS Keychain) for trust material
  (`docs/roadmap.md` item 4). *Where:* `crates/client-core/src/trust.rs`.
- **[P2] Threat-model refresh.** Re-validate `docs/threat-model.md` against the
  current protocol (parallel streams, revision adoption, offline cache trust).

---

## 7. Reliability & fault tolerance

- **[P1] Fault-injection / chaos harness.** No systematic testing of daemon
  restart mid-session, mid-write disconnect, partial-frame corruption, or
  reconnect under partition. Build a fault-injection layer around the transport
  and a soak harness (`docs/roadmap.md` item 2).
- **[P1] CoreServices registration coin-flip (macOS-side, mitigated).** Fresh
  mounts sometimes register unusably for Finder — an identity-independent
  coreservicesd coin flip cleared only by reboot. Mitigated by raising the
  verify-and-remount budget to 8 (`VOLUME_REGISTRATION_ATTEMPTS`), but it
  cannot reach zero and the retries churn the volume (interrupting a video
  opened during the window). Not fixable from the filesystem; track upstream
  macFUSE/macOS behavior. *Where:* `native.rs` registration loop; see a.md.
- **[P2] Reconnect correctness proofs.** Server-epoch matching prevents
  cross-epoch handle reuse; add explicit tests for reconnect during in-flight
  write coalescing and lock replay.

---

## 8. Server scalability & limits

- **[P1] Directory memory & the 1 MiB frame.** Beyond §2 pagination, verify the
  server's per-directory memory when projecting a very large enriched view, and
  bound it independently of the frame cap.
- **[P2] Node-ceiling scaling.** Per-connection / global node ceilings are
  `131072 / 524288` (counter-based semaphores, ~400–500 B/node tracked). Fine
  for current libraries; document the sizing formula and revisit for very
  large working sets (the `auto_xattr` sidecar doubling makes this closer than
  it looks). *Where:* `crates/server-core` `NodeRegistry`, `crates/common`
  `Limits`.
- **[P2] Backpressure tuning.** Store-behind backpressure drops range jobs over
  1024 jobs / 128 MiB pending; validate these bounds under many concurrent
  clients.
- **[P2] Multi-client server load characterization.** No profiling of the
  daemon under N simultaneous media clients; needed before claiming multi-user
  readiness.

---

## 9. Coherence & kernel invalidation (macFUSE constraint)

- **[P1] Safe kernel-cache invalidation from callbacks.** Synchronous fuser
  invalidation messages are deliberately *not* sent from inside macOS callbacks
  because they can wedge macFUSE's single receive loop. A safe out-of-band
  invalidation path (dispatched off the receive loop) is needed so
  server-pushed invalidations (§3/§2) can actually evict kernel-cached pages.
  *Where:* `native.rs` (the single-receive-loop discipline), fuser shim.

---

## 10. Observability & operations

- **[P1] Real telemetry.** Diagnostics today are ad-hoc env-gated `eprintln!`
  (`QUICKFS_FUSE_DEBUG`, `QUICKFS_PREFETCH_DEBUG`). Add structured logging
  (`tracing`), metrics (throughput, cache hit rate, prefetch decisions,
  reconnects, manifest coalescing, node-registry occupancy), and optional
  export. `NonBlockingPersistentCache::manifest_writes()` is the kind of
  counter that should be a first-class metric.
- **[P2] Health/status surface.** A client-side status command / socket
  (connection state, cache stats, current prefetch windows) and a daemon
  health endpoint.
- **[P2] Deployment packaging.** systemd unit for the Linux daemon, launchd/pkg
  for the macOS client, config files, and administrator recovery tooling
  (`docs/roadmap.md` item 4).

---

## 11. Testing & quality

- **[P1] Protocol fuzzing.** Fuzz `decode::<Request>`/`decode::<Response>` and
  the directory-view fitting logic (untrusted-length paths already have size
  guards — fuzz them).
- **[P1] CI matrix.** Linux/macOS CI for daemon restart, macFUSE 4/5 backends,
  FSKit backend, backing-filesystem variants, special nodes, large resource
  forks (`docs/roadmap.md` item 2). Currently validation is manual/local.
- **[P2] Long-haul soak + real-hardware WAN benchmarks.** Automate the
  paced-player / ceiling-pair measurements against the deployed RAID box so
  regressions (e.g., the kernel-readahead-era stutter signature) are caught,
  not rediscovered by hand.
- **[P2] Property tests** for cache coherence (revision adoption invariants),
  write coalescing (bytes in == bytes out under interleaving), and inode/handle
  lifecycle.

---

## 12. Client GUI

- **[P2] Assess and complete `clients/macos/client-gui`** (Svelte + Tauri).
  Audit its current state and define the intended UX: mount/unmount, trust
  approval, connection/cache status, and the diagnostics from §10. Scope TBD
  after an inventory — listed so it isn't forgotten, not yet specified.

---

## 13. Explicitly out of scope (this document)

Per the request, **new OS/platform ports are excluded**, even though scaffolding
exists:
- **WinFsp / Windows client** (`clients/windows`, `docs/roadmap.md` item 5).
- **Linux FUSE client** (`clients/linux`).
- **macOS-hosted daemon** parity (`servers/macos`) beyond what already builds.

If platform parity is later prioritized, the protocol/transport/security items
above are prerequisites and should land first.

---

## Suggested sequencing

1. **§1 connection wedge + demand-path ceiling** and **§2 directory
   pagination** — the two things that bite real usage today.
2. **§2 server-push + version negotiation** — unblocks §3 cross-client
   coherence and §6 live revocation, and ends the flag-day upgrade pain.
3. **§6 security review + §10 telemetry + §11 CI** — the release-readiness
   trio.
4. Everything else as capacity allows; keep §5 offline as one coherent feature,
   not piecemeal.
