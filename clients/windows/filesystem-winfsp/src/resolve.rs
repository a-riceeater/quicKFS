// SPDX-License-Identifier: Apache-2.0
//! Turning a Windows path into a quicKFS node.
//!
//! WinFsp is a path-addressed interface: almost every callback arrives with a
//! full path and no context from the one before it. quicKFS, like FUSE, is
//! node-addressed: a child is reached by looking it up inside its parent. The
//! walk between the two happens here.
//!
//! # Why this is not a per-callback round trip
//!
//! Explorer issues an enormous number of metadata queries, and a naive walk
//! would turn `\a\b\c.txt` into three network lookups every time. It does not,
//! because [`quickfs_fs_adapter::Adapter`] already caches an enriched directory
//! view for thirty seconds and single-flights the cold load, so each component
//! after the first visit is answered from memory. This module's job is to keep
//! that cache *reachable*, which is what [`DirectoryReferences`] is for.
//!
//! # Replacing FUSE's `forget`
//!
//! The adapter's inode table is reference counted: a lookup adds one, and FUSE
//! kernels return them through `forget` when they drop an entry. WinFsp has no
//! equivalent callback, so the bridge has to decide for itself when a
//! translation may be released, or the table would grow without bound.
//!
//! The rule is:
//!
//! * The final component of a resolved path is returned holding one reference,
//!   which its caller owns and must return with [`Resolver::release`] — for an
//!   open file that happens at `Close`.
//! * Every *directory* traversed on the way is handed to a bounded LRU. Holding
//!   the reference is what keeps that directory's cached listing alive, so the
//!   next walk through the same tree costs nothing; the LRU bound is what stops
//!   a recursive crawl of a huge export from retaining every directory in it
//!   forever.
//!
//! A directory with an open handle is never actually evicted even when the LRU
//! releases its reference: the adapter refuses to evict an inode that any open
//! handle still names.

use quickfs_fs_adapter::{Adapter, AdapterError, ROOT_INODE};
use quickfs_protocol::{Metadata, Name, NodeKind};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// How many traversed directories keep their adapter reference, and with it
/// their cached listing.
///
/// Sized so an ordinary project tree or photo library stays entirely warm while
/// a recursive crawl of a very large export still has a ceiling. Each retained
/// entry costs one inode-table row plus, for at most thirty seconds, one cached
/// directory listing.
pub const DEFAULT_RETAINED_DIRECTORIES: usize = 16_384;

/// A bounded, least-recently-used set of directory inodes whose adapter lookup
/// reference this bridge holds.
struct DirectoryReferences {
    capacity: usize,
    /// Inode to its current sequence number.
    sequence_of: HashMap<u64, u64>,
    /// Sequence number to inode, giving an ordered view for eviction.
    by_sequence: BTreeMap<u64, u64>,
}

impl DirectoryReferences {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            sequence_of: HashMap::new(),
            by_sequence: BTreeMap::new(),
        }
    }

    /// Record a reference for `inode`, or refresh its position if it is already
    /// held.
    ///
    /// Returns `Retained` when the caller's reference was taken over by the set
    /// and `AlreadyHeld` when the caller must release its own, together with
    /// any inodes evicted to stay within capacity.
    fn touch(&mut self, inode: u64, sequence: u64) -> (Retention, Vec<u64>) {
        let retention = match self.sequence_of.insert(inode, sequence) {
            Some(previous) => {
                self.by_sequence.remove(&previous);
                Retention::AlreadyHeld
            }
            None => Retention::Retained,
        };
        self.by_sequence.insert(sequence, inode);

        let mut evicted = Vec::new();
        while self.by_sequence.len() > self.capacity {
            let Some((&oldest, &inode)) = self.by_sequence.iter().next() else {
                break;
            };
            self.by_sequence.remove(&oldest);
            self.sequence_of.remove(&inode);
            evicted.push(inode);
        }
        (retention, evicted)
    }

    fn drain(&mut self) -> Vec<u64> {
        self.sequence_of.clear();
        std::mem::take(&mut self.by_sequence)
            .into_values()
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Retention {
    /// The set took ownership of the caller's reference.
    Retained,
    /// The set already held a reference; the caller keeps its own.
    AlreadyHeld,
}

/// One resolved node, holding one adapter lookup reference.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub inode: u64,
    pub metadata: Metadata,
}

impl Resolved {
    pub fn kind(&self) -> NodeKind {
        self.metadata.kind
    }

    pub fn is_directory(&self) -> bool {
        self.metadata.kind == NodeKind::Directory
    }
}

/// Walks Windows paths onto quicKFS nodes for one mount.
pub struct Resolver {
    adapter: Adapter,
    retained: Mutex<DirectoryReferences>,
    sequence: AtomicU64,
}

impl Resolver {
    pub fn new(adapter: Adapter) -> Self {
        Self::with_capacity(adapter, DEFAULT_RETAINED_DIRECTORIES)
    }

