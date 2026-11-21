// SPDX-License-Identifier: Apache-2.0
//! Random-access reads for previews that play while they load.
//!
//! A media player asks for byte ranges as it needs them: the header, the
//! index at the end of the file, then the stretch after the playhead, and a
//! new stretch on every seek. Downloading the whole file first would make a
//! two-hour video wait for all of its gigabytes, so a stream keeps one file
//! open and serves those ranges directly.
//!
//! Reads go through the same [`CachedFilesystem`] the mount uses, so a
//! sequential reader gets its adaptive read-ahead (blocks already in flight
//! before the player asks for them) and every block lands in a bounded
//! persistent cache: watching something again, or seeking back, costs no
//! network. Each session gets its own cached view over its connection, and
//! sessions for the same server, epoch, and account share one cache, because
//! a cache directory admits only one owner at a time.
//!
//! Resource use: at most `READ_AHEAD_MAX_BYTES` of speculative reads in
//! flight per session (client-core further caps the wire convoy at 32 MiB),
//! one `MAX_STREAM_READ` reply buffer per outstanding read, and on disk at
//! most the cache budget the app passes per namespace.

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
/// How deep the read-ahead window may grow for one sequential reader.
pub const READ_AHEAD_MAX_BYTES: u64 = 64 * 1024 * 1024;
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
        read_ahead_max_bytes: READ_AHEAD_MAX_BYTES,
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
    with_registry(|registry| {
        registry.streams.insert(
            id,
            Arc::new(Stream {
                session: session_id,
                filesystem,
                handle,
                size,
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
    Ok(stream
        .filesystem
        .read_range(stream.handle, offset, length)
        .await?)
}

pub async fn close(id: u64) -> BridgeResult<Value> {
    let removed = with_registry(|registry| registry.streams.remove(&id));
    match removed {
        Some(stream) => {
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
