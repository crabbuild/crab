use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[cfg(test)]
use super::read_cache::MAX_LOCAL_FILES;
use super::read_cache::{LOCAL_BLOCK_BYTES, LfsReadCache};

// The full SHA-256 authenticates these block proofs. Subsequent reads verify
// copied bytes against them, never file timestamps or a reusable pathname.
pub(super) struct VerifiedLocal {
    file: File,
    blocks: Vec<blake3::Hash>,
}

/// Tracks verification jobs whose read futures can be cancelled independently.
///
/// Drop read futures/streams before closing. Close rejects further hash work and
/// waits for every admitted blocking verification job, including detached joins.
#[derive(Clone)]
pub struct LfsReadSession(Arc<State>);

struct State {
    cache: Mutex<Option<LfsReadCache>>,
    tasks: TaskTracker,
}

impl Default for LfsReadSession {
    fn default() -> Self {
        Self::new(LfsReadCache::default())
    }
}

impl LfsReadSession {
    /// Track one operation while retaining proofs in a caller-owned cache.
    ///
    /// Close each operation after dropping its read futures/streams. Drop the
    /// shared cache after all operations close to release retained local sources.
    #[must_use]
    pub fn new(cache: LfsReadCache) -> Self {
        Self(Arc::new(State {
            cache: Mutex::new(Some(cache)),
            tasks: TaskTracker::new(),
        }))
    }

    pub(crate) fn cache(&self) -> std::io::Result<LfsReadCache> {
        self.0
            .cache
            .lock()
            .map_err(|_| std::io::Error::other("LFS verification admission is unavailable"))?
            .clone()
            .ok_or_else(closed)
    }

    /// Stop verification admission and wait for already-started jobs.
    pub async fn close(&self) {
        {
            let mut cache = self
                .0
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(cache.take());
            self.0.tasks.close();
        }
        self.0.tasks.wait().await;
    }

    /// Read verified local bytes, retaining bounded block proofs and the opened source.
    ///
    /// The first read verifies complete SHA-256 and size. Later ranges verify every
    /// covered block; source replacement/unlink cannot redirect the retained file.
    /// Only open-time absence returns None. Close drains admitted work and releases
    /// this session's cache claim; a caller-owned cache can retain checked sources
    /// for later operations. Stop/drop read futures before closing.
    pub async fn read_local_range(
        &self,
        storage: &Path,
        oid: &[u8; 32],
        size: u64,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> crate::Result<Option<Bytes>> {
        let path = crate::local::object_path(storage, oid);
        self.read_range(path, None, oid, size, range, cancel).await
    }

    /// Read a captured file using whole-object and retained block verification.
    ///
    /// `key` identifies the caller's selected source in this proof cache; the
    /// descriptor supplies bytes without reopening that path. Keep its external
    /// cache lease until this session and its proof cache have been released.
    pub async fn read_opened_range(
        &self,
        key: &Path,
        file: File,
        oid: &[u8; 32],
        size: u64,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> crate::Result<Bytes> {
        self.read_range(key.to_owned(), Some(file), oid, size, range, cancel)
            .await?
            .ok_or_else(|| std::io::Error::other("captured LFS source is unavailable").into())
    }

    async fn read_range(
        &self,
        path: std::path::PathBuf,
        opened: Option<File>,
        oid: &[u8; 32],
        size: u64,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> crate::Result<Option<Bytes>> {
        if range.start > range.end || range.end > size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "local LFS read range exceeds its declared size",
            )
            .into());
        }
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(cancelled().into()),
            result = crate::object_store::origin::verification_permit() => result?,
        };
        let entry = self.cache()?.local(path.clone(), *oid, size)?;
        let oid = *oid;
        let cancel = cancel.clone();
        self.spawn_blocking(move || {
            let _permit = permit;
            let mut entry = entry
                .as_ref()
                .map(|entry| {
                    entry.lock().map_err(|_| {
                        std::io::Error::other("local LFS source ownership is unavailable")
                    })
                })
                .transpose()?;
            if let Some(guard) = entry.as_mut()
                && let Some(source) = guard.as_mut()
            {
                let mut bytes = range_buffer(&range)?;
                let result = source.read_range(&oid, size, range, &cancel, &mut bytes);
                if result.is_err() {
                    // This read fails against the selected source. A later read
                    // may freshly verify a repaired path; it never hides this error.
                    **guard = None;
                }
                result?;
                return Ok(Some(Bytes::from(bytes)));
            }
            let mut file = match opened.map(Ok).unwrap_or_else(|| File::open(path)) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            file.rewind()?;
            // Absence selects remote before allocating another range buffer.
            // Otherwise remote-only reads allocate and discard local output first.
            let mut bytes = range_buffer(&range)?;
            let source = VerifiedLocal::verify(
                file,
                &oid,
                size,
                range,
                &cancel,
                &mut bytes,
                entry.is_some(),
            )?;
            if let Some(mut guard) = entry {
                *guard = Some(source);
            }
            Ok(Some(Bytes::from(bytes)))
        })?
        .await
        .map_err(std::io::Error::other)?
    }

    pub(crate) fn spawn_blocking<F, T>(&self, job: F) -> std::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let state = self
            .0
            .cache
            .lock()
            .map_err(|_| std::io::Error::other("LFS verification admission is unavailable"))?;
        if state.is_none() {
            return Err(closed());
        }
        Ok(self.0.tasks.spawn_blocking(job))
    }
}

