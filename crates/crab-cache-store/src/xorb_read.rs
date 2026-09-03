//! Verified xorb reads with source-specific cache repair.
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;

use bytes::Bytes;
use crab_cache::CacheError;
use crab_storage::StorageError;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::format::{ChunkMeta, FOOTER_SIZE, MAX_XORB_SIZE};
use crab_xet::xorb::parser::{
    XorbParser, decode_chunk_range_bytes, xorb_chunks_from_metadata, xorb_metadata_region,
};
use object_store::path::Path;

use crate::{CacheStoreError, CachingStore, Result};

const XORB_READ_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const XORB_READ_CACHE_MAX_ENTRIES: usize = 4096;
const XORB_READ_LOCK_STRIPES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XorbSource {
    Local,
    Service,
    Origin,
}

impl CachingStore {
    /// Read verified chunk ranges, installing full xorbs for high-coverage requests.
    pub async fn get_xorb_chunks(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        ranges: &[(u32, u32)],
    ) -> Result<(Bytes, Vec<u32>)> {
        self.read_xorb_chunks(path, xorb_hash, ranges, true).await
    }

    /// Read verified chunks from a complete body without installing a full-xorb copy.
    pub async fn get_xorb_chunks_without_install(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        ranges: &[(u32, u32)],
    ) -> Result<(Bytes, Vec<u32>)> {
        self.read_xorb_chunks(path, xorb_hash, ranges, false).await
    }

    /// Read bounded, hash-verified xorb metadata with the same repair policy as hydration.
    pub async fn xorb_chunk_metadata(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
    ) -> Result<Vec<ChunkMeta>> {
        let _fill_guard = self.xorb_reads.lock(xorb_hash).await;
        for source in [XorbSource::Local, XorbSource::Service, XorbSource::Origin] {
            match self.xorb_read_plan(path, xorb_hash, source).await {
                Ok(Some(plan)) => return Ok(plan.chunks),
                Ok(None) => {}
                Err(error) => {
                    self.xorb_read_failed(path, xorb_hash, source, error)
                        .await?
                }
            }
        }
        Err(StorageError::NotFound {
            path: path.to_string(),
        }
        .into())
    }

