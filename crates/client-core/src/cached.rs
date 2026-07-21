// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

use crate::{
    ClientError, CreatedFile, DirectorySnapshot, OpenedFile, RangeRead, RemoteFilesystem, Result,
    WriteResult, XattrRead,
};
use async_trait::async_trait;
use futures::future::join_all;
use quickfs_cache::{
    DirectoryCache, FilesystemStateCache, MemoryCache, MetadataCache, NodeCacheInvalidation,
    RangeCache, RangeKey, RevisionKey,
};
use quickfs_protocol::{
    AttributeChanges, DirectoryEntry, DirectoryView, DirectoryViewEntry, DirectoryViewOptions,
    ErrorCode, FileAccess, FileHandle, FileLock, FileOpenOptions, FilesystemCapabilities,
    FilesystemStats, Metadata, Name, NodeId, ROOT_NODE, RenameMode, SafeIoctl, SeekWhence,
    SpecialNodeKind, XattrSetMode,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OnceCell, RwLock, Semaphore};
use uuid::Uuid;

const SMALL_READ_AHEAD_BLOCK_SIZE: u64 = 1024 * 1024;
const SMALL_READ_THRESHOLD: u64 = 1024 * 1024;

/// Default speculative read-ahead memory ceiling. The adaptive window can grow
/// to at most this many bytes of in-flight speculative data per mount, which
/// also keeps a single client comfortably below the server's default
/// `--max-in-flight-read-bytes` (128 MiB) once demand reads are included. See
/// [`SequentialPrefetcher`] for how the window auto-tunes within this bound.
pub const DEFAULT_READ_AHEAD_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A sequential run must reach this many consecutive in-order reads before the
/// *adaptive* window arms, so an isolated touch or a header probe never triggers
/// a full wave of read-ahead. The very first read still primes a shallow
/// [`PRIME_WINDOW_BLOCKS`] read-ahead (below) so the second read is a cache hit
/// rather than another serial cold fetch.
const SEQUENTIAL_TRIGGER: u64 = 2;

/// Speculative blocks fetched on the very first read of a freshly opened handle
/// (when no stream is tracked yet), before a sequential run is confirmed. The
/// adaptive window only arms after [`SEQUENTIAL_TRIGGER`] in-order reads, which
/// used to leave the opening reads of a file with no cushion at all: each cold
/// block was demand-fetched serially at full per-block latency, and on a
/// low-latency LAN that serial ramp — not bandwidth — is what stalls the start
/// of playback (measured: a first-open paced stream stalled 150 ms–1 s at the
/// start, and ~1 s under a concurrent directory crawl; priming drops both to a
/// single sub-130 ms first-block fetch). Priming pipelines the next fetch
/// alongside the first demand read. It is gated on there being no tracked
/// stream, so only the cold open primes — a mid-playback seek and the many
/// short-lived streams of a metadata/thumbnail crawl do not — bounding the
/// wasted read-ahead of a one-shot probe to at most this much (reclaimed by the
/// LRU range cache anyway).
const PRIME_WINDOW_BLOCKS: u64 = 2;

/// Window depth a stream arms with once its sequential run is confirmed
/// (replacing a cold start at a single block). Sized so the buffer is already a
/// few blocks deep the moment speculation begins — deep enough to hide per-block
/// latency at the start of playback, still far below both the concurrent-fetch
/// cap and the byte ceiling, so it fills as a trickle rather than a convoy.
const INITIAL_ACTIVE_WINDOW_BLOCKS: u64 = 4;

/// Forward/backward slack (in blocks) still treated as one sequential stream.
/// Kernel read-ahead and overlapping FUSE callbacks reorder slightly, so a
/// strict `offset == previous_end` test would spuriously classify a real
/// sequential scan as random.
const SEQUENTIAL_SLACK_BLOCKS: u64 = 1;

/// Maximum concurrently tracked sequential streams per open handle.
///
/// macOS multiplexes several independent readers over one FUSE file handle:
/// page-ins for a media player's sequential stream arrive interleaved with
/// Spotlight indexing, Quick Look thumbnailing, and the player's own index
/// probes, all carrying the same `fh`. A single-stream tracker treats every
/// switch between those readers as a seek, zeroes its window, and never
/// keeps speculation alive; live traces during video playback showed exactly
/// that (see docs/caching.md). Tracking a small fixed set of streams lets each
/// interleaved sequential reader keep its own cursor and window.
const MAX_PREFETCH_STREAMS: usize = 4;

/// A demand read that blocked at least this long is treated as evidence that
/// the speculative window is too small (a prefetch miss or near-miss), which
/// is what drives window growth. Memory-cache hits complete in microseconds
/// and persistent-cache verifications in a few milliseconds; a real network
/// fetch is well above this floor on any link.
const DEMAND_STALL_FLOOR: Duration = Duration::from_millis(10);

/// Granularity of the global speculative in-flight budget. The permit pool is
/// denominated in these units rather than in whole `block_size` blocks so
/// that small-granularity streams (video players read well under 1 MiB per
/// kernel read, so their blocks are 1 MiB) can use the whole configured byte
/// ceiling instead of being capped at `read_ahead_max / block_size` fetches.
const PREFETCH_PERMIT_UNIT: u64 = 64 * 1024;

/// Most speculative blocks one demand read may spawn. Without this, a window
/// refill after a stall or ramp step launches the whole window in one burst,
/// and a concurrent demand miss then waits behind that entire wall of
/// transfers on the wire. Topping the window up a few blocks per kernel read
/// fills it within milliseconds of real traffic while keeping any instant's
/// burst small.
const SCHEDULE_BURST_BLOCKS: u64 = 8;

/// Ceiling on speculative prefetch bytes *in flight on the wire at once*, kept
/// separate from the read-ahead buffer-depth ceiling (`--read-ahead-max-bytes`).
/// Prefetch permits are held for a fetch's whole lifetime, so this bounds the
/// convoy of background data a latency-sensitive demand read can end up queued
/// behind on the one shared QUIC connection. The buffer may still grow deep —
/// many blocks already fetched and cached — because that is governed by the
/// window depth and the range cache, not by this; only how much is
/// *simultaneously on the wire* is capped here.
///
/// The [`PREFETCH_MAX_CONCURRENT_FETCHES`] fetch-count cap alone was sized for
/// 1 MiB blocks (8 fetches ≈ 8 MiB in flight). With a large cache block
/// (`--cache-block-kib 8192` → 8 MiB) it instead put 8 × 8 = 64 MiB of
/// speculation on the wire, and under concurrent load — Spotlight or Quick Look
/// scanning other files during playback — a video demand read stalled hundreds
/// of milliseconds behind that convoy (reproduced: paced playback under bulk
/// reads spiked to ~700 ms). This is deliberately at least
/// `PREFETCH_MAX_CONCURRENT_FETCHES` MiB so 1 MiB-granularity streams are
/// unaffected (their fetch-count cap still binds first); it only shortens the
/// convoy of large-block streams, which is exactly where it ran long.
const PREFETCH_MAX_INFLIGHT_BYTES: u64 = 32 * 1024 * 1024;

/// Most speculative fetches in flight at once across the whole mount,
/// independent of the byte budget.
///
/// The window is a *buffer* target — how far ahead of the consumer data
/// should already be cached — not a concurrency target. Fetching the whole
/// window concurrently runs the link as a convoy: QUIC fair-shares bandwidth,
/// so every block's completion time becomes `in-flight bytes ÷ link rate`
/// (live-traced at 64 × 1 MiB in flight: every fetch took ~1–3.5 s, and any
/// demand miss during a convoy stalled playback that long). A handful of
/// concurrent transfers is enough to run any realistic link at capacity with
/// 1 MiB blocks (8 ÷ ~250 ms round trip ≈ 32 MB/s even at WAN latency) while
/// keeping each individual fetch — and any demand read sharing the wire —
/// fast. The byte pool stays the binding constraint for large-block streams.
const PREFETCH_MAX_CONCURRENT_FETCHES: usize = 8;

/// Hard deadline on one speculative fetch, comfortably above a full transport
/// phase timeout plus reconnect budget so it only fires when the fetch is
/// genuinely wedged rather than slow.
const PREFETCH_FETCH_DEADLINE: Duration = Duration::from_secs(120);

/// How many times a demand read refreshes its revision snapshot and retries
/// after observing that the remote file moved past the handle's recorded
/// revision. Each retry re-reads at the newest known revision, so more than a
/// couple only lose against a continuously rewriting concurrent writer.
const STALE_REVISION_RETRIES: u32 = 3;

/// Temporary diagnostic switch: `QUICKFS_PREFETCH_DEBUG=1` streams controller
/// decisions and demand-stall events to stderr.
fn prefetch_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("QUICKFS_PREFETCH_DEBUG").is_some())
}

pub trait FilesystemCache:
    MetadataCache + DirectoryCache + RangeCache + FilesystemStateCache + NodeCacheInvalidation
{
}
impl<T> FilesystemCache for T where
    T: MetadataCache + DirectoryCache + RangeCache + FilesystemStateCache + NodeCacheInvalidation
{
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachePolicy {
    /// Remote reads are aligned to blocks so nearby and overlapping random
    /// reads can be served without another network round trip. This is also the
    /// speculative read-ahead fetch granularity.
    pub block_size: u64,
    /// Hard ceiling on in-flight speculative read-ahead bytes for the whole
    /// mount. The adaptive prefetch window auto-tunes below this. `0` disables
    /// speculative read-ahead entirely (only demand reads are issued).
    pub read_ahead_max_bytes: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            block_size: crate::MAX_CLIENT_READ_SIZE,
            read_ahead_max_bytes: DEFAULT_READ_AHEAD_MAX_BYTES,
        }
    }
}

#[derive(Clone)]
struct CachedHandle {
    node: NodeId,
    inner: Option<FileHandle>,
    revision: u64,
    size: u64,
    mutation: Arc<Mutex<()>>,
    /// Per-handle adaptive read-ahead state. Shared across the cheap clones of
    /// `CachedHandle` taken by each operation, so one file's sequential stream
    /// keeps one converging window.
    prefetch: Arc<HandlePrefetch>,
}

/// Owns the mutable controller for one open handle behind an async mutex so the
/// read path and completing prefetch tasks can update it without blocking the
/// filesystem lock.
struct HandlePrefetch {
    controller: Mutex<SequentialPrefetcher>,
}

impl HandlePrefetch {
    fn new() -> Self {
        Self {
            controller: Mutex::new(SequentialPrefetcher::default()),
        }
    }
}

