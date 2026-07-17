// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use async_trait::async_trait;
use dashmap::DashMap;
use quickfs_protocol::{DirectoryEntry, Metadata, NodeId};
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RevisionKey {
    pub node: NodeId,
    pub revision: u64,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RangeKey {
    pub file: RevisionKey,
    pub offset: u64,
    pub length: u64,
}
#[async_trait]
pub trait MetadataCache: Send + Sync {
    async fn get(&self, node: NodeId) -> Option<Metadata>;
    async fn insert(&self, value: Metadata);
    async fn invalidate(&self, node: NodeId);
}
#[async_trait]
pub trait DirectoryCache: Send + Sync {
    async fn get(&self, key: RevisionKey) -> Option<Vec<DirectoryEntry>>;
    async fn insert(&self, key: RevisionKey, value: Vec<DirectoryEntry>);
    async fn invalidate(&self, node: NodeId);
}
#[async_trait]
pub trait RangeCache: Send + Sync {
    async fn get(&self, key: RangeKey) -> Option<Vec<u8>>;
    async fn insert(&self, key: RangeKey, value: Vec<u8>);
    async fn invalidate(&self, node: NodeId);
}
#[derive(Default)]
pub struct MemoryCache {
    metadata: DashMap<NodeId, Metadata>,
    directories: DashMap<RevisionKey, Vec<DirectoryEntry>>,
    ranges: DashMap<RangeKey, Vec<u8>>,
}
#[async_trait]
impl MetadataCache for MemoryCache {
    async fn get(&self, n: NodeId) -> Option<Metadata> {
        self.metadata.get(&n).map(|v| v.clone())
    }
    async fn insert(&self, v: Metadata) {
        self.metadata.insert(v.node, v);
    }
    async fn invalidate(&self, n: NodeId) {
        self.metadata.remove(&n);
    }
}
#[async_trait]
impl DirectoryCache for MemoryCache {
    async fn get(&self, k: RevisionKey) -> Option<Vec<DirectoryEntry>> {
        self.directories.get(&k).map(|v| v.clone())
    }
    async fn insert(&self, k: RevisionKey, v: Vec<DirectoryEntry>) {
        self.directories.insert(k, v);
    }
    async fn invalidate(&self, n: NodeId) {
        self.directories.retain(|k, _| k.node != n);
    }
}
#[async_trait]
impl RangeCache for MemoryCache {
    async fn get(&self, k: RangeKey) -> Option<Vec<u8>> {
        self.ranges.get(&k).map(|v| v.clone())
    }
    async fn insert(&self, k: RangeKey, v: Vec<u8>) {
        self.ranges.insert(k, v);
    }
    async fn invalidate(&self, n: NodeId) {
        self.ranges.retain(|k, _| k.file.node != n);
    }
}
