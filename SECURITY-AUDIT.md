# Security review notes — remotely reachable paths

This document records a threat-model-driven review of quicKFS's remotely
reachable code, the issues it confirmed, and the mitigations that were
implemented alongside it. It was produced by the project itself, not by an
independent party: **this is not an independent or professional security
audit, and external security review is still recommended** before any
production exposure (see [SECURITY.md](SECURITY.md)).

Reviewed at protocol **6.4**. All network input was treated as untrusted:
malformed, truncated, oversized, compressed, duplicated, reordered, and
concurrent requests, and disconnects at arbitrary points.

## Trust boundaries reviewed

- **Network → daemon**: QUIC/TLS transport, frame parsing and decompression,
  `Request` decoding and dispatch (`crates/protocol`, `crates/transport-quic`,
  `servers/linux/server-daemon`).
- **Unauthenticated peer → authentication**: `Hello`/`Ping`/`Pair`/
  `Authenticate` handling, pairing proofs, throttling (`crates/auth`, daemon).
- **Authenticated read-only account → mutations and shared state**: the
  per-request authorization chain (`authenticated` → daemon `--allow-writes` →
  per-account write grant → handle/session ownership) in
  `crates/server-core`.
- **Node/handle references → backing filesystem**: descriptor-relative
  confinement beneath the export root, node registry, resource-fork sidecars.
- **Server → client**: response decoding, streamed directory-view reassembly,
  raw read bodies, reconnect/epoch/lock-replay state
  (`crates/client-core`, including `resilient.rs` and `trust.rs`).

## Confirmed issues and implemented mitigations

Ordered by severity. None required a wire-protocol change.

### 1. High — request naming an unknown node ran a full-export walk on the async runtime; `GetMetadataBatch` amplified it up to 4096×

- **Prerequisites**: any authenticated account (read-only suffices). A single
  `GetMetadata` naming a nonexistent node ID ran a synchronous recursive walk
  of the export (bounded only by `max_total_known_nodes` visits) directly on a
  Tokio worker thread; because the walk never yielded, the request timeout
  could not fire until it finished, and a handful of such requests could stall
  every worker thread — freezing the daemon for all clients. One
  `GetMetadataBatch` of 4096 unknown IDs multiplied the walk 4096-fold.
- **Affected code**: `crates/server-core/src/lib.rs`
  (`discover_node`/`node_snapshot`), daemon `GetMetadataBatch` dispatch.
- **Mitigation**: discovery walks now run on the blocking pool (the request
  deadline can fire and the runtime keeps serving other connections), are
  bounded to `MAX_CONCURRENT_DISCOVERY_WALKS` (4) concurrent walks export-wide,
  and a metadata batch performs **at most one** walk for all unknown nodes in
  the request, resolving each slot from the registry afterwards.
- **Regression tests**: `crates/server-core/src/lib.rs`
  `tests::metadata_batch_resolves_positionally_with_one_bounded_walk`,
  `tests::metadata_batch_discovers_unknown_nodes_from_a_fresh_registry`; the
  daemon-level `metadata_batch_resolves_per_node_and_bounds_size` still covers
  the wire behavior (order, per-slot failures, `TooLarge` over-limit).

### 2. High — client allocated a server-declared xattr byte count without any bound

- **Prerequisites**: the client talks to a hostile or compromised server that
  passed the trust policy (or a MITM holding the server key). A `XattrData`
  response could declare an arbitrary `length` (up to 2⁶⁴−1) and
  `NetworkFilesystem::get_xattr` allocated it before reading, allowing a
  trivial client memory-exhaustion crash. The file-read path already validated
  this; the xattr path did not.
- **Affected code**: `crates/client-core/src/lib.rs` (`get_xattr`).
- **Mitigation**: `validated_raw_body_length` caps every declared raw body by
  the requested length and `MAX_CLIENT_READ_SIZE` before allocation; both the
  read and xattr paths share it.
- **Regression test**: `crates/client-core/src/lib.rs`
  `tests::raw_body_length_is_bounded_by_request_and_client_limit`.

### 3. Medium — reconnect lock replay could silently discard locks still held

- **Prerequisites**: a client holds byte-range locks, performs more than 4096
  lock operations on one handle, then unlocks any range; the old code then
  cleared the handle's whole lock history, so a later reconnect replayed
  nothing — advisory mutual exclusion silently lost, risking data corruption
  between cooperating clients.
- **Affected code**: `crates/client-core/src/resilient.rs` (`set_lock`,
  lock replay in `remote_handle`).
- **Mitigation**: the history is now a compacted mirror of the handle's
  current lock set (per-owner overlapping ranges replaced, unlocks removing or
  splitting ranges), so replay is exact and memory is bounded by the lock set
  the server itself accepted rather than by operation count.