    async fn read_xorb_chunks(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        ranges: &[(u32, u32)],
        install_full_xorb: bool,
    ) -> Result<(Bytes, Vec<u32>)> {
        if ranges.is_empty() {
            return Ok((Bytes::new(), vec![0]));
        }
        let key = XorbReadKey::new(*xorb_hash, ranges, install_full_xorb);
        if let Some(result) = key.as_ref().and_then(|key| self.xorb_reads.get(key)) {
            return Ok(result);
        }
        let _fill_guard = self.xorb_reads.lock(xorb_hash).await;
        if let Some(result) = key.as_ref().and_then(|key| self.xorb_reads.get(key)) {
            return Ok(result);
        }

        // An attempt never switches sources. A parser failure therefore has
        // a known owner, even when a cache disappears between metadata and payload.
        for source in [XorbSource::Local, XorbSource::Service, XorbSource::Origin] {
            match self
                .xorb_chunks_from_source(path, xorb_hash, ranges, source, install_full_xorb)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(key) = key {
                        self.xorb_reads.insert(key, &result);
                    }
                    return Ok(result);
                }
                Ok(None) => {}
                Err(error) => {
                    self.xorb_read_failed(path, xorb_hash, source, error)
                        .await?
                }
            }
        }
        Err(StorageError::NotFound {
            path: path.to_string(),
        }
        .into())
    }

    async fn xorb_read_failed(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        source: XorbSource,
        error: CacheStoreError,
    ) -> Result<()> {
        // Invalid requested chunk bounds are a caller error, not damaged cache.
        if matches!(
            &error,
            CacheStoreError::Cache(CacheError::ChunkNotFound { .. })
        ) {
            return Err(error);
        }
        if source == XorbSource::Origin {
            return Err(match error {
                CacheStoreError::Cache(source) => CacheStoreError::OriginIntegrity {
                    path: path.to_string(),
                    source,
                },
                other => other,
            });
        }
        tracing::warn!(
            family = "xorb",
            operation = "read-and-verify",
            path = %path,
            cache_source = ?source,
            recovery = "try-next-source",
            %error,
            "optional xorb cache read failed"
        );
        // Decoding outlives the local read handle. Reverify the current payload
        // under its removal lock; old bytes cannot authorize deleting a refill.
        if source == XorbSource::Local
            && let Err(error) = self.local_cache.evict_corrupt_xorb(xorb_hash).await
        {
            tracing::warn!(
                family = "xorb",
                operation = "evict",
                path = %path,
                recovery = "bypass-cache",
                %error,
                "local cache eviction failed"
            );
        }
        Ok(())
    }

    async fn xorb_chunks_from_source(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        ranges: &[(u32, u32)],
        source: XorbSource,
        install_full_xorb: bool,
    ) -> Result<Option<(Bytes, Vec<u32>)>> {
        if !install_full_xorb {
            return self
                .complete_xorb_ranges(path, xorb_hash, ranges, source, false)
                .await;
        }
        let Some(plan) = self.xorb_read_plan(path, xorb_hash, source).await? else {
            return Ok(None);
        };
        let requested_bytes = requested_payload_bytes(&plan.chunks, ranges)?;
        if ranges_cover_all_chunks(plan.chunks.len(), ranges)
            || requested_bytes.saturating_mul(2) >= plan.payload_len
        {
            return self
                .complete_xorb_ranges(path, xorb_hash, ranges, source, true)
                .await;
        }

        let mut decoded = Vec::with_capacity(ranges.len());
        for &(start, end) in ranges {
            let metas = chunk_meta_range(&plan.chunks, start, end)?;
            let (Some(first), Some(last)) = (metas.first(), metas.last()) else {
                decoded.push((Bytes::new(), vec![0]));
                continue;
            };
            let payload_start = u64::from(first.offset);
            let payload_end = u64::from(last.offset) + u64::from(last.compressed_len);
            let Some(payload) = self
                .xorb_range(path, xorb_hash, payload_start..payload_end, source)
                .await?
            else {
                return Ok(None);
            };
            decoded.push(decode_chunk_range_bytes(metas, payload).map_err(CacheError::from)?);
        }
        concatenate_decoded_ranges(decoded).map(Some)
    }

    async fn complete_xorb_ranges(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        ranges: &[(u32, u32)],
        source: XorbSource,
        install: bool,
    ) -> Result<Option<(Bytes, Vec<u32>)>> {
        let data = match source {
            XorbSource::Local => self.local_cache.get_read_xorb_if_present(xorb_hash).await?,
            XorbSource::Service => {
                self.get_cache_service_object_without_install_limit(
                    path,
                    Some(MAX_XORB_SIZE as u64),
                )
                .await?
            }
            XorbSource::Origin => Some(
                self.origin
                    .get_with_etag_bounded(path, MAX_XORB_SIZE as u64)
                    .await?
                    .0,
            ),
        };
        let Some(data) = data else {
            return Ok(None);
        };
        let parser = XorbParser::parse(data.clone()).map_err(CacheError::from)?;
        if parser.hash() != *xorb_hash {
            return Err(CacheError::HashMismatch {
                requested: xorb_hash.hex(),
                actual: parser.hash().hex(),
            }
            .into());
        }
        parser.verify_payload_digest().map_err(CacheError::from)?;
        let result = collect_full_xorb_ranges(&parser, ranges)?;
        if install && source != XorbSource::Local {
            // Validate at the source boundary before best-effort installation.
            // A local write/index failure cannot change a verified read result.
            parser.verify_all_chunks().map_err(CacheError::from)?;
            if let Err(error) = self.local_cache.put_read_xorb(xorb_hash, data).await {
                tracing::warn!(
                    family = "xorb",
                    operation = "install",
                    path = %path,
                    recovery = "return-verified-bytes",
                    %error,
                    "local xorb cache write failed"
                );
            }
        }
        Ok(Some(result))
    }

    async fn xorb_read_plan(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        source: XorbSource,
    ) -> Result<Option<XorbReadPlan>> {
        let object_len = match source {
            XorbSource::Local => {
                return Ok(self
                    .local_cache
                    .get_xorb_metadata_if_present(xorb_hash)
                    .await?
                    .map(|(chunks, payload_len)| XorbReadPlan {
                        chunks,
                        payload_len,
                    }));
            }
            XorbSource::Service => {
                let Some(head) = self.head_cache_service_object(path).await? else {
                    return Ok(None);
                };
                head.size
            }
            XorbSource::Origin => self.origin.head(path).await?.size,
        };
        if !(FOOTER_SIZE as u64..=MAX_XORB_SIZE as u64).contains(&object_len) {
            return Err(CacheError::CorruptObject {
                path: path.to_string(),
                reason: format!("xorb size {object_len} is outside format bounds"),
            }
            .into());
        }
        let footer_start = object_len - FOOTER_SIZE as u64;
        let Some(footer) = self
            .xorb_range(path, xorb_hash, footer_start..object_len, source)
            .await?
        else {
            return Ok(None);
        };
        let object_len = object_len as usize; // Bounded by MAX_XORB_SIZE above.
        let region = xorb_metadata_region(object_len, &footer).map_err(CacheError::from)?;
        let metadata_range = region.offset as u64..(region.offset + region.len) as u64;
        let Some(metadata) = self
            .xorb_range(path, xorb_hash, metadata_range, source)
            .await?
        else {
            return Ok(None);
        };
        let (chunks, actual) =
            xorb_chunks_from_metadata(object_len, &footer, &metadata).map_err(CacheError::from)?;
        if actual != *xorb_hash {
            return Err(CacheError::HashMismatch {
                requested: xorb_hash.hex(),
                actual: actual.hex(),
            }
            .into());
        }
        Ok(Some(XorbReadPlan {
            chunks,
            payload_len: region.offset as u64,
        }))
    }

    async fn xorb_range(
        &self,
        path: &Path,
        xorb_hash: &MerkleHash,
        range: Range<u64>,
        source: XorbSource,
    ) -> Result<Option<Bytes>> {
        match source {
            XorbSource::Local => Ok(self
                .local_cache
                .get_xorb_range_if_present(xorb_hash, range)
                .await),
            XorbSource::Service => Ok(self
                .range_get_cache_service_object(path, range)
                .await?
                .map(|result| result.data)),
            XorbSource::Origin => Ok(Some(self.origin.range_get(path, range).await?)),
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
struct XorbReadKey {
    xorb_hash: MerkleHash,
    ranges: Vec<(u32, u32)>,
    install_full_xorb: bool,
}

impl XorbReadKey {
    fn retained_charge(&self, data: usize, offsets: usize, queued_ranges: usize) -> usize {
        let key_bytes = self
            .ranges
            .capacity()
            .saturating_add(queued_ranges)
            .saturating_mul(std::mem::size_of::<(u32, u32)>());
        data.saturating_add(offsets.saturating_mul(std::mem::size_of::<u32>()))
            .saturating_add(key_bytes)
            .saturating_add(2 * std::mem::size_of::<Self>())
            .saturating_add(std::mem::size_of::<CachedXorbRead>())
    }

    fn new(xorb_hash: MerkleHash, ranges: &[(u32, u32)], install_full_xorb: bool) -> Option<Self> {
        // The map and eviction queue each retain a key. Skip this optional
        // result cache before cloning a range list that cannot fit its budget.
        let charge = ranges
            .len()
            .checked_mul(2 * std::mem::size_of::<(u32, u32)>())?
            .checked_add(2 * std::mem::size_of::<Self>() + std::mem::size_of::<CachedXorbRead>())?;
        if charge > XORB_READ_CACHE_MAX_BYTES {
            return None;
        }
        let mut owned = Vec::new();
        owned.try_reserve_exact(ranges.len()).ok()?;
        owned.extend_from_slice(ranges);
        Some(Self {
            xorb_hash,
            ranges: owned,
            install_full_xorb,
        })
    }
}

struct CachedXorbRead {
    data: Bytes,
    offsets: Vec<u32>,
    charge: usize,
}

struct XorbReadCache {
    entries: HashMap<XorbReadKey, CachedXorbRead>,
    insertion_order: VecDeque<XorbReadKey>,
    charged_bytes: usize,
}

impl XorbReadCache {
    fn make_room(&mut self, charge: usize) {
        while self.charged_bytes.saturating_add(charge) > XORB_READ_CACHE_MAX_BYTES
            || self.entries.len() >= XORB_READ_CACHE_MAX_ENTRIES
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.charged_bytes = self.charged_bytes.saturating_sub(removed.charge);
            }
        }
    }
}

