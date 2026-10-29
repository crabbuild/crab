//! Conditional ownership of an immutable captured-read root.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::push_lock::{create_strict, store_error, update};
use crate::{CoordinationError, DEFAULT_GC_FENCE_QUARANTINE, Result};

/// Backend lifetime of an active read claim between conditional renewals.
pub const READ_RETENTION_TTL: Duration = Duration::from_secs(300);
const MAX_CLAIM_BYTES: u64 = 4096;

/// Immutable descriptor whose dependencies remain live while its claim is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedReadRoot {
    hash: String,
    size: u64,
}

impl RetainedReadRoot {
    /// Bind a nonempty descriptor to its canonical BLAKE3 digest and encoded length.
    pub fn new(hash: String, size: u64) -> Result<Self> {
        let root = Self { hash, size };
        root.validate("read retention root")?;
        Ok(root)
    }

    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    fn validate(&self, path: &str) -> Result<()> {
        if self.size == 0
            || !blake3::Hash::from_hex(&self.hash)
                .is_ok_and(|hash| hash.to_hex().as_str() == self.hash)
        {
            return Err(malformed(path, "invalid captured descriptor identity"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClaimState {
    Active,
    Released,
    Retired,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    version: u32,
    incarnation: String,
    sequence: u64,
    state: ClaimState,
    root: RetainedReadRoot,
}

impl Claim {
    fn encode(&self, path: &str) -> Result<Bytes> {
        serde_json::to_vec(self)
            .map(Bytes::from)
            .map_err(|source| CoordinationError::Serialize {
                key: path.to_owned(),
                context: "read retention claim",
                source,
            })
    }

    fn decode(path: &str, bytes: &[u8]) -> Result<Self> {
        let claim: Self =
            serde_json::from_slice(bytes).map_err(|source| CoordinationError::Serialize {
                key: path.to_owned(),
                context: "read retention claim",
                source,
            })?;
        if claim.version != 1
            || !uuid::Uuid::try_parse(&claim.incarnation)
                .is_ok_and(|id| id.get_version_num() == 7 && id.to_string() == claim.incarnation)
        {
            return Err(malformed(
                path,
                "unsupported version or invalid incarnation",
            ));
        }
        claim.root.validate(path)?;
        Ok(claim)
    }
}

/// Exclusive renewal owner for one reader's private conditional claim.
///
/// The caller publishes its immutable descriptor first, then validates both GC
/// domain barriers after creation and before admitting reads. Keep ownership
/// through every admitted read; stop and drain them before releasing. This type
/// does not start a worker or renew on drop.
#[must_use = "retain and renew the claim through captured reads, then drain and release"]
pub struct ReadRetentionLease {
    store: Arc<dyn ObjectStore>,
    path: Path,
    claim: Claim,
    version: UpdateVersion,
    deadline: Option<Instant>,
}

impl ReadRetentionLease {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Create a unique claim without overwriting or recovering an existing owner.
    ///
    /// An uncertain creation is abandoned, never retried with the same claim path.
    pub async fn create(
        store: Arc<dyn ObjectStore>,
        path: Path,
        root: RetainedReadRoot,
    ) -> Result<Self> {
        root.validate(path.as_ref())?;
        if path.as_ref().is_empty() {
            return Err(malformed(path.as_ref(), "claim path is empty"));
        }
        let claim = Claim {
            version: 1,
            incarnation: uuid::Uuid::now_v7().to_string(),
            sequence: 0,
            state: ClaimState::Active,
            root,
        };
        let deadline = renewal_deadline(&path)?;
        let version = create_strict(&store, &path, claim.encode(path.as_ref())?)
            .await
            .map_err(|source| store_error(path.as_ref(), source))?;
        let lease = Self {
            store,
            path,
            claim,
            version,
            deadline: Some(deadline),
        };
        lease.ensure_valid()?;
        Ok(lease)
    }

    /// Reject locally expired or uncertain ownership before admitting more reads.
    pub fn ensure_valid(&self) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return Ok(());
        }
        Err(lost(&self.path))
    }

    /// Return the conservative local deadline of the current acknowledged renewal.
    pub fn deadline(&self) -> Result<Instant> {
        self.ensure_valid()?;
        self.deadline.ok_or_else(|| lost(&self.path))
    }

    /// Renew by changing the sequence under the last acknowledged provider version.
    ///
    /// Failure or cancellation permanently loses this owner. A delayed successful
    /// response cannot extend the local deadline from its arrival time.
    pub async fn renew(&mut self) -> Result<()> {
        self.ensure_valid()?;
        // Clearing before I/O makes cancellation terminal too. A content-derived
        // ETag must change, or a stale GC retirement could overwrite renewal.
        self.deadline = None;
        let deadline = renewal_deadline(&self.path)?;
        self.claim.sequence = self
            .claim
            .sequence
            .checked_add(1)
            .ok_or_else(|| lost(&self.path))?;
        let version = update(
            &self.store,
            &self.path,
            self.claim.encode(self.path.as_ref())?,
            self.version.clone(),
        )
        .await
        .map_err(|source| store_error(self.path.as_ref(), source))?;
        if Instant::now() >= deadline {
            return Err(lost(&self.path));
        }
        self.version = version;
        self.deadline = Some(deadline);
        Ok(())
    }

    /// Release only this acknowledged version after all captured reads have drained.
    pub async fn release(mut self) -> Result<()> {
        self.deadline = None;
        self.claim.state = ClaimState::Released;
        update(
            &self.store,
            &self.path,
            self.claim.encode(self.path.as_ref())?,
            self.version,
        )
        .await
        .map_err(|source| store_error(self.path.as_ref(), source))?;
        Ok(())
    }
}

/// Capture one claim's GC root, conditionally retiring an expired active owner.
///
/// The caller holds sweep admission and supplies a fresh backend-clock sample.
/// Retirement retains the root through the same uncertainty quarantine as a
/// crashed writer. None permits reclaiming this terminal claim and descriptor;
/// decode, storage and conditional conflicts abort the sweep instead.
pub async fn retained_read_root(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
    backend_now: i64,
) -> Result<Option<RetainedReadRoot>> {
    if backend_now <= 0 {
        return Err(malformed(path.as_ref(), "invalid backend clock sample"));
    }
    let (mut claim, version, modified) = load_claim(store, path).await?;
    let lifetime = match claim.state {
        ClaimState::Released => return Ok(None),
        ClaimState::Active => READ_RETENTION_TTL,
        ClaimState::Retired => DEFAULT_GC_FENCE_QUARANTINE,
    };
    let expires = i64::try_from(lifetime.as_secs())
        .ok()
        .and_then(|seconds| modified.checked_add(seconds))
        .ok_or_else(|| malformed(path.as_ref(), "claim expiry overflows"))?;
    if backend_now < expires {
        return Ok(Some(claim.root));
    }
    if claim.state == ClaimState::Retired {
        return Ok(None);
    }
    claim.state = ClaimState::Retired;
    update(store, path, claim.encode(path.as_ref())?, version)
        .await
        .map_err(|source| store_error(path.as_ref(), source))?;
    // The retirement write starts a new backend-age quarantine. This sweep
    // must retain the root even if the prior active claim was very old.
    Ok(Some(claim.root))
}

/// Read a claim conservatively without expiring owners or writing during GC planning.
///
/// Retired claims remain roots until repository GC completes their quarantine
/// and removes them. Renewals change versions, not this immutable root identity.
pub async fn load_read_retention_root(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
) -> Result<Option<RetainedReadRoot>> {
    let (claim, _, _) = load_claim(store, path).await?;
    Ok((claim.state != ClaimState::Released).then_some(claim.root))
}

async fn load_claim(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
) -> Result<(Claim, UpdateVersion, i64)> {
    let object = store
        .get(path)
        .await
        .map_err(|source| store_error(path.as_ref(), source))?;
    if object.meta.size > MAX_CLAIM_BYTES {
        return Err(malformed(path.as_ref(), "claim exceeds its byte bound"));
    }
    let version = UpdateVersion {
        e_tag: object.meta.e_tag.clone(),
        version: object.meta.version.clone(),
    };
    let modified = object.meta.last_modified.timestamp();
    if modified <= 0 {
        return Err(malformed(
            path.as_ref(),
            "invalid backend modification time",
        ));
    }
    let mut bytes = BytesMut::new();
    let mut stream = object.into_stream();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|source| store_error(path.as_ref(), source))?
    {
        if bytes.len().saturating_add(chunk.len()) as u64 > MAX_CLAIM_BYTES {
            return Err(malformed(path.as_ref(), "claim exceeds its byte bound"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let claim = Claim::decode(path.as_ref(), &bytes)?;
    Ok((claim, version, modified))
}

fn renewal_deadline(path: &Path) -> Result<Instant> {
    // Renew before half the backend lifetime. Even a delayed renewal accepted
    // before this new deadline precedes expiry of the last acknowledged claim.
    Instant::now()
        .checked_add(READ_RETENTION_TTL / 2)
        .ok_or_else(|| lost(path))
}

fn lost(path: &Path) -> CoordinationError {
    CoordinationError::ReadRetentionLost {
        path: path.to_string(),
    }
}

fn malformed(path: &str, reason: &str) -> CoordinationError {
    CoordinationError::ReadRetentionMalformed {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn root() -> RetainedReadRoot {
        RetainedReadRoot::new(blake3::hash(b"capture").to_hex().to_string(), 7).unwrap()
    }

    async fn fixture() -> (Arc<dyn ObjectStore>, Path, ReadRetentionLease) {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("repo/reader/claim");
        let lease = ReadRetentionLease::create(store.clone(), path.clone(), root())
            .await
            .unwrap();
        (store, path, lease)
    }

    #[tokio::test]
    async fn renewal_changes_content_and_release_retires_exact_capture() {
        let (store, path, mut lease) = fixture().await;
        let before = store.get(&path).await.unwrap().bytes().await.unwrap();
        lease.renew().await.unwrap();
        let after = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert_ne!(before, after);
        let now = store.head(&path).await.unwrap().last_modified.timestamp();
        assert_eq!(
            retained_read_root(&store, &path, now).await.unwrap(),
            Some(root())
        );
        lease.release().await.unwrap();
        assert_eq!(retained_read_root(&store, &path, now).await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_owner_is_retired_before_quarantine_can_release_its_root() {
        let (store, path, mut lease) = fixture().await;
        let modified = store.head(&path).await.unwrap().last_modified.timestamp();
        let expired = modified + READ_RETENTION_TTL.as_secs() as i64;
        assert_eq!(
            retained_read_root(&store, &path, expired).await.unwrap(),
            Some(root())
        );
        assert!(lease.renew().await.is_err());
        assert!(matches!(
            lease.ensure_valid(),
            Err(CoordinationError::ReadRetentionLost { .. })
        ));
        let retired = store.head(&path).await.unwrap().last_modified.timestamp();
        let end = retired + DEFAULT_GC_FENCE_QUARANTINE.as_secs() as i64;
        assert_eq!(
            retained_read_root(&store, &path, end - 1).await.unwrap(),
            Some(root())
        );
        assert_eq!(retained_read_root(&store, &path, end).await.unwrap(), None);
        // Global planning retains the root until repository GC actually removes
        // the terminal claim and descriptor; a local clock cannot retire it.
        assert_eq!(
            load_read_retention_root(&store, &path).await.unwrap(),
            Some(root())
        );
    }

    #[tokio::test]
    async fn elapsed_local_deadline_cannot_renew_or_recover_ownership() {
        let (store, path, mut lease) = fixture().await;
        lease.deadline = Some(Instant::now());
        let before = store.get(&path).await.unwrap().bytes().await.unwrap();
        for _ in 0..2 {
            assert!(matches!(
                lease.renew().await,
                Err(CoordinationError::ReadRetentionLost { .. })
            ));
        }
        assert_eq!(
            store.get(&path).await.unwrap().bytes().await.unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn existing_claim_cannot_be_replaced_by_another_capture() {
        let (store, path, _lease) = fixture().await;
        let other = RetainedReadRoot::new(blake3::hash(b"other").to_hex().to_string(), 5).unwrap();
        assert!(
            ReadRetentionLease::create(store.clone(), path.clone(), other)
                .await
                .is_err()
        );
        let now = store.head(&path).await.unwrap().last_modified.timestamp();
        assert_eq!(
            retained_read_root(&store, &path, now).await.unwrap(),
            Some(root())
        );
    }

    #[tokio::test]
    async fn corrupt_or_oversized_claim_cannot_become_empty_gc_protection() {
        let (store, path, _lease) = fixture().await;
        for body in [
            Bytes::from_static(b"{}"),
            Bytes::from(vec![b' '; MAX_CLAIM_BYTES as usize + 1]),
        ] {
            store.put(&path, body.into()).await.unwrap();
            let now = store.head(&path).await.unwrap().last_modified.timestamp();
            assert!(retained_read_root(&store, &path, now).await.is_err());
        }
    }
    #[derive(Debug, Default)]
    struct FaultStore {
        inner: InMemory,
        mode: std::sync::atomic::AtomicU8,
        entered: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    impl std::fmt::Display for FaultStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("claim-fault-store")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for FaultStore {
        async fn put_opts(
            &self,
            path: &Path,
            payload: object_store::PutPayload,
            options: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            let mode = if matches!(options.mode, object_store::PutMode::Update(_)) {
                self.mode.swap(0, std::sync::atomic::Ordering::SeqCst)
            } else {
                0
            };
            if mode == 1 {
                self.entered.notify_one();
                self.resume.notified().await;
            }
            let result = self.inner.put_opts(path, payload, options).await?;
            if mode == 2 {
                self.entered.notify_one();
                self.resume.notified().await;
            }
            if mode == 3 {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "lost committed response".into(),
                });
            }
            Ok(result)
        }
        async fn put_multipart_opts(
            &self,
            path: &Path,
            options: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(path, options).await
        }
        async fn get_opts(
            &self,
            path: &Path,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            self.inner.get_opts(path, options).await
        }
        fn delete_stream(
            &self,
            paths: futures_util::stream::BoxStream<'static, object_store::Result<Path>>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(paths)
        }
        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_renewal_cannot_resume_before_or_after_the_backend_commit() {
        for mode in [1, 2] {
            let backend = Arc::new(FaultStore::default());
            let store: Arc<dyn ObjectStore> = backend.clone();
            let path = Path::from("repo/reader/claim");
            let mut lease = ReadRetentionLease::create(store.clone(), path.clone(), root())
                .await
                .unwrap();
            backend
                .mode
                .store(mode, std::sync::atomic::Ordering::SeqCst);
            let mut renewal = Box::pin(lease.renew());
            tokio::select! {
                result = &mut renewal => panic!("renewal escaped its gate: {result:?}"),
                () = backend.entered.notified() => {},
            }
            drop(renewal);
            assert!(matches!(
                lease.renew().await,
                Err(CoordinationError::ReadRetentionLost { .. })
            ));
            assert_eq!(
                load_read_retention_root(&store, &path).await.unwrap(),
                Some(root())
            );
        }
    }

    #[tokio::test]
    async fn lost_committed_renewal_response_cannot_release_or_recover_the_unknown_version() {
        let backend = Arc::new(FaultStore::default());
        let store: Arc<dyn ObjectStore> = backend.clone();
        let path = Path::from("repo/reader/claim");
        let mut lease = ReadRetentionLease::create(store.clone(), path.clone(), root())
            .await
            .unwrap();
        backend.mode.store(3, std::sync::atomic::Ordering::SeqCst);
        assert!(lease.renew().await.is_err());
        assert!(lease.ensure_valid().is_err());
        assert!(lease.release().await.is_err());
        assert_eq!(
            load_read_retention_root(&store, &path).await.unwrap(),
            Some(root())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn racing_retirement_and_renewal_have_one_conditional_winner_in_both_orders() {
        for retirement_first in [true, false] {
            let backend = Arc::new(FaultStore::default());
            let store: Arc<dyn ObjectStore> = backend.clone();
            let path = Path::from("repo/reader/claim");
            let mut lease = ReadRetentionLease::create(store.clone(), path.clone(), root())
                .await
                .unwrap();
            let expired = store.head(&path).await.unwrap().last_modified.timestamp()
                + READ_RETENTION_TTL.as_secs() as i64;
            backend.mode.store(1, std::sync::atomic::Ordering::SeqCst);
            if retirement_first {
                let mut retire = Box::pin(retained_read_root(&store, &path, expired));
                tokio::select! {
                    result = &mut retire => panic!("retirement escaped its gate: {result:?}"),
                    () = backend.entered.notified() => {},
                }
                lease.renew().await.unwrap();
                backend.resume.notify_one();
                assert!(retire.await.is_err());
                lease.ensure_valid().unwrap();
            } else {
                let mut renewal = Box::pin(lease.renew());
                tokio::select! {
                    result = &mut renewal => panic!("renewal escaped its gate: {result:?}"),
                    () = backend.entered.notified() => {},
                }
                retained_read_root(&store, &path, expired).await.unwrap();
                backend.resume.notify_one();
                assert!(renewal.await.is_err());
                assert!(lease.ensure_valid().is_err());
            }
            assert_eq!(
                load_read_retention_root(&store, &path).await.unwrap(),
                Some(root())
            );
        }
    }
}