    pub fn with_capacity(adapter: Adapter, capacity: usize) -> Self {
        Self {
            adapter,
            retained: Mutex::new(DirectoryReferences::new(capacity)),
            sequence: AtomicU64::new(0),
        }
    }

    pub fn adapter(&self) -> &Adapter {
        &self.adapter
    }

    /// Resolve an absolute component list to a node, holding one reference for
    /// the caller.
    ///
    /// An empty list is the volume root, whose reference count the adapter
    /// pins permanently, so releasing it is a no-op rather than an error.
    pub async fn resolve(&self, components: &[String]) -> Result<Resolved, AdapterError> {
        let mut inode = ROOT_INODE;
        let mut metadata = self.adapter.getattr_async(ROOT_INODE).await?;

        for (index, component) in components.iter().enumerate() {
            // Everything before the last component has to be a directory. The
            // server would refuse the lookup anyway, but answering here turns
            // `\file.txt\nope` into the status Windows expects instead of a
            // round trip that returns a less specific error.
            if metadata.kind != NodeKind::Directory {
                self.release_traversed(inode, index);
                return Err(not_a_directory());
            }
            let result = self
                .adapter
                .lookup_async(inode, Name::from(component.clone()))
                .await;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    self.release_traversed(inode, index);
                    return Err(error);
                }
            };
            // The parent's reference is now either donated to the retained set
            // or released, whichever keeps the table bounded.
            self.hand_off_directory(inode, index);
            inode = result.inode;
            metadata = result.metadata;
        }

        if components.is_empty() {
            // The root was never looked up, so no reference was taken; the
            // adapter pins it, and `release` treats it as a no-op.
            return Ok(Resolved { inode, metadata });
        }
        Ok(Resolved { inode, metadata })
    }

    /// Resolve the parent directory of a path and return it alongside the final
    /// component's name.
    pub async fn resolve_parent(
        &self,
        parent_components: &[String],
    ) -> Result<Resolved, AdapterError> {
        let parent = self.resolve(parent_components).await?;
        if !parent.is_directory() {
            self.release(parent.inode);
            return Err(not_a_directory());
        }
        Ok(parent)
    }

    /// Return one lookup reference taken by [`Self::resolve`].
    ///
    /// The root is pinned by the adapter and needs no accounting, and a
    /// directory the retained set already holds keeps that separate reference.
    pub fn release(&self, inode: u64) {
        if inode == ROOT_INODE {
            return;
        }
        let _ = self.adapter.forget_inode(inode, 1);
    }

    /// Release the reference held on a resolved node.
    ///
    /// A directory's reference is donated to the retained set instead of being
    /// dropped, because that reference is what keeps its cached listing alive
    /// for the next callback. A file's is returned immediately: nothing caches
    /// per-file state that a retained inode would protect.
    pub fn release_resolved(&self, resolved: &Resolved) {
        if resolved.inode == ROOT_INODE {
            return;
        }
        if resolved.is_directory() {
            self.hand_off_directory(resolved.inode, 1);
        } else {
            let _ = self.adapter.forget_inode(resolved.inode, 1);
        }
    }

    /// Release every retained directory reference. Called when the mount stops
    /// so the adapter can drop its translations in one pass.
    pub fn release_all(&self) {
        let inodes = match self.retained.lock() {
            Ok(mut retained) => retained.drain(),
            Err(_) => return,
        };
        let requests: Vec<(u64, u64)> = inodes.into_iter().map(|inode| (inode, 1)).collect();
        let _ = self.adapter.forget_inodes(&requests);
    }

    /// How many directories currently hold a retained reference.
    pub fn retained_directories(&self) -> usize {
        self.retained
            .lock()
            .map(|retained| retained.sequence_of.len())
            .unwrap_or(0)
    }

    /// Give a traversed directory's reference to the retained set, or release
    /// it if the set already holds one. `index` is the component index that was
    /// just resolved *inside* this directory, used only to skip the root.
    fn hand_off_directory(&self, inode: u64, _index: usize) {
        if inode == ROOT_INODE {
            // The root's reference was never taken, so there is nothing to
            // donate and nothing to release.
            return;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let (retention, evicted) = match self.retained.lock() {
            Ok(mut retained) => retained.touch(inode, sequence),
            // A poisoned set must not leak the reference; release it and carry
            // on with a cold cache rather than failing the operation.
            Err(_) => (Retention::AlreadyHeld, Vec::new()),
        };
        if retention == Retention::AlreadyHeld {
            let _ = self.adapter.forget_inode(inode, 1);
        }
        if !evicted.is_empty() {
            let requests: Vec<(u64, u64)> = evicted.into_iter().map(|inode| (inode, 1)).collect();
            let _ = self.adapter.forget_inodes(&requests);
        }
    }

    /// Release the reference held on a partially walked path when the walk
    /// fails part way through.
    fn release_traversed(&self, inode: u64, index: usize) {
        if index == 0 {
            // Still on the root, whose reference was never taken.
            return;
        }
        self.hand_off_directory(inode, index);
    }
}