pub(super) struct XorbReadState {
    cache: std::sync::Mutex<XorbReadCache>,
    fill_locks: Box<[tokio::sync::Mutex<()>]>,
}

impl XorbReadState {
    pub(super) fn new() -> Self {
        Self {
            cache: std::sync::Mutex::new(XorbReadCache {
                entries: HashMap::new(),
                insertion_order: VecDeque::new(),
                charged_bytes: 0,
            }),
            fill_locks: std::iter::repeat_with(|| tokio::sync::Mutex::new(()))
                .take(XORB_READ_LOCK_STRIPES)
                .collect(),
        }
    }

    fn get(&self, key: &XorbReadKey) -> Option<(Bytes, Vec<u32>)> {
        let cache = self.cache_guard();
        let entry = cache.entries.get(key)?;
        let mut offsets = Vec::new();
        offsets.try_reserve_exact(entry.offsets.len()).ok()?;
        offsets.extend_from_slice(&entry.offsets);
        Some((entry.data.clone(), offsets))
    }

    fn insert(&self, key: XorbReadKey, value: &(Bytes, Vec<u32>)) {
        let charge = key.retained_charge(value.0.len(), value.1.len(), key.ranges.len());
        if value.0.is_empty() || charge > XORB_READ_CACHE_MAX_BYTES {
            return;
        }

        let mut cache = self.cache_guard();
        if cache.entries.contains_key(&key) {
            return;
        }
        cache.make_room(charge);
        if cache.entries.try_reserve(1).is_err() || cache.insertion_order.try_reserve(1).is_err() {
            return;
        }
        // Raw decoded ranges can borrow a tiny slice of a complete xorb. Own
        // precisely the retained bytes so a small charged entry cannot pin an
        // arbitrarily larger backing allocation. Allocation failure is a miss.
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        if data.try_reserve_exact(value.0.len()).is_err()
            || offsets.try_reserve_exact(value.1.len()).is_err()
        {
            return;
        }
        let Some(queued_key) = XorbReadKey::new(key.xorb_hash, &key.ranges, key.install_full_xorb)
        else {
            return;
        };
        let charge = key.retained_charge(
            data.capacity(),
            offsets.capacity(),
            queued_key.ranges.capacity(),
        );
        if charge > XORB_READ_CACHE_MAX_BYTES {
            return;
        }
        // try_reserve_exact may provide additional capacity. Charge the actual
        // retained buffers, not only the requested lengths.
        cache.make_room(charge);
        data.extend_from_slice(&value.0);
        offsets.extend_from_slice(&value.1);
        cache.charged_bytes = cache.charged_bytes.saturating_add(charge);
        cache.insertion_order.push_back(queued_key);
        cache.entries.insert(
            key,
            CachedXorbRead {
                data: Bytes::from(data),
                offsets,
                charge,
            },
        );
    }