impl VerifiedLocal {
    fn verify(
        mut file: File,
        oid: &[u8; 32],
        size: u64,
        range: Range<u64>,
        cancel: &CancellationToken,
        output: &mut Vec<u8>,
        retain: bool,
    ) -> crate::Result<Self> {
        if file.metadata()?.len() != size {
            return Err(corrupt(oid));
        }
        let mut blocks = Vec::new();
        if retain {
            blocks
                .try_reserve_exact(size.div_ceil(LOCAL_BLOCK_BYTES as u64) as usize)
                .map_err(|source| std::io::Error::new(std::io::ErrorKind::OutOfMemory, source))?;
        }
        let mut sha256 = Sha256::new();
        let mut buffer = vec![0; LOCAL_BLOCK_BYTES];
        let mut offset = 0;
        while offset < size {
            let length = (size - offset).min(LOCAL_BLOCK_BYTES as u64) as usize;
            read_block(&mut file, &mut buffer[..length], oid, cancel)?;
            sha256.update(&buffer[..length]);
            if retain {
                blocks.push(blake3::hash(&buffer[..length]));
            }
            copy_selected(&buffer[..length], offset, &range, output);
            offset += length as u64;
        }
        if file.read(&mut [0; 1])? != 0 || sha256.finalize().as_slice() != oid {
            return Err(corrupt(oid));
        }
        Ok(Self { file, blocks })
    }

    fn read_range(
        &mut self,
        oid: &[u8; 32],
        size: u64,
        range: Range<u64>,
        cancel: &CancellationToken,
        output: &mut Vec<u8>,
    ) -> crate::Result<()> {
        if self.file.metadata()?.len() != size {
            return Err(corrupt(oid));
        }
        let first = range.start / LOCAL_BLOCK_BYTES as u64;
        let end = range.end.div_ceil(LOCAL_BLOCK_BYTES as u64);
        let mut buffer = vec![0; LOCAL_BLOCK_BYTES];
        self.file
            .seek(SeekFrom::Start(first * LOCAL_BLOCK_BYTES as u64))?;
        for index in first..end {
            let offset = index * LOCAL_BLOCK_BYTES as u64;
            let length = (size - offset).min(LOCAL_BLOCK_BYTES as u64) as usize;
            read_block(&mut self.file, &mut buffer[..length], oid, cancel)?;
            if self.blocks.get(index as usize) != Some(&blake3::hash(&buffer[..length])) {
                return Err(corrupt(oid));
            }
            copy_selected(&buffer[..length], offset, &range, output);
        }
        Ok(())
    }
}

fn read_block(
    file: &mut File,
    bytes: &mut [u8],
    oid: &[u8; 32],
    cancel: &CancellationToken,
) -> crate::Result<()> {
    if cancel.is_cancelled() {
        return Err(cancelled().into());
    }
    file.read_exact(bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt(oid)
        } else {
            error.into()
        }
    })
}

fn range_buffer(range: &Range<u64>) -> crate::Result<Vec<u8>> {
    let capacity = usize::try_from(range.end - range.start).map_err(std::io::Error::other)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|source| std::io::Error::new(std::io::ErrorKind::OutOfMemory, source))?;
    Ok(bytes)
}

fn copy_selected(block: &[u8], offset: u64, range: &Range<u64>, output: &mut Vec<u8>) {
    let start = range.start.max(offset);
    let end = range.end.min(offset + block.len() as u64);
    if start < end {
        output.extend_from_slice(&block[(start - offset) as usize..(end - offset) as usize]);
    }
}

fn corrupt(oid: &[u8; 32]) -> crate::LfsError {
    crate::LfsError::ObjectCorrupt {
        oid: crab_git::lfs_pointer::hex_encode(oid),
    }
}

fn closed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "LFS read session is closed")
}

