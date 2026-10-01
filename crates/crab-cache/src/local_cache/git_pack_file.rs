use super::*;
use crate::private_fs::{PendingFile, PinnedRoot};

impl LocalCache {
    /// Retain a native Git pack file under its authenticated byte-content identity.
    ///
    /// Publication uses the shared catalog reservation and private temporary-file
    /// lifecycle. An entry that exceeds capacity is not retained. Git structure,
    /// sidecars and authorization remain caller-owned; BLAKE3 proves only bytes.
    pub async fn put_git_pack_file(
        &self,
        hash: &blake3::Hash,
        source: &Path,
        expected_len: u64,
    ) -> Result<()> {
        if self
            .catalog
            .max_bytes()
            .is_some_and(|max| expected_len > max)
        {
            return Ok(());
        }
        let path = self.git_pack_path(hash);
        let Some(reservation) = self.catalog.reserve(&path, expected_len).await? else {
            return Ok(());
        };
        let temporary = reservation.pending_file().await?;
        let mut output = temporary.file()?;
        let actual = copy_file_with_blake3(source, &mut output, expected_len).await?;
        if actual != *hash {
            return Err(CacheError::HashMismatch {
                requested: hash.to_hex().to_string(),
                actual: actual.to_hex().to_string(),
            });
        }
        drop(output);
        let reservation = temporary.commit().await?;
        self.record_completed_file(
            "git-pack",
            &path,
            hash.to_hex().to_string(),
            expected_len,
            reservation,
        )
        .await;
        Ok(())
    }

    /// Materialize a hash-verified cached pack at an unpublished destination.
    ///
    /// A hit uses a filesystem copy-on-write clone when available and a bounded
    /// copy otherwise; it never hard-links the mutable repository pack to cache.
    /// The destination parent must be an existing private directory. A miss or
    /// corrupt entry returns false. Destination errors propagate without
    /// evicting a healthy cache entry.
    pub async fn copy_git_pack_if_present(
        &self,
        hash: &blake3::Hash,
        expected_len: u64,
        destination: &Path,
    ) -> Result<bool> {
        let path = self.git_pack_path(hash);
        let Ok((entry, mut input)) = PayloadRead::open(&self.root, &path).await else {
            return Ok(false);
        };
        // A conflicting caller length is not proof of cached corruption. The
        // origin path must resolve it without deleting potentially valid bytes.
        if !input
            .metadata()
            .await
            .is_ok_and(|metadata| metadata.len() == expected_len)
        {
            return Ok(false);
        }

        let destination = destination.to_owned();
        let pending = new_pack_pending_file(&destination).await?;
        #[cfg(unix)]
        let (pending, copy_on_write) = match entry.copy_on_write_to(pending).await? {
            Some(pending) => (pending, true),
            None => (new_pack_pending_file(&destination).await?, false),
        };
        #[cfg(not(unix))]
        let (mut pending, copy_on_write) = (pending, false);

        let mut output = pending.file()?;
        let validation = if copy_on_write {
            verify_pack_file(&mut output, hash, expected_len, &path).await?
        } else {
            copy_pack_file(&mut input, &mut output, hash, expected_len, &path).await?
        };
        drop(output);
        if let Err(error) = validation {
            drop(pending);
            let _ = entry.finish::<(), CacheError>(Err(error)).await;
            return Ok(false);
        }
        tokio::task::spawn_blocking(move || pending.commit_sync())
            .await
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))??;
        let (_, entry) = entry.finish::<(), CacheError>(Ok(())).await?;
        entry.touch().await;
        Ok(true)
    }

    fn git_pack_path(&self, hash: &blake3::Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root
            .join("git-packs")
            .join(&hex[..2])
            .join(hex.as_str())
    }
}

async fn new_pack_pending_file(destination: &Path) -> Result<PendingFile> {
    let parent = destination
        .parent()
        .ok_or_else(|| CacheError::Internal("Git pack destination has no parent".into()))?
        .to_owned();
    let name = destination
        .file_name()
        .ok_or_else(|| CacheError::Internal("Git pack destination has no filename".into()))?;
    let name = PathBuf::from(name);
    tokio::task::spawn_blocking(move || {
        let root = PinnedRoot::open(&parent)?;
        root.pending_file(&name)
    })
    .await
    .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
}

async fn copy_pack_file(
    input: &mut tokio::fs::File,
    output: &mut tokio::fs::File,
    hash: &blake3::Hash,
    expected_len: u64,
    path: &Path,
) -> Result<std::result::Result<(), CacheError>> {
    let mut hasher = blake3::Hasher::new();
    let mut remaining = expected_len;
    let mut buffer = vec![0; 1024 * 1024];
    let validation = loop {
        let read = match input.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => break Err(CacheError::Io(error)),
        };
        if read == 0 {
            break validate_pack_hash(hasher.finalize(), remaining, hash);
        }
        let Some(rest) = remaining.checked_sub(read as u64) else {
            break Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "cached Git pack exceeds its authenticated length".to_owned(),
            });
        };
        // A destination failure does not prove that the cached source is bad.
        output.write_all(&buffer[..read]).await?;
        hasher.update(&buffer[..read]);
        remaining = rest;
    };
    if validation.is_ok() {
        output.flush().await?;
    }
    Ok(validation)
}

