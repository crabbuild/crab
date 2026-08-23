//! Partitioned, durable mark sets for high-cardinality bucket GC roots.
//!
//! A mark set is an object-store side index owned by one GC run. Writers keep
//! only a small per-partition buffer and publish immutable chunks. Readers
//! load one hash partition at a time, so membership checks never require the
//! complete bucket root set in process memory.

use std::collections::{HashMap, HashSet, VecDeque};

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;
use crab_xet::hash::MerkleHash;

const SCHEMA_VERSION: u32 = 1;
const MARK_CHUNK_SIZE: usize = 1_024;
const MAX_MARK_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAX_MARK_KEY_BYTES: usize = 4 * 1024;
const MAX_BUFFERED_MARK_ENTRIES: usize = 65_536;
const MAX_PARTITION_ENTRIES: usize = 4_000_000;
// Membership probes are driven by provider listing order, not mark partition
// order. Retaining several multi-million-entry partitions would turn a
// bounded mark read into an accidental process-wide heap; one partition is
// enough to amortize adjacent probes while keeping the high-water bound
// explicit.
const MAX_CACHED_PARTITIONS: usize = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkChunk {
    schema_version: u32,
    namespace: String,
    partition: String,
    #[serde(default)]
    key_mode: bool,
    hashes: Vec<String>,
}

/// Bounded object-store writer for one run-owned mark namespace.
pub struct DurableMarkWriter {
    store: Store,
    prefix: String,
    namespace: String,
    key_mode: bool,
    partition_width: usize,
    buffers: HashMap<String, Vec<String>>,
    buffered_entries: usize,
}

impl DurableMarkWriter {
    #[must_use]
    pub fn new(store: Store, prefix: String, namespace: &str) -> Self {
        Self {
            store,
            prefix,
            namespace: namespace.to_owned(),
            key_mode: false,
            partition_width: 4,
            buffers: HashMap::new(),
            buffered_entries: 0,
        }
    }

    #[must_use]
    pub fn new_keys(store: Store, prefix: String, namespace: &str) -> Self {
        Self {
            store,
            prefix,
            namespace: namespace.to_owned(),
            key_mode: true,
            partition_width: 4,
            buffers: HashMap::new(),
            buffered_entries: 0,
        }
    }

    #[must_use]
    pub fn new_hash_width(
        store: Store,
        prefix: String,
        namespace: &str,
        // Number of raw hash bytes used by each partition (2–4).
        partition_width: usize,
    ) -> Self {
        Self {
            store,
            prefix,
            namespace: namespace.to_owned(),
            key_mode: false,
            partition_width,
            buffers: HashMap::new(),
            buffered_entries: 0,
        }
    }

    pub async fn add(&mut self, hash: &str) -> Result<()> {
        if hash.len() > MAX_MARK_KEY_BYTES {
            return Err(CrabError::Configuration {
                key: "gc.marks.key_bytes".to_owned(),
                origin: format!(
                    "GC mark key is {} bytes, above the {MAX_MARK_KEY_BYTES}-byte budget",
                    hash.len()
                ),
            });
        }
        let partition = partition_for(self.key_mode, self.partition_width, hash)?;
        self.buffered_entries = self.buffered_entries.saturating_add(1);
        let hashes = {
            let buffer = self.buffers.entry(partition.clone()).or_default();
            buffer.push(hash.to_owned());
            if buffer.len() >= MARK_CHUNK_SIZE {
                self.buffered_entries = self.buffered_entries.saturating_sub(buffer.len());
                Some(std::mem::take(buffer))
            } else {
                None
            }
        };
        if let Some(hashes) = hashes {
            self.flush_partition(&partition, hashes).await?;
            self.buffers.remove(&partition);
        }
        while self.buffered_entries > MAX_BUFFERED_MARK_ENTRIES {
            let Some(partition) = self
                .buffers
                .iter()
                .find(|(_, hashes)| !hashes.is_empty())
                .map(|(partition, _)| partition.clone())
            else {
                break;
            };
            let hashes = self
                .buffers
                .remove(&partition)
                .filter(|hashes| !hashes.is_empty())
                .ok_or_else(|| CrabError::Internal("GC mark buffer disappeared".to_owned()))?;
            self.buffered_entries = self.buffered_entries.saturating_sub(hashes.len());
            self.flush_partition(&partition, hashes).await?;
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.buffers);
        self.buffered_entries = 0;
        for (partition, hashes) in pending {
            if !hashes.is_empty() {
                self.flush_partition(&partition, hashes).await?;
            }
        }
        Ok(())
    }