/// Adaptive sequential read-ahead controller for a single handle.
///
/// quicKFS exists to hide long round trips. A lone sequential read fails to do
/// that because macFUSE issues sequential access as independent `read`
/// callbacks: each one stalls a full RTT before the next begins, so only one
/// range request is ever in flight and the link sits mostly idle. This
/// controller keeps a *window* of speculative block fetches in flight ahead of
/// each sequential consumer so that, by the time the kernel asks for the next
/// block, it is already cached or already being fetched.
///
/// Two properties matter for real macOS traffic:
///
/// * **Multiple interleaved streams.** The kernel multiplexes independent
///   readers over one FUSE handle (a video player's page-ins interleave with
///   Spotlight and Quick Look touching the same file). The controller tracks
///   up to [`MAX_PREFETCH_STREAMS`] concurrent sequential streams, matching
///   each read to the stream it continues, so one reader's seeks never destroy
///   another reader's window. Reads that continue no tracked stream recycle
///   the least-recently-used slot and start cold — genuinely random access
///   therefore never speculates.
/// * **Stall-driven window sizing.** The window grows exactly when a demand
///   read actually blocked on the network ([`DEMAND_STALL_FLOOR`]) — direct
///   evidence the window does not cover the link's bandwidth-delay product —
///   and holds while demand reads are being served from prefetched data.
///   Growth is paced by delivered bytes (at most one doubling per half-window
///   delivered) so a single long stall cannot balloon the window, and it is
///   bounded by [`CachePolicy`]'s memory ceiling. An earlier design
///   hill-climbed on delivered throughput measured over demand-blocked time;
///   once reads became prefetch hits that signal was pure noise and the
///   window whipsawed instead of holding (live-debugged on a LAN mount —
///   see docs/caching.md).
#[derive(Default)]
struct SequentialPrefetcher {
    /// File revision the streams are tracking. A revision bump orphans
    /// speculatively fetched blocks, so all stream state resets when it
    /// changes.
    revision: u64,
    /// Monotonic source for stream identifiers, used by completion callbacks.
    next_stream_id: u64,
    /// Monotonic touch ordinal for least-recently-used slot recycling.
    touch_counter: u64,
    /// Concurrently tracked sequential streams, at most
    /// [`MAX_PREFETCH_STREAMS`].
    streams: Vec<PrefetchStream>,
}

/// One tracked sequential stream within a handle.
struct PrefetchStream {
    /// Stable identifier so completion callbacks survive slot recycling.
    id: u64,
    /// Last touch ordinal for LRU replacement.
    last_touch: u64,
    /// Whether the run is long enough to speculate on.
    active: bool,
    /// Byte offset at which this stream's next read is expected to begin.
    next_expected: u64,
    /// Consecutive in-order reads observed in the current run.
    run: u64,
    /// Next byte offset not yet scheduled for speculative fetch.
    cursor: u64,
    /// Current target speculative depth, in blocks.
    window_blocks: u64,
    /// Speculative fetches spawned but not yet completed.
    inflight: u64,
    /// Demand bytes delivered since the window last grew, pacing growth.
    bytes_since_growth: u64,
}

impl SequentialPrefetcher {
    /// Records a completed demand read and returns the stream it matched plus
    /// the speculative blocks to schedule next. The stream's `inflight` is
    /// bumped for every returned block so the caller only has to spawn the
    /// fetches and report each completion via [`Self::complete`].
    #[allow(clippy::too_many_arguments)]
    fn observe(
        &mut self,
        node: NodeId,
        offset: u64,
        returned: u64,
        revision: u64,
        size: u64,
        elapsed: Duration,
        block_size: u64,
        cap_blocks: u64,
    ) -> (u64, Vec<RangeKey>) {
        if block_size == 0 || cap_blocks == 0 {
            return (0, Vec::new());
        }
        // A new revision invalidates every speculatively fetched block; start
        // over rather than reading stale cursors forward.
        if revision != self.revision {
            *self = SequentialPrefetcher {
                revision,
                ..SequentialPrefetcher::default()
            };
        }

        let read_end = offset.saturating_add(returned);
        let slack = block_size.saturating_mul(SEQUENTIAL_SLACK_BLOCKS);
        self.touch_counter = self.touch_counter.wrapping_add(1);
        let touch = self.touch_counter;

        // Match this read to the tracked stream it continues: the one whose
        // expected offset is nearest, within slack for kernel read-ahead
        // reordering.
        let matched = self
            .streams
            .iter()
            .enumerate()
            .filter(|(_, stream)| {
                offset <= stream.next_expected.saturating_add(slack)
                    && offset.saturating_add(slack) >= stream.next_expected
            })
            .min_by_key(|(_, stream)| stream.next_expected.abs_diff(offset))
            .map(|(index, _)| index);

        let Some(index) = matched else {
            // A read continuing no tracked stream starts a fresh one; recycle
            // the least-recently-used slot. Other streams keep their windows,
            // so one reader's seek never resets another reader's speculation.
            //
            // Prime a shallow read-ahead only when this is the first read of the
            // handle (no stream tracked yet) — the cold open the user actually
            // waits on. A seek that lands mid-file while another stream is live,
            // or a crawl's short-lived one-shot reads, start cold with no
            // speculation exactly as before.
            let prime_cold_start = self.streams.is_empty();
            if self.streams.len() >= MAX_PREFETCH_STREAMS
                && let Some(oldest) = self
                    .streams
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, stream)| stream.last_touch)
                    .map(|(index, _)| index)
            {
                let evicted = self.streams.swap_remove(oldest);
                if prefetch_debug() && evicted.active {
                    eprintln!(
                        "[prefetch] stream {} evicted: expected={} window={} inflight={}",
                        evicted.id, evicted.next_expected, evicted.window_blocks, evicted.inflight
                    );
                }
            }
            self.next_stream_id = self.next_stream_id.wrapping_add(1);
            let id = self.next_stream_id;
            let mut stream = PrefetchStream {
                id,
                last_touch: touch,
                active: false,
                next_expected: read_end,
                run: 1,
                cursor: read_end,
                window_blocks: 0,
                inflight: 0,
                bytes_since_growth: 0,
            };
            // Prime a shallow read-ahead so the next read is served from cache
            // instead of a serial cold fetch. `window_blocks` is only borrowed
            // to bound this one schedule pass and is reset so the adaptive
            // controller still starts from a clean slate when the run is
            // confirmed below.
            let prime = if prime_cold_start {
                stream.window_blocks = PRIME_WINDOW_BLOCKS.min(cap_blocks);
                let scheduled = Self::schedule(
                    &mut stream,
                    node,
                    revision,
                    read_end,
                    size,
                    block_size,
                    cap_blocks,
                );
                stream.window_blocks = 0;
                scheduled
            } else {
                Vec::new()
            };
            self.streams.push(stream);
            return (id, prime);
        };

        let revision = self.revision;
        let stream = &mut self.streams[index];
        stream.last_touch = touch;
        stream.run = stream.run.saturating_add(1);
        stream.next_expected = stream.next_expected.max(read_end);
        if stream.run < SEQUENTIAL_TRIGGER {
            return (stream.id, Vec::new());
        }
        if !stream.active {
            stream.active = true;
            stream.window_blocks = INITIAL_ACTIVE_WINDOW_BLOCKS.min(cap_blocks);
            stream.cursor = stream.cursor.max(read_end);
            stream.bytes_since_growth = 0;
        }

        // Grow on direct evidence of a too-small window: the demand read
        // blocked on the network. Pace doublings by delivered bytes so one
        // stall spread across several kernel reads grows the window once, and
        // hold (never shrink) while prefetch is keeping demand reads unblocked
        // — jitter absorption is worth far more than the bounded memory a
        // resting window holds.
        stream.bytes_since_growth = stream.bytes_since_growth.saturating_add(returned);
        if elapsed >= DEMAND_STALL_FLOOR
            && stream.window_blocks < cap_blocks
            && stream.bytes_since_growth
                >= stream.window_blocks.max(1).saturating_mul(block_size) / 2
        {
            stream.window_blocks = (stream.window_blocks.saturating_mul(2)).min(cap_blocks);
            stream.bytes_since_growth = 0;
            if prefetch_debug() {
                eprintln!(
                    "[prefetch] stream {} grow: window={} blocked={}ms inflight={}",
                    stream.id,
                    stream.window_blocks,
                    elapsed.as_millis(),
                    stream.inflight
                );
            }
        }

        let blocks = Self::schedule(
            stream, node, revision, read_end, size, block_size, cap_blocks,
        );
        (stream.id, blocks)
    }

    /// Builds the list of not-yet-scheduled blocks in `[cursor, target)` up to
    /// the stream's window depth, advancing its cursor and reserving in-flight
    /// slots.
    #[allow(clippy::too_many_arguments)]
    fn schedule(
        stream: &mut PrefetchStream,
        node: NodeId,
        revision: u64,
        read_end: u64,
        size: u64,
        block_size: u64,
        cap_blocks: u64,
    ) -> Vec<RangeKey> {
        let window = stream.window_blocks.min(cap_blocks);
        if window == 0 || size == 0 {
            return Vec::new();
        }
        // Never re-fetch what the demand path is already pulling: start the
        // speculative region at the block boundary past the current read.
        let demand_block_end = read_end
            .div_ceil(block_size)
            .saturating_mul(block_size)
            .min(size);
        stream.cursor = stream.cursor.max(demand_block_end);
        let target = demand_block_end
            .saturating_add(window.saturating_mul(block_size))
            .min(size);
        // Cap new fetches so this stream's outstanding speculation stays
        // within its window (and any one read's burst stays small); the rest
        // is picked up on later reads as slots free.
        let mut budget = window
            .saturating_sub(stream.inflight)
            .min(SCHEDULE_BURST_BLOCKS);
        let mut blocks = Vec::new();
        while stream.cursor < target && budget > 0 {
            let length = block_size.min(size - stream.cursor);
            if length == 0 {
                break;
            }
            blocks.push(RangeKey {
                file: RevisionKey { node, revision },
                offset: stream.cursor,
                length,
            });
            stream.cursor = stream.cursor.saturating_add(block_size);
            budget -= 1;
        }
        stream.inflight = stream.inflight.saturating_add(blocks.len() as u64);
        blocks
    }

    /// Returns blocks whose permit budget was unavailable to `stream_id`: the
    /// cursor rewinds to the first unscheduled block and the reserved
    /// in-flight slots are released, so a later read re-schedules them once
    /// budget frees instead of leaving a never-fetched hole in the window.
    fn unschedule(&mut self, stream_id: u64, blocks: &[RangeKey]) {
        let Some(first) = blocks.first() else {
            return;
        };
        let Some(stream) = self
            .streams
            .iter_mut()
            .find(|stream| stream.id == stream_id)
        else {
            return;
        };
        stream.inflight = stream.inflight.saturating_sub(blocks.len() as u64);
        stream.cursor = stream.cursor.min(first.offset);
    }

    /// Marks one spawned speculative fetch of `stream_id` as finished. On
    /// error that stream is paused so a dead connection is not hammered; a
    /// later successful demand read re-establishes it. The stream may already
    /// have been recycled, in which case there is nothing to update.
    fn complete(&mut self, stream_id: u64, ok: bool) {
        let Some(stream) = self
            .streams
            .iter_mut()
            .find(|stream| stream.id == stream_id)
        else {
            return;
        };
        stream.inflight = stream.inflight.saturating_sub(1);
        if !ok {
            if prefetch_debug() {
                eprintln!("[prefetch] stream {stream_id} speculative fetch FAILED; pausing");
            }
            stream.active = false;
            stream.window_blocks = 0;
            stream.bytes_since_growth = 0;
        }
    }
}

struct RangeFetch {
    result: OnceCell<std::result::Result<Arc<Vec<u8>>, SharedFetchError>>,
}