    async fn lock(&self, xorb_hash: &MerkleHash) -> tokio::sync::MutexGuard<'_, ()> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        xorb_hash.hash(&mut hasher);
        let index = hasher.finish() as usize % self.fill_locks.len();
        self.fill_locks[index].lock().await
    }

    fn cache_guard(&self) -> std::sync::MutexGuard<'_, XorbReadCache> {
        match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

struct XorbReadPlan {
    chunks: Vec<ChunkMeta>,
    payload_len: u64,
}

fn requested_payload_bytes(chunks: &[ChunkMeta], ranges: &[(u32, u32)]) -> Result<u64> {
    let mut bytes = 0u64;
    for &(start, end) in ranges {
        for meta in chunk_meta_range(chunks, start, end)? {
            bytes = bytes
                .checked_add(u64::from(meta.compressed_len))
                .ok_or_else(|| CacheError::CorruptObject {
                    path: "xorb".to_owned(),
                    reason: "requested xorb payload length overflow".to_owned(),
                })?;
        }
    }
    Ok(bytes)
}

fn ranges_cover_all_chunks(chunk_count: usize, ranges: &[(u32, u32)]) -> bool {
    let Ok(chunk_count) = u32::try_from(chunk_count) else {
        return false;
    };
    let mut ranges = ranges.to_vec();
    ranges.sort_unstable();

    let mut covered_until = 0;
    for (start, end) in ranges {
        if start > covered_until {
            return false;
        }
        covered_until = covered_until.max(end);
        if covered_until >= chunk_count {
            return true;
        }
    }
    chunk_count == 0
}

