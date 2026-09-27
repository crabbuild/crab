use super::*;

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

    /// Copy a hash-verified cached pack to a caller-owned unpublished file.
    ///
    /// A miss, corrupt entry or cache read error returns false. The destination
    /// may contain rejected bytes and must be reset before origin fallback.
    /// Destination write errors propagate without evicting a healthy cache entry.
    pub async fn copy_git_pack_if_present(
        &self,
        hash: &blake3::Hash,
        expected_len: u64,
        output: &mut tokio::fs::File,
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
        let mut hasher = blake3::Hasher::new();
        let mut remaining = expected_len;
        let mut buffer = vec![0; 1024 * 1024];
        let validation = loop {
            let read = match input.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) => break Err(CacheError::Io(error)),
            };
            if read == 0 {
                let actual = hasher.finalize();
                break if remaining == 0 && actual == *hash {
                    Ok(())
                } else {
                    Err(CacheError::HashMismatch {
                        requested: hash.to_hex().to_string(),
                        actual: actual.to_hex().to_string(),
                    })
                };
            }
            let Some(rest) = remaining.checked_sub(read as u64) else {
                break Err(CacheError::CorruptObject {
                    path: path.display().to_string(),
                    reason: "cached Git pack exceeds its authenticated length".to_owned(),
                });
            };
            // An output failure is not evidence that the cached source is bad.
            output.write_all(&buffer[..read]).await?;
            hasher.update(&buffer[..read]);
            remaining = rest;
        };
        drop(input);
        if validation.is_ok() {
            // Tokio file writes may defer their I/O error until flush. Surface
            // destination failures before reporting a hit or repairing source.
            output.flush().await?;
        }
        let Ok(((), entry)) = entry.finish(validation).await else {
            return Ok(false);
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn git_pack_cache_roundtrip_accounting_and_cleanup() {
        let directory = tempfile::tempdir().unwrap();
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
        let mut output = tokio::fs::File::create(&destination).await.unwrap();
        assert!(
            cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &mut output)
                .await
                .unwrap()
        );
        output.flush().await.unwrap();
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
        let directory = tempfile::tempdir().unwrap();
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
        let mut output = tokio::fs::File::create(directory.path().join("output"))
            .await
            .unwrap();
        assert!(
            !cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &mut output)
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
        let directory = tempfile::tempdir().unwrap();
        let body = b"authenticated pack bytes";
        let hash = blake3::hash(body);
        let source = directory.path().join("source");
        tokio::fs::write(&source, body).await.unwrap();
        let cache = LocalCache::new(directory.path().join("cache"));
        cache
            .put_git_pack_file(&hash, &source, body.len() as u64)
            .await
            .unwrap();
        // A read-only destination makes a real writer failure without relying
        // on mode bits, which a privileged test runner could bypass.
        let mut output = tokio::fs::File::open(&source).await.unwrap();
        assert!(
            !cache
                .copy_git_pack_if_present(&hash, body.len() as u64 + 1, &mut output)
                .await
                .unwrap()
        );
        assert!(matches!(
            cache
                .copy_git_pack_if_present(&hash, body.len() as u64, &mut output)
                .await,
            Err(CacheError::Io(_))
        ));
        assert_eq!(
            tokio::fs::read(cache.git_pack_path(&hash)).await.unwrap(),
            body
        );
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
