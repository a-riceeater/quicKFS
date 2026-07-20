# quicKFS handoff — high-latency read performance

## Why this document exists

quicKFS's whole reason to exist is making a **high-latency** mount feel fast — the goal is to *hide* long round trips, not pay for them one read at a time. The write path already does this (v6 write-coalescing batches the kernel's 16 KiB `FUSE_WRITE` chunks into 8 MiB `WriteRange`s, so a copy is a handful of big requests instead of thousands of serial ones). **The read path has no equivalent yet, and it is the current bottleneck on real high-latency mounts.** This document is the plan to fix that.

Scope: read throughput on high-latency links. Uploads are already fine (see "Non-goals"). No wire-protocol change is required — this is a client-side scheduling problem.

## What we measured (live RAID over an ~82 ms WAN/VPN link, 2026-07-20)

Client and RAID are **not** on the same LAN — `ping 10.0.0.74` is **min/avg/max = 72/82/97 ms**. The user's HTTP speed test on the same machine reports ~**50 Mbps down / 12 Mbps up** (speed tests open many parallel connections, so they report the true link capacity).

| Workload (cold, uncached, fresh files) | Throughput |
| --- | --- |
| Upload, `cp` 30 MB → RAID (×2) | 1.8–2.0 MB/s (**14.5–15.7 Mbit/s**) — byte-exact |
| Download, **single** sequential read (`dd`, 8–64 MB) | 0.6–1.0 MB/s (**~8 Mbit/s**) |
| Download, 4 parallel reads (different files) | 2.7 MB/s (**21.9 Mbit/s**) |
| Download, 8 parallel reads | 4.75 MB/s (**38.0 Mbit/s**) |
| Download, **one file** as 8 parallel 8 MiB ranges | 3.3 MB/s (**26.6 Mbit/s**) |
| Download, **one file** as **16 parallel 8 MiB ranges** | 6.0 MB/s (**48.3 Mbit/s**) ← saturates the ~50 Mbps link |

Read these two rows together: the *same single file* went from **7.8 Mbit/s (sequential) to 48.3 Mbit/s (16 parallel ranges)** — a ~6× win that lands right at the link ceiling the HTTP test reports. On loopback (0 ms RTT) the same read path does ~21 MB/s, so the code is not slow — it is **round-trip-bound**. Upload already matches the 12 Mbit/s uplink, which is why upload "correlates" and download does not.

**Conclusion: the link has the bandwidth; a single sequential quicKFS read fails to fill the pipe.** The fix is to keep enough range requests in flight to cover the bandwidth-delay product (BDP).

## Root cause in the code

Read path lives in `crates/client-core/src/cached.rs`:

- `read_range_versioned` (~line 814) serves **exactly the range the kernel asked for**. It computes the covering blocks (`blocks_for`, ~272; aligned to `policy.block_size`, default `MAX_CLIENT_READ_SIZE` = 16 MiB) and fetches them with `join_all` (~850). So blocks *within one kernel read* are already fetched **concurrently** — good.
- `fetch_block` (~318) has a single-flight gate: a `range_fetches` map of `RangeKey → RangeFetch { OnceCell }`. Concurrent readers of the same block share one network fetch or persistent-cache verification. **This is the primitive a prefetcher should reuse:** a speculative fetch and a later real kernel read for the same block coalesce automatically.

What is missing: **there is no read-ahead *across* kernel reads.** macOS/macFUSE issues sequential reads as separate callbacks (e.g. `dd bs=1m` → 1 MiB reads; even 16 MiB kernel reads arrive one after another). Each becomes a fresh `read_range_versioned`, and nothing fetches block *N+1* while the app is still consuming block *N*. So at every block boundary the client stalls a full ~82 ms RTT, and only ~1 request is ever in flight. That is the ~8 Mbit/s ceiling.

Relevant constants / limits (already generous enough — not the bottleneck):
- `MAX_CLIENT_READ_SIZE` / `MAX_FUSE_IO_SIZE` = 16 MiB (`crates/client-core/src/lib.rs:41`, `clients/macos/filesystem-macfuse/src/lib.rs:38`).
- Mount flag `--cache-block-kib` (default 16384 KiB = 16 MiB) sets `Policy.block_size`.
- QUIC: 256 concurrent bidi streams, 32 MiB stream / 128 MiB connection receive windows, one connection (`docs/protocol.md`). A new bidi stream is used per request, so concurrent range fetches already work on one connection (proven by the parallel measurements).
- Server: `--max-in-flight-read-bytes` (128 MiB), `--max-concurrent-requests` (128), `--max-in-flight-read-bytes` gate reads queue rather than fail. A prefetcher must stay under these.