- **Regression tests**: `resilient.rs`
  `tests::lock_history_compacts_instead_of_growing_or_discarding`,
  `tests::reconnect_replays_only_locks_still_held_after_unlock`.

### 4. Medium — one connection could occupy every global request slot

- **Prerequisites**: a single connection (no authentication required) opening
  many streams with stalled or slow requests. With 256 QUIC streams allowed
  per connection and 128 global request permits, one peer could pin all
  permits for up to the request timeout, repeatedly, starving all other
  clients.
- **Affected code**: `servers/linux/server-daemon/src/main.rs` (connection
  accept/request loop).
- **Mitigation**: a per-connection request semaphore
  (`--max-concurrent-requests-per-connection`, default 64) is acquired before
  a global slot, so a stalled connection can pin at most its own share.
- **Regression test**: daemon
  `tests::stalled_connection_cannot_hold_every_global_request_slot`
  (deterministic: 2 global slots, 1 per connection, two stalled mid-frame
  streams, a second connection's ping must still be served).

### 5. Medium — one session could fill the export-wide advisory-lock table

- **Prerequisites**: an authenticated account with an open handle (read locks
  need only read access). 65,536 disjoint ranges filled the global table and
  every other connection's `SetLock` then failed `TooManyLocks`.
- **Affected code**: `crates/server-core/src/lib.rs` (`replace_owner_range`).
- **Mitigation**: `MAX_SESSION_LOCK_RECORDS` (8192) caps one session's records
  below the global table; unlocks are never blocked by the cap.
- **Regression test**: server-core
  `tests::session_lock_records_are_bounded_below_the_global_table`.

### 6. Medium — a hostile server could hang the streamed directory-view client loop forever

- **Prerequisites**: hostile/compromised trusted server. The reassembly loop
  accepted empty `DirectoryViewChunk` frames, which never advance the entry
  count, so the `MAX_DIRECTORY_ENTRIES` ceiling was never reached and the
  client read frames forever (unbounded when `NetworkFilesystem` is used
  directly, e.g. by the CLI).
- **Affected code**: `crates/client-core/src/lib.rs`
  (`list_directory_view_streamed`).
- **Mitigation**: every chunk must make progress; an empty chunk (which the
  server never legitimately sends) is rejected, bounding the whole stream at
  `MAX_DIRECTORY_ENTRIES` frames.
- **Regression test**: client-core
  `tests::streamed_directory_chunks_must_make_progress_and_stay_bounded`.

### 7. Low — pairing attempts were not throttled

- **Prerequisites**: any unauthenticated peer. Each `Pair` request cost the
  server a pairing-record disk lookup and HMAC work with no rate limit (the
  160-bit code itself is not guessable; this was a resource/probing concern,
  and expired-record deletion could be driven remotely).
- **Affected code**: `servers/linux/server-daemon/src/main.rs` (`Pair` arm).
- **Mitigation**: pairing attempts now consume the same per-source-IP
  authentication rate limit as password attempts; a wrong code still does not
  consume the pairing record.
- **Regression test**: daemon
  `tests::pairing_attempts_share_the_authentication_rate_limit`.

## Authorization coverage

Every mutation-capable `Request` variant was traced through daemon dispatch to
its `ExportSession` implementation and requires, in order: an authenticated
connection (checked in `handle()` before dispatch), the daemon-wide
`--allow-writes` policy **and** the account's write grant (both folded into
`ExportSession::can_write`, re-read per call via `require_writable`), and
ownership of any referenced handle by the current connection (handles are
connection-local; foreign handles yield `InvalidHandle`):

`CreateFile`, `CreateDirectory`, `CreateSymlink`, `CreateHardLink`,
`CreateSpecialNode`, `RemoveNode`, `RenameNode` (all modes), `ExchangeData`,
`SetAttributes` (including truncate-by-size; a pure no-change request is
treated as a read), `WriteRange` (also gated by write budgets before the body
is read), `AllocateFile`, `CopyFileRange` (write access required on the output
handle, read on the input), `SetXattr`/`RemoveXattr` (including the
resource-fork sidecar path), `SetVolumeName`, `SetLock` with `Write` kind, and
`OpenFile` with write access or truncate. Less obvious state-touching
operations were checked as well: `FlushFile`/`CloseFile` only release the
calling session's own locks; `ForgetNodes` only drops the calling connection's
references (bounded at 65,536 per request); `SyncFile`/`SyncDirectory` are
permitted for readers deliberately (they mutate nothing logically);
`GetLock`/read `SetLock` require handle read access. Write grants are
re-evaluated on every operation, so `set_write_authorized(false)` takes effect
immediately for in-flight sessions (covered by
`session_write_grants_are_isolated_and_revocable`).

## Areas reviewed with no vulnerability confirmed

