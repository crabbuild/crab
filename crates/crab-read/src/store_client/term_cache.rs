use std::sync::Arc;

use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use xet_client::cas_types::{ChunkRange, Key};
use xet_client::chunk_cache::ChunkCache;

pub(super) struct TermCache {
    inner: Arc<dyn ChunkCache>,
    prefix: String,
}

impl TermCache {
    pub(super) fn new(inner: Arc<dyn ChunkCache>, prefix: String) -> Self {
        Self { inner, prefix }
    }

    pub(super) async fn read(
        &self,
        hash: MerkleHash,
        ranges: &[ChunkRange],
    ) -> Option<(Bytes, Vec<u32>)> {
        let key = Key {
            prefix: self.prefix.clone(),
            hash,
        };
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for range in ranges {
            let cached = match self.inner.get(&key, range).await {
                Ok(Some(cached)) => cached,
                Ok(None) => return None,
                Err(error) => {
                    tracing::warn!(%error, "decoded cache read failed");
                    return None;
                }
            };
            let count = range.end.checked_sub(range.start)? as usize;
            if cached.range != *range
                || cached.offsets.len() != count + 1
                || cached.offsets.first().copied() != Some(0)
                || cached.offsets.last().copied()? as usize != cached.data.len()
                || !cached.offsets.windows(2).all(|pair| pair[0] < pair[1])
            {
                return None;
            }
            let base = u32::try_from(data.len()).ok()?;
            for offset in cached.offsets.into_iter().take(count) {
                offsets.push(base.checked_add(offset)?);
            }
            if data.is_empty() {
                data = cached.data;
            } else {
                data.extend_from_slice(&cached.data);
            }
        }
        offsets.push(u32::try_from(data.len()).ok()?);
        Some((data.into(), offsets))
    }

    pub(super) async fn write(
        &self,
        hash: MerkleHash,
        ranges: &[ChunkRange],
        data: &Bytes,
        offsets: &[u32],
    ) {
        let key = Key {
            prefix: self.prefix.clone(),
            hash,
        };
        let mut first = 0usize;
        for range in ranges {
            let Some(count) = range.end.checked_sub(range.start) else {
                return;
            };
            let Some(end) = first.checked_add(count as usize) else {
                return;
            };
            let Some(indices) = offsets.get(first..=end) else {
                return;
            };
            let (Some(&start_byte), Some(&end_byte)) = (indices.first(), indices.last()) else {
                return;
            };
            let Some(bytes) = data.get(start_byte as usize..end_byte as usize) else {
                return;
            };
            let Some(relative) = indices
                .iter()
                .map(|offset| offset.checked_sub(start_byte))
                .collect::<Option<Vec<_>>>()
            else {
                return;
            };
            if let Err(error) = self.inner.put(&key, range, &relative, bytes).await {
                tracing::warn!(%error, "decoded cache write failed");
            }
            first = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use xet_client::chunk_cache::{CacheRange, error::ChunkCacheError};

    type StoredRange = (ChunkRange, Vec<u32>, Vec<u8>);

    #[derive(Default)]
    struct Ranges(Mutex<Vec<StoredRange>>);
    #[async_trait::async_trait]
    impl ChunkCache for Ranges {
        async fn get(
            &self,
            _: &Key,
            range: &ChunkRange,
        ) -> Result<Option<CacheRange>, ChunkCacheError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|(stored, _, _)| stored == range)
                .map(|(range, offsets, data)| CacheRange {
                    range: *range,
                    offsets: offsets.clone(),
                    data: data.clone(),
                }))
        }
        async fn put(
            &self,
            _: &Key,
            range: &ChunkRange,
            offsets: &[u32],
            data: &[u8],
        ) -> Result<(), ChunkCacheError> {
            self.0
                .lock()
                .unwrap()
                .push((*range, offsets.to_vec(), data.to_vec()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn disjoint_ranges_have_independent_cache_offsets() {
        let inner = Arc::new(Ranges::default());
        let cache = TermCache::new(inner.clone(), "fixture".to_owned());
        let hash = MerkleHash::from([1, 2, 3, 4]);
        let ranges = [ChunkRange::new(2, 4), ChunkRange::new(8, 9)];
        cache
            .write(hash, &ranges, &Bytes::from_static(b"abcdef"), &[0, 1, 3, 6])
            .await;
        assert_eq!(
            *inner.0.lock().unwrap(),
            vec![
                (ranges[0], vec![0, 1, 3], b"abc".to_vec()),
                (ranges[1], vec![0, 3], b"def".to_vec())
            ]
        );
        assert_eq!(
            cache.read(hash, &ranges).await,
            Some((Bytes::from_static(b"abcdef"), vec![0, 1, 3, 6]))
        );
    }
}