## The fix: adaptive, self-tuning read-ahead (do NOT hardcode a depth)

The single most important design constraint, and the reason this needs real engineering rather than a constant:

> **The prefetch depth must auto-tune to each connection's measured bandwidth-delay product.** 16 parallel ranges saturates *this* 82 ms / 50 Mbps link. A LAN (sub-ms RTT) needs ~1 and should not waste memory or hammer the server. A satellite or congested-WiFi link (RTT 300 ms, 100 Mbps) needs far more in-flight than 16. Different locations, networks, devices, and even minute-to-minute conditions all move the target. A fixed number is wrong everywhere except where it was measured.

So the read-ahead window is a **runtime-controlled variable**, sized to keep `in_flight_bytes ≈ BDP` (delivered-bandwidth × RTT), bounded by a memory cap and the server's in-flight limits. This is the same principle a TCP/QUIC congestion controller applies to a single flow — we apply it at the application range-request layer, where we control concurrency.

### Controller sketch

Maintain per-handle read-ahead state on `CachedHandle` (or a sibling map keyed by `FileHandle`):

1. **Sequential detection.** Track the last served offset and a short run-length counter. Treat a read as sequential when `offset ≈ last_end` (allow small gaps/reordering from kernel read-ahead). Reset/disable prefetch on a non-trivial seek. Random/seeky access must *not* prefetch — that wastes bandwidth and cache.
2. **Online link estimation (EWMA).** Time each `read_range_versioned` inner fetch: derive an RTT estimate (time-to-first-byte of a fetch) and a delivered-bandwidth estimate (bytes / transfer-time), smoothed with an EWMA per handle or per connection. `BDP = bandwidth × RTT`.
3. **Window control.** Keep a target of `desired_inflight_bytes ≈ k · BDP` (k a small safety factor, e.g. 1.5–2). Convert to a block count against `block_size`. Grow the window while (a) throughput is still increasing with depth and (b) the app keeps consuming sequentially; stop growing when throughput plateaus (link saturated), the memory cap is hit, or server backpressure appears (rising fetch latency / queueing). Shrink on random access, on consumer stalls (app not keeping up → don't prefetch megabytes it may never read), or on errors. An AIMD-style growth/backoff is a reasonable first cut; a BDP-estimate feed-forward converges faster.
4. **Issue prefetches through the existing gate.** For each block in `[cursor, cursor + window)` not already cached or in-flight, `spawn` a `fetch_block` via the `range_fetches` single-flight map so it populates `RangeCache` ahead of demand. When the kernel's next read arrives, `read_range_versioned` finds the block cached or joins the in-flight fetch — no stall.
5. **Prioritize demand over speculation.** A real kernel read must never queue behind speculative prefetches (don't starve interactive small reads / metadata / thumbnails). Consider a small concurrency reservation or priority for demand fetches.

### Where to build it

- New module/struct in `crates/client-core/src/cached.rs` (e.g. `SequentialPrefetcher`), owned per handle. This keeps the benefit in **client-core**, so both the macFUSE mount *and* the CLI (`clients/macos/client-cli`) get it for free.
- Hook: at the top/tail of `read_range_versioned`, update the per-handle cursor + access-pattern state and, when sequential, kick the prefetcher to top up the window (fire-and-forget `tokio::spawn`, results land in `RangeCache`).
- Reuse: `range_fetches` (single-flight), `RangeCache` (revision-keyed), `blocks_for` (alignment). Do **not** invent a second fetch path.
- Coherence is already handled: blocks are keyed by `RevisionKey { node, revision }`; a mutation bumps the revision and orphans stale prefetched blocks. Verify prefetch respects the same revision check `fetch_block` already does (`StaleRevision`).

### Config surface (keep it minimal; the point is auto-tuning)

- One memory ceiling knob, generous default (e.g. `--read-ahead-max-bytes`, default sized like 64–128 MiB) so the adaptive window can't blow up RAM on a fat pipe. Everything else is measured, not configured.
- Keep `--cache-block-kib` as the fetch granularity. Optionally let the controller pick a smaller chunk at very high concurrency (more, smaller requests fill a pipe with less head-of-line waste — the PoC used 8 MiB chunks).
- Respect and stay under server `--max-in-flight-read-bytes` / `--max-concurrent-requests`; ideally learn them from capabilities or back off on observed queueing.

## How to validate (reproduce and regression-test)

The measurement recipe used above, generalized so it proves *auto*-tuning, not one lucky number:

1. **Baseline the link out-of-band:** `ping <server>` for RTT; a parallel-range read of one fresh large file at increasing depth (`dd if=FILE of=/dev/null bs=1m skip=$off count=8 &` ×N) to find the empirical ceiling. This is the target the adaptive single read should approach on its own.
2. **Single sequential read must approach that ceiling automatically** after a short ramp, on *several different* link profiles — not just 82 ms/50 Mbps. Test at least: LAN (sub-ms), mid WAN (~30–80 ms), high RTT (~200–300 ms, e.g. `dnctl`/`pfctl` or Network Link Conditioner to shape). Confirm the window converges to roughly `BDP` in each case.
3. **Regressions to guard:**
   - LAN / low-latency: no meaningful over-prefetch, no memory blowup, no extra server load vs. today.
   - Random/seeky access (e.g. a database file, `lseek`-heavy workload): prefetch stays *off* — measure that it doesn't inflate bytes-read.
   - Consumer slower than link (app reads a bit then pauses): window shrinks; we don't fetch tens of MiB it never uses.
   - Coherence: a concurrent write on another handle invalidates prefetched blocks (revision bump) — read returns fresh data, no stale bytes.
   - Cache pollution: prefetched-but-unused blocks are evicted cleanly by the LRU and don't thrash the persistent cache (prefer landing speculative blocks in the memory tier first).
4. **Cross-check against the HTTP speed test** on the same machine: a single large sequential read should land in the same ballpark as the parallel-connection speed-test download, instead of ~16% of it.

## Risks & considerations

- **Memory** = `window_bytes` which scales with BDP; hard-cap it. A 300 ms × 200 Mbps link wants ~7.5 MiB BDP but head-of-line and jitter push the useful window higher — bound it and let throughput plateau rather than chase infinitely.
- **Wasted bandwidth** on misfired prefetch (random access, early close, seek). Sequential detection + fast backoff is the mitigation; be conservative about *starting* to prefetch, aggressive about *stopping*.
- **Server & multi-client load:** speculative reads multiply per-connection load; respect server in-flight/concurrency caps and back off on queueing so one greedy client doesn't starve others (`--max-in-flight-read-bytes` is global).
- **Fairness with interactive reads:** demand reads (metadata probes, thumbnails, small random reads) must not sit behind a wall of speculative 16 MiB fetches.
- **QUIC layer:** one connection already reached 48 Mbit/s across 16 streams, so the connection window (128 MiB) and stream count (256) are not limiting at this scale. On much fatter pipes, revisit quinn congestion control (BBR vs. Cubic) and the receive-window sizing before assuming the app layer is at fault.

## Complementary ideas (after the core adaptive prefetch lands)

- **Split one large demand read into parallel sub-range fetches.** Even a single 16 MiB kernel read is one stream today; fanning it into k concurrent sub-ranges cuts the latency of the *first* big read (cold-open of a file), before read-ahead has ramped.
- **Access-pattern-scaled window (Linux-style):** grow the read-ahead length with the sequential run length; start small, expand as confidence rises.
- **Adaptive chunk size:** at high concurrency, more/smaller requests can fill a pipe with less tail waste than few/huge ones — let the controller trade off chunk size vs. depth.
- **Reuse the write-path philosophy:** writes were fixed by coalescing many small ops into few big ones; reads are the mirror image — turn few big *sequential* demands into many concurrent *speculative* fetches. Same goal (hide RTT), opposite direction.

## Non-goals

- **Uploads.** Measured ~15 Mbit/s ≈ the 12 Mbps uplink ceiling; write-coalescing already saturates it. Nothing to do there.
- **Wire protocol / server changes.** The server already streams arbitrary ranges concurrently; this is entirely a client-side scheduling change in `client-core`.
- **Offline behavior.** Prefetch is a live-session optimization; the offline cache semantics are unchanged.

## Pointers

- Read path: `crates/client-core/src/cached.rs` — `read_range_versioned` (~814), `blocks_for` (~272), `fetch_block` (~318), `range_fetches` single-flight map.
- Limits/constants: `crates/client-core/src/lib.rs:41` (`MAX_CLIENT_READ_SIZE`), mount `--cache-block-kib`.
- Mount read entry: `clients/macos/filesystem-macfuse/src/lib.rs:849` (`read_async`).
- Transport/window facts and the enriched-directory design: `docs/protocol.md`.
- The node-limit / `auto_xattr` / write-coalescing history and current state: `a.md`.