#[derive(Clone)]
enum SharedFetchError {
    Server(ErrorCode, String),
    UnexpectedResponse,
    ReadTooLarge(u64),
    WriteTooLarge(u64),
    StaleRevision,
    Offline,
    OfflineCacheMiss,
    AmbiguousMutation,
}

impl From<ClientError> for SharedFetchError {
    fn from(error: ClientError) -> Self {
        match error {
            // A transport failure has already passed through the resilient
            // reconnect layer. Followers of the same fetch must observe the
            // same offline result instead of starting another reconnect storm.
            ClientError::Transport(_) | ClientError::Offline => Self::Offline,
            ClientError::Server(code, message) => Self::Server(code, message),
            ClientError::UnexpectedResponse => Self::UnexpectedResponse,
            ClientError::ReadTooLarge(limit) => Self::ReadTooLarge(limit),
            ClientError::WriteTooLarge(limit) => Self::WriteTooLarge(limit),
            ClientError::StaleRevision => Self::StaleRevision,
            ClientError::OfflineCacheMiss => Self::OfflineCacheMiss,
            ClientError::AmbiguousMutation => Self::AmbiguousMutation,
        }
    }
}

impl From<SharedFetchError> for ClientError {
    fn from(error: SharedFetchError) -> Self {
        match error {
            SharedFetchError::Server(code, message) => Self::Server(code, message),
            SharedFetchError::UnexpectedResponse => Self::UnexpectedResponse,
            SharedFetchError::ReadTooLarge(limit) => Self::ReadTooLarge(limit),
            SharedFetchError::WriteTooLarge(limit) => Self::WriteTooLarge(limit),
            SharedFetchError::StaleRevision => Self::StaleRevision,
            SharedFetchError::Offline => Self::Offline,
            SharedFetchError::OfflineCacheMiss => Self::OfflineCacheMiss,
            SharedFetchError::AmbiguousMutation => Self::AmbiguousMutation,
        }
    }
}

/// Shared, `Arc`-owned block-fetch engine. Demand reads and speculative
/// read-ahead tasks both route through one instance so a prefetched block and a
/// later real read for the same block coalesce in the single-flight map. It is
/// separated from [`CachedFilesystem`] precisely so a fire-and-forget prefetch
/// task can hold an owned `Arc<Fetcher>` and outlive the read that scheduled it.
struct Fetcher {
    inner: Arc<dyn RemoteFilesystem>,
    cache: Arc<dyn FilesystemCache>,
    range_fetches: Mutex<HashMap<RangeKey, Arc<RangeFetch>>>,
    /// Global budget on speculative bytes in flight, denominated in
    /// [`PREFETCH_PERMIT_UNIT`] units and sized to the read-ahead memory
    /// ceiling. Each speculative fetch acquires permits proportional to its
    /// block length, so the ceiling holds regardless of block granularity
    /// (an earlier version counted whole `block_size` blocks, which capped
    /// 1 MiB-granularity video streams at four concurrent fetches). Demand
    /// fetches never take a permit, so a wall of speculation can never delay
    /// an interactive read.
    prefetch_permits: Arc<Semaphore>,
    /// Total permits in `prefetch_permits`, for clamping one block's request.
    prefetch_permit_total: u32,
    /// Cap on concurrent speculative fetches
    /// ([`PREFETCH_MAX_CONCURRENT_FETCHES`]), so speculation streams the
    /// window as a fast trickle instead of a convoy.
    prefetch_fetch_slots: Arc<Semaphore>,
}

/// Permits one speculative fetch of `length` bytes must hold, clamped so a
/// single block can never request more than the whole pool.
fn prefetch_permits_for(length: u64, total: u32) -> u32 {
    u32::try_from(length.div_ceil(PREFETCH_PERMIT_UNIT))
        .unwrap_or(u32::MAX)
        .clamp(1, total.max(1))
}

impl Fetcher {
    async fn fetch_block(
        &self,
        inner_handle: FileHandle,
        expected_revision: u64,
        block: RangeKey,
    ) -> Result<Vec<u8>> {
        let fetch = {
            let mut fetches = self.range_fetches.lock().await;
            fetches
                .entry(block)
                .or_insert_with(|| {
                    Arc::new(RangeFetch {
                        result: OnceCell::new(),
                    })
                })
                .clone()
        };
        let result = fetch
            .result
            .get_or_init(|| async {
                let loaded = async {
                    if let Some(data) = RangeCache::get(self.cache.as_ref(), block).await {
                        return Ok(Arc::new(data));
                    }
                    let read = self
                        .inner
                        .read_range_versioned(inner_handle, block.offset, block.length)
                        .await?;
                    if read.revision != expected_revision {
                        return Err(ClientError::StaleRevision);
                    }
                    let actual = u64::try_from(read.data.len())
                        .map_err(|_| ClientError::UnexpectedResponse)?;
                    if actual > block.length {
                        return Err(ClientError::UnexpectedResponse);
                    }
                    if actual > 0 {
                        let actual_key = RangeKey {
                            length: actual,
                            ..block
                        };
                        RangeCache::store_readthrough(
                            self.cache.as_ref(),
                            actual_key,
                            read.data.clone(),
                        )
                        .await;
                    }
                    Ok(Arc::new(read.data))
                }
                .await;
                loaded.map_err(SharedFetchError::from)
            })
            .await
            .clone();
        let mut fetches = self.range_fetches.lock().await;
        if fetches
            .get(&block)
            .is_some_and(|registered| Arc::ptr_eq(registered, &fetch))
        {
            fetches.remove(&block);
        }
        result
            .map(|data| data.as_ref().clone())
            .map_err(ClientError::from)
    }
}

/// Adds a revision-keyed read-through/offline cache without weakening remote
/// write, fsync, or lock semantics. Offline access is intentionally read-only.
pub struct CachedFilesystem {
    inner: Arc<dyn RemoteFilesystem>,
    cache: Arc<dyn FilesystemCache>,
    policy: CachePolicy,
    handles: RwLock<HashMap<FileHandle, CachedHandle>>,
    capabilities: RwLock<Option<FilesystemCapabilities>>,
    refreshing_directories: Arc<Mutex<HashSet<NodeId>>>,
    fetcher: Arc<Fetcher>,
    directory_parents: RwLock<HashMap<NodeId, NodeId>>,
}

impl CachedFilesystem {
    pub fn new(
        inner: Arc<dyn RemoteFilesystem>,
        cache: Arc<dyn FilesystemCache>,
        policy: CachePolicy,
    ) -> Result<Self> {
        if policy.block_size == 0 || policy.block_size > crate::MAX_CLIENT_READ_SIZE {
            return Err(ClientError::Server(
                ErrorCode::InvalidRequest,
                "cache block size must be within the client read limit".into(),
            ));
        }
        // At least one permit keeps the semaphore usable even when read-ahead
        // is configured off; scheduling is separately gated on the block cap.
        // The pool is denominated in PREFETCH_PERMIT_UNIT bytes (clamped well
        // under tokio's permit ceiling) so streams of any block granularity
        // share the same byte budget.
        // The in-flight prefetch budget is the *convoy* ceiling, not the
        // buffer-depth ceiling: cap it well below `read_ahead_max_bytes` (which
        // still governs how deep the window may grow) so a demand read never
        // queues behind a large-block speculation convoy on the shared link.
        let prefetch_inflight_budget = policy.read_ahead_max_bytes.min(PREFETCH_MAX_INFLIGHT_BYTES);
        let prefetch_permit_total =
            u32::try_from((prefetch_inflight_budget / PREFETCH_PERMIT_UNIT).clamp(1, 1 << 20))
                .unwrap_or(1 << 20);
        let prefetch_permits = Arc::new(Semaphore::new(prefetch_permit_total as usize));
        let fetcher = Arc::new(Fetcher {
            inner: Arc::clone(&inner),
            cache: Arc::clone(&cache),
            range_fetches: Mutex::new(HashMap::new()),
            prefetch_permits,
            prefetch_permit_total,
            prefetch_fetch_slots: Arc::new(Semaphore::new(PREFETCH_MAX_CONCURRENT_FETCHES)),
        });
        Ok(Self {
            inner,
            cache,
            policy,
            handles: RwLock::new(HashMap::new()),
            capabilities: RwLock::new(None),
            refreshing_directories: Arc::new(Mutex::new(HashSet::new())),
            fetcher,
            directory_parents: RwLock::new(HashMap::from([(ROOT_NODE, ROOT_NODE)])),
        })
    }

    async fn cache_metadata(&self, metadata: Metadata) -> Metadata {
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        metadata
    }

    async fn cached_metadata(&self, node: NodeId) -> Result<Metadata> {
        MetadataCache::get(self.cache.as_ref(), node)
            .await
            .ok_or(ClientError::OfflineCacheMiss)
    }

    async fn remember_handle(
        &self,
        node: NodeId,
        inner: Option<FileHandle>,
        revision: u64,
        size: u64,
        _options: FileOpenOptions,
    ) -> OpenedFile {
        let logical = FileHandle(Uuid::new_v4());
        self.handles.write().await.insert(
            logical,
            CachedHandle {
                node,
                inner,
                revision,
                size,
                mutation: Arc::new(Mutex::new(())),
                prefetch: Arc::new(HandlePrefetch::new()),
            },
        );
        OpenedFile {
            handle: logical,
            revision,
            size,
        }
    }

    async fn handle(&self, logical: FileHandle) -> Result<CachedHandle> {
        self.handles
            .read()
            .await
            .get(&logical)
            .cloned()
            .ok_or_else(|| {
                ClientError::Server(ErrorCode::InvalidHandle, "unknown cached handle".into())
            })
    }

    async fn invalidate_node(&self, node: NodeId) {
        NodeCacheInvalidation::invalidate_node_state(self.cache.as_ref(), node).await;
    }

    /// In-memory-only node invalidation for the streaming write hot path. See
    /// `NodeCacheInvalidation::invalidate_node_memory`.
    async fn invalidate_node_memory(&self, node: NodeId) {
        NodeCacheInvalidation::invalidate_node_memory(self.cache.as_ref(), node).await;
    }

    async fn offline_directory(&self, node: NodeId) -> Result<DirectorySnapshot> {
        let metadata = self.cached_metadata(node).await?;
        let key = RevisionKey {
            node,
            revision: metadata.revision,
        };
        DirectoryCache::get(self.cache.as_ref(), key)
            .await
            .map(|entries| DirectorySnapshot {
                revision: metadata.revision,
                entries,
            })
            .ok_or(ClientError::OfflineCacheMiss)
    }

