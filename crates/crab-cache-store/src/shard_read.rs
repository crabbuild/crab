use super::*;
use crab_xet::hash::compute_data_hash;

impl CachingStore {
    /// Read a shard body using the captured catalog's exact size and content identity.
    /// The caller authorizes its physical path. This read does not establish an ETag.
    pub async fn get_captured_shard(
        &self,
        path: &Path,
        hash: MerkleHash,
        size: u64,
    ) -> Result<Bytes> {
        let corrupt = || StorageError::CorruptObject {
            path: path.to_string(),
            reason: "shard bytes differ from their authenticated size or digest".to_owned(),
        };
        if size == 0 || size > MAX_CACHE_SHARD_BYTES {
            return Err(corrupt().into());
        }
        self.origin.check_cached_read(path).await?;
        let key = cache_key_for_path(path.as_ref());
        // A projected shard path must not select the bucket-global cache namespace.
        if let Some(key) = &key {
            if !matches!(key, CacheKey::Shard(key_hash) if *key_hash == hash) {
                return Err(corrupt().into());
            }
            if let Some(bytes) = self.bounded_cached_body(path, key, size).await {
                return (bytes.len() as u64 == size)
                    .then_some(bytes)
                    .ok_or_else(|| corrupt().into());
            }
        }

        let bytes = self
            .origin
            .read_verified(path, size, None, |bytes| async {
                if bytes.len() as u64 != size {
                    return Err(corrupt());
                }
                let (bytes, valid) = tokio::task::spawn_blocking(move || {
                    let valid = compute_data_hash(&bytes) == hash;
                    (bytes, valid)
                })
                .await
                .map_err(|source| StorageError::Io {
                    source: std::io::Error::other(source),
                })?;
                if !valid {
                    return Err(corrupt());
                }
                Ok(bytes)
            })
            .await?;
        if let Some(key) = key {
            self.install_bounded_body(path, &key, bytes.clone()).await?;
        }
        Ok(bytes)
    }
}