- **Framing/decoding**: length prefix validated against `MAX_FRAME_SIZE`
  before allocation; zstd decompression output bounded to `MAX_FRAME_SIZE`
  (bomb guard); postcard errors are clean failures; foreign protocol majors
  rejected (uncompressed error); declared raw-body lengths are read exactly
  (`read_exact`) with truncation surfacing as an error that releases permits;
  trailing stream bytes are inert because every stream carries exactly one
  operation; offset/length arithmetic uses checked/saturating forms at the
  validation boundaries (`validate_range`, `map_block`, xattr positions,
  copy ranges).
- **Pairing**: client/server proofs are domain-separated and bind pairing ID,
  presented certificate fingerprint, and a fresh client nonce; the record is
  consumed only after a valid client proof, and consumption (file unlink) is
  atomic and single-use under concurrency; expiry is enforced server-side;
  wrong proofs do not consume the record; the `PairingClient` API cannot send
  credentials or filesystem requests over the untrusted bootstrap connection.
- **Password authentication**: Argon2id hashes with pinned parameters
  (foreign/weaker parameter strings rejected); unknown users pay an equalized
  hashing cost; per-connection attempts serialized with a 5-failure cap;
  per-IP rolling-window limiter with a bounded bucket table that fails closed
  when full; a global semaphore bounds concurrent Argon2 jobs; credentials are
  zeroized on every path and never logged; authentication state is
  per-connection and cannot leak across sessions.
- **Export confinement**: all filesystem access walks descriptor-relative
  components with `O_NOFOLLOW` at every step and re-verifies the opened file's
  device/inode identity against the registry snapshot after open (defeating
  rename/symlink races); client-supplied names are single components (no `/`,
  `NUL`, `.`/`..`); symlink targets that are absolute or contain `..` are
  rejected at creation and at readlink; hardlinking directories or the root is
  refused; resource-fork sidecars live outside the export under
  UUID-derived names the client cannot influence; the daemon refuses to start
  when state and export directories overlap; the `com.apple.*` xattr namespace
  translation is injective and cannot address the private
  `user.quickfs.*` names.
- **Resource lifecycle**: connections, request tasks (global and now
  per-connection), auth jobs, read/write byte budgets (global and
  per-connection for writes, reserved before the body is read), open handles,
  known nodes (per-connection and global, with self-healing eviction),
  directory-entry workers, decompression output, metadata batches, forget
  batches, and pairing records all have explicit bounds; cleanup on
  disconnect releases handles, node references, and the session's advisory
  locks, and an aborted request drops its permits (a write body already
  handed to the filesystem intentionally retains its memory reservation until
  the write completes).
- **Reconnect state (client)**: replacement connections re-apply the identical
  trust policy and full authentication; the persisted server epoch must match
  exactly or the mount fails stale; handles reopen only at the recorded
  revision (read-only, lock-free handles may adopt a newer revision;
  writable/locked handles fail `StaleRevision`); mutations interrupted by
  transport failure surface `AmbiguousMutation` and are never auto-retried;
  the operation deadline spans reconnects and is not reset by retries; a
  timed-out waiter cannot cancel or corrupt a shared reconnect; generation
  checks prevent an old connection's cleanup from touching a replacement.

## Unresolved risks

- **Per-user export isolation** is still absent: every account shares one
  export namespace (write grants are per-account). Documented limitation.
- **Enriched directory views** of a pathological directory can require large
  server-side buffers (up to `MAX_DIRECTORY_ENTRIES` = 1,048,576 entries plus
  per-entry xattr-name sets). Creating such content requires write access to
  the export; the entry cap and per-request inline-value budgets bound but do
  not tightly limit bytes.
- **Inode reuse aliasing**: node IDs key on device+inode. After a file is
  unlinked, a new file reusing the inode aliases to the old node ID; if the
  daemon crashed between an unlink and its sidecar cleanup, a stale resource
  fork could surface on the aliased node. Confined to users already sharing
  the export; revisions include ctime so range caches do not alias.
- **Discovery cost**: resolving an unknown node ID still costs one bounded
  full-export walk (visit-capped, four concurrent walks max, request
  deadline enforceable). A hostile authenticated client can keep the backing
  disk busy within those bounds.
- **Distributed password guessing** is out of scope of per-source throttling
  (documented), and live-session revocation is not implemented.
- The non-Unix daemon build is a development stub: most filesystem operations
  fail closed with `NotSupported`.
- `cargo audit` was not run in this environment (tool not installed); the
  scheduled RustSec CI workflow covers it.

## Protocol-breaking fixes deferred to a future major

None of the confirmed issues required a wire change; every mitigation is
compatible within protocol major 6. The previously documented non-additive
items (a distinct `ctime` metadata field; server-initiated messages) remain
deferred per `docs/protocol-spec.md` §8.3. No new fail-open behavior was
grandfathered for compatibility.
