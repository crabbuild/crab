use super::*;
use crab_git::batch::CapturedBlob;
use crab_git::incoming_pack::{ScratchAdmission, ScratchError, ScratchSpace};
use std::collections::VecDeque;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Weak};

const MAX_SCRATCH_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_CAPTURES: usize = 128;
type Slot = Mutex<Option<CapturedBlob>>;

// One process-wide operational budget includes retained captures and temporary
// delta bases. Disposable retention policy must not disable active exact reads.
static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| {
    Arc::new(Budget {
        used: AtomicU64::new(0),
        capacity: MAX_SCRATCH_BYTES,
        slots: std::sync::Mutex::new(VecDeque::new()),
    })
});

pub(super) struct GitCapture {
    root: PathBuf,
    pub(super) oid: String,
    size: u64,
    slot: Arc<Slot>,
    budget: Arc<Budget>,
}

impl GitCapture {
    pub(super) fn new(root: &Path, oid: String, size: u64) -> Self {
        Self {
            root: root.into(),
            oid,
            size,
            slot: Arc::new(Mutex::new(None)),
            budget: BUDGET.clone(),
        }
    }
}

struct Budget {
    used: AtomicU64,
    capacity: u64,
    slots: std::sync::Mutex<VecDeque<Weak<Slot>>>,
}

struct Admission {
    budget: Arc<Budget>,
    bytes: AtomicU64,
}

impl ScratchAdmission for Admission {
    fn resize(&self, bytes: u64) -> std::result::Result<(), ScratchError> {
        // ScratchSpace serializes this admission's resizes. The atomic total
        // coordinates distinct workspaces without holding locks during eviction.
        let previous = self.bytes.load(Ordering::Relaxed);
        if bytes <= previous {
            self.budget
                .used
                .fetch_sub(previous - bytes, Ordering::AcqRel);
        } else {
            let added = bytes - previous;
            loop {
                let used = self.budget.used.load(Ordering::Acquire);
                if let Some(next) = used
                    .checked_add(added)
                    .filter(|next| *next <= self.budget.capacity)
                {
                    if self
                        .budget
                        .used
                        .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        break;
                    }
                    continue;
                }
                if !self
                    .budget
                    .evict()
                    .map_err(|error| ScratchError::Admission(Box::new(error)))?
                {
                    return Err(ScratchError::Capacity {
                        required: self
                            .budget
                            .used
                            .load(Ordering::Acquire)
                            .saturating_add(added),
                        capacity: self.budget.capacity,
                    });
                }
            }
        }
        self.bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }
}

impl Budget {
    fn space(self: &Arc<Self>) -> io::Result<ScratchSpace> {
        let root = crab_cache::default_cache_root();
        crab_cache::ensure_private_cache_directory(&root).map_err(io::Error::other)?;
        Ok(ScratchSpace::new(
            &root,
            Admission {
                budget: self.clone(),
                bytes: AtomicU64::new(0),
            },
        ))
    }

    fn remember(&self, slot: &Arc<Slot>) -> io::Result<()> {
        loop {
            {
                let mut slots = self
                    .slots
                    .lock()
                    .map_err(|_| io::Error::other("Git capture admission is unavailable"))?;
                slots.retain(|entry| {
                    entry.strong_count() != 0 && !entry.ptr_eq(&Arc::downgrade(slot))
                });
                if slots.len() < MAX_CAPTURES {
                    slots.push_back(Arc::downgrade(slot));
                    return Ok(());
                }
            }
            if !self.evict()? {
                return Err(io::Error::other(
                    "Git captures are busy; close a preview before opening more files",
                ));
            }
        }
    }

    fn evict(&self) -> io::Result<bool> {
        let mut removed = false;
        let mut captured = None;
        {
            let mut slots = self
                .slots
                .lock()
                .map_err(|_| io::Error::other("Git capture admission is unavailable"))?;
            for _ in 0..slots.len() {
                let Some(entry) = slots.pop_front() else {
                    break;
                };
                let Some(slot) = entry.upgrade() else {
                    removed = true;
                    continue;
                };
                if let Ok(mut guard) = slot.try_lock() {
                    captured = guard.take();
                    removed = true;
                    break;
                }
                slots.push_back(entry);
            }
        }
        // Dropping a file releases its admission. Never retain the registry lock
        // across close or acquire a busy source; active reads keep their bytes.
        drop(captured);
        Ok(removed)
    }
}

