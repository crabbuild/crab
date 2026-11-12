use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use super::*;
use crate::catalog::ReservedFile;

/// Captured LFS cache bytes whose integrity must be checked by the LFS reader.
///
/// Keep this owner alive through verification and delivery. Release any retained
/// reader descriptors before finishing so cleanup measures reclaimable bytes.
pub struct LfsCacheFile {
    entry: PayloadRead,
    file: std::fs::File,
    path: PathBuf,
}

impl LfsCacheFile {
    /// Open another reader of the captured object, without resolving its pathname.
    pub fn reader(&self) -> std::io::Result<std::fs::File> {
        self.file.try_clone()
    }

    /// Source key for caller-owned verification proofs.
    #[must_use]
    pub fn key(&self) -> &Path {
        &self.path
    }

    /// Finish verified delivery, or discard exactly the failed captured object.
    pub async fn finish<E: std::fmt::Display>(
        self,
        validation: std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let Self { entry, file, .. } = self;
        drop(file);
        let ((), entry) = entry.finish(validation).await?;
        entry.touch().await;
        Ok(())
    }
}

/// Journaled, bounded-memory publication of a complete SHA-256 LFS object.
pub struct LfsCacheFill {
    cache: Arc<LocalCache>,
    // Release the writer before its journal owner; Windows can defer unlink
    // until that final file handle closes.
    writer: tokio::fs::File,
    pending: ReservedFile,
    hash: Sha256,
    oid: [u8; 32],
    expected_len: u64,
    written: u64,
    path: PathBuf,
}

impl LfsCacheFill {
    /// Drain admitted writes before releasing an unpublished fill.
    pub async fn abort(mut self) -> Result<()> {
        self.writer.flush().await?;
        Ok(())
    }

    /// Append bytes without permitting a fill beyond its reserved complete size.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let written = self
            .written
            .checked_add(bytes.len() as u64)
            .filter(|written| *written <= self.expected_len)
            .ok_or_else(|| CacheError::CorruptObject {
                path: self.path.display().to_string(),
                reason: "LFS fill exceeds its declared size".into(),
            })?;
        self.writer.write_all(bytes).await?;
        self.hash.update(bytes);
        self.written = written;
        Ok(())
    }

    /// Publish only complete bytes whose SHA-256 matches the selected identity.
    pub async fn commit(self) -> Result<()> {
        let Self {
            cache,
            pending,
            mut writer,
            hash,
            oid,
            expected_len,
            written,
            path,
        } = self;
        writer.flush().await?;
        drop(writer);
        if written != expected_len || hash.finalize().as_slice() != oid {
            return Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "LFS fill does not match its declared SHA-256 and size".into(),
            });
        }
        let reservation = pending.commit().await?;
        cache
            .record_completed_file("lfs", &path, lfs_hex(&oid), expected_len, reservation)
            .await;
        Ok(())
    }
}