    async fn flush_partition(&self, partition: &str, mut hashes: Vec<String>) -> Result<()> {
        hashes.sort_unstable();
        hashes.dedup();
        let chunk = MarkChunk {
            schema_version: SCHEMA_VERSION,
            namespace: self.namespace.clone(),
            partition: partition.to_owned(),
            key_mode: self.key_mode,
            hashes,
        };
        let body = serde_json::to_vec(&chunk).map_err(|error| CrabError::CorruptObject {
            path: self.prefix.clone(),
            reason: format!("GC mark chunk serialization failed: {error}"),
        })?;
        if body.len() > MAX_MARK_CHUNK_BYTES {
            return Err(CrabError::Configuration {
                key: "gc.marks.chunk_bytes".to_owned(),
                origin: format!(
                    "GC mark chunk is {} bytes, above the {MAX_MARK_CHUNK_BYTES}-byte budget",
                    body.len()
                ),
            });
        }
        let path = Path::from(format!(
            "{}/{}/{}/{}.json",
            self.prefix,
            self.namespace,
            partition,
            Uuid::now_v7()
        ));
        self.store.create_strict(&path, Bytes::from(body)).await
    }
}

/// Bounded membership reader for one run-owned mark namespace.
pub struct DurableMarkReader {
    store: Store,
    prefix: String,
    namespace: String,
    key_mode: bool,
    partition_width: usize,
    cached: HashMap<String, HashSet<String>>,
    cache_order: VecDeque<String>,
    cached_entries: usize,
}

impl DurableMarkReader {
    #[must_use]
    pub fn new(store: Store, prefix: String, namespace: &str) -> Self {
        Self {
            store,
            prefix,
            namespace: namespace.to_owned(),
            key_mode: false,
            partition_width: 4,
            cached: HashMap::new(),
            cache_order: VecDeque::new(),
            cached_entries: 0,
        }
    }

    #[must_use]
    pub fn new_keys(store: Store, prefix: String, namespace: &str) -> Self {
        Self {
            store,
            prefix,
            namespace: namespace.to_owned(),
            key_mode: true,
            partition_width: 4,
            cached: HashMap::new(),
            cache_order: VecDeque::new(),
            cached_entries: 0,
        }
    }

    #[must_use]
    pub fn new_hash_width(
        store: Store,
        prefix: String,
        namespace: &str,
        partition_width: usize,
    ) -> Self {
        Self {
            store,
            prefix,
            namespace: namespace.to_owned(),
            key_mode: false,
            partition_width,
            cached: HashMap::new(),
            cache_order: VecDeque::new(),
            cached_entries: 0,
        }
    }

    pub async fn contains(&mut self, hash: &str) -> Result<bool> {
        let partition = partition_for(self.key_mode, self.partition_width, hash)?;
        if !self.cached.contains_key(&partition) {
            let loaded = self.load_partition(&partition).await?;
            while (self.cache_order.len() >= MAX_CACHED_PARTITIONS
                || self.cached_entries.saturating_add(loaded.len()) > MAX_PARTITION_ENTRIES)
                && let Some(evicted) = self.cache_order.pop_front()
            {
                if let Some(values) = self.cached.remove(&evicted) {
                    self.cached_entries = self.cached_entries.saturating_sub(values.len());
                }
            }
            self.cached_entries = self.cached_entries.saturating_add(loaded.len());
            self.cache_order.push_back(partition.clone());
            self.cached.insert(partition.clone(), loaded);
        } else if let Some(position) = self.cache_order.iter().position(|item| item == &partition) {
            self.cache_order.remove(position);
            self.cache_order.push_back(partition.clone());
        }
        Ok(self
            .cached
            .get(&partition)
            .is_some_and(|hashes| hashes.contains(hash)))
    }