fn not_a_directory() -> AdapterError {
    AdapterError::Client(quickfs_client_core::ClientError::Server(
        quickfs_protocol::ErrorCode::NotDirectory,
        "path component is not a directory".into(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::testing::{MockFilesystem, adapter_for};
    use std::sync::Arc;

    fn resolver(mock: Arc<MockFilesystem>, capacity: usize) -> Resolver {
        Resolver::with_capacity(adapter_for(mock), capacity)
    }

    #[test]
    fn a_nested_path_resolves_to_the_node_the_server_named() {
        let mock = MockFilesystem::tree();
        let resolver = resolver(Arc::clone(&mock), 64);
        let runtime = resolver.adapter().runtime().clone();

        let deep = runtime
            .block_on(resolver.resolve(&["docs".into(), "notes.txt".into()]))
            .unwrap();
        assert_eq!(deep.kind(), NodeKind::File);
        assert_eq!(deep.metadata.size, 11);

        let directory = runtime
            .block_on(resolver.resolve(&["docs".into()]))
            .unwrap();
        assert!(directory.is_directory());

        let root = runtime.block_on(resolver.resolve(&[])).unwrap();
        assert_eq!(root.inode, ROOT_INODE);
        assert!(root.is_directory());
    }

    #[test]
    fn walking_a_tree_costs_one_listing_per_directory_however_often_it_is_walked() {
        // This is the property that keeps Explorer usable: the first walk warms
        // the adapter's directory cache and the retained reference is what
        // stops that cache being dropped between callbacks.
        let mock = MockFilesystem::tree();
        let resolver = resolver(Arc::clone(&mock), 64);
        let runtime = resolver.adapter().runtime().clone();

        for _ in 0..25 {
            runtime
                .block_on(resolver.resolve(&["docs".into(), "notes.txt".into()]))
                .unwrap();
        }
        assert_eq!(
            mock.directory_listings(),
            2,
            "the root and docs should each have been listed exactly once"
        );
    }

    #[test]
    fn descending_through_a_file_is_refused_without_asking_the_server() {
        let mock = MockFilesystem::tree();
        let resolver = resolver(Arc::clone(&mock), 64);
        let runtime = resolver.adapter().runtime().clone();

        // Warm the walk first, so what the assertion measures is the cost of
        // the refusal and not the cost of discovering the tree.
        runtime
            .block_on(resolver.resolve(&["docs".into(), "notes.txt".into()]))
            .unwrap();
        let before = mock.directory_listings();
        let error = runtime
            .block_on(resolver.resolve(&["docs".into(), "notes.txt".into(), "deeper".into()]))
            .unwrap_err();
        assert!(matches!(
            error,
            AdapterError::Client(quickfs_client_core::ClientError::Server(
                quickfs_protocol::ErrorCode::NotDirectory,
                _
            ))
        ));
        assert_eq!(
            mock.directory_listings() - before,
            0,
            "a file is never enumerated to discover it has no children"
        );
    }

    #[test]
    fn a_missing_component_reports_absence_and_leaks_no_reference() {
        let mock = MockFilesystem::tree();
        let resolver = resolver(Arc::clone(&mock), 64);
        let runtime = resolver.adapter().runtime().clone();

        assert!(
            runtime
                .block_on(resolver.resolve(&["docs".into(), "absent.txt".into()]))
                .is_err()
        );
        // `docs` was traversed and retained; nothing else was.
        assert_eq!(resolver.retained_directories(), 1);
    }

    #[test]
    fn the_retained_set_stays_within_its_bound_under_a_wide_crawl() {
        // A recursive crawl of a wide export must not retain a translation for
        // every directory it passes through.
        let mock = MockFilesystem::wide(64);
        let resolver = resolver(Arc::clone(&mock), 8);
        let runtime = resolver.adapter().runtime().clone();

        for index in 0..64 {
            runtime
                .block_on(resolver.resolve(&[format!("dir{index}"), "leaf".into()]))
                .unwrap();
        }
        assert_eq!(resolver.retained_directories(), 8);
    }

    #[test]
    fn releasing_the_mount_returns_every_retained_reference() {
        let mock = MockFilesystem::wide(4);
        let resolver = resolver(Arc::clone(&mock), 64);
        let runtime = resolver.adapter().runtime().clone();

        for index in 0..4 {
            runtime
                .block_on(resolver.resolve(&[format!("dir{index}"), "leaf".into()]))
                .unwrap();
        }
        assert_eq!(resolver.retained_directories(), 4);
        resolver.release_all();
        assert_eq!(resolver.retained_directories(), 0);
    }

    #[test]
    fn a_repeated_walk_refreshes_rather_than_duplicating_a_retained_reference() {
        let mock = MockFilesystem::tree();
        let resolver = resolver(Arc::clone(&mock), 64);
        let runtime = resolver.adapter().runtime().clone();

        for _ in 0..10 {
            runtime
                .block_on(resolver.resolve(&["docs".into(), "notes.txt".into()]))
                .unwrap();
        }
        assert_eq!(resolver.retained_directories(), 1);
    }
}