fn chunk_meta_range(chunks: &[ChunkMeta], start: u32, end: u32) -> Result<&[ChunkMeta]> {
    let range = usize::try_from(start).ok().zip(usize::try_from(end).ok());
    range
        .and_then(|(start, end)| chunks.get(start..end))
        .ok_or_else(|| {
            CacheError::ChunkNotFound {
                hash: format!("range [{start}, {end}) exceeds {} chunks", chunks.len()),
            }
            .into()
        })
}

fn collect_full_xorb_ranges(
    parser: &crab_xet::xorb::parser::XorbParser,
    ranges: &[(u32, u32)],
) -> Result<(Bytes, Vec<u32>)> {
    let mut decoded = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges {
        if start > end || end > parser.num_chunks() {
            return Err(CacheError::ChunkNotFound {
                hash: format!(
                    "range [{start}, {end}) exceeds {} chunks",
                    parser.num_chunks()
                ),
            }
            .into());
        }
        decoded.push(
            parser
                .get_chunk_range_bytes(start, end)
                .map_err(CacheError::from)?,
        );
    }
    concatenate_decoded_ranges(decoded)
}

fn concatenate_decoded_ranges(decoded: Vec<(Bytes, Vec<u32>)>) -> Result<(Bytes, Vec<u32>)> {
    if let [single] = decoded.as_slice() {
        return Ok(single.clone());
    }

    let total_bytes: usize = decoded.iter().map(|(data, _)| data.len()).sum();
    let total_chunks: usize = decoded
        .iter()
        .map(|(_, offsets)| offsets.len().saturating_sub(1))
        .sum();
    let mut data = Vec::with_capacity(total_bytes);
    let mut offsets = Vec::with_capacity(total_chunks + 1);
    for (range_data, range_offsets) in decoded {
        let base = u32::try_from(data.len()).map_err(|_| CacheError::CorruptObject {
            path: "xorb".to_owned(),
            reason: "decoded xorb range exceeds u32 offset space".to_owned(),
        })?;
        for offset in range_offsets
            .iter()
            .take(range_offsets.len().saturating_sub(1))
        {
            offsets.push(
                base.checked_add(*offset)
                    .ok_or_else(|| CacheError::CorruptObject {
                        path: "xorb".to_owned(),
                        reason: "decoded xorb range offset overflow".to_owned(),
                    })?,
            );
        }
        data.extend_from_slice(&range_data);
    }
    offsets.push(
        u32::try_from(data.len()).map_err(|_| CacheError::CorruptObject {
            path: "xorb".to_owned(),
            reason: "decoded xorb ranges exceed u32 offset space".to_owned(),
        })?,
    );
    Ok((Bytes::from(data), offsets))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests;