    async fn refresh_directory_in_background(&self, node: NodeId) {
        {
            let mut refreshing = self.refreshing_directories.lock().await;
            if !refreshing.insert(node) {
                return;
            }
        }
        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);
        let refreshing = Arc::clone(&self.refreshing_directories);
        tokio::spawn(async move {
            if let Ok(snapshot) = inner.list_directory_snapshot(node).await {
                let key = RevisionKey {
                    node,
                    revision: snapshot.revision,
                };
                DirectoryCache::store_readthrough_snapshot(cache.as_ref(), key, snapshot.entries)
                    .await;
                if let Ok(metadata) = inner.get_metadata(node).await
                    && metadata.revision == snapshot.revision
                {
                    MetadataCache::store_readthrough(cache.as_ref(), metadata).await;
                }
            }
            refreshing.lock().await.remove(&node);
        });
    }

    async fn cached_range(
        &self,
        state: &CachedHandle,
        offset: u64,
        length: u64,
    ) -> Option<Vec<u8>> {
        let available = state.size.saturating_sub(offset);
        let length = length.min(available);
        RangeCache::get(
            self.cache.as_ref(),
            RangeKey {
                file: RevisionKey {
                    node: state.node,
                    revision: state.revision,
                },
                offset,
                length,
            },
        )
        .await
    }

    /// Block granularity used to serve a demand read of `length` bytes. Header
    /// probes and thumbnail reads should not pull 16 MiB from every file Finder
    /// touches: sequential/copy-sized requests retain the large aligned block,
    /// while small reads use a bounded 1 MiB window. Read-ahead schedules at
    /// the same granularity so speculative and demand fetches coalesce.
    fn demand_block_size(&self, length: u64) -> u64 {
        // Inclusive: macFUSE commonly delivers reads of exactly 1 MiB (its
        // negotiated max read-ahead), and serving those from 16 MiB blocks
        // makes every prefetch miss a whole-large-block wait — a multi-second
        // playback freeze on links in the tens of MB/s.
        if length <= SMALL_READ_THRESHOLD {
            self.policy.block_size.min(SMALL_READ_AHEAD_BLOCK_SIZE)
        } else {
            self.policy.block_size
        }
    }

    fn blocks_for(&self, state: &CachedHandle, offset: u64, length: u64) -> Result<Vec<RangeKey>> {
        if offset.checked_add(length).is_none() {
            return Err(ClientError::Server(
                ErrorCode::InvalidRequest,
                "read range overflows".into(),
            ));
        }
        let available = state.size.saturating_sub(offset);
        let requested = length.min(available);
        if requested == 0 {
            return Ok(Vec::new());
        }
        let requested_end = offset
            .checked_add(requested)
            .ok_or(ClientError::UnexpectedResponse)?;
        let block_size = self.demand_block_size(length);
        let mut block_offset = offset / block_size * block_size;
        let mut blocks = Vec::new();
        while block_offset < requested_end {
            let block_length = self
                .policy
                .block_size
                .min(block_size)
                .min(state.size.saturating_sub(block_offset));
            blocks.push(RangeKey {
                file: RevisionKey {
                    node: state.node,
                    revision: state.revision,
                },
                offset: block_offset,
                length: block_length,
            });
            block_offset = block_offset
                .checked_add(block_size)
                .ok_or(ClientError::UnexpectedResponse)?;
        }
        Ok(blocks)
    }

    /// After a demand read, feed the per-handle adaptive controller and, when a
    /// sequential run is established, kick speculative fetches for blocks ahead
    /// of the consumer. Every fetch routes through the shared single-flight
    /// gate, so a later kernel read for a prefetched block finds it cached or
    /// joins the in-flight fetch instead of stalling a full round trip.
    ///
    /// `granularity` is the block size the demand path chose for this read
    /// (`blocks_for` uses 1 MiB blocks for sub-1 MiB reads). Scheduling
    /// read-ahead at the same granularity keeps speculative and demand
    /// `RangeKey`s identical so they coalesce, and it is what lets a stream of
    /// small kernel reads — Preview and Quick Look read images this way — and
    /// files smaller than the large block size get read-ahead at all.
    async fn maybe_prefetch(
        &self,
        state: &CachedHandle,
        inner_handle: FileHandle,
        offset: u64,
        returned: u64,
        elapsed: Duration,
        granularity: u64,
    ) {
        if granularity == 0 {
            return;
        }
        let cap_blocks = self.policy.read_ahead_max_bytes / granularity;
        if cap_blocks == 0 {
            return;
        }
        let (stream_id, blocks) = {
            let mut controller = state.prefetch.controller.lock().await;
            let (stream_id, scheduled) = controller.observe(
                state.node,
                offset,
                returned,
                state.revision,
                state.size,
                elapsed,
                granularity,
                cap_blocks,
            );
            if prefetch_debug() && elapsed.as_millis() > 250 {
                eprintln!(
                    "[prefetch] demand stalled: off={:.1}MB len={}KiB blocked={}ms gran={}KiB stream={} sched={}",
                    offset as f64 / 1e6,
                    returned / 1024,
                    elapsed.as_millis(),
                    granularity / 1024,
                    stream_id,
                    scheduled.len()
                );
            }
            // Reserve byte-proportional permits from the global pool *now*,
            // while the blocks are still ours to unschedule. A block whose
            // budget is unavailable must not be scheduled at all: a task
            // parked on the semaphore counts as in-flight and has advanced
            // the cursor, so the controller believes the block is coming
            // while nothing fetches it — a hole in the prefetched region
            // that later surfaces as a multi-second demand stall behind a
            // wall of speculative transfers. Demand reads never take
            // permits, so speculation still can never starve interactive
            // I/O.
            let mut granted = Vec::with_capacity(scheduled.len());
            let mut denied = None;
            for (index, block) in scheduled.iter().enumerate() {
                let units = prefetch_permits_for(block.length, self.fetcher.prefetch_permit_total);
                let Ok(slot) = Arc::clone(&self.fetcher.prefetch_fetch_slots).try_acquire_owned()
                else {
                    denied = Some(index);
                    break;
                };
                match Arc::clone(&self.fetcher.prefetch_permits).try_acquire_many_owned(units) {
                    Ok(permit) => granted.push((*block, slot, permit)),
                    Err(_) => {
                        denied = Some(index);
                        break;
                    }
                }
            }
            if let Some(denied) = denied {
                controller.unschedule(stream_id, &scheduled[denied..]);
                if prefetch_debug() {
                    eprintln!(
                        "[prefetch] stream {} permit-starved: unscheduled {} of {} blocks",
                        stream_id,
                        scheduled.len() - denied,
                        scheduled.len()
                    );
                }
            }
            (stream_id, granted)
        };
        for (block, slot, permit) in blocks {
            let fetcher = Arc::clone(&self.fetcher);
            let handle_prefetch = Arc::clone(&state.prefetch);
            let expected_revision = state.revision;
            tokio::spawn(async move {
                // The fetch slot and byte permits reserved above are held for
                // the fetch's lifetime and released on drop. The fetch itself
                // is deadlined: a fetch wedged behind a dying connection and
                // its reconnect/retry ladder must not pin its slot and byte
                // budget indefinitely — with all slots pinned, speculation
                // halts mount-wide, and a demand read that single-flight-joins
                // the wedged fetch inherits the unbounded wait (observed live
                // as a multi-minute mmap page-in hang). On timeout the fetch
                // future is dropped (a single-flight joiner, if any, takes
                // over the initialization), the stream is paused, and the
                // block is simply refetched on demand later.
                let _slot = slot;
                let _permit = permit;
                let started = Instant::now();
                let ok = tokio::time::timeout(
                    PREFETCH_FETCH_DEADLINE,
                    fetcher.fetch_block(inner_handle, expected_revision, block),
                )
                .await
                .map(|result| result.is_ok())
                .unwrap_or(false);
                if prefetch_debug() && (!ok || started.elapsed().as_millis() > 1000) {
                    eprintln!(
                        "[prefetch] speculative fetch off={:.1}MB took {}ms ok={ok}",
                        block.offset as f64 / 1e6,
                        started.elapsed().as_millis()
                    );
                }
                handle_prefetch
                    .controller
                    .lock()
                    .await
                    .complete(stream_id, ok);
            });
        }
    }

    async fn update_handle(&self, logical: FileHandle, result: WriteResult) {
        if let Some(state) = self.handles.write().await.get_mut(&logical) {
            state.revision = result.revision;
            state.size = result.size;
        }
    }

    /// One attempt to serve an online demand read at the revision recorded in
    /// `state`. Returns [`ClientError::StaleRevision`] when the remote file has
    /// moved past that revision; the caller refreshes its snapshot and retries.
    async fn read_online(
        &self,
        state: &CachedHandle,
        inner_handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<RangeRead> {
        let blocks = self.blocks_for(state, offset, length)?;
        if blocks.is_empty() {
            return Ok(RangeRead {
                revision: state.revision,
                data: Vec::new(),
            });
        }
        let assembled = MemoryCache::default();
        // Every block, including a cache lookup, passes through the per-block
        // gate. Concurrent kernel read-ahead callbacks therefore share one
        // persistent-cache verification or one network request instead of all
        // hashing the same large block before they reach the fetch gate.
        let reads = blocks.iter().map(|block| async {
            (
                *block,
                self.fetcher
                    .fetch_block(inner_handle, state.revision, *block)
                    .await,
            )
        });
        // Time how long the demand read blocks. When the adaptive window is too
        // small, blocks miss and this is dominated by network RTT; once the
        // window covers the pipe, blocks are prefetched hits and this collapses.
        // That gradient is exactly what drives the controller.
        let demand_started = Instant::now();
        let mut offline_error = false;
        for (block, result) in join_all(reads).await {
            match result {
                Ok(data) => {
                    let actual =
                        u64::try_from(data.len()).map_err(|_| ClientError::UnexpectedResponse)?;
                    if actual > block.length {
                        return Err(ClientError::UnexpectedResponse);
                    }
                    if actual > 0 {
                        let actual_key = RangeKey {
                            length: actual,
                            ..block
                        };
                        RangeCache::insert(&assembled, actual_key, data).await;
                    }
                }
                Err(error) if is_offline(&error) => offline_error = true,
                Err(error) => return Err(error),
            }
        }
        let demand_elapsed = demand_started.elapsed();
        let requested = RangeKey {
            file: RevisionKey {
                node: state.node,
                revision: state.revision,
            },
            offset,
            length: length.min(state.size.saturating_sub(offset)),
        };
        if let Some(data) = RangeCache::get(&assembled, requested).await {
            let returned = u64::try_from(data.len()).unwrap_or(0);
            self.maybe_prefetch(
                state,
                inner_handle,
                offset,
                returned,
                demand_elapsed,
                self.demand_block_size(length),
            )
            .await;
            return Ok(RangeRead {
                revision: state.revision,
                data,
            });
        }
        if let Some(data) = self.cached_range(state, offset, length).await {
            return Ok(RangeRead {
                revision: state.revision,
                data,
            });
        }
        if offline_error {
            Err(ClientError::OfflineCacheMiss)
        } else {
            Err(ClientError::UnexpectedResponse)
        }
    }
}

fn is_offline(error: &ClientError) -> bool {
    matches!(error, ClientError::Transport(_) | ClientError::Offline)
}

fn require_online_handle(state: &CachedHandle) -> Result<FileHandle> {
    state.inner.ok_or(ClientError::Offline)
}

#[async_trait]
impl RemoteFilesystem for CachedFilesystem {
    async fn ping(&self, nonce: u64) -> Result<u64> {
        self.inner.ping(nonce).await
    }

