// SPDX-License-Identifier: Apache-2.0
//! Random-access reads for previews that play while they load.
//!
//! A media player asks for byte ranges as it needs them: the header, the
//! index at the end of the file, then the stretch after the playhead, and a
//! new stretch on every seek. Downloading the whole file first would make a
//! two-hour video wait for all of its gigabytes, so a stream keeps one file
//! open and serves those ranges directly.
//!
//! Reads go through the same [`CachedFilesystem`] the mount uses, so every
//! block lands in a bounded persistent cache: watching something again, or
//! seeking back, costs no network. Each session gets its own cached view over
//! its connection, and sessions for the same server, epoch, and account share
//! one cache, because a cache directory admits only one owner at a time.
//!
//! Read-ahead is the stream's own, not the cache's. AVFoundation does not
//! read a file front to back: it opens a request at the playhead's next
//! segment (about every half second of video), takes a megabyte or two,
//! cancels, and fills the gap behind it with separate requests. The cache's
//! sequential detector, tuned for the kernel's steady page-ins, sees each
//! jump as a new random read, so every segment's first block was fetched
//! cold: a full round trip plus transfer, 100 to 350 ms on Wi-Fi, against a
//! segment every 0.5 s at 95 Mb/s. Playback stalled. [`ReadAhead`] instead
//! follows the furthest byte the reader has asked for and keeps the
//! `AHEAD_BYTES` after it fetched, whatever order requests arrive in, and
//! starts over at a real seek.
//!
//! Resource use: at most `AHEAD_CONCURRENCY` blocks (12 MiB) of read-ahead on
//! the wire per stream, small enough that a demand read after a seek is not
//! stuck behind a convoy; `AHEAD_BYTES` (64 MiB, about 5 s at 100 Mb/s) of
//! fetched-ahead data per stream, held in the disk cache, not memory; one
//! `MAX_STREAM_READ` reply buffer per outstanding read; and on disk at most
//! the cache budget the app passes per namespace.

use crate::{
    model::{BridgeError, BridgeResult, node_id},
    session::{self, Session},
};
use quickfs_cache::{CacheError, CacheNamespace, NonBlockingPersistentCache};
use quickfs_client_core::{CachePolicy, CachedFilesystem, MAX_CLIENT_READ_SIZE, RemoteFilesystem};
use quickfs_protocol::FileHandle;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

/// Cache and read-ahead granularity. A player cannot start until its first
/// block has arrived, so this is smaller than the mount's 16 MiB: half a
/// second on a 4 MB/s relayed link instead of four.
pub const BLOCK_SIZE: u64 = 2 * 1024 * 1024;
/// How far past the furthest byte read the stream keeps fetched.
pub const AHEAD_BYTES: u64 = 64 * 1024 * 1024;
/// Read-ahead block fetches one stream keeps in flight.
pub const AHEAD_CONCURRENCY: usize = 6;
/// The most one read returns. Players ask for far more at once and take it
/// in pieces, so a bounded reply keeps memory flat and cancellation prompt.
pub const MAX_STREAM_READ: u64 = 4 * 1024 * 1024;
/// The cache budget when the app does not name one.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// How many times, 100 ms apart, to wait for a released cache's last user.
const CACHE_OPEN_ATTEMPTS: u32 = 30;

struct Stream {
    session: u64,
    filesystem: Arc<CachedFilesystem>,
    handle: FileHandle,
    size: u64,
    block: u64,
    ahead: Mutex<ReadAhead>,
}

/// Which blocks to fetch before they are asked for.
///
/// The *edge* is the furthest byte the reader has asked for. Reads behind it
/// (a player filling in a segment) leave it alone; a read more than
/// `AHEAD_BYTES` away from it in either direction is a seek and moves it.
/// Blocks from the edge to `AHEAD_BYTES` past it are fetched in order, at
/// most `AHEAD_CONCURRENCY` at a time.
#[derive(Debug, Default)]
struct ReadAhead {
    edge: u64,
    /// The next block to fetch.
    next: u64,
    in_flight: usize,
    closed: bool,
}

impl ReadAhead {
    /// Note a read of `offset..end` and return the blocks to start fetching.
    fn observe(&mut self, offset: u64, end: u64, block: u64, size: u64) -> Vec<u64> {
        let seek = offset > self.edge.saturating_add(AHEAD_BYTES)
            || offset.saturating_add(AHEAD_BYTES) < self.edge;
        if seek {
            self.edge = end;
            self.next = end - end % block;
        } else {
            self.edge = self.edge.max(end);
            self.next = self.next.max(self.edge - self.edge % block);
        }
        self.plan(block, size)
    }

    /// One fetch finished; return the blocks to start in its place.
    fn finished(&mut self, block: u64, size: u64) -> Vec<u64> {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.plan(block, size)
    }

