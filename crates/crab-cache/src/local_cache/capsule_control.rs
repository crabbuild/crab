use super::*;

pub(super) const MAX_PAYLOAD_BYTES: u64 = 64 * 1024;
const DIGEST_BYTES: usize = 32;
pub(super) const MAX_FILE_BYTES: u64 = MAX_PAYLOAD_BYTES + DIGEST_BYTES as u64;

impl LocalCache {
    /// Read an intact capsule window before the caller rechecks its authenticated pointer.
    pub async fn read_capsule_control(
        &self,
        key: &blake3::Hash,
        size: u64,
    ) -> Result<Option<Bytes>> {
        if size == 0 || size > MAX_PAYLOAD_BYTES {
            return Ok(None);
        }
        let path = self.capsule_control_path(key);
        let Some((bytes, entry)) = read_file_bounded_result(
            self.catalog.root(),
            &path,
            size + DIGEST_BYTES as u64,
            |bytes| {
                if bytes.len() as u64 == size + DIGEST_BYTES as u64 && valid(bytes, key) {
                    Ok(())
                } else {
                    Err(CacheError::CorruptObject {
                        path: path.display().to_string(),
                        reason: "capsule control cache entry failed its size or checksum check"
                            .into(),
                    })
                }
            },
        )
        .await?
        else {
            return Ok(None);
        };
        entry.touch().await;
        Ok(Some(bytes.slice(DIGEST_BYTES..)))
    }

    /// Retain a capsule window the caller has verified against its authenticated pointer.
    pub async fn put_capsule_control(&self, key: &blake3::Hash, bytes: &Bytes) -> Result<()> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_PAYLOAD_BYTES {
            return Ok(());
        }
        let mut record = Vec::with_capacity(DIGEST_BYTES + bytes.len());
        record.extend_from_slice(checksum(key, bytes).as_bytes());
        record.extend_from_slice(bytes);
        self.atomic_write(&self.capsule_control_path(key), &record)
            .await
    }

    fn capsule_control_path(&self, key: &blake3::Hash) -> PathBuf {
        let hash = key.to_hex();
        self.root
            .join("capsule-controls")
            .join(&hash[..2])
            .join(hash.as_str())
    }
}

pub(super) fn valid(record: &[u8], key: &blake3::Hash) -> bool {
    record
        .split_first_chunk::<DIGEST_BYTES>()
        .is_some_and(|(digest, bytes)| {
            !bytes.is_empty() && digest == checksum(key, bytes).as_bytes()
        })
}

fn checksum(key: &blake3::Hash, bytes: &[u8]) -> blake3::Hash {
    let mut hash = blake3::Hasher::new_derive_key("Crab capsule control cache record v1");
    hash.update(key.as_bytes());
    hash.update(bytes);
    hash.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capsule_control_reopens_and_repairs_its_exact_budgeted_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        let key = blake3::hash(b"captured target and window");
        let bytes = Bytes::from_static(b"verified control window");
        let size = bytes.len() as u64;
        LocalCache::new(root.clone())
            .put_capsule_control(&key, &bytes)
            .await
            .unwrap();
        let cache = LocalCache::new(root);
        assert_eq!(
            cache.read_capsule_control(&key, size).await.unwrap(),
            Some(bytes)
        );
        let stats = cache.stats().await.unwrap();
        assert_eq!(
            (stats.capsule_control_count, stats.capsule_control_bytes),
            (1, size + DIGEST_BYTES as u64)
        );
        assert_eq!(cache.verify().await.unwrap().valid, 1);
        tokio::fs::write(
            cache.capsule_control_path(&key),
            vec![0; size as usize + DIGEST_BYTES],
        )
        .await
        .unwrap();
        assert!(cache.read_capsule_control(&key, size).await.is_err());
        assert_eq!(cache.read_capsule_control(&key, size).await.unwrap(), None);
    }
}