fn cancelled() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "local LFS verification cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captured_sources_do_not_reopen_paths_or_reuse_another_oid_proof() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("source");
        std::fs::write(&key, b"first").unwrap();
        let captured = File::open(&key).unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"other").unwrap();
        #[cfg(unix)]
        std::fs::rename(&replacement, &key).unwrap();
        let session = LfsReadSession::default();
        let cancel = CancellationToken::new();
        let oid = Sha256::digest(b"first").into();
        let bytes = session
            .read_opened_range(&key, captured, &oid, 5, 0..5, &cancel)
            .await
            .unwrap();
        assert_eq!(bytes, b"first".as_slice());
        let wrong_oid = [0; 32];
        let invalid = session
            .read_opened_range(
                &key,
                File::open(&key).unwrap(),
                &wrong_oid,
                5,
                0..5,
                &cancel,
            )
            .await;
        session.close().await;
        assert!(matches!(
            invalid,
            Err(crate::LfsError::ObjectCorrupt { .. })
        ));
    }

    #[tokio::test]
    async fn shared_proofs_survive_operation_close_until_the_cache_owner_releases_them() {
        let root = tempfile::tempdir().unwrap();
        let body = b"retained payload";
        let oid = Sha256::digest(body).into();
        let path = crate::local::object_path(root.path(), &oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        let cache = LfsReadCache::default();
        let cancel = CancellationToken::new();
        let first = LfsReadSession::new(cache.clone());
        first
            .read_local_range(root.path(), &oid, body.len() as u64, 0..3, &cancel)
            .await
            .unwrap();
        first.close().await;
        std::fs::remove_file(&path).unwrap();
        let source = Arc::downgrade(&cache.local(path, oid, body.len() as u64).unwrap().unwrap());
        let second = LfsReadSession::new(cache.clone());
        let bytes = second
            .read_local_range(root.path(), &oid, body.len() as u64, 4..8, &cancel)
            .await
            .unwrap();
        second.close().await;
        assert_eq!(bytes, Some(Bytes::from_static(b"ined")));
        assert!(source.upgrade().is_some());
        drop(cache);
        assert!(source.upgrade().is_none());
    }

    #[tokio::test]
    async fn absent_local_objects_do_not_reserve_the_requested_payload() {
        let root = tempfile::tempdir().unwrap();
        let session = LfsReadSession::default();
        let result = session
            .read_local_range(
                root.path(),
                &[0; 32],
                u64::MAX,
                0..u64::MAX,
                &CancellationToken::new(),
            )
            .await;
        session.close().await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn cached_local_ranges_reject_changed_blocks_and_allow_fresh_repair() {
        let root = tempfile::tempdir().unwrap();
        let body = vec![b'a'; LOCAL_BLOCK_BYTES + 17];
        let oid = Sha256::digest(&body).into();
        let path = crate::local::object_path(root.path(), &oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &body).unwrap();
        let session = LfsReadSession::default();
        let cancel = CancellationToken::new();
        session
            .read_local_range(root.path(), &oid, body.len() as u64, 0..1, &cancel)
            .await
            .unwrap();
        let mut changed = body.clone();
        changed[LOCAL_BLOCK_BYTES - 1] = b'X';
        std::fs::write(&path, changed).unwrap();
        let invalid = session
            .read_local_range(root.path(), &oid, body.len() as u64, 0..1, &cancel)
            .await;
        std::fs::write(&path, &body).unwrap();
        let boundary = LOCAL_BLOCK_BYTES as u64;
        let repaired = session
            .read_local_range(
                root.path(),
                &oid,
                body.len() as u64,
                boundary - 2..boundary + 17,
                &cancel,
            )
            .await
            .unwrap();
        session.close().await;
        assert!(
            matches!(invalid, Err(crate::LfsError::ObjectCorrupt { .. }))
                && repaired == Some(Bytes::from(vec![b'a'; 19]))
        );
    }

    #[tokio::test]
    async fn local_proof_retention_evicts_old_sources_and_close_releases_all() {
        let root = tempfile::tempdir().unwrap();
        let session = LfsReadSession::default();
        let cancel = CancellationToken::new();
        let mut identities = Vec::new();
        for value in 0..=MAX_LOCAL_FILES as u64 {
            let body = value.to_le_bytes();
            let oid = Sha256::digest(body).into();
            let path = crate::local::object_path(root.path(), &oid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            session
                .read_local_range(root.path(), &oid, 8, 0..8, &cancel)
                .await
                .unwrap();
            std::fs::remove_file(path).unwrap();
            identities.push(oid);
        }
        let oldest = session
            .read_local_range(root.path(), &identities[0], 8, 0..8, &cancel)
            .await
            .unwrap();
        let newest = session
            .read_local_range(root.path(), identities.last().unwrap(), 8, 0..8, &cancel)
            .await
            .unwrap();
        session.close().await;
        let closed = session
            .read_local_range(root.path(), identities.last().unwrap(), 8, 0..8, &cancel)
            .await;
        assert!(
            oldest.is_none()
                && newest
                    == Some(Bytes::copy_from_slice(
                        &(MAX_LOCAL_FILES as u64).to_le_bytes()
                    ))
                && matches!(closed, Err(crate::LfsError::Io { source }) if source.kind() == std::io::ErrorKind::BrokenPipe)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_waits_for_detached_verification_and_rejects_new_work() {
        let session = LfsReadSession::default();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, entered) = tokio::sync::oneshot::channel();
        let job = session
            .spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            })
            .unwrap();
        entered.await.unwrap();
        drop(job);
        let mut close = Box::pin(session.close());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut close)
                .await
                .is_err()
        );
        assert!(session.spawn_blocking(|| ()).is_err());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), close)
            .await
            .unwrap();
        session.close().await;
    }
}