async fn verify_pack_file(
    input: &mut tokio::fs::File,
    hash: &blake3::Hash,
    expected_len: u64,
    path: &Path,
) -> Result<std::result::Result<(), CacheError>> {
    let mut hasher = blake3::Hasher::new();
    let mut remaining = expected_len;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            return Ok(validate_pack_hash(hasher.finalize(), remaining, hash));
        }
        let Some(rest) = remaining.checked_sub(read as u64) else {
            return Ok(Err(CacheError::CorruptObject {
                path: path.display().to_string(),
                reason: "cached Git pack exceeds its authenticated length".to_owned(),
            }));
        };
        hasher.update(&buffer[..read]);
        remaining = rest;
    }
}

fn validate_pack_hash(
    actual: blake3::Hash,
    remaining: u64,
    requested: &blake3::Hash,
) -> std::result::Result<(), CacheError> {
    if remaining == 0 && actual == *requested {
        Ok(())
    } else {
        Err(CacheError::HashMismatch {
            requested: requested.to_hex().to_string(),
            actual: actual.to_hex().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        directory
    }

    #[tokio::test]
    async fn git_pack_cache_roundtrip_accounting_and_cleanup() {
        let directory = private_tempdir();
        let body = b"authenticated pack bytes";
        let hash = blake3::hash(body);
        let source = directory.path().join("source");
        tokio::fs::write(&source, body).await.unwrap();
        let cache = LocalCache::with_limits(directory.path().join("cache"), Some(1024), None);
        cache
            .put_git_pack_file(&hash, &source, body.len() as u64)
            .await
            .unwrap();
        let destination = directory.path().join("output");
        assert!(
            cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &destination)
                .await
                .unwrap()
        );
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), body);
        let stats = cache.stats().await.unwrap();
        assert_eq!(
            (stats.git_pack_count, stats.git_pack_bytes),
            (1, body.len() as u64)
        );
        let health =
            crate::health::inspect_cache(&cache.root, Some(1024), &CancellationToken::new())
                .await
                .unwrap();
        assert_eq!(
            health.families["git-pack"].usage.logical_bytes,
            body.len() as u64
        );
        assert_eq!(cache.verify().await.unwrap().valid, 1);
        let clean = crate::clean_cache(&cache.root, false, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (clean.files_removed, clean.bytes_reclaimed),
            (1, body.len() as u64)
        );
    }

    #[tokio::test]
    async fn git_pack_cache_rejects_corruption_and_respects_capacity() {
        let directory = private_tempdir();
        let body = b"authenticated pack bytes";
        let hash = blake3::hash(body);
        let source = directory.path().join("source");
        tokio::fs::write(&source, body).await.unwrap();
        let bounded = LocalCache::with_limits(directory.path().join("bounded"), Some(1), None);
        bounded
            .put_git_pack_file(&hash, &source, body.len() as u64)
            .await
            .unwrap();
        assert!(!bounded.git_pack_path(&hash).exists());
        let cache = LocalCache::new(directory.path().join("cache"));
        cache
            .put_git_pack_file(&hash, &source, body.len() as u64)
            .await
            .unwrap();
        let corrupt = vec![b'X'; body.len()];
        tokio::fs::write(cache.git_pack_path(&hash), &corrupt)
            .await
            .unwrap();
        let destination = directory.path().join("output");
        assert!(
            !cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &destination)
                .await
                .unwrap()
        );
        assert!(!cache.git_pack_path(&hash).exists());
        tokio::fs::write(&source, &corrupt).await.unwrap();
        assert!(
            cache
                .put_git_pack_file(&hash, &source, body.len() as u64)
                .await
                .is_err()
        );
        assert!(!cache.git_pack_path(&hash).exists());
    }

    #[tokio::test]
    async fn git_pack_request_and_destination_failures_do_not_evict_healthy_bytes() {
        let directory = private_tempdir();
        let body = b"authenticated pack bytes";
        let hash = blake3::hash(body);
        let source = directory.path().join("source");
        tokio::fs::write(&source, body).await.unwrap();
        let cache = LocalCache::new(directory.path().join("cache"));
        cache
            .put_git_pack_file(&hash, &source, body.len() as u64)
            .await
            .unwrap();
        let blocked_destination = directory.path().join("blocked-destination");
        tokio::fs::create_dir(&blocked_destination).await.unwrap();
        assert!(
            !cache
                .copy_git_pack_if_present(
                    &hash,
                    body.len() as u64 + 1,
                    &directory.path().join("wrong-length"),
                )
                .await
                .unwrap()
        );
        assert!(matches!(
            cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &blocked_destination)
                .await,
            Err(CacheError::Io(_))
        ));
        assert_eq!(
            tokio::fs::read(cache.git_pack_path(&hash)).await.unwrap(),
            body
        );
        let destination = directory.path().join("output");
        assert!(
            cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &destination)
                .await
                .unwrap()
        );
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), body);
        let prune = LocalCache::with_limits(cache.root.clone(), Some(0), None)
            .prune()
            .await
            .unwrap();
        assert_eq!(
            (prune.git_packs_evicted, prune.bytes_freed),
            (1, body.len() as u64)
        );
        assert_eq!(cache.stats().await.unwrap().git_pack_count, 0);
    }
}