    async fn load_partition(&self, partition: &str) -> Result<HashSet<String>> {
        let prefix = Path::from(format!("{}/{}/{}/", self.prefix, self.namespace, partition));
        let mut stream = self.store.inner().list(Some(&prefix));
        let mut hashes = HashSet::new();
        while let Some(meta) = stream.try_next().await.map_err(CrabError::Storage)? {
            let path = meta.location.to_string();
            if !path.starts_with(prefix.as_ref()) || !path.ends_with(".json") {
                return Err(CrabError::CorruptObject {
                    path,
                    reason: "GC mark chunk is outside its canonical namespace".to_owned(),
                });
            }
            let (body, _) = self.store.get_with_etag(&meta.location).await?;
            if body.len() > MAX_MARK_CHUNK_BYTES {
                return Err(CrabError::Configuration {
                    key: "gc.marks.chunk_bytes".to_owned(),
                    origin: format!(
                        "GC mark chunk {} is {} bytes, above the {MAX_MARK_CHUNK_BYTES}-byte budget",
                        meta.location,
                        body.len()
                    ),
                });
            }
            let chunk: MarkChunk =
                serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
                    path: meta.location.to_string(),
                    reason: format!("invalid GC mark chunk: {error}"),
                })?;
            if chunk.schema_version != SCHEMA_VERSION
                || chunk.namespace != self.namespace
                || chunk.partition != partition
                || chunk.key_mode != self.key_mode
                || chunk.hashes.windows(2).any(|window| window[0] >= window[1])
            {
                return Err(CrabError::CorruptObject {
                    path: meta.location.to_string(),
                    reason: "GC mark chunk identity or ordering is invalid".to_owned(),
                });
            }
            if hashes.len().saturating_add(chunk.hashes.len()) > MAX_PARTITION_ENTRIES {
                return Err(CrabError::Configuration {
                    key: "gc.marks.memory_budget".to_owned(),
                    origin: format!(
                        "GC mark partition {partition} exceeds the {MAX_PARTITION_ENTRIES}-entry memory budget"
                    ),
                });
            }
            for hash in chunk.hashes {
                if hash.len() > MAX_MARK_KEY_BYTES {
                    return Err(CrabError::CorruptObject {
                        path: meta.location.to_string(),
                        reason: "GC mark key exceeds the bounded key length".to_owned(),
                    });
                }
                if partition_for(self.key_mode, self.partition_width, hash.as_str())? != partition {
                    return Err(CrabError::CorruptObject {
                        path: meta.location.to_string(),
                        reason: "GC mark hash is in the wrong partition".to_owned(),
                    });
                }
                hashes.insert(hash);
            }
        }
        Ok(hashes)
    }

    pub async fn partition_hashes(&mut self, partition: &str) -> Result<HashSet<String>> {
        if partition.len() != self.partition_width.saturating_mul(2)
            || !partition
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(CrabError::Configuration {
                key: "gc.marks.partition".to_owned(),
                origin: "GC mark partition is not canonical lowercase hexadecimal".to_owned(),
            });
        }
        // Reconciliation consumes one partition and then moves on. Do not
        // put this path in the membership cache: retaining old file-hash
        // partitions would multiply the mark memory budget by the cache size.
        self.load_partition(partition).await
    }

    /// Lists populated partitions for key-mode marks. Key-mode partitions use
    /// four hex digits, so the directory list is bounded by 65,536 entries.
    /// Hash-mode marks intentionally do not expose this operation: their
    /// four-byte raw-hash partition space is large enough that callers must
    /// drive reconciliation from the bounded metadata index instead.
    pub async fn key_partitions(&self) -> Result<Vec<String>> {
        if !self.key_mode {
            return Err(CrabError::Configuration {
                key: "gc.marks.partitions".to_owned(),
                origin: "key partitions are only available for key-mode marks".to_owned(),
            });
        }
        let prefix = Path::from(format!("{}/{}/", self.prefix, self.namespace));
        let listing = self
            .store
            .inner()
            .list_with_delimiter(Some(&prefix))
            .await
            .map_err(CrabError::Storage)?;
        if !listing.objects.is_empty() {
            return Err(CrabError::CorruptObject {
                path: prefix.to_string(),
                reason: "GC key mark namespace contains objects outside a partition".to_owned(),
            });
        }
        let mut partitions = listing
            .common_prefixes
            .into_iter()
            .map(|partition| {
                let value = partition
                    .as_ref()
                    .strip_prefix(prefix.as_ref())
                    .map(|suffix| suffix.strip_prefix('/').unwrap_or(suffix))
                    .map(|suffix| suffix.strip_suffix('/').unwrap_or(suffix))
                    .unwrap_or_default();
                if value.len() != 4
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                {
                    return Err(CrabError::CorruptObject {
                        path: partition.to_string(),
                        reason:
                            "GC key mark partition must be four lowercase hexadecimal characters"
                                .to_owned(),
                    });
                }
                Ok(value.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        partitions.sort_unstable();
        partitions.dedup();
        Ok(partitions)
    }

    /// Counts distinct key marks without retaining more than one partition.
    pub async fn key_count(&self) -> Result<u64> {
        let partitions = self.key_partitions().await?;
        let mut total = 0u64;
        for partition in partitions {
            let values = self.load_key_partition(&partition).await?;
            total = total
                .checked_add(values.len() as u64)
                .ok_or_else(|| CrabError::Internal("GC mark count overflow".to_owned()))?;
        }
        Ok(total)
    }

    pub async fn key_partition_hashes(&self, partition: &str) -> Result<HashSet<String>> {
        if !self.key_mode
            || partition.len() != 4
            || !partition
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(CrabError::Configuration {
                key: "gc.marks.partition".to_owned(),
                origin: "GC key mark partition is not canonical lowercase hexadecimal".to_owned(),
            });
        }
        self.load_partition(partition).await
    }

    async fn load_key_partition(&self, partition: &str) -> Result<HashSet<String>> {
        self.key_partition_hashes(partition).await
    }
}

fn partition_for(key_mode: bool, partition_width: usize, key: &str) -> Result<String> {
    if key_mode {
        return Ok(blake3::hash(key.as_bytes()).to_hex()[..4].to_owned());
    }
    hash_partition(key, partition_width)
}

fn hash_partition(hash: &str, partition_width: usize) -> Result<String> {
    if !(2..=4).contains(&partition_width) {
        return Err(CrabError::Configuration {
            key: "gc.marks.partition_width".to_owned(),
            origin: "GC hash mark partition width must be between two and four bytes".to_owned(),
        });
    }
    let parsed = MerkleHash::from_hex(hash).map_err(|_| CrabError::CorruptObject {
        path: hash.to_owned(),
        reason: "GC mark key must be a canonical lowercase Merkle hash".to_owned(),
    })?;
    let bytes: [u8; 32] = parsed.into();
    Ok(bytes[..partition_width]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn partitioned_marks_round_trip_membership() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut writer = DurableMarkWriter::new(
            store.clone(),
            ".crab/gc/runs/run/marks".to_owned(),
            "shards",
        );
        writer.add(&"a".repeat(64)).await.unwrap();
        writer.add(&"b".repeat(64)).await.unwrap();
        writer.finish().await.unwrap();

        let mut reader =
            DurableMarkReader::new(store, ".crab/gc/runs/run/marks".to_owned(), "shards");
        assert!(reader.contains(&"a".repeat(64)).await.unwrap());
        assert!(!reader.contains(&"c".repeat(64)).await.unwrap());
        assert!(reader.contains(&"b".repeat(64)).await.unwrap());
    }

    #[tokio::test]
    async fn malformed_mark_hash_fails_closed() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut reader =
            DurableMarkReader::new(store, ".crab/gc/runs/run/marks".to_owned(), "shards");
        assert!(reader.contains("not-a-hash").await.is_err());
    }

    #[tokio::test]
    async fn key_partitions_accept_object_store_common_prefixes() {
        let store = Store::new(Arc::new(InMemory::new()));
        let prefix = ".crab/gc/runs/run/marks".to_owned();
        let mut writer = DurableMarkWriter::new_keys(store.clone(), prefix.clone(), "roots");
        writer.add("repo/manifest").await.unwrap();
        writer.finish().await.unwrap();

        let reader = DurableMarkReader::new_keys(store, prefix, "roots");
        let expected = blake3::hash("repo/manifest".as_bytes()).to_hex()[..4].to_owned();
        assert_eq!(reader.key_partitions().await.unwrap(), vec![expected]);
    }

    #[tokio::test]
    async fn hash_partitions_follow_raw_merkle_bytes() {
        let store = Store::new(Arc::new(InMemory::new()));
        let hash = MerkleHash::from([1, 2, 3, 4]);
        let prefix = ".crab/gc/runs/run/marks".to_owned();
        let mut writer =
            DurableMarkWriter::new_hash_width(store.clone(), prefix.clone(), "files", 4);
        writer.add(&hash.hex()).await.unwrap();
        writer.finish().await.unwrap();
        let mut reader = DurableMarkReader::new_hash_width(store, prefix, "files", 4);
        assert!(
            reader
                .partition_hashes("01000000020000000300000004000000")
                .await
                .is_err()
        );
        let partition = reader.partition_hashes("01000000").await.unwrap();
        assert_eq!(partition, HashSet::from([hash.hex()]));
    }
}