    fn plan(&mut self, block: u64, size: u64) -> Vec<u64> {
        let limit = self.edge.saturating_add(AHEAD_BYTES).min(size);
        let mut start = Vec::new();
        while !self.closed && self.in_flight < AHEAD_CONCURRENCY && self.next < limit {
            start.push(self.next);
            self.next += block;
            self.in_flight += 1;
        }
        start
    }
}

fn lock_ahead(stream: &Stream) -> std::sync::MutexGuard<'_, ReadAhead> {
    stream
        .ahead
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Fetch `blocks` into the cache in the background. Each finished fetch
/// starts the next, so the window stays full while the reader advances.
fn fetch_ahead(stream: &Arc<Stream>, blocks: Vec<u64>) {
    for offset in blocks {
        let stream = Arc::clone(stream);
        tokio::spawn(async move {
            let length = stream.block.min(stream.size.saturating_sub(offset));
            // A failure is left to the reader's own read, which retries.
            let _ = stream
                .filesystem
                .read_range(stream.handle, offset, length)
                .await;
            let next = lock_ahead(&stream).finished(stream.block, stream.size);
            fetch_ahead(&stream, next);
        });
    }
}

#[derive(Default)]
struct Registry {
    caches: HashMap<(PathBuf, CacheNamespace), Arc<NonBlockingPersistentCache>>,
    /// The cached view of each session that has opened a stream.
    filesystems: HashMap<u64, (PathBuf, Arc<CachedFilesystem>)>,
    streams: HashMap<u64, Arc<Stream>>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

fn with_registry<T>(body: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    body(guard.get_or_insert_with(Registry::default))
}

fn stream(id: u64) -> BridgeResult<Arc<Stream>> {
    with_registry(|registry| registry.streams.get(&id).cloned()).ok_or(BridgeError::UnknownStream)
}

/// The session's cached view, built on its first stream.
async fn cached_view(
    id: u64,
    session: &Session,
    cache_dir: &Path,
    cache_max_bytes: u64,
) -> BridgeResult<Arc<CachedFilesystem>> {
    if let Some((_, filesystem)) = with_registry(|registry| registry.filesystems.get(&id).cloned())
    {
        return Ok(filesystem);
    }
    let identity = session.server_identity().await?;
    let namespace = CacheNamespace::new(
        identity,
        session.capabilities.server_epoch.to_string(),
        session.username(),
    )?;
    let key = (cache_dir.to_path_buf(), namespace);
    let cache = match with_registry(|registry| registry.caches.get(&key).cloned()) {
        Some(cache) => cache,
        None => {
            let opened = Arc::new(open_cache(cache_dir, &key.1, cache_max_bytes).await?);
            // Another open may have raced this one; keep whichever landed.
            with_registry(|registry| Arc::clone(registry.caches.entry(key).or_insert(opened)))
        }
    };
    let policy = CachePolicy {
        block_size: BLOCK_SIZE
            .min(session.capabilities.max_read_size.max(1))
            .min(MAX_CLIENT_READ_SIZE),
        // The stream's own read-ahead replaces the cache's; see the module notes.
        read_ahead_max_bytes: 0,
    };
    let filesystem = Arc::new(CachedFilesystem::new(
        Arc::clone(&session.filesystem),
        cache,
        policy,
    )?);
    Ok(with_registry(|registry| {
        let (_, kept) = registry
            .filesystems
            .entry(id)
            .or_insert_with(|| (cache_dir.to_path_buf(), filesystem));
        Arc::clone(kept)
    }))
}

/// Open a namespace's cache, waiting out a previous owner. After a clear
/// releases a cache, read-ahead tasks already in flight still hold it for a
/// moment, and the directory lock admits one owner at a time.
async fn open_cache(
    cache_dir: &Path,
    namespace: &CacheNamespace,
    cache_max_bytes: u64,
) -> BridgeResult<NonBlockingPersistentCache> {
    let mut attempts = 0;
    loop {
        match NonBlockingPersistentCache::open(
            cache_dir,
            namespace.clone(),
            cache_max_bytes.max(BLOCK_SIZE),
        ) {
            Err(CacheError::CacheInUse) if attempts < CACHE_OPEN_ATTEMPTS => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            result => return Ok(result?),
        }
    }
}

pub async fn open(
    session_id: u64,
    node: Uuid,
    cache_dir: &Path,
    cache_max_bytes: Option<u64>,
) -> BridgeResult<Value> {
    let session = session::session(session_id)?;
    let filesystem = cached_view(
        session_id,
        &session,
        cache_dir,
        cache_max_bytes.unwrap_or(DEFAULT_CACHE_MAX_BYTES),
    )
    .await?;
    let (handle, revision, size) = filesystem.open_file(node_id(node)).await?;
    let id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    let block = BLOCK_SIZE
        .min(session.capabilities.max_read_size.max(1))
        .min(MAX_CLIENT_READ_SIZE);
    with_registry(|registry| {
        registry.streams.insert(
            id,
            Arc::new(Stream {
                session: session_id,
                filesystem,
                handle,
                size,
                block,
                ahead: Mutex::new(ReadAhead::default()),
            }),
        )
    });
    Ok(json!({ "stream": id, "size": size, "revision": revision }))
}

/// Up to `capacity` bytes (and at most [`MAX_STREAM_READ`]) from `offset`.
/// Shorter only at the end of the file; empty at or past it.
pub async fn read(id: u64, offset: u64, capacity: u64) -> BridgeResult<Vec<u8>> {
    let stream = stream(id)?;
    if offset >= stream.size || capacity == 0 {
        return Ok(Vec::new());
    }
    let length = capacity.min(MAX_STREAM_READ).min(stream.size - offset);
    let ahead = lock_ahead(&stream).observe(offset, offset + length, stream.block, stream.size);
    fetch_ahead(&stream, ahead);
    Ok(stream
        .filesystem
        .read_range(stream.handle, offset, length)
        .await?)
}

pub async fn close(id: u64) -> BridgeResult<Value> {
    let removed = with_registry(|registry| registry.streams.remove(&id));
    match removed {
        Some(stream) => {
            lock_ahead(&stream).closed = true;
            stream.filesystem.close_file(stream.handle).await?;
            Ok(json!({ "closed": true }))
        }
        None => Ok(json!({ "closed": false })),
    }
}

/// Drop a closed session's cached view. Streams it opened keep their own
/// reference and keep working until they are closed.
pub fn forget_session(id: u64) {
    with_registry(|registry| registry.filesystems.remove(&id));
}

/// Release every cache under `cache_dir` so its files can be deleted.
/// Refused while a stream reads from one, since it holds blocks in use.
pub fn release_caches(cache_dir: &Path) -> BridgeResult<()> {
    with_registry(|registry| {
        let busy = registry
            .streams
            .values()
            .filter(|stream| {
                registry
                    .filesystems
                    .get(&stream.session)
                    .is_none_or(|(dir, _)| dir == cache_dir)
            })
            .count();
        if busy > 0 {
            return Err(BridgeError::Busy(
                "close open previews before clearing the cache".into(),
            ));
        }
        registry.filesystems.retain(|_, (dir, _)| dir != cache_dir);
        registry.caches.retain(|(dir, _), _| dir != cache_dir);
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        pipeline::Tuning,
        transfer::tests::{FlakyFs, session as flaky_session},
    };
    use quickfs_protocol::NodeId;

    const FILE: NodeId = NodeId(Uuid::from_u128(0x5eed));

    fn content(length: usize) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    fn opened(reply: &Value) -> (u64, u64) {
        (
            reply["stream"].as_u64().unwrap(),
            reply["size"].as_u64().unwrap(),
        )
    }

    const MIB: u64 = 1024 * 1024;
    const BLOCK: u64 = 2 * MIB;
    const SIZE: u64 = 1024 * MIB;

    /// Finish every fetch in flight, as the network would, and collect what
    /// the planner starts in their place until the window is full.
    fn settle(ahead: &mut ReadAhead) -> Vec<u64> {
        let mut started = Vec::new();
        while ahead.in_flight > 0 {
            started.extend(ahead.finished(BLOCK, SIZE));
            if started.len() > 1000 {
                break;
            }
        }
        started
    }

    #[test]
    fn read_ahead_primes_from_the_first_read_with_bounded_concurrency() {
        let mut ahead = ReadAhead::default();
        let first = ahead.observe(0, 2, BLOCK, SIZE);
        assert_eq!(
            first,
            (0..AHEAD_CONCURRENCY as u64)
                .map(|i| i * BLOCK)
                .collect::<Vec<_>>()
        );
        // Draining fills the window up to the edge (byte 2) plus AHEAD_BYTES,
        // and no further.
        assert_eq!(settle(&mut ahead).last().copied(), Some(AHEAD_BYTES));
        assert_eq!(ahead.in_flight, 0);
    }

    #[test]
    fn read_ahead_follows_the_leading_edge_not_the_fill_ins_behind_it() {
        let mut ahead = ReadAhead::default();
        ahead.observe(0, MIB, BLOCK, SIZE);
        settle(&mut ahead);
        // The player jumps 6 MB ahead, then fills in behind; only the jump
        // moves the window.
        let jump = ahead.observe(6 * MIB, 7 * MIB, BLOCK, SIZE);
        assert_eq!(ahead.edge, 7 * MIB);
        assert!(jump.iter().all(|block| *block >= AHEAD_BYTES - BLOCK));
        let behind = ahead.observe(MIB, 6 * MIB, BLOCK, SIZE);
        assert!(behind.is_empty());
        assert_eq!(ahead.edge, 7 * MIB);
    }

    #[test]
    fn read_ahead_starts_over_at_a_seek_either_way() {
        let mut ahead = ReadAhead::default();
        ahead.observe(0, MIB, BLOCK, SIZE);
        settle(&mut ahead);
        // Forward past the window: fetch from the new place.
        let forward = ahead.observe(500 * MIB, 501 * MIB, BLOCK, SIZE);
        assert_eq!(forward.first(), Some(&(500 * MIB)));
        settle(&mut ahead);
        // Back to the start: fetch from there again, not after 500 MiB.
        let back = ahead.observe(10 * MIB, 11 * MIB, BLOCK, SIZE);
        assert_eq!(back.first(), Some(&(10 * MIB)));
        assert_eq!(ahead.edge, 11 * MIB);
    }

    #[test]
    fn read_ahead_stops_at_the_end_of_the_file_and_when_closed() {
        let mut ahead = ReadAhead::default();
        let near_end = ahead.observe(SIZE - 3 * MIB, SIZE - 2 * MIB, BLOCK, SIZE);
        assert!(near_end.iter().all(|block| *block < SIZE));
        assert!(settle(&mut ahead).iter().all(|block| *block < SIZE));
        ahead.closed = true;
        assert!(ahead.observe(0, MIB, BLOCK, SIZE).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ranges_come_back_exactly_and_end_at_the_file_size() {
        let data = content(5 * 1024 * 1024 + 123);
        let fs = Arc::new(FlakyFs::with_file(FILE, data.clone()));
        let id = session::register(flaky_session(fs, Tuning::default()));
        let scratch = tempfile::tempdir().unwrap();
        // The cache creates its root privately; a shared temp folder is refused.
        let cache = scratch.path().join("cache");

        let (stream, size) = opened(&open(id, FILE.0, cache.as_path(), None).await.unwrap());
        assert_eq!(size, data.len() as u64);

        // A range that straddles cache blocks, as a player's reads do.
        let at = BLOCK_SIZE - 1000;
        let middle = read(stream, at, 300_000).await.unwrap();
        assert_eq!(middle, data[at as usize..at as usize + 300_000]);

        // Requests are capped, the tail is short, and the end is empty.
        let big = read(stream, 0, 64 * 1024 * 1024).await.unwrap();
        assert_eq!(big.len() as u64, MAX_STREAM_READ);
        assert_eq!(big, data[..MAX_STREAM_READ as usize]);
        let tail = read(stream, size - 23, 1_000).await.unwrap();
        assert_eq!(tail, data[data.len() - 23..]);
        assert!(read(stream, size, 1_000).await.unwrap().is_empty());

        assert_eq!(close(stream).await.unwrap()["closed"], true);
        assert!(matches!(
            read(stream, 0, 10).await,
            Err(BridgeError::UnknownStream)
        ));
        assert_eq!(close(stream).await.unwrap()["closed"], false);
        forget_session(id);
        session::disconnect(id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_share_one_cache_and_clearing_waits_for_open_streams() {
        let data = content(3 * 1024 * 1024);
        let scratch = tempfile::tempdir().unwrap();
        // The cache creates its root privately; a shared temp folder is refused.
        let cache = scratch.path().join("cache");
        let first = session::register(flaky_session(
            Arc::new(FlakyFs::with_file(FILE, data.clone())),
            Tuning::default(),
        ));
        let second = session::register(flaky_session(
            Arc::new(FlakyFs::with_file(FILE, data.clone())),
            Tuning::default(),
        ));

        // Both sessions name the same server, epoch, and user, so the second
        // must reuse the first cache rather than fail on its lock.
        let (a, _) = opened(
            &open(first, FILE.0, cache.as_path(), Some(64 << 20))
                .await
                .unwrap(),
        );
        let (b, _) = opened(
            &open(second, FILE.0, cache.as_path(), Some(64 << 20))
                .await
                .unwrap(),
        );
        assert_eq!(read(a, 0, 100).await.unwrap(), data[..100]);
        assert_eq!(read(b, 100, 100).await.unwrap(), data[100..200]);

        assert!(matches!(
            release_caches(cache.as_path()),
            Err(BridgeError::Busy(_))
        ));
        close(a).await.unwrap();
        close(b).await.unwrap();
        release_caches(cache.as_path()).unwrap();
        assert!(with_registry(|registry| registry
            .caches
            .keys()
            .all(|(dir, _)| dir.as_path() != cache.as_path())));

        // A fresh stream reopens the cache after the release.
        let (c, _) = opened(&open(first, FILE.0, cache.as_path(), None).await.unwrap());
        assert_eq!(read(c, 0, 10).await.unwrap(), data[..10]);
        close(c).await.unwrap();
        for id in [first, second] {
            forget_session(id);
            session::disconnect(id);
        }
    }
}