pub(super) fn scratch() -> io::Result<ScratchSpace> {
    BUDGET.space()
}

impl GitCapture {
    async fn copy<W: std::io::Write + Send + 'static>(
        &self,
        range: Range<u64>,
        mut output: W,
        scope: &ReadScope,
    ) -> Result<W> {
        scope.check()?;
        let slot = self.slot.clone();
        let guard = tokio::select! {
            guard = slot.lock_owned() => guard,
            error = scope.cancelled() => return Err(error),
        };
        let budget = self.budget.clone();
        let slot = self.slot.clone();
        let (mut guard, scratch) = command::blocking(scope, move |_| {
            let mut guard = guard;
            if let Some(captured) = guard.as_ref()
                && !captured.source_unchanged()?
            {
                guard.take();
            }
            let scratch = if guard.is_none() {
                match budget.remember(&slot).and_then(|()| budget.space()) {
                    Ok(space) => Some(space),
                    Err(error) => {
                        tracing::debug!(%error, "Git capture storage unavailable");
                        None
                    }
                }
            } else {
                budget.remember(&slot)?;
                None
            };
            Ok((guard, scratch))
        })
        .await?;
        if let Some(scratch) = scratch {
            match command::capture_blob(&self.root, &self.oid, self.size, scratch, scope).await {
                Ok(captured) => *guard = Some(captured),
                Err(AgentError::Io(error))
                    if error
                        .get_ref()
                        .is_some_and(|source| source.is::<ScratchError>()) =>
                {
                    tracing::debug!(%error, "Git capture scratch unavailable");
                }
                Err(error) => return Err(error),
            }
        }
        if guard.is_none() {
            // Capture reuse is optional, as with other disposable read caches.
            // Storage/admission failure cannot hide readable source bytes; the
            // same physical decoder still verifies the complete Git identity.
            return command::copy_range(&self.root, &self.oid, self.size, range, output, scope)
                .await;
        }
        command::blocking(scope, move |cancelled| {
            let captured = guard
                .as_mut()
                .ok_or_else(|| io::Error::other("Git capture was released"))?;
            let result = captured.copy_range(range, &mut output, cancelled);
            if let Err(error) = &result
                && error.kind() != io::ErrorKind::Interrupted
            {
                guard.take();
            }
            result?;
            Ok(output)
        })
        .await
    }

    pub(super) async fn range(
        &self,
        start: u64,
        length: usize,
        scope: &ReadScope,
    ) -> Result<Bytes> {
        let mut output = Vec::new();
        output.try_reserve_exact(length).map_err(io::Error::other)?;
        self.copy(start..start + length as u64, output, scope)
            .await
            .map(Bytes::from)
    }

    pub(super) async fn copy_complete(&self, file: std::fs::File, scope: &ReadScope) -> Result<()> {
        self.copy(0..self.size, file, scope).await.map(drop)
    }

    pub(super) async fn close(&self) {
        self.slot.lock().await.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(capacity: u64) -> Arc<Budget> {
        Arc::new(Budget {
            used: AtomicU64::new(0),
            capacity,
            slots: std::sync::Mutex::new(VecDeque::new()),
        })
    }

    fn reader(budget: &Arc<Budget>, root: &Path, oid: &str) -> GitCapture {
        let mut capture = GitCapture::new(root, oid.into(), 8);
        capture.budget = budget.clone();
        capture
    }

    async fn fixture() -> Result<(tempfile::TempDir, String)> {
        let root = tempfile::tempdir()?;
        let scope = ReadScope::default();
        command::run(
            root.path(),
            &["init", "--template=", "--initial-branch=main"],
            &scope,
        )
        .await?;
        std::fs::write(root.path().join("body.bin"), b"12345678")?;
        let oid = command::run(
            root.path(),
            &["hash-object", "--no-filters", "-w", "body.bin"],
            &scope,
        )
        .await?;
        Ok((root, String::from_utf8(oid)?.trim().into()))
    }

    #[tokio::test]
    async fn pressure_evicts_idle_captures_and_recaptures_exact_bytes() -> Result<()> {
        let (root, oid) = fixture().await?;
        let budget = budget(8);
        let left = reader(&budget, root.path(), &oid);
        let right = reader(&budget, root.path(), &oid);
        let scope = ReadScope::default();
        left.range(0, 8, &scope).await?;
        right.range(0, 8, &scope).await?;
        assert!(left.slot.lock().await.is_none());
        assert_eq!(left.range(0, 8, &scope).await?.as_ref(), b"12345678");
        assert!(right.slot.lock().await.is_none());
        left.close().await;
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test]
    async fn pressure_preserves_active_bytes_and_streams_without_a_second_capture() -> Result<()> {
        let (root, oid) = fixture().await?;
        let budget = budget(8);
        let left = reader(&budget, root.path(), &oid);
        let right = reader(&budget, root.path(), &oid);
        let scope = ReadScope::default();
        left.range(0, 8, &scope).await?;
        let active = left.slot.lock().await;
        assert_eq!(right.range(0, 8, &scope).await?.as_ref(), b"12345678");
        assert!(right.slot.lock().await.is_none());
        assert!(active.is_some());
        assert_eq!(budget.used.load(Ordering::Acquire), 8);
        drop(active);
        left.close().await;
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        Ok(())
    }

    struct StopWriter(ReadScope);

    impl std::io::Write for StopWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.cancel();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelled_warm_copy_keeps_verified_bytes_for_an_independent_scope() -> Result<()> {
        let (root, oid) = fixture().await?;
        let budget = budget(8);
        let captured = reader(&budget, root.path(), &oid);
        captured.range(0, 8, &ReadScope::default()).await?;
        let scope = ReadScope::default();
        let result = captured.copy(0..8, StopWriter(scope.clone()), &scope).await;
        assert!(matches!(result, Err(AgentError::OpCancelled { .. })));
        assert!(captured.slot.lock().await.is_some());
        assert_eq!(
            captured.range(0, 8, &ReadScope::default()).await?.as_ref(),
            b"12345678"
        );
        captured.close().await;
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test]
    async fn missing_promisor_blob_is_fetched_then_captured_by_the_physical_reader() -> Result<()> {
        let (root, oid) = fixture().await?;
        let scope = ReadScope::default();
        command::run(root.path(), &["add", "body.bin"], &scope).await?;
        command::run(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Captured source",
            ],
            &scope,
        )
        .await?;
        command::run(
            root.path(),
            &["config", "uploadpack.allowFilter", "true"],
            &scope,
        )
        .await?;
        let url = url::Url::from_directory_path(root.path())
            .map_err(|()| io::Error::other("invalid fixture URL"))?;
        let checkout = root.path().join("clone");
        let checkout_arg = checkout
            .to_str()
            .ok_or_else(|| io::Error::other("invalid fixture path"))?;
        command::run(
            root.path(),
            &[
                "clone",
                "--template=",
                "--filter=blob:none",
                "--no-checkout",
                url.as_str(),
                checkout_arg,
            ],
            &scope,
        )
        .await?;
        let missing = command::output(
            &checkout,
            &["--no-lazy-fetch", "cat-file", "-e", &oid],
            &scope,
        )
        .await?;
        assert!(
            !missing.status.success(),
            "fixture must exercise a genuinely absent promisor blob"
        );
        let capture = reader(&budget(8), &checkout, &oid);
        assert_eq!(capture.range(0, 8, &scope).await?.as_ref(), b"12345678");
        let fetched = command::output(
            &checkout,
            &["--no-lazy-fetch", "cat-file", "-e", &oid],
            &scope,
        )
        .await?;
        assert!(fetched.status.success());
        capture.close().await;
        Ok(())
    }
}