impl LocalCache {
    /// Capture optional cached LFS bytes under the shared private root.
    ///
    /// A size disagreement is a miss, not proof of corruption. The consumer
    /// must verify SHA-256 and every delivered range before finishing a hit.
    pub async fn open_lfs_file(
        &self,
        oid: &[u8; 32],
        expected_len: u64,
    ) -> Result<Option<LfsCacheFile>> {
        let path = self.lfs_path(oid);
        let (entry, file) = match PayloadRead::open(self.catalog.root(), &path).await {
            Ok(source) => source,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if file.metadata().await?.len() != expected_len {
            return Ok(None);
        }
        Ok(Some(LfsCacheFile {
            entry,
            file: file.into_std().await,
            path,
        }))
    }

    /// Reserve complete LFS retention under caller and persisted root budgets.
    ///
    /// A declined reservation returns None; cancellation drops its exact private
    /// temporary through the shared journal, without publishing partial bytes.
    pub async fn begin_lfs_fill(
        self: &Arc<Self>,
        oid: &[u8; 32],
        expected_len: u64,
    ) -> Result<Option<LfsCacheFill>> {
        let path = self.lfs_path(oid);
        let Some(reservation) = self.catalog.reserve(&path, expected_len).await? else {
            return Ok(None);
        };
        let pending = reservation.pending_file().await?;
        let writer = pending.file()?;
        Ok(Some(LfsCacheFill {
            cache: Arc::clone(self),
            pending,
            writer,
            hash: Sha256::new(),
            oid: *oid,
            expected_len,
            written: 0,
            path,
        }))
    }

    fn lfs_path(&self, oid: &[u8; 32]) -> PathBuf {
        let hash = lfs_hex(oid);
        self.root.join("lfs").join(&hash[..2]).join(hash)
    }
}

fn lfs_hex(oid: &[u8; 32]) -> String {
    oid.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn cache(directory: &tempfile::TempDir) -> Arc<LocalCache> {
        Arc::new(LocalCache::new(directory.path().join("cache")))
    }

    async fn retain(cache: &Arc<LocalCache>, body: &[u8]) -> [u8; 32] {
        let oid = Sha256::digest(body).into();
        let mut fill = cache
            .begin_lfs_fill(&oid, body.len() as u64)
            .await
            .unwrap()
            .unwrap();
        fill.write(body).await.unwrap();
        fill.commit().await.unwrap();
        oid
    }

    #[tokio::test]
    async fn complete_lfs_uses_native_sha256_accounting_and_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(&directory);
        let body = b"complete LFS retained bytes";
        let oid = retain(&cache, body).await;
        let source = cache
            .open_lfs_file(&oid, body.len() as u64)
            .await
            .unwrap()
            .unwrap();
        let mut file = source.reader().unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
        drop(file);
        source.finish::<CacheError>(Ok(())).await.unwrap();
        assert_eq!(bytes, body);
        let stats = cache.stats().await.unwrap();
        assert_eq!((stats.lfs_count, stats.lfs_bytes), (1, body.len() as u64));
        assert_eq!(cache.verify().await.unwrap().valid, 1);
        let report = crate::health::inspect_cache(cache.root(), None, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            report.families["lfs"].usage.logical_bytes,
            body.len() as u64
        );
        let removed = crate::clean_cache(cache.root(), false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (removed.files_removed, removed.bytes_reclaimed),
            (1, body.len() as u64)
        );
    }

    #[tokio::test]
    async fn incomplete_wrong_hash_and_cancelled_fills_never_publish() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(&directory);
        let oid = Sha256::digest(b"right").into();
        for body in [b"short".as_slice(), b"wrong".as_slice()] {
            let size = if body == b"short" { 6 } else { 5 };
            let mut fill = cache.begin_lfs_fill(&oid, size).await.unwrap().unwrap();
            fill.write(body).await.unwrap();
            assert!(fill.commit().await.is_err());
            assert!(!cache.lfs_path(&oid).exists());
        }
        let mut fill = cache.begin_lfs_fill(&oid, 5).await.unwrap().unwrap();
        fill.write(b"ri").await.unwrap();
        fill.abort().await.unwrap();
        cache.catalog().maintain().await.unwrap();
        assert_eq!(
            crate::CacheCatalog::read_only_stats(cache.catalog().root())
                .unwrap()
                .temporary_bytes,
            0
        );
        assert!(!cache.lfs_path(&oid).exists());
    }

    #[tokio::test]
    async fn corruption_is_verified_and_a_caller_size_disagreement_is_non_destructive() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(&directory);
        let body = b"same declared length";
        let oid = retain(&cache, body).await;
        assert!(
            cache
                .open_lfs_file(&oid, body.len() as u64 + 1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(cache.lfs_path(&oid).exists());
        tokio::fs::write(cache.lfs_path(&oid), vec![0; body.len()])
            .await
            .unwrap();
        assert_eq!(cache.verify().await.unwrap().corrupt, 1);
        assert!(!cache.lfs_path(&oid).exists());
    }

    #[tokio::test]
    async fn shared_root_policy_controls_namespaced_lfs_and_rejects_zero_retention() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let catalog = crate::CacheCatalog::new(root.clone(), None);
        let cache = Arc::new(LocalCache::namespaced(catalog.clone(), &"a".repeat(64)).unwrap());
        let oid = retain(&cache, b"retained LFS").await;
        let reading = cache.open_lfs_file(&oid, 12).await.unwrap().unwrap();
        catalog.set_retention_budget(Some(0)).await.unwrap();
        catalog.maintain().await.unwrap();
        assert!(
            cache.lfs_path(&oid).exists(),
            "an active captured read must survive pressure"
        );
        reading.finish::<CacheError>(Ok(())).await.unwrap();
        catalog.maintain().await.unwrap();
        assert!(!cache.lfs_path(&oid).exists());
        assert!(cache.begin_lfs_fill(&oid, 12).await.unwrap().is_none());
        assert_eq!(
            crate::CacheCatalog::read_only_stats(&root)
                .unwrap()
                .temporary_bytes,
            0
        );
    }
}
