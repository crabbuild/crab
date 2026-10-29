use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const LOCAL_BLOCK_BYTES: usize = 1024 * 1024;
const MAX_LOCAL_PROOF_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LOCAL_FILES: usize = 128;

type LocalEntry = Arc<Mutex<Option<VerifiedLocal>>>;

#[derive(PartialEq, Eq)]
struct LocalKey {
    path: PathBuf,
    size: u64,
}

impl LocalKey {
    fn proof_bytes(&self) -> u64 {
        self.size.div_ceil(LOCAL_BLOCK_BYTES as u64) * 32
    }
}

#[derive(Default)]
struct LocalCache {
    entries: VecDeque<(LocalKey, LocalEntry)>,
    proof_bytes: u64,
}

impl LocalCache {
    fn claim(&mut self, key: LocalKey) -> Option<LocalEntry> {
        if let Some(entry) = self
            .entries
            .iter()
            .position(|(found, _)| *found == key)
            .and_then(|index| self.entries.remove(index))
        {
            let claim = Arc::clone(&entry.1);
            self.entries.push_back(entry);
            return Some(claim);
        }
        let required = key.proof_bytes();
        if required > MAX_LOCAL_PROOF_BYTES {
            return None;
        }
        while self.entries.len() >= MAX_LOCAL_FILES
            || self.proof_bytes + required > MAX_LOCAL_PROOF_BYTES
        {
            // A worker or waiter still owns this exact incarnation. Evicting
            // its entry would admit a second verifier for the same source.
            let index = self
                .entries
                .iter()
                .position(|(_, entry)| Arc::strong_count(entry) == 1)?;
            if let Some((retired, _)) = self.entries.remove(index) {
                self.proof_bytes -= retired.proof_bytes();
            }
        }
        let entry = Arc::new(Mutex::new(None));
        self.proof_bytes += required;
        self.entries.push_back((key, Arc::clone(&entry)));
        Some(entry)
    }
}

// The full SHA-256 authenticates these block proofs. Subsequent reads verify
// copied bytes against them, never file timestamps or a reusable pathname.
struct VerifiedLocal {
    file: File,
    blocks: Vec<blake3::Hash>,
}

/// Tracks verification jobs whose read futures can be cancelled independently.
///
/// Drop read futures/streams before closing. Close rejects further hash work and
/// waits for every admitted blocking verification job, including detached joins.
#[derive(Clone, Default)]
pub struct LfsReadSession(Arc<State>);

#[derive(Default)]
struct State {
    state: Mutex<SessionState>,
    tasks: TaskTracker,
}

#[derive(Default)]
struct SessionState {
    closed: bool,
    local: LocalCache,
}

impl LfsReadSession {
    /// Stop verification admission and wait for already-started jobs.
    pub async fn close(&self) {
        {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.local = LocalCache::default();
            self.0.tasks.close();
        }
        self.0.tasks.wait().await;
    }

    /// Read verified local bytes, retaining bounded block proofs and the opened source.
    ///
    /// The first read verifies complete SHA-256 and size. Later ranges verify every
    /// covered block; source replacement/unlink cannot redirect the retained file.
    /// Only open-time absence returns None. Close drains admitted work and releases
    /// retained sources; callers must stop/drop read futures before closing.
    pub async fn read_local_range(
        &self,
        storage: &Path,
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
        let path = crate::local::object_path(storage, oid);
        let entry = {
            let mut state =
                self.0.state.lock().map_err(|_| {
                    std::io::Error::other("LFS verification admission is unavailable")
                })?;
            if state.closed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "LFS read session is closed",
                )
                .into());
            }
            state.local.claim(LocalKey {
                path: path.clone(),
                size,
            })
        };
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
            let file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
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
            .state
            .lock()
            .map_err(|_| std::io::Error::other("LFS verification admission is unavailable"))?;
        if state.closed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "LFS read session is closed",
            ));
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

    #[test]
    fn proof_budget_cannot_evict_live_source_claims() {
        let mut cache = LocalCache::default();
        let full_size = MAX_LOCAL_PROOF_BYTES / 32 * LOCAL_BLOCK_BYTES as u64;
        let key = |name: &str| LocalKey {
            path: name.into(),
            size: full_size,
        };
        let first = cache.claim(key("first")).unwrap();
        let same = cache.claim(key("first")).unwrap();
        assert!(
            Arc::ptr_eq(&first, &same)
                && cache.claim(key("second")).is_none()
                && cache.proof_bytes == MAX_LOCAL_PROOF_BYTES
        );
        drop(first);
        drop(same);
        assert!(
            cache.claim(key("second")).is_some()
                && cache.entries.len() == 1
                && cache.proof_bytes == MAX_LOCAL_PROOF_BYTES
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