    async fn capabilities(&self) -> Result<FilesystemCapabilities> {
        match self.inner.capabilities().await {
            Ok(capabilities) => {
                *self.capabilities.write().await = Some(capabilities.clone());
                Ok(capabilities)
            }
            Err(error) if is_offline(&error) => self
                .capabilities
                .read()
                .await
                .clone()
                .ok_or(ClientError::Offline),
            Err(error) => Err(error),
        }
    }

    async fn stat_filesystem(&self) -> Result<FilesystemStats> {
        match self.inner.stat_filesystem().await {
            Ok(statistics) => {
                self.cache
                    .store_readthrough_filesystem_stats(statistics)
                    .await;
                Ok(statistics)
            }
            Err(error) if is_offline(&error) => self
                .cache
                .get_filesystem_stats()
                .await
                .ok_or(ClientError::OfflineCacheMiss),
            Err(error) => Err(error),
        }
    }

    async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
        match self.inner.get_metadata(node).await {
            Ok(metadata) => Ok(self.cache_metadata(metadata).await),
            Err(error) if is_offline(&error) => self.cached_metadata(node).await,
            Err(error) => Err(error),
        }
    }

    async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
        Ok(self.list_directory_snapshot(node).await?.entries)
    }

    async fn list_directory_snapshot(&self, node: NodeId) -> Result<DirectorySnapshot> {
        if let Ok(snapshot) = self.offline_directory(node).await {
            self.refresh_directory_in_background(node).await;
            return Ok(snapshot);
        }
        match self.inner.list_directory_snapshot(node).await {
            Ok(snapshot) => {
                let key = RevisionKey {
                    node,
                    revision: snapshot.revision,
                };
                DirectoryCache::store_readthrough_snapshot(
                    self.cache.as_ref(),
                    key,
                    snapshot.entries.clone(),
                )
                .await;
                Ok(snapshot)
            }
            Err(error) if is_offline(&error) => self.offline_directory(node).await,
            Err(error) => Err(error),
        }
    }

    async fn list_directory_view(
        &self,
        node: NodeId,
        options: DirectoryViewOptions,
    ) -> Result<DirectoryView> {
        match self.inner.list_directory_view(node, options).await {
            Ok(view) => {
                let entries = view
                    .entries
                    .iter()
                    .map(|entry| entry.entry.clone())
                    .collect::<Vec<_>>();
                let key = RevisionKey {
                    node,
                    revision: view.revision,
                };
                DirectoryCache::store_readthrough_snapshot(self.cache.as_ref(), key, entries).await;
                self.cache_metadata(view.directory.clone()).await;
                self.cache_metadata(view.parent.clone()).await;
                self.directory_parents
                    .write()
                    .await
                    .insert(node, view.parent.node);
                Ok(view)
            }
            Err(error) if is_offline(&error) => {
                let snapshot = self.offline_directory(node).await?;
                let directory = self.cached_metadata(node).await?;
                let parent_node = self
                    .directory_parents
                    .read()
                    .await
                    .get(&node)
                    .copied()
                    .ok_or(ClientError::OfflineCacheMiss)?;
                let parent = self.cached_metadata(parent_node).await?;
                Ok(DirectoryView {
                    revision: snapshot.revision,
                    parent,
                    directory,
                    xattrs: None,
                    entries: snapshot
                        .entries
                        .into_iter()
                        .map(|entry| DirectoryViewEntry {
                            entry,
                            xattrs: None,
                        })
                        .collect(),
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)> {
        let opened = self
            .open_file_with_options(node, FileOpenOptions::READ_ONLY)
            .await?;
        Ok((opened.handle, opened.revision, opened.size))
    }

    async fn open_file_with_options(
        &self,
        node: NodeId,
        options: FileOpenOptions,
    ) -> Result<OpenedFile> {
        match self.inner.open_file_with_options(node, options).await {
            Ok(opened) => Ok(self
                .remember_handle(
                    node,
                    Some(opened.handle),
                    opened.revision,
                    opened.size,
                    options,
                )
                .await),
            Err(error)
                if is_offline(&error)
                    && options.access == FileAccess::ReadOnly
                    && !options.truncate
                    && !options.append =>
            {
                let metadata = self.cached_metadata(node).await?;
                Ok(self
                    .remember_handle(node, None, metadata.revision, metadata.size, options)
                    .await)
            }
            Err(error) => Err(error),
        }
    }

    async fn create_file(
        &self,
        parent: NodeId,
        name: Name,
        mode: u32,
        options: FileOpenOptions,
    ) -> Result<CreatedFile> {
        let created = self.inner.create_file(parent, name, mode, options).await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), created.metadata.clone()).await;
        let opened = self
            .remember_handle(
                created.metadata.node,
                Some(created.opened.handle),
                created.opened.revision,
                created.opened.size,
                options,
            )
            .await;
        Ok(CreatedFile {
            metadata: created.metadata,
            opened,
        })
    }

    async fn create_directory(&self, parent: NodeId, name: Name, mode: u32) -> Result<Metadata> {
        let metadata = self.inner.create_directory(parent, name, mode).await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        self.directory_parents
            .write()
            .await
            .insert(metadata.node, parent);
        Ok(metadata)
    }

    async fn create_symlink(
        &self,
        parent: NodeId,
        name: Name,
        target: Vec<u8>,
    ) -> Result<Metadata> {
        let cached_target = target.clone();
        let metadata = self.inner.create_symlink(parent, name, target).await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        if u64::try_from(cached_target.len()).ok() == Some(metadata.size) {
            RangeCache::store_readthrough(
                self.cache.as_ref(),
                RangeKey {
                    file: RevisionKey {
                        node: metadata.node,
                        revision: metadata.revision,
                    },
                    offset: 0,
                    length: metadata.size,
                },
                cached_target,
            )
            .await;
        }
        Ok(metadata)
    }

    async fn create_hard_link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: Name,
    ) -> Result<Metadata> {
        let metadata = self
            .inner
            .create_hard_link(node, new_parent, new_name)
            .await?;
        self.invalidate_node(node).await;
        self.invalidate_node(new_parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        Ok(metadata)
    }

    async fn create_special_node(
        &self,
        parent: NodeId,
        name: Name,
        kind: SpecialNodeKind,
        mode: u32,
        device_major: u32,
        device_minor: u32,
    ) -> Result<Metadata> {
        let metadata = self
            .inner
            .create_special_node(parent, name, kind, mode, device_major, device_minor)
            .await?;
        self.invalidate_node(parent).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        Ok(metadata)
    }

    async fn remove_node(&self, parent: NodeId, name: Name, directory: bool) -> Result<()> {
        let cached_child = self
            .offline_directory(parent)
            .await
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .entries
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.node)
            });
        self.inner.remove_node(parent, name, directory).await?;
        self.invalidate_node(parent).await;
        if let Some(node) = cached_child {
            self.invalidate_node(node).await;
            self.directory_parents.write().await.remove(&node);
        }
        Ok(())
    }

    async fn rename_node(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        mode: RenameMode,
    ) -> Result<()> {
        let source = self
            .offline_directory(parent)
            .await
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .entries
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.node)
            });
        let destination = self
            .offline_directory(new_parent)
            .await
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .entries
                    .into_iter()
                    .find(|entry| entry.name == new_name)
                    .map(|entry| entry.node)
            });
        self.inner
            .rename_node(parent, name, new_parent, new_name, mode)
            .await?;
        self.invalidate_node(parent).await;
        self.invalidate_node(new_parent).await;
        let mut directory_parents = self.directory_parents.write().await;
        if let Some(source) = source {
            directory_parents.insert(source, new_parent);
        }
        if mode == RenameMode::Exchange {
            if let Some(destination) = destination {
                directory_parents.insert(destination, parent);
            }
        } else if let Some(destination) = destination {
            directory_parents.remove(&destination);
        }
        Ok(())
    }

    async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
        match self.inner.read_link(node).await {
            Ok(target) => {
                let metadata = match self.inner.get_metadata(node).await {
                    Ok(metadata) => self.cache_metadata(metadata).await,
                    Err(_) => return Ok(target),
                };
                if u64::try_from(target.len()).ok() == Some(metadata.size) {
                    RangeCache::store_readthrough(
                        self.cache.as_ref(),
                        RangeKey {
                            file: RevisionKey {
                                node,
                                revision: metadata.revision,
                            },
                            offset: 0,
                            length: metadata.size,
                        },
                        target.clone(),
                    )
                    .await;
                }
                Ok(target)
            }
            Err(error) if is_offline(&error) => {
                let metadata = self.cached_metadata(node).await?;
                RangeCache::get(
                    self.cache.as_ref(),
                    RangeKey {
                        file: RevisionKey {
                            node,
                            revision: metadata.revision,
                        },
                        offset: 0,
                        length: metadata.size,
                    },
                )
                .await
                .ok_or(ClientError::OfflineCacheMiss)
            }
            Err(error) => Err(error),
        }
    }

    async fn set_attributes(
        &self,
        node: NodeId,
        handle: Option<FileHandle>,
        changes: AttributeChanges,
    ) -> Result<Metadata> {
        let state = match handle {
            Some(logical) => Some(self.handle(logical).await?),
            None => None,
        };
        let _mutation = match &state {
            Some(state) => Some(state.mutation.lock().await),
            None => None,
        };
        let mapped = state.as_ref().map(require_online_handle).transpose()?;
        let metadata = self.inner.set_attributes(node, mapped, changes).await?;
        self.invalidate_node(node).await;
        MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
        if let Some(logical) = handle
            && let Some(state) = self.handles.write().await.get_mut(&logical)
        {
            state.revision = metadata.revision;
            state.size = metadata.size;
        }
        Ok(metadata)
    }

    async fn read_range(&self, handle: FileHandle, offset: u64, length: u64) -> Result<Vec<u8>> {
        Ok(self
            .read_range_versioned(handle, offset, length)
            .await?
            .data)
    }

    async fn read_range_versioned(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<RangeRead> {
        let mut state = self.handle(handle).await?;
        if offset.checked_add(length).is_none() {
            return Err(ClientError::Server(
                ErrorCode::InvalidRequest,
                "read range overflows".into(),
            ));
        }
        let Some(inner_handle) = state.inner else {
            return self
                .cached_range(&state, offset, length)
                .await
                .map(|data| RangeRead {
                    revision: state.revision,
                    data,
                })
                .ok_or(ClientError::OfflineCacheMiss);
        };

        // A newer revision observed on the wire is not an application-visible
        // error: POSIX guarantees an open descriptor keeps reading after
        // another actor updates the file, and macOS does so constantly —
        // Finder's copy engine sets a fresh copy's timestamps immediately
        // after writing it, which bumps the remote revision and used to turn
        // every later read on an already-open handle into ESTALE (Finder
        // error 100070; SIGBUS under mmap for Preview/QuickTime). Refresh the
        // handle's revision snapshot and retry instead. Each attempt is still
        // torn-free because every block within it is fetched at one expected
        // revision.
        let mut attempt = 0;
        loop {
            match self.read_online(&state, inner_handle, offset, length).await {
                Err(ClientError::StaleRevision) if attempt < STALE_REVISION_RETRIES => {
                    attempt += 1;
                    let metadata = self.inner.get_metadata(state.node).await?;
                    MetadataCache::store_readthrough(self.cache.as_ref(), metadata.clone()).await;
                    if let Some(entry) = self.handles.write().await.get_mut(&handle) {
                        entry.revision = metadata.revision;
                        entry.size = metadata.size;
                    }
                    state.revision = metadata.revision;
                    state.size = metadata.size;
                }
                result => return result,
            }
        }
    }

    async fn write_range(
        &self,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<WriteResult> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        let inner_handle = require_online_handle(&state)?;
        let result = self.inner.write_range(inner_handle, offset, data).await?;
        // Invalidate only the volatile in-memory projections. A media copy is
        // delivered by the kernel as thousands of small FUSE writes; doing any
        // durable per-chunk cache work (a range insert or a manifest
        // invalidation) floods the single cache-writer thread and its backlog
        // later stalls unrelated durable operations. Written ranges are also
        // revision-orphaned by the next chunk, so persisting them is pure churn;
        // a later read repopulates the cache through the read fill path.
        self.invalidate_node_memory(state.node).await;
        self.update_handle(handle, result).await;
        Ok(result)
    }

    async fn flush_file(&self, handle: FileHandle, lock_owner: Option<u64>) -> Result<()> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        if let Some(inner) = state.inner {
            self.inner.flush_file(inner, lock_owner).await
        } else {
            // Offline handles are necessarily read-only. There are no dirty
            // bytes or remote locks to flush, so FUSE close must still succeed.
            Ok(())
        }
    }

    async fn sync_file(&self, handle: FileHandle, data_only: bool) -> Result<()> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        if let Some(inner) = state.inner {
            self.inner.sync_file(inner, data_only).await
        } else {
            // A cached read-only handle has no pending mutations.
            Ok(())
        }
    }

    async fn sync_directory(&self, node: NodeId) -> Result<()> {
        self.inner.sync_directory(node).await
    }

    async fn allocate_file(
        &self,
        handle: FileHandle,
        offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        let state = self.handle(handle).await?;
        let _mutation = state.mutation.lock().await;
        let result = self
            .inner
            .allocate_file(require_online_handle(&state)?, offset, length)
            .await?;
        self.invalidate_node(state.node).await;
        self.update_handle(handle, result).await;
        Ok(result)
    }

    async fn get_xattr(
        &self,
        node: NodeId,
        name: Name,
        offset: u64,
        length: u64,
    ) -> Result<XattrRead> {
        self.inner.get_xattr(node, name, offset, length).await
    }

    async fn set_xattr(
        &self,
        node: NodeId,
        name: Name,
        value: &[u8],
        mode: XattrSetMode,
        position: u32,
    ) -> Result<()> {
        self.inner
            .set_xattr(node, name, value, mode, position)
            .await?;
        self.invalidate_node(node).await;
        Ok(())
    }

    async fn list_xattrs(&self, node: NodeId) -> Result<Vec<Name>> {
        self.inner.list_xattrs(node).await
    }

    async fn remove_xattr(&self, node: NodeId, name: Name) -> Result<()> {
        self.inner.remove_xattr(node, name).await?;
        self.invalidate_node(node).await;
        Ok(())
    }

    async fn copy_file_range(
        &self,
        input: FileHandle,
        input_offset: u64,
        output: FileHandle,
        output_offset: u64,
        length: u64,
    ) -> Result<WriteResult> {
        let input_state = self.handle(input).await?;
        let output_state = self.handle(output).await?;
        let input_handle = require_online_handle(&input_state)?;
        let output_handle = require_online_handle(&output_state)?;
        let result = self
            .inner
            .copy_file_range(
                input_handle,
                input_offset,
                output_handle,
                output_offset,
                length,
            )
            .await?;
        self.invalidate_node(output_state.node).await;
        self.update_handle(output, result).await;
        Ok(result)
    }

    async fn seek_file(&self, handle: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
        let state = self.handle(handle).await?;
        self.inner
            .seek_file(require_online_handle(&state)?, offset, whence)
            .await
    }

    async fn safe_ioctl(&self, handle: FileHandle, operation: SafeIoctl) -> Result<u64> {
        let state = self.handle(handle).await?;
        self.inner
            .safe_ioctl(require_online_handle(&state)?, operation)
            .await
    }

    async fn map_block(&self, node: NodeId, block_size: u32, block: u64) -> Result<u64> {
        self.inner.map_block(node, block_size, block).await
    }

    async fn exchange_data(
        &self,
        parent: NodeId,
        name: Name,
        new_parent: NodeId,
        new_name: Name,
        options: u64,
    ) -> Result<()> {
        self.inner
            .exchange_data(parent, name, new_parent, new_name, options)
            .await?;
        self.invalidate_node(parent).await;
        self.invalidate_node(new_parent).await;
        Ok(())
    }

    async fn set_volume_name(&self, name: Name) -> Result<()> {
        self.inner.set_volume_name(name).await
    }

    async fn forget_nodes(&self, nodes: Vec<NodeId>) -> Result<()> {
        self.inner.forget_nodes(nodes).await
    }

    async fn get_lock(&self, handle: FileHandle, lock: FileLock) -> Result<Option<FileLock>> {
        let state = self.handle(handle).await?;
        self.inner
            .get_lock(require_online_handle(&state)?, lock)
            .await
    }

    async fn set_lock(&self, handle: FileHandle, lock: FileLock, wait: bool) -> Result<()> {
        let state = self.handle(handle).await?;
        self.inner
            .set_lock(require_online_handle(&state)?, lock, wait)
            .await
    }

    async fn close_file(&self, handle: FileHandle) -> Result<()> {
        let mutation = self.handle(handle).await?.mutation;
        let _mutation = mutation.lock().await;
        let state = self.handles.write().await.remove(&handle).ok_or_else(|| {
            ClientError::Server(ErrorCode::InvalidHandle, "unknown cached handle".into())
        })?;
        if let Some(inner) = state.inner {
            self.inner.close_file(inner).await
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quickfs_cache::MemoryCache;
    use quickfs_protocol::{DirectoryRevision, NodeKind, ROOT_NODE};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const FILE_NODE: NodeId = NodeId(Uuid::from_u128(7));
    const LINK_NODE: NodeId = NodeId(Uuid::from_u128(8));
    const LINK_TARGET: &[u8] = b"clip.mov";

    fn filesystem_stats() -> FilesystemStats {
        FilesystemStats {
            blocks: 1_000,
            blocks_free: 600,
            blocks_available: 500,
            files: 100,
            files_free: 80,
            block_size: 4_096,
            name_length: 255,
            fragment_size: 4_096,
        }
    }

    struct ToggleFilesystem {
        offline: AtomicBool,
        fail_reads: AtomicBool,
        reads: AtomicUsize,
        read_lengths: std::sync::Mutex<Vec<u64>>,
        metadata_reads: AtomicUsize,
        directory_reads: AtomicUsize,
        directory_delay_ms: AtomicU64,
        read_delay_ms: AtomicU64,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        /// Added to `FILE_NODE`'s base revision (17), simulating another actor
        /// updating the file while handles are open.
        revision_bump: AtomicU64,
        data: Vec<u8>,
    }

    impl ToggleFilesystem {
        fn check_online(&self) -> Result<()> {
            if self.offline.load(Ordering::SeqCst) {
                Err(ClientError::Offline)
            } else {
                Ok(())
            }
        }

        fn file_revision(&self) -> u64 {
            17 + self.revision_bump.load(Ordering::SeqCst)
        }

        fn metadata(node: NodeId) -> Metadata {
            Metadata {
                node,
                kind: match node {
                    ROOT_NODE => NodeKind::Directory,
                    LINK_NODE => NodeKind::Symlink,
                    _ => NodeKind::File,
                },
                size: match node {
                    FILE_NODE => 2 * 1024 * 1024,
                    LINK_NODE => LINK_TARGET.len() as u64,
                    _ => 0,
                },
                mode: if node == FILE_NODE { 0o644 } else { 0o755 },
                allocated_blocks: if node == FILE_NODE { 4_096 } else { 0 },
                revision: match node {
                    FILE_NODE => 17,
                    LINK_NODE => 18,
                    _ => 9,
                },
                accessed_unix_ms: 1,
                modified_unix_ms: 1,
                created_unix_ms: Some(1),
                backup_unix_ms: None,
                link_count: if node == ROOT_NODE { 2 } else { 1 },
                device_major: 0,
                device_minor: 0,
            }
        }
    }

    #[async_trait]
    impl RemoteFilesystem for ToggleFilesystem {
        async fn ping(&self, nonce: u64) -> Result<u64> {
            self.check_online()?;
            Ok(nonce)
        }

        async fn stat_filesystem(&self) -> Result<FilesystemStats> {
            self.check_online()?;
            Ok(filesystem_stats())
        }

        async fn get_metadata(&self, node: NodeId) -> Result<Metadata> {
            self.check_online()?;
            self.metadata_reads.fetch_add(1, Ordering::SeqCst);
            let mut metadata = Self::metadata(node);
            if node == FILE_NODE {
                metadata.revision = self.file_revision();
                if !self.data.is_empty() {
                    metadata.size = self.data.len() as u64;
                }
            }
            Ok(metadata)
        }

        async fn list_directory(&self, node: NodeId) -> Result<Vec<DirectoryEntry>> {
            Ok(self.list_directory_snapshot(node).await?.entries)
        }

        async fn list_directory_snapshot(&self, _node: NodeId) -> Result<DirectorySnapshot> {
            self.check_online()?;
            self.directory_reads.fetch_add(1, Ordering::SeqCst);
            let delay_ms = self.directory_delay_ms.load(Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Ok(DirectorySnapshot {
                revision: 9 as DirectoryRevision,
                entries: vec![DirectoryEntry {
                    node: FILE_NODE,
                    name: "clip.mov".into(),
                    kind: NodeKind::File,
                    metadata: Self::metadata(FILE_NODE),
                }],
            })
        }

        async fn open_file(&self, node: NodeId) -> Result<(FileHandle, u64, u64)> {
            let opened = self
                .open_file_with_options(node, FileOpenOptions::READ_ONLY)
                .await?;
            Ok((opened.handle, opened.revision, opened.size))
        }

        async fn open_file_with_options(
            &self,
            node: NodeId,
            _options: FileOpenOptions,
        ) -> Result<OpenedFile> {
            self.check_online()?;
            Ok(OpenedFile {
                handle: FileHandle(Uuid::new_v4()),
                revision: if node == FILE_NODE {
                    self.file_revision()
                } else {
                    Self::metadata(node).revision
                },
                size: if node == FILE_NODE && !self.data.is_empty() {
                    self.data.len() as u64
                } else {
                    Self::metadata(node).size
                },
            })
        }

        async fn read_range(
            &self,
            handle: FileHandle,
            offset: u64,
            length: u64,
        ) -> Result<Vec<u8>> {
            Ok(self
                .read_range_versioned(handle, offset, length)
                .await?
                .data)
        }

        async fn read_range_versioned(
            &self,
            _handle: FileHandle,
            offset: u64,
            length: u64,
        ) -> Result<RangeRead> {
            self.check_online()?;
            let concurrent = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(concurrent, Ordering::SeqCst);
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.read_lengths.lock().unwrap().push(length);
            let delay_ms = self.read_delay_ms.load(Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(ClientError::Offline);
            }
            let start = usize::try_from(offset).unwrap();
            let end = usize::try_from(offset.saturating_add(length))
                .unwrap()
                .min(self.data.len());
            Ok(RangeRead {
                revision: self.file_revision(),
                data: self.data[start..end].to_vec(),
            })
        }

        async fn close_file(&self, _handle: FileHandle) -> Result<()> {
            self.check_online()
        }

        async fn read_link(&self, node: NodeId) -> Result<Vec<u8>> {
            self.check_online()?;
            if node == LINK_NODE {
                Ok(LINK_TARGET.to_vec())
            } else {
                Err(ClientError::Server(
                    ErrorCode::InvalidNode,
                    "not a cached test symlink".into(),
                ))
            }
        }
    }

    #[tokio::test]
    async fn cached_ranges_and_directory_snapshots_work_offline() {
        let data: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            revision_bump: AtomicU64::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(0),
            data: data.clone(),
        });
        let cache = Arc::new(MemoryCache::default());
        let policy = CachePolicy {
            block_size: 1024 * 1024,
            read_ahead_max_bytes: 0,
        };
        let filesystem = CachedFilesystem::new(inner.clone(), cache.clone(), policy).unwrap();

        filesystem.get_metadata(ROOT_NODE).await.unwrap();
        filesystem.get_metadata(FILE_NODE).await.unwrap();
        assert_eq!(
            filesystem.stat_filesystem().await.unwrap(),
            filesystem_stats()
        );
        assert_eq!(filesystem.read_link(LINK_NODE).await.unwrap(), LINK_TARGET);
        let metadata_reads = inner.metadata_reads.load(Ordering::SeqCst);
        filesystem.list_directory(ROOT_NODE).await.unwrap();
        assert_eq!(inner.metadata_reads.load(Ordering::SeqCst), metadata_reads);
        let opened = filesystem.open_file(FILE_NODE).await.unwrap().0;
        assert_eq!(
            filesystem.read_range(opened, 123, 4096).await.unwrap(),
            data[123..4219]
        );
        filesystem.close_file(opened).await.unwrap();
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);

        inner.offline.store(true, Ordering::SeqCst);
        let offline = CachedFilesystem::new(inner.clone(), cache, policy).unwrap();
        assert_eq!(offline.stat_filesystem().await.unwrap(), filesystem_stats());
        assert_eq!(offline.read_link(LINK_NODE).await.unwrap(), LINK_TARGET);
        assert_eq!(offline.list_directory(ROOT_NODE).await.unwrap().len(), 1);
        let opened = offline.open_file(FILE_NODE).await.unwrap().0;
        assert_eq!(
            offline.read_range(opened, 200, 512).await.unwrap(),
            data[200..712]
        );
        assert!(matches!(
            offline.read_range(opened, 1024 * 1024 + 4, 128).await,
            Err(ClientError::OfflineCacheMiss)
        ));
        offline.flush_file(opened, Some(42)).await.unwrap();
        offline.sync_file(opened, false).await.unwrap();
        offline.close_file(opened).await.unwrap();
    }

    #[tokio::test]
    async fn cached_directory_is_returned_while_one_remote_refresh_runs() {
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            revision_bump: AtomicU64::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(0),
            data: Vec::new(),
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy::default(),
        )
        .unwrap();

        filesystem.get_metadata(ROOT_NODE).await.unwrap();
        filesystem.list_directory(ROOT_NODE).await.unwrap();
        assert_eq!(inner.directory_reads.load(Ordering::SeqCst), 1);

        inner.directory_delay_ms.store(500, Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            filesystem.list_directory(ROOT_NODE),
        )
        .await
        .unwrap()
        .unwrap();
        while inner.directory_reads.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            filesystem.list_directory(ROOT_NODE),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(inner.directory_reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_overlapping_reads_share_one_remote_block_fetch() {
        let data = vec![0x5a; 2 * 1024 * 1024];
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            revision_bump: AtomicU64::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(50),
            data,
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 1024 * 1024,
                read_ahead_max_bytes: 0,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;
        let (first, second) = tokio::join!(
            filesystem.read_range(handle, 64, 4096),
            filesystem.read_range(handle, 128, 4096)
        );
        assert_eq!(first.unwrap(), vec![0x5a; 4096]);
        assert_eq!(second.unwrap(), vec![0x5a; 4096]);
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn small_random_read_uses_bounded_read_ahead_with_large_policy_blocks() {
        let data = vec![0x5a; 2 * 1024 * 1024];
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            revision_bump: AtomicU64::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(0),
            data,
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy::default(),
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        assert_eq!(
            filesystem.read_range(handle, 64, 4096).await.unwrap(),
            vec![0x5a; 4096]
        );
        assert_eq!(*inner.read_lengths.lock().unwrap(), [1024 * 1024]);
    }

    #[tokio::test]
    async fn concurrent_failed_reads_share_one_remote_fetch() {
        let inner = Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(true),
            reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            revision_bump: AtomicU64::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(50),
            data: vec![0x5a; 2 * 1024 * 1024],
        });
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 1024 * 1024,
                read_ahead_max_bytes: 0,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;
        let (first, second) = tokio::join!(
            filesystem.read_range(handle, 64, 4096),
            filesystem.read_range(handle, 128, 4096)
        );

        assert!(matches!(first, Err(ClientError::OfflineCacheMiss)));
        assert!(matches!(second, Err(ClientError::OfflineCacheMiss)));
        assert_eq!(inner.reads.load(Ordering::SeqCst), 1);
    }

    /// Builds a `FILE_NODE`-sized filesystem whose reads take `delay_ms`,
    /// simulating a high-latency link so the effect of concurrency is visible.
    fn latency_toggle(delay_ms: u64) -> Arc<ToggleFilesystem> {
        latency_toggle_with_size(delay_ms, 2 * 1024 * 1024)
    }

    fn latency_toggle_with_size(delay_ms: u64, bytes: usize) -> Arc<ToggleFilesystem> {
        Arc::new(ToggleFilesystem {
            offline: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            read_lengths: std::sync::Mutex::new(Vec::new()),
            metadata_reads: AtomicUsize::new(0),
            directory_reads: AtomicUsize::new(0),
            directory_delay_ms: AtomicU64::new(0),
            read_delay_ms: AtomicU64::new(delay_ms),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            revision_bump: AtomicU64::new(0),
            data: vec![0x5a; bytes],
        })
    }

    const BLOCK: u64 = 256 * 1024;

    /// A sequential scan over a high-latency link must issue speculative
    /// read-ahead: fetches overlap (concurrency rises above one) so the scan is
    /// no longer one-round-trip-per-block, and single-flight keeps every block
    /// fetched exactly once with no redundant refetch.
    #[tokio::test]
    async fn sequential_reads_prefetch_ahead_and_raise_concurrency() {
        let inner = latency_toggle(20);
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: BLOCK,
                read_ahead_max_bytes: 8 * 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        let blocks = (2 * 1024 * 1024) / BLOCK;
        let mut assembled = Vec::new();
        for index in 0..blocks {
            let data = filesystem
                .read_range(handle, index * BLOCK, BLOCK)
                .await
                .unwrap();
            assembled.extend_from_slice(&data);
        }
        // Let any trailing speculative fetches drain.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert_eq!(assembled, vec![0x5a; 2 * 1024 * 1024]);
        // Read-ahead kept multiple fetches in flight instead of stalling a full
        // round trip at every block boundary.
        assert!(
            inner.max_in_flight.load(Ordering::SeqCst) >= 2,
            "expected overlapping read-ahead, got {}",
            inner.max_in_flight.load(Ordering::SeqCst)
        );
        // Every block was fetched exactly once; prefetch and demand coalesced.
        assert_eq!(inner.reads.load(Ordering::SeqCst) as u64, blocks);
    }

    /// A stream of sub-1 MiB kernel reads — Preview and Quick Look read images
    /// this way — must get read-ahead at the small-read (1 MiB) granularity,
    /// even when the file is smaller than the large cache block. Before the
    /// granularity fix the prefetch cursor was aligned to the full
    /// `block_size`, which for a file smaller than one block meant no
    /// speculation at all and one serial round trip per megabyte.
    #[tokio::test]
    async fn small_sequential_reads_prefetch_at_demand_granularity() {
        let inner = latency_toggle(20);
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 16 * 1024 * 1024,
                read_ahead_max_bytes: 8 * 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        // Demand only the first megabyte, in Preview-sized 256 KiB steps. The
        // small-read path serves these from 1 MiB blocks.
        let step = 256 * 1024_u64;
        let mut assembled = Vec::new();
        for index in 0..4 {
            let data = filesystem
                .read_range(handle, index * step, step)
                .await
                .unwrap();
            assembled.extend_from_slice(&data);
        }
        // The second megabyte was never demanded; read-ahead at the 1 MiB
        // demand granularity must fetch it speculatively. (Cursor alignment to
        // the 16 MiB policy block would schedule nothing for a 2 MiB file.)
        let deadline = Instant::now() + Duration::from_secs(2);
        while inner.reads.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            inner.reads.load(Ordering::SeqCst),
            2,
            "expected the trailing megabyte to be prefetched"
        );

        // The demanded tail then coalesces with the speculative fetch: bytes
        // are correct and no block is fetched twice.
        for index in 4..8 {
            let data = filesystem
                .read_range(handle, index * step, step)
                .await
                .unwrap();
            assembled.extend_from_slice(&data);
        }
        assert_eq!(assembled, vec![0x5a; 2 * 1024 * 1024]);
        assert_eq!(inner.reads.load(Ordering::SeqCst), 2);
    }

    /// An open handle must keep serving reads after another actor updates the
    /// file. macOS relies on this constantly: Finder's copy engine sets a
    /// fresh copy's timestamps right after writing it, bumping the remote
    /// revision; pinning handles to their open-time revision surfaced that as
    /// ESTALE (Finder error 100070) and SIGBUS under mmap. The cached layer
    /// must refresh its snapshot and retry instead.
    #[tokio::test]
    async fn open_handle_survives_revision_bump_and_serves_fresh_data() {
        let inner = latency_toggle(0);
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 1024 * 1024,
                read_ahead_max_bytes: 0,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        let first = filesystem.read_range(handle, 0, 4_096).await.unwrap();
        assert_eq!(first, vec![0x5a; 4_096]);

        // Another actor updates the file: revision moves past the handle's
        // open-time snapshot.
        inner.revision_bump.store(5, Ordering::SeqCst);

        // An uncached block forces a remote fetch that observes the newer
        // revision. It must be served, not fail with StaleRevision.
        let second = filesystem
            .read_range(handle, 1024 * 1024, 4_096)
            .await
            .unwrap();
        assert_eq!(second, vec![0x5a; 4_096]);

        // The handle adopted the new revision: previously cached blocks of the
        // old revision are orphaned, so re-reading the first block fetches
        // fresh bytes instead of serving the stale cache entry.
        let reads_before = inner.reads.load(Ordering::SeqCst);
        let third = filesystem.read_range(handle, 0, 4_096).await.unwrap();
        assert_eq!(third, vec![0x5a; 4_096]);
        assert!(
            inner.reads.load(Ordering::SeqCst) > reads_before,
            "old-revision cache entry must not satisfy the refreshed handle"
        );
    }

    /// Random, non-sequential access must not speculate: prefetch stays off, so
    /// concurrency stays at one and no block beyond the demand is fetched.
    /// With multi-stream tracking a "random" pattern is one where no read
    /// continues any tracked stream's expected offset — descending jumps
    /// guarantee that, since every tracked expectation is ahead of its last
    /// read.
    #[tokio::test]
    async fn random_reads_do_not_prefetch() {
        let inner = latency_toggle(5);
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: BLOCK,
                read_ahead_max_bytes: 8 * 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        let offsets = [7 * BLOCK, 5 * BLOCK, 3 * BLOCK, BLOCK];
        for offset in offsets {
            filesystem.read_range(handle, offset, BLOCK).await.unwrap();
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert_eq!(inner.max_in_flight.load(Ordering::SeqCst), 1);
        assert_eq!(inner.reads.load(Ordering::SeqCst), offsets.len());
    }

    /// Two sequential readers interleaved on one handle — a media player's
    /// stream plus Spotlight/Quick Look page-ins share one FUSE `fh` on macOS
    /// — must each keep their own speculative window. The old single-stream
    /// tracker treated every switch as a seek and reset the window to zero,
    /// so playback never got read-ahead while anything else touched the file.
    #[tokio::test]
    async fn interleaved_sequential_streams_both_prefetch() {
        let inner = latency_toggle(20);
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: BLOCK,
                read_ahead_max_bytes: 8 * 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        // Stream A reads blocks 0..4, stream B reads blocks 4..8, interleaved.
        let mut assembled = Vec::new();
        for index in 0..4_u64 {
            let a = filesystem
                .read_range(handle, index * BLOCK, BLOCK)
                .await
                .unwrap();
            let b = filesystem
                .read_range(handle, (4 + index) * BLOCK, BLOCK)
                .await
                .unwrap();
            assembled.extend_from_slice(&a);
            assembled.extend_from_slice(&b);
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert_eq!(assembled.len(), 2 * 1024 * 1024);
        assert!(assembled.iter().all(|byte| *byte == 0x5a));
        // Speculation overlapped fetches despite the interleaving...
        assert!(
            inner.max_in_flight.load(Ordering::SeqCst) >= 2,
            "interleaved streams got no read-ahead: max in flight {}",
            inner.max_in_flight.load(Ordering::SeqCst)
        );
        // ...and single-flight still fetched every block exactly once.
        assert_eq!(inner.reads.load(Ordering::SeqCst) as u64, 8);
    }

    /// The speculative in-flight budget is a byte ceiling, not a fetch-count
    /// ceiling: streams fetching at the small (1 MiB) demand granularity must
    /// be able to hold more concurrent fetches than
    /// `read_ahead_max / block_size` would allow. With the old block-count
    /// permits this configuration (8 MiB ceiling, 16 MiB policy blocks) had a
    /// single permit and could never overlap speculation at all.
    #[tokio::test]
    async fn small_granularity_streams_use_full_byte_budget() {
        let inner = latency_toggle_with_size(20, 16 * 1024 * 1024);
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: 16 * 1024 * 1024,
                read_ahead_max_bytes: 8 * 1024 * 1024,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        // A paced sequential consumer in sub-1 MiB steps: every stall grows
        // the window, which only pays off if more than one 1 MiB speculative
        // fetch can actually be in flight.
        let step = 256 * 1024_u64;
        for index in 0..24 {
            filesystem
                .read_range(handle, index * step, step)
                .await
                .unwrap();
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert!(
            inner.max_in_flight.load(Ordering::SeqCst) >= 3,
            "byte-denominated permits should allow several 1 MiB fetches, got {}",
            inner.max_in_flight.load(Ordering::SeqCst)
        );
    }

    /// The adaptive window is hard-bounded by the configured memory ceiling: even
    /// a long sequential scan on a high-latency link never exceeds the cap of
    /// speculative fetches in flight (plus the one concurrent demand read).
    #[tokio::test]
    async fn prefetch_window_bounded_by_memory_cap() {
        let inner = latency_toggle(15);
        // Ceiling of two blocks of speculative read-ahead.
        let filesystem = CachedFilesystem::new(
            inner.clone(),
            Arc::new(MemoryCache::default()),
            CachePolicy {
                block_size: BLOCK,
                read_ahead_max_bytes: 2 * BLOCK,
            },
        )
        .unwrap();
        let handle = filesystem.open_file(FILE_NODE).await.unwrap().0;

        let blocks = (2 * 1024 * 1024) / BLOCK;
        for index in 0..blocks {
            filesystem
                .read_range(handle, index * BLOCK, BLOCK)
                .await
                .unwrap();
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        // At most cap (2) speculative fetches plus one demand read.
        assert!(
            inner.max_in_flight.load(Ordering::SeqCst) <= 3,
            "window exceeded the memory cap: {}",
            inner.max_in_flight.load(Ordering::SeqCst)
        );
        assert_eq!(inner.reads.load(Ordering::SeqCst) as u64, blocks);
    }

    /// The controller starts a fresh cold stream on a far seek and resets all
    /// stream state when the tracked revision changes, so a speculatively
    /// fetched block from an old revision is never carried forward.
    #[test]
    fn controller_detects_pattern_and_resets_on_revision_change() {
        let node = FILE_NODE;
        let block = 1024 * 1024;
        let cap = 8;
        let size = 64 * 1024 * 1024;
        let mut controller = SequentialPrefetcher::default();

        // The first read of a cold handle primes a shallow read-ahead past the
        // demand block — so the next read is a cache hit instead of a serial
        // cold fetch — but does not yet arm the adaptive window.
        let (first_stream, scheduled) =
            controller.observe(node, 0, block, 17, size, Duration::ZERO, block, cap);
        assert_eq!(scheduled.len() as u64, PRIME_WINDOW_BLOCKS);
        assert!(scheduled.iter().all(|key| key.offset >= block));
        assert!(scheduled.iter().all(|key| key.file.revision == 17));
        // Second, contiguous read crosses the trigger and schedules read-ahead
        // beyond the demand region on the same stream.
        let (second_stream, scheduled) =
            controller.observe(node, block, block, 17, size, Duration::ZERO, block, cap);
        assert_eq!(first_stream, second_stream);
        assert!(!scheduled.is_empty());
        assert!(scheduled.iter().all(|key| key.offset >= 2 * block));
        assert!(scheduled.iter().all(|key| key.file.revision == 17));

        // A far seek continues no tracked stream: it starts a cold one with no
        // speculation, and the original stream keeps its window.
        let (seek_stream, scheduled) = controller.observe(
            node,
            40 * block,
            block,
            17,
            size,
            Duration::ZERO,
            block,
            cap,
        );
        assert_ne!(seek_stream, second_stream);
        assert!(scheduled.is_empty());
        assert!(
            controller
                .streams
                .iter()
                .any(|stream| stream.id == second_stream && stream.active),
            "an unrelated seek must not reset an established stream"
        );

        // A revision bump resets tracking; the next contiguous pair speculates
        // only on the new revision.
        controller.observe(node, 0, block, 18, size, Duration::ZERO, block, cap);
        assert_eq!(controller.streams.len(), 1);
        let (_, after) =
            controller.observe(node, block, block, 18, size, Duration::ZERO, block, cap);
        assert!(!after.is_empty());
        assert!(after.iter().all(|key| key.file.revision == 18));
    }

    /// A cold handle's very first read primes a shallow read-ahead so the
    /// opening reads of playback are served from cache instead of a serial cold
    /// fetch, and the second in-order read arms the adaptive window at its full
    /// initial depth rather than a single block — together the fix for the
    /// first-open buffering the adaptive-only ramp left exposed. Priming past
    /// end-of-file schedules nothing.
    #[test]
    fn cold_open_primes_read_ahead_and_arms_a_deep_window() {
        let node = FILE_NODE;
        let block = 1024 * 1024;
        let cap = 64;
        let size = 64 * 1024 * 1024;
        let mut controller = SequentialPrefetcher::default();

        // First read of the cold handle primes exactly PRIME_WINDOW_BLOCKS,
        // starting past the demand block so the two never overlap.
        let (_, primed) = controller.observe(node, 0, block, 17, size, Duration::ZERO, block, cap);
        assert_eq!(primed.len() as u64, PRIME_WINDOW_BLOCKS);
        assert_eq!(primed[0].offset, block);

        // The second in-order read arms the adaptive window at its full initial
        // depth, so read-ahead is several blocks deep the moment speculation
        // begins instead of ramping up from one.
        controller.observe(node, block, block, 17, size, Duration::ZERO, block, cap);
        let window = controller
            .streams
            .iter()
            .find(|stream| stream.active)
            .map(|stream| stream.window_blocks);
        assert_eq!(window, Some(INITIAL_ACTIVE_WINDOW_BLOCKS));

        // A cold open whose first read is already at end-of-file has nothing to
        // prime, and must not schedule past the end.
        let mut at_eof = SequentialPrefetcher::default();
        let (_, none) = at_eof.observe(
            node,
            size - block,
            block,
            17,
            size,
            Duration::ZERO,
            block,
            cap,
        );
        assert!(none.is_empty());
    }

    /// The window grows only on direct evidence of a too-small window — a
    /// demand read that actually blocked — and holds while demand reads are
    /// served from prefetched data. The old throughput hill-climb shrank the
    /// window on measurement noise, which live traces showed whipsawing
    /// between 1 and 4 blocks forever during smooth playback.
    #[test]
    fn controller_grows_on_stalls_and_holds_on_hits() {
        let node = FILE_NODE;
        let block = 1024 * 1024_u64;
        let cap = 64;
        let size = 1024 * 1024 * 1024;
        let stall = Duration::from_millis(80);
        let mut controller = SequentialPrefetcher::default();

        // Establish the stream.
        controller.observe(node, 0, block, 17, size, stall, block, cap);
        let (stream, _) = controller.observe(node, block, block, 17, size, stall, block, cap);
        let window_of = |controller: &SequentialPrefetcher| {
            controller
                .streams
                .iter()
                .find(|candidate| candidate.id == stream)
                .map(|candidate| candidate.window_blocks)
                .unwrap_or(0)
        };

        // Stalled boundary reads grow the window multiplicatively.
        let mut offset = 2 * block;
        let mut grown = window_of(&controller);
        assert!(grown >= 1);
        for _ in 0..12 {
            controller.observe(node, offset, block, 17, size, stall, block, cap);
            offset += block;
        }
        let after_stalls = window_of(&controller);
        assert!(
            after_stalls > grown,
            "stalled reads must grow the window: {grown} -> {after_stalls}"
        );
        grown = after_stalls;

        // A long stretch of prefetch hits (no blocking) holds the window
        // instead of shrinking it — hits carry no evidence the window is
        // wrong, and jitter absorption depends on keeping it.
        for _ in 0..64 {
            controller.observe(node, offset, block, 17, size, Duration::ZERO, block, cap);
            offset += block;
        }
        assert_eq!(
            window_of(&controller),
            grown,
            "unblocked reads must hold the window"
        );
    }
}
