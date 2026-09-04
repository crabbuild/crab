//! Storage-backed short-TTL lease for serializing repository mutations.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{CoordinationError, Result};

/// Default TTL for a push lock: five minutes.
pub const DEFAULT_PUSH_LOCK_TTL: Duration = Duration::from_secs(300);

/// Internal resource serializing Git object-locator publication.
pub const GIT_OBJECT_LOCATOR_RESOURCE: &str = "git-object-locator";
/// Internal resource electing one long-lived Git generation repair owner.
pub const GIT_GENERATION_OWNER_RESOURCE: &str = "git-generation-owner";
/// Internal resource serializing unified manifest publication.
pub const GIT_MANIFEST_RESOURCE: &str = "git-manifest";
/// Internal resource used when a push has no destination ref.
pub const BATCH_RESOURCE: &str = "batch";
/// Internal resource serializing history recovery without existing refs.
pub const HISTORY_RECOVERY_RESOURCE: &str = "history-recovery";
/// Internal resource serializing destructive repository maintenance.
pub const REPOSITORY_MAINTENANCE_RESOURCE: &str = "repository-maintenance";

const COORDINATION_RETRY_MAX_ATTEMPTS: u32 = 5;
const COORDINATION_RETRY_BASE_MILLIS: u64 = 100;
const COORDINATION_RETRY_CAP_MILLIS: u64 = 10_000;

/// JSON payload stored in a push-lock object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushLockPayload {
    /// Unique holder identity for one push attempt.
    pub holder: String,
    /// Client-estimated Unix expiry used for diagnostics; zero means released.
    pub expires_at: u64,
    /// Lease duration measured from the backend-authored object modification time.
    #[serde(default)]
    pub lease_secs: u64,
}

impl PushLockPayload {
    /// Creates a live push-lock payload for `holder`.
    #[must_use]
    pub fn new(holder: impl Into<String>, expires_at: u64, lease_secs: u64) -> Self {
        Self {
            holder: holder.into(),
            expires_at,
            lease_secs,
        }
    }

    /// Creates the released tombstone payload for `holder`.
    #[must_use]
    pub fn released(holder: impl Into<String>) -> Self {
        Self {
            holder: holder.into(),
            expires_at: 0,
            lease_secs: 0,
        }
    }

    /// Returns true when the lock has been explicitly released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.expires_at == 0
    }

    /// Returns true when the lock has expired at `now_unix`.
    #[must_use]
    pub fn is_expired_at(&self, now_unix: u64) -> bool {
        self.expires_at != 0 && self.expires_at <= now_unix
    }
}

/// Canonical object key for a push lock protecting a full Git ref.
///
/// `ref_name` must begin with exactly one `refs/` prefix.
pub fn push_lock_path(prefix: &str, ref_name: &str) -> Result<String> {
    validate_repo_prefix(prefix)?;
    validate_full_ref(ref_name)?;
    Ok(format!("{prefix}/locks/{ref_name}/lock"))
}

/// Canonical object key for a lock protecting an internal repository resource.
pub fn internal_lock_path(prefix: &str, resource: &str) -> Result<String> {
    validate_repo_prefix(prefix)?;
    validate_resource(resource)?;
    Ok(format!("{prefix}/locks/internal/{resource}/lock"))
}

/// Object prefix containing all push locks for a repository.
pub fn push_locks_prefix(prefix: &str) -> Result<String> {
    validate_repo_prefix(prefix)?;
    Ok(format!("{prefix}/locks"))
}

fn validate_repo_prefix(prefix: &str) -> Result<()> {
    if valid_key(prefix) {
        return Ok(());
    }
    Err(invalid_lock_target(prefix, "repository prefix"))
}

fn validate_full_ref(ref_name: &str) -> Result<()> {
    if !ref_name.starts_with("refs/")
        || ref_name.starts_with("refs/refs/")
        || !valid_key(ref_name)
        || ref_name.contains("..")
        || ref_name.contains("@{")
        || ref_name.chars().any(|ch| {
            ch.is_control() || matches!(ch, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        || ref_name.split('/').any(|segment| {
            segment.starts_with('.') || segment.ends_with('.') || segment.ends_with(".lock")
        })
    {
        return Err(invalid_lock_target(ref_name, "full Git ref"));
    }
    Ok(())
}

fn validate_resource(resource: &str) -> Result<()> {
    let valid = !resource.is_empty()
        && !resource.starts_with('-')
        && !resource.ends_with('-')
        && !resource.contains("--")
        && resource
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        return Ok(());
    }
    Err(invalid_lock_target(resource, "internal resource"))
}

fn valid_key(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("//")
        && value
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn invalid_lock_target(value: &str, kind: &str) -> CoordinationError {
    CoordinationError::Configuration {
        key: value.to_owned(),
        origin: format!("invalid {kind} for object-store lock"),
    }
}

/// Short-TTL lease for serializing mutations under one repository key.
pub struct PushLock {
    store: Arc<dyn ObjectStore>,
    path: String,
    ttl: Duration,
    holder: String,
    etag: Option<UpdateVersion>,
    released: bool,
}

#[derive(Default)]
pub(crate) struct BackendClock {
    sample: Option<(i64, Instant)>,
}

impl BackendClock {
    pub(crate) async fn now(&mut self, store: &Arc<dyn ObjectStore>, anchor: &Path) -> Result<i64> {
        // Backend time anchors lease age; monotonic elapsed avoids trusting the
        // client's wall clock while a bounded acquisition owner is alive.
        if let Some((sample, sampled_at)) = self.sample {
            return Ok(sample.saturating_add(
                i64::try_from(sampled_at.elapsed().as_secs()).unwrap_or(i64::MAX),
            ));
        }
        let sample = backend_unix_time(store, anchor).await?;
        self.sample = Some((sample, Instant::now()));
        Ok(sample)
    }
}

/// Reusable state for repeated lock acquisitions against one object store.
///
/// The context remembers paths that already exist and one backend clock
/// sample. Reuse it only with the store supplied to [`Self::new`].
pub struct PushLockAcquireContext {
    store: Arc<dyn ObjectStore>,
    known_paths: HashSet<String>,
    backend_clock: BackendClock,
}

impl PushLockAcquireContext {
    /// Creates an acquisition context scoped to one object store.
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            known_paths: HashSet::new(),
            backend_clock: BackendClock::default(),
        }
    }

    /// Acquires the lease protecting a full Git ref.
    ///
    /// Returns an error when the ref is invalid, another holder owns the
    /// lease, or the object store cannot complete the conditional operation.
    pub async fn acquire_ref(
        &mut self,
        prefix: &str,
        ref_name: &str,
        ttl: Duration,
    ) -> Result<PushLock> {
        let path = push_lock_path(prefix, ref_name)?;
        self.acquire_path(ref_name, path, ttl).await
    }

    /// Acquires the lease protecting an internal repository resource.
    ///
    /// Returns an error when the resource is invalid, another holder owns the
    /// lease, or the object store cannot complete the conditional operation.
    pub async fn acquire_internal(
        &mut self,
        prefix: &str,
        resource: &str,
        ttl: Duration,
    ) -> Result<PushLock> {
        let path = internal_lock_path(prefix, resource)?;
        self.acquire_path(resource, path, ttl).await
    }

    /// Attempts to acquire an internal lease without waiting behind a holder.
    ///
    /// A live lease is conservatively reported as held without probing backend
    /// time. If the diagnostic expiry says that reclamation may be necessary,
    /// the normal backend-authored expiry check is used before returning.
    /// Transient object-store failures are retried within a bounded probe
    /// budget; a live holder is still reported immediately.
    pub async fn try_acquire_internal(
        &mut self,
        prefix: &str,
        resource: &str,
        ttl: Duration,
    ) -> Result<PushLock> {
        let path = internal_lock_path(prefix, resource)?;
        self.try_acquire_path(resource, path, ttl).await
    }

    async fn acquire_path(
        &mut self,
        target: &str,
        path: String,
        ttl: Duration,
    ) -> Result<PushLock> {
        let holder = generate_holder_id();
        let expires_at = unix_now() + ttl.as_secs();
        let known_existing = !self.known_paths.insert(path.clone());
        let etag = acquire_one(
            &self.store,
            &path,
            target,
            &holder,
            expires_at,
            ttl,
            known_existing,
            &mut self.backend_clock,
        )
        .await?;
        Ok(PushLock {
            store: Arc::clone(&self.store),
            path,
            ttl,
            holder,
            etag: Some(etag),
            released: false,
        })
    }

    async fn try_acquire_path(
        &mut self,
        target: &str,
        path: String,
        ttl: Duration,
    ) -> Result<PushLock> {
        let holder = generate_holder_id();
        let expires_at = unix_now() + ttl.as_secs();
        let body = serialize_payload(
            &path,
            &PushLockPayload::new(&holder, expires_at, ttl.as_secs()),
        )?;
        let mut attempt = 0_u32;
        loop {
            let known_existing = !self.known_paths.insert(path.clone());
            let result: Result<ContendedAcquire> = if known_existing {
                try_acquire_contended(
                    &self.store,
                    &Path::from(path.as_str()),
                    target,
                    body.clone(),
                    &mut self.backend_clock,
                )
                .await
            } else {
                match create_strict(&self.store, &Path::from(path.as_str()), body.clone()).await {
                    Ok(etag) => Ok(ContendedAcquire::Acquired(etag)),
                    Err(object_store::Error::AlreadyExists { .. })
                    | Err(object_store::Error::Precondition { .. }) => {
                        try_acquire_contended(
                            &self.store,
                            &Path::from(path.as_str()),
                            target,
                            body.clone(),
                            &mut self.backend_clock,
                        )
                        .await
                    }
                    Err(source) => Err(store_error(&path, source)),
                }
            };
            match result {
                Ok(ContendedAcquire::Acquired(etag)) => {
                    debug!(
                        lock_path = %path,
                        holder,
                        ttl_secs = ttl.as_secs(),
                        "push lock acquired without waiting"
                    );
                    return Ok(PushLock {
                        store: Arc::clone(&self.store),
                        path,
                        ttl,
                        holder,
                        etag: Some(etag),
                        released: false,
                    });
                }
                Ok(ContendedAcquire::Held {
                    holder,
                    expires_at_unix,
                }) => {
                    return Err(CoordinationError::PushLockHeld {
                        ref_name: target.to_owned(),
                        holder,
                        expires_at_unix,
                    });
                }
                Err(error)
                    if coordination_error_is_retryable(&error)
                        && attempt + 1 < COORDINATION_RETRY_MAX_ATTEMPTS =>
                {
                    let delay = coordination_retry_delay(&path, &holder, attempt);
                    debug!(
                        lock_path = %path,
                        retry_attempt = attempt + 1,
                        retry_limit = COORDINATION_RETRY_MAX_ATTEMPTS,
                        delay_ms = delay.as_millis(),
                        "retrying transient push-lock probe"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl PushLock {
    /// Acquires the lease protecting a full Git ref.
    pub async fn acquire_ref(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        ref_name: &str,
        ttl: Duration,
    ) -> Result<Self> {
        let path = push_lock_path(prefix, ref_name)?;
        Self::acquire_path(store, ref_name, path, ttl).await
    }

    /// Acquires the lease protecting an internal repository resource.
    pub async fn acquire_internal(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        resource: &str,
        ttl: Duration,
    ) -> Result<Self> {
        let path = internal_lock_path(prefix, resource)?;
        Self::acquire_path(store, resource, path, ttl).await
    }

    async fn acquire_path(
        store: &Arc<dyn ObjectStore>,
        target: &str,
        path: String,
        ttl: Duration,
    ) -> Result<Self> {
        PushLockAcquireContext::new(Arc::clone(store))
            .acquire_path(target, path, ttl)
            .await
    }

    /// Acquires the lease protecting a full Git ref with the default TTL.
    pub async fn acquire_ref_default(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        ref_name: &str,
    ) -> Result<Self> {
        Self::acquire_ref(store, prefix, ref_name, DEFAULT_PUSH_LOCK_TTL).await
    }

    /// Acquires the lease protecting an internal resource with the default TTL.
    pub async fn acquire_internal_default(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        resource: &str,
    ) -> Result<Self> {
        Self::acquire_internal(store, prefix, resource, DEFAULT_PUSH_LOCK_TTL).await
    }

    /// Releases the lease with a holder-checked compare-and-swap tombstone.
    pub async fn release(mut self) -> Result<()> {
        let result =
            release_with_known_etag(&self.store, &self.path, &self.holder, self.etag.clone()).await;
        self.released = true;
        result
    }

    /// Returns the object-store key containing this lease.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns this lease's unique holder identity.
    #[must_use]
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// Returns this lease's configured lifetime.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Extends the lease using a holder-checked compare-and-swap update.
    pub async fn renew(&mut self) -> Result<()> {
        self.etag = Some(
            renew_one(
                &self.store,
                &self.path,
                &self.holder,
                self.ttl,
                self.etag.clone(),
            )
            .await?,
        );
        Ok(())
    }

    /// Renews a non-released claim only while its stored holder matches.
    ///
    /// Background owners without the current CAS token use the same bounded
    /// renewal policy as [`Self::renew`]. A released or replaced claim fails.
    pub async fn renew_if_holder(
        store: &Arc<dyn ObjectStore>,
        path: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<()> {
        renew_one(store, path, holder, ttl, None).await?;
        Ok(())
    }

    /// Marks every expired lease beneath `prefix` released.
    pub async fn reclaim_expired(store: &Arc<dyn ObjectStore>, prefix: &str) -> Result<u64> {
        let locks_prefix = Path::from(push_locks_prefix(prefix)?);
        let objects = store
            .list(Some(&locks_prefix))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|source| store_error(locks_prefix.as_ref(), source))?;
        let now = backend_unix_time(store, &locks_prefix).await?;
        let mut reclaimed = 0;
        for meta in objects {
            let key = meta.location.as_ref();
            if !key.ends_with("/lock") {
                continue;
            }
            match expire_stale_lock_at(store, &meta.location, key, now).await {
                Ok(true) => reclaimed += 1,
                Ok(false)
                | Err(CoordinationError::ObjectStore {
                    source: object_store::Error::NotFound { .. },
                    ..
                })
                | Err(CoordinationError::MalformedPushLock { .. }) => {}
                Err(error) => warn!(lock = %key, %error, "failed to reclaim lock"),
            }
        }
        if reclaimed > 0 {
            info!(reclaimed, "expired push locks reclaimed");
        }
        Ok(reclaimed)
    }

    /// Repairs one expired lease by writing a released tombstone.
    pub async fn repair_expired(store: &Arc<dyn ObjectStore>, key: &str) -> Result<bool> {
        let object_path = Path::from(key);
        let now = backend_unix_time(store, &object_path).await?;
        expire_stale_lock_at(store, &Path::from(key), key, now).await
    }

    /// Release one ref lease only while its stored holder still matches.
    pub async fn release_ref_if_holder(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        ref_name: &str,
        holder: &str,
    ) -> Result<bool> {
        let path = push_lock_path(prefix, ref_name)?;
        release_if_holder_checked(store, &path, holder).await
    }

    /// Return whether a ref lease currently carries a non-released claim.
    ///
    /// This is an immediate handoff signal, not an acquisition decision: it
    /// deliberately avoids a backend-clock probe and may conservatively report
    /// an expired claimant until the next acquirer reclaims it.
    pub async fn ref_lease_is_claimed(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        ref_name: &str,
    ) -> Result<bool> {
        let path = push_lock_path(prefix, ref_name)?;
        lease_is_claimed(store, &path).await
    }

    /// Return whether an internal lease is active by its diagnostic expiry.
    ///
    /// This one-read hint may disagree under client clock skew; callers may use
    /// it only to hand derived work to an owner, never to decide correctness.
    pub async fn internal_lease_is_active(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        resource: &str,
    ) -> Result<bool> {
        let path = internal_lock_path(prefix, resource)?;
        let object_path = Path::from(path.as_str());
        let (body, _) = match get_with_version(store, &object_path).await {
            Ok(lock) => lock,
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(source) => return Err(store_error(&path, source)),
        };
        deserialize_payload(&path, &body)
            .map(|payload| !payload.is_released() && !payload.is_expired_at(unix_now()))
    }

    /// Record that a contender observed `predecessor_holder` on a ref lease.
    ///
    /// One fixed object per ref is overwritten across handoffs. The marker is
    /// advisory and carries the predecessor identity so stale handoffs cannot
    /// suppress publication by a later owner.
    pub async fn announce_ref_successor(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        ref_name: &str,
        predecessor_holder: &str,
    ) -> Result<()> {
        if predecessor_holder.is_empty() {
            return Err(CoordinationError::Configuration {
                key: predecessor_holder.to_owned(),
                origin: "push successor predecessor holder must not be empty".to_owned(),
            });
        }
        let path = format!("{}.successor", push_lock_path(prefix, ref_name)?);
        store
            .put(
                &Path::from(path.as_str()),
                Bytes::copy_from_slice(predecessor_holder.as_bytes()).into(),
            )
            .await
            .map(|_| ())
            .map_err(|source| store_error(&path, source))
    }

    /// Return whether a contender announced a handoff from this exact holder.
    pub async fn ref_successor_was_announced(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        ref_name: &str,
        predecessor_holder: &str,
    ) -> Result<bool> {
        let path = format!("{}.successor", push_lock_path(prefix, ref_name)?);
        let result = match store.get(&Path::from(path.as_str())).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(source) => return Err(store_error(&path, source)),
        };
        result
            .bytes()
            .await
            .map(|body| body.as_ref() == predecessor_holder.as_bytes())
            .map_err(|source| store_error(&path, source))
    }
}

impl Drop for PushLock {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let store = self.store.clone();
        let path = self.path.clone();
        let holder = self.holder.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = release_if_holder(&store, &path, &holder).await {
                    warn!(lock_path = %path, %error, "failed to release push lock on drop");
                }
            });
        }
    }
}

async fn acquire_one(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    target: &str,
    holder: &str,
    expires_at: u64,
    ttl: Duration,
    known_existing: bool,
    backend_clock: &mut BackendClock,
) -> Result<UpdateVersion> {
    let object_path = Path::from(path);
    let body = serialize_payload(
        path,
        &PushLockPayload::new(holder, expires_at, ttl.as_secs()),
    )?;
    let created = if known_existing {
        None
    } else {
        match create_strict(store, &object_path, body.clone()).await {
            Ok(etag) => Some(etag),
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => None,
            Err(source) => return Err(store_error(path, source)),
        }
    };
    let etag = match created {
        Some(etag) => etag,
        None => match acquire_contended(store, &object_path, target, body, backend_clock).await? {
            ContendedAcquire::Acquired(etag) => etag,
            ContendedAcquire::Held {
                holder,
                expires_at_unix,
            } => {
                return Err(CoordinationError::PushLockHeld {
                    ref_name: target.to_owned(),
                    holder,
                    expires_at_unix,
                });
            }
        },
    };
    debug!(lock_path = %path, holder, ttl_secs = ttl.as_secs(), "push lock acquired");
    Ok(etag)
}

async fn renew_one(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
    ttl: Duration,
    etag: Option<UpdateVersion>,
) -> Result<UpdateVersion> {
    // Renewal retries must finish before the next lease window expires. A
    // late retry could let a slow owner outlive a legitimate reclamation.
    let deadline = Instant::now() + (ttl / 3).max(Duration::from_secs(1));
    retry_coordination_operation_until(path, holder, deadline, || {
        renew_one_once(store, path, holder, ttl, etag.clone())
    })
    .await
}

async fn renew_one_once(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
    ttl: Duration,
    etag: Option<UpdateVersion>,
) -> Result<UpdateVersion> {
    if let Some(etag) = etag.filter(has_cas_token) {
        let object_path = Path::from(path);
        let body = serialize_payload(
            path,
            &PushLockPayload::new(holder, unix_now() + ttl.as_secs(), ttl.as_secs()),
        )?;
        return match update(store, &object_path, body, etag).await {
            Ok(etag) => Ok(etag),
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {
                renew_one_after_cas_conflict(store, path, holder, ttl).await
            }
            Err(source) => Err(store_error(path, source)),
        };
    }

    renew_one_after_cas_conflict(store, path, holder, ttl).await
}

async fn renew_one_after_cas_conflict(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
    ttl: Duration,
) -> Result<UpdateVersion> {
    let object_path = Path::from(path);
    let (body, etag) = get_with_version(store, &object_path)
        .await
        .map_err(|source| store_error(path, source))?;
    let payload = deserialize_payload(path, &body)?;
    if payload.holder != holder {
        return Err(CoordinationError::PushLockHeld {
            ref_name: path.to_owned(),
            holder: payload.holder,
            expires_at_unix: Some(payload.expires_at),
        });
    }
    // A tombstone keeps the previous holder for diagnostics, not authority.
    // Successor handoff or repair may revoke a claim while its heartbeat is
    // still running; refreshing that payload would resurrect the old owner.
    if payload.is_released() {
        return Err(CoordinationError::CasConflict {
            path: path.to_owned(),
            expected_etag: etag.e_tag,
        });
    }
    let body = serialize_payload(
        path,
        &PushLockPayload::new(holder, unix_now() + ttl.as_secs(), ttl.as_secs()),
    )?;
    update(store, &object_path, body, etag)
        .await
        .map_err(|source| store_error(path, source))
}

enum ContendedAcquire {
    Acquired(UpdateVersion),
    Held {
        holder: String,
        expires_at_unix: Option<u64>,
    },
}

fn authoritative_expiry(payload: &PushLockPayload, last_modified: i64) -> Option<u64> {
    if payload.is_released() {
        return None;
    }
    // Locks written before backend-authored leases shipped have no
    // `lease_secs`. Their client expiry remains the migration boundary;
    // otherwise a crashed legacy writer would hold the ref forever.
    if payload.lease_secs == 0 {
        return Some(payload.expires_at);
    }
    u64::try_from(last_modified)
        .ok()
        .map(|modified| modified.saturating_add(payload.lease_secs))
}

fn lease_expired(payload: &PushLockPayload, last_modified: i64, backend_now: i64) -> bool {
    authoritative_expiry(payload, last_modified)
        .and_then(|expires| i64::try_from(expires).ok())
        .is_some_and(|expires| expires <= backend_now)
}

pub(crate) async fn backend_unix_time(store: &Arc<dyn ObjectStore>, anchor: &Path) -> Result<i64> {
    // One reusable clock object supplies backend time without leaking a key
    // for every blocked contender when cleanup fails.
    let probe = Path::from(format!("{}/clock", anchor.as_ref()));
    store
        .put(&probe, Bytes::new().into())
        .await
        .map_err(|source| store_error(probe.as_ref(), source))?;
    store
        .head(&probe)
        .await
        .map(|metadata| metadata.last_modified.timestamp())
        .map_err(|source| store_error(probe.as_ref(), source))
}

async fn acquire_contended(
    store: &Arc<dyn ObjectStore>,
    object_path: &Path,
    ref_name: &str,
    body: Bytes,
    backend_clock: &mut BackendClock,
) -> Result<ContendedAcquire> {
    let (existing_body, reclaim_etag, last_modified) =
        match get_with_version_and_modified(store, object_path).await {
            Ok(existing) => existing,
            Err(object_store::Error::NotFound { .. }) => {
                return match create_strict(store, object_path, body).await {
                    Ok(etag) => Ok(ContendedAcquire::Acquired(etag)),
                    Err(object_store::Error::AlreadyExists { .. })
                    | Err(object_store::Error::Precondition { .. }) => {
                        let (holder, expires_at_unix) =
                            lock_holder_snapshot(store, object_path).await;
                        Ok(ContendedAcquire::Held {
                            holder,
                            expires_at_unix,
                        })
                    }
                    Err(source) => Err(store_error(object_path.as_ref(), source)),
                };
            }
            Err(source) => return Err(store_error(object_path.as_ref(), source)),
        };

    let existing = match serde_json::from_slice::<PushLockPayload>(&existing_body) {
        Ok(existing) => existing,
        Err(_) => {
            return Ok(ContendedAcquire::Held {
                holder: String::new(),
                expires_at_unix: None,
            });
        }
    };
    if !existing.is_released() {
        let now = backend_clock.now(store, object_path).await?;
        if !lease_expired(&existing, last_modified, now) {
            let expires_at_unix = authoritative_expiry(&existing, last_modified);
            return Ok(ContendedAcquire::Held {
                holder: existing.holder,
                expires_at_unix,
            });
        }
    }

    // An explicit release is already authoritative. Sampling backend time is
    // only required before reclaiming a live lease whose age could be skewed.
    debug!(ref_name, prior_holder = %existing.holder, "reclaiming available push lock");
    match update(store, object_path, body, reclaim_etag).await {
        Ok(etag) => Ok(ContendedAcquire::Acquired(etag)),
        Err(object_store::Error::AlreadyExists { .. })
        | Err(object_store::Error::Precondition { .. }) => {
            let (holder, expires_at_unix) = lock_holder_snapshot(store, object_path).await;
            Ok(ContendedAcquire::Held {
                holder,
                expires_at_unix,
            })
        }
        Err(source) => Err(store_error(object_path.as_ref(), source)),
    }
}

async fn try_acquire_contended(
    store: &Arc<dyn ObjectStore>,
    object_path: &Path,
    ref_name: &str,
    body: Bytes,
    backend_clock: &mut BackendClock,
) -> Result<ContendedAcquire> {
    let (existing_body, _, last_modified) =
        match get_with_version_and_modified(store, object_path).await {
            Ok(existing) => existing,
            Err(object_store::Error::NotFound { .. }) => {
                return acquire_contended(store, object_path, ref_name, body, backend_clock).await;
            }
            Err(source) => return Err(store_error(object_path.as_ref(), source)),
        };

    let existing = match serde_json::from_slice::<PushLockPayload>(&existing_body) {
        Ok(existing) => existing,
        Err(_) => {
            return Ok(ContendedAcquire::Held {
                holder: String::new(),
                expires_at_unix: None,
            });
        }
    };
    if !existing.is_released() && !existing.is_expired_at(unix_now()) {
        let expires_at_unix = authoritative_expiry(&existing, last_modified);
        return Ok(ContendedAcquire::Held {
            holder: existing.holder,
            expires_at_unix,
        });
    }

    // A diagnostic expiry is only a hint. Reuse the normal acquisition path so
    // reclaim still requires an authoritative backend clock and CAS.
    acquire_contended(store, object_path, ref_name, body, backend_clock).await
}

fn has_cas_token(etag: &UpdateVersion) -> bool {
    etag.e_tag.is_some() || etag.version.is_some()
}

async fn lock_holder_snapshot(
    store: &Arc<dyn ObjectStore>,
    object_path: &Path,
) -> (String, Option<u64>) {
    match get_with_version_and_modified(store, object_path).await {
        Ok((body, _, last_modified)) => match serde_json::from_slice::<PushLockPayload>(&body) {
            Ok(payload) => {
                let expires_at_unix = authoritative_expiry(&payload, last_modified);
                (payload.holder, expires_at_unix)
            }
            Err(_) => (String::new(), None),
        },
        Err(_) => (String::new(), None),
    }
}

async fn lease_is_claimed(store: &Arc<dyn ObjectStore>, path: &str) -> Result<bool> {
    let object_path = Path::from(path);
    let (body, _) = match get_with_version(store, &object_path).await {
        Ok(lock) => lock,
        Err(object_store::Error::NotFound { .. }) => return Ok(false),
        Err(source) => return Err(store_error(path, source)),
    };
    deserialize_payload(path, &body).map(|payload| !payload.is_released())
}

pub(crate) async fn release_with_known_etag(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
    etag: Option<UpdateVersion>,
) -> Result<()> {
    retry_coordination_operation(path, holder, || {
        release_with_known_etag_once(store, path, holder, etag.clone())
    })
    .await
}

async fn release_with_known_etag_once(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
    etag: Option<UpdateVersion>,
) -> Result<()> {
    if let Some(etag) = etag
        && has_cas_token(&etag)
    {
        let object_path = Path::from(path);
        let body = serialize_payload(path, &PushLockPayload::released(holder))?;
        return match update(store, &object_path, body, etag).await {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {
                release_if_holder(store, path, holder).await
            }
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(source) => Err(store_error(path, source)),
        };
    }
    release_if_holder(store, path, holder).await
}

pub(crate) async fn release_if_holder(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
) -> Result<()> {
    retry_coordination_operation(path, holder, || {
        release_if_holder_checked(store, path, holder)
    })
    .await
    .map(|_| ())
}

async fn retry_coordination_operation<T, F, Fut>(
    path: &str,
    holder: &str,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    retry_coordination_operation_with_deadline(path, holder, None, operation).await
}

async fn retry_coordination_operation_until<T, F, Fut>(
    path: &str,
    holder: &str,
    deadline: Instant,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    retry_coordination_operation_with_deadline(path, holder, Some(deadline), operation).await
}

async fn retry_coordination_operation_with_deadline<T, F, Fut>(
    path: &str,
    holder: &str,
    deadline: Option<Instant>,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0_u32;
    let mut last_error = None;
    loop {
        let result = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(coordination_retry_deadline(path, last_error.take()));
                }
                match tokio::time::timeout(remaining, operation()).await {
                    Ok(result) => result,
                    Err(_) => return Err(coordination_retry_deadline(path, last_error.take())),
                }
            }
            None => operation().await,
        };
        match result {
            Ok(value) => return Ok(value),
            Err(error)
                if coordination_error_is_retryable(&error)
                    && attempt + 1 < COORDINATION_RETRY_MAX_ATTEMPTS =>
            {
                let delay = coordination_retry_delay(path, holder, attempt);
                debug!(
                    lock_path = %path,
                    retry_attempt = attempt + 1,
                    retry_limit = COORDINATION_RETRY_MAX_ATTEMPTS,
                    delay_ms = delay.as_millis(),
                    "retrying transient coordination operation"
                );
                if let Some(deadline) = deadline {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if delay >= remaining {
                        return Err(coordination_retry_deadline(path, Some(error)));
                    }
                }
                last_error = Some(error);
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn coordination_retry_deadline(path: &str, source: Option<CoordinationError>) -> CoordinationError {
    let source = source.unwrap_or_else(|| CoordinationError::Configuration {
        key: path.to_owned(),
        origin: "coordination operation timed out before completion".to_owned(),
    });
    CoordinationError::RetryDeadline {
        path: path.to_owned(),
        source: Box::new(source),
    }
}

fn coordination_error_is_retryable(error: &CoordinationError) -> bool {
    matches!(
        error,
        CoordinationError::ObjectStore {
            source: object_store::Error::Generic { .. },
            ..
        }
    )
}

fn coordination_retry_delay(path: &str, holder: &str, attempt: u32) -> Duration {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(holder.as_bytes());
    hasher.update(b"\0");
    hasher.update(&attempt.to_le_bytes());
    let digest = hasher.finalize();
    let mut random = [0_u8; 8];
    random.copy_from_slice(&digest.as_bytes()[..8]);
    let multiplier = 1_u64.checked_shl(attempt.min(6)).unwrap_or(u64::MAX);
    let bound = COORDINATION_RETRY_BASE_MILLIS
        .saturating_mul(multiplier)
        .min(COORDINATION_RETRY_CAP_MILLIS);
    let delay = u64::from_le_bytes(random) % bound.saturating_add(1);
    Duration::from_millis(delay)
}

async fn release_if_holder_checked(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
) -> Result<bool> {
    let object_path = Path::from(path);
    let (body, etag) = match get_with_version(store, &object_path).await {
        Ok(lock) => lock,
        Err(object_store::Error::NotFound { .. }) => return Ok(true),
        Err(source) => return Err(store_error(path, source)),
    };
    let payload = deserialize_payload(path, &body)?;
    if payload.holder != holder {
        return Ok(false);
    }
    let body = serialize_payload(path, &PushLockPayload::released(holder))?;
    match update(store, &object_path, body, etag).await {
        Ok(_) | Err(object_store::Error::NotFound { .. }) => Ok(true),
        Err(object_store::Error::AlreadyExists { .. })
        | Err(object_store::Error::Precondition { .. }) => Ok(false),
        Err(source) => Err(store_error(path, source)),
    }
}

async fn expire_stale_lock_at(
    store: &Arc<dyn ObjectStore>,
    object_path: &Path,
    path: &str,
    now: i64,
) -> Result<bool> {
    let (body, etag, last_modified) = match get_with_version_and_modified(store, object_path).await
    {
        Ok(lock) => lock,
        Err(object_store::Error::NotFound { .. }) => return Ok(true),
        Err(source) => return Err(store_error(path, source)),
    };
    let payload = deserialize_payload(path, &body)?;
    if !lease_expired(&payload, last_modified, now) {
        return Ok(false);
    }
    let body = serialize_payload(path, &PushLockPayload::released(&payload.holder))?;
    match update(store, object_path, body, etag).await {
        Ok(_) | Err(object_store::Error::NotFound { .. }) => Ok(true),
        Err(object_store::Error::AlreadyExists { .. })
        | Err(object_store::Error::Precondition { .. }) => Ok(false),
        Err(source) => Err(store_error(path, source)),
    }
}

pub(crate) async fn create_strict(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
    body: Bytes,
) -> object_store::Result<UpdateVersion> {
    store
        .put_opts(path, body.into(), PutOptions::from(PutMode::Create))
        .await
        .map(Into::into)
}

pub(crate) async fn get_with_version(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
) -> object_store::Result<(Bytes, UpdateVersion)> {
    get_with_version_and_modified(store, path)
        .await
        .map(|(body, version, _)| (body, version))
}

pub(crate) async fn get_with_version_and_modified(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
) -> object_store::Result<(Bytes, UpdateVersion, i64)> {
    let result = store.get(path).await?;
    let version = UpdateVersion {
        e_tag: result.meta.e_tag.clone(),
        version: result.meta.version.clone(),
    };
    let last_modified = result.meta.last_modified.timestamp();
    Ok((result.bytes().await?, version, last_modified))
}

pub(crate) async fn update(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
    body: Bytes,
    version: UpdateVersion,
) -> object_store::Result<UpdateVersion> {
    store
        .put_opts(
            path,
            body.into(),
            PutOptions::from(PutMode::Update(version)),
        )
        .await
        .map(Into::into)
}

pub(crate) fn store_error(path: &str, source: object_store::Error) -> CoordinationError {
    CoordinationError::ObjectStore {
        path: path.to_owned(),
        source,
    }
}

pub(crate) fn serialize_payload(path: &str, payload: &PushLockPayload) -> Result<Bytes> {
    serde_json::to_vec(payload)
        .map(Bytes::from)
        .map_err(|source| CoordinationError::Serialize {
            key: path.to_owned(),
            context: "push lock payload",
            source,
        })
}

pub(crate) fn deserialize_payload(path: &str, body: &[u8]) -> Result<PushLockPayload> {
    serde_json::from_slice(body).map_err(|source| CoordinationError::MalformedPushLock {
        path: path.to_owned(),
        source,
    })
}

pub(crate) fn generate_holder_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static HOLDER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let sequence = HOLDER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("pid-{}-{nanos}-{sequence}", std::process::id())
}

/// Returns the current Unix timestamp in seconds.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use futures_util::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn memory_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    #[derive(Debug)]
    struct RequestCountingStore {
        inner: Arc<InMemory>,
        requests: Arc<AtomicUsize>,
        fail_next_create: AtomicBool,
        fail_next_get: AtomicBool,
        fail_next_update: AtomicBool,
    }

    impl RequestCountingStore {
        fn fail_next_create(&self) {
            self.fail_next_create.store(true, Ordering::Release);
        }

        fn fail_next_get(&self) {
            self.fail_next_get.store(true, Ordering::Release);
        }

        fn fail_next_update(&self) {
            self.fail_next_update.store(true, Ordering::Release);
        }
    }

    impl std::fmt::Display for RequestCountingStore {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("request-counting-store")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for RequestCountingStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            if matches!(&options.mode, PutMode::Create)
                && self.fail_next_create.swap(false, Ordering::AcqRel)
            {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "service unavailable: slow down".into(),
                });
            }
            if matches!(&options.mode, PutMode::Update(_))
                && self.fail_next_update.swap(false, Ordering::AcqRel)
            {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "service unavailable: slow down".into(),
                });
            }
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            if self.fail_next_get.swap(false, Ordering::AcqRel) {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "service unavailable: slow down".into(),
                });
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[tokio::test]
    async fn locator_publishers_serialize_on_reserved_lock() {
        let store = memory_store();
        let first = PushLock::acquire_internal(
            &store,
            "org/repo",
            GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        let blocked = PushLock::acquire_internal(
            &store,
            "org/repo",
            GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await;
        assert!(matches!(
            blocked,
            Err(CoordinationError::PushLockHeld { .. })
        ));

        first.release().await.unwrap();
        PushLock::acquire_internal(
            &store,
            "org/repo",
            GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lock_release_retries_transient_update() {
        let inner = Arc::new(InMemory::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let metered = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let store: Arc<dyn ObjectStore> = metered.clone();
        let lock = PushLock::acquire_internal(
            &store,
            "org/repo",
            GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let path = Path::from(lock.path());
        metered.fail_next_update();

        lock.release().await.unwrap();

        let (body, _) = get_with_version(&store, &path).await.unwrap();
        let payload: PushLockPayload = serde_json::from_slice(&body).unwrap();
        assert!(payload.is_released());
        assert!(requests.load(Ordering::Relaxed) >= 3);
    }

    #[tokio::test]
    async fn lock_renew_retries_transient_update() {
        let inner = Arc::new(InMemory::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let metered = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let store: Arc<dyn ObjectStore> = metered.clone();
        let mut lock = PushLock::acquire_internal(
            &store,
            "org/repo",
            GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        metered.fail_next_update();

        lock.renew().await.unwrap();
        lock.release().await.unwrap();

        assert!(requests.load(Ordering::Relaxed) >= 4);
    }

    #[tokio::test]
    async fn lock_renew_reuses_known_version_without_a_read() {
        let inner = Arc::new(InMemory::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let metered = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let store: Arc<dyn ObjectStore> = metered.clone();
        let mut lock = PushLock::acquire_internal(
            &store,
            "org/repo",
            GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        metered.fail_next_get();

        lock.renew().await.unwrap();

        assert!(metered.fail_next_get.swap(false, Ordering::AcqRel));
        lock.release().await.unwrap();
        assert!(requests.load(Ordering::Relaxed) >= 3);
    }

    #[tokio::test]
    async fn renewal_cannot_resurrect_an_explicitly_released_claim() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let prefix = "org/repo";
        let mut lock =
            PushLock::acquire_ref(&store, prefix, "refs/heads/main", Duration::from_secs(60))
                .await
                .unwrap();
        PushLock::release_ref_if_holder(&store, prefix, "refs/heads/main", lock.holder())
            .await
            .unwrap();
        let path = Path::from(lock.path());
        let before = get_with_version(&store, &path).await.unwrap();
        let cached_result = lock.renew().await;
        let uncached_result =
            PushLock::renew_if_holder(&store, lock.path(), lock.holder(), lock.ttl()).await;
        let after = get_with_version(&store, &path).await.unwrap();
        lock.release().await.unwrap();

        assert!(
            cached_result.is_err() && uncached_result.is_err(),
            "release revokes the old holder's renewal authority"
        );
        assert_eq!(
            after, before,
            "a rejected renewal must not rewrite the tombstone"
        );
    }

    #[tokio::test]
    async fn lock_renewal_retry_budget_cancels_slow_operation() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let result: Result<()> = retry_coordination_operation_until(
            "org/repo/locks/internal/git-object-locator/lock",
            "holder",
            deadline,
            || async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(CoordinationError::Configuration {
                    key: "test".to_owned(),
                    origin: "operation should have timed out".to_owned(),
                })
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(CoordinationError::RetryDeadline { .. })
        ));
    }

    #[tokio::test]
    async fn lock_renewal_deadline_preserves_last_transient_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        let deadline = Instant::now() + Duration::from_millis(20);
        let result: Result<()> = retry_coordination_operation_until(
            "org/repo/locks/internal/git-object-locator/lock",
            "holder",
            deadline,
            move || {
                let calls = Arc::clone(&calls_ref);
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(CoordinationError::ObjectStore {
                            path: "org/repo/lock".to_owned(),
                            source: object_store::Error::Generic {
                                store: "test",
                                source: "connection reset".into(),
                            },
                        })
                    } else {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Err(CoordinationError::Configuration {
                            key: "test".to_owned(),
                            origin: "operation should have timed out".to_owned(),
                        })
                    }
                }
            },
        )
        .await;

        let Err(CoordinationError::RetryDeadline { source, .. }) = result else {
            panic!("expected retry deadline with the last transient source");
        };
        assert!(matches!(
            *source,
            CoordinationError::ObjectStore {
                source: object_store::Error::Generic { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stale_owner_release_does_not_clear_reacquired_lock() {
        let store = memory_store();
        let first = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        let stale_holder = first.holder().to_owned();
        let path = first.path().to_owned();
        release_if_holder(&store, &path, &stale_holder)
            .await
            .unwrap();

        let second = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        first.release().await.unwrap();

        let (body, _) = get_with_version(&store, &Path::from(path)).await.unwrap();
        let payload: PushLockPayload = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.holder, second.holder());
        assert!(!payload.is_released());
        second.release().await.unwrap();
    }

    #[tokio::test]
    async fn checked_ref_release_requires_current_holder() {
        let store = memory_store();
        let lock = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();

        assert!(
            !PushLock::release_ref_if_holder(
                &store,
                "org/repo",
                "refs/heads/main",
                "different-holder",
            )
            .await
            .unwrap()
        );
        assert!(
            PushLock::release_ref_if_holder(&store, "org/repo", "refs/heads/main", lock.holder(),)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn ref_claim_probe_tracks_acquire_and_release_without_creating_a_lock() {
        let store = memory_store();

        assert!(
            !PushLock::ref_lease_is_claimed(&store, "org/repo", "refs/heads/main")
                .await
                .unwrap()
        );
        let lock = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        assert!(
            PushLock::ref_lease_is_claimed(&store, "org/repo", "refs/heads/main")
                .await
                .unwrap()
        );

        lock.release().await.unwrap();
        assert!(
            !PushLock::ref_lease_is_claimed(&store, "org/repo", "refs/heads/main")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn internal_active_probe_tracks_generation_owner_lease() {
        let store = memory_store();

        assert!(
            !PushLock::internal_lease_is_active(&store, "org/repo", GIT_GENERATION_OWNER_RESOURCE,)
                .await
                .unwrap()
        );
        let lock =
            PushLock::acquire_internal_default(&store, "org/repo", GIT_GENERATION_OWNER_RESOURCE)
                .await
                .unwrap();
        assert!(
            PushLock::internal_lease_is_active(&store, "org/repo", GIT_GENERATION_OWNER_RESOURCE,)
                .await
                .unwrap()
        );

        lock.release().await.unwrap();
        assert!(
            !PushLock::internal_lease_is_active(&store, "org/repo", GIT_GENERATION_OWNER_RESOURCE,)
                .await
                .unwrap()
        );

        let path = internal_lock_path("org/repo", GIT_GENERATION_OWNER_RESOURCE).unwrap();
        let expired = serialize_payload(
            &path,
            &PushLockPayload::new("expired-owner", unix_now().saturating_sub(1), 60),
        )
        .unwrap();
        store.put(&Path::from(path), expired.into()).await.unwrap();
        assert!(
            !PushLock::internal_lease_is_active(&store, "org/repo", GIT_GENERATION_OWNER_RESOURCE,)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn try_internal_contention_skips_backend_clock_for_live_holder() {
        let inner = Arc::new(InMemory::new());
        let setup_store: Arc<dyn ObjectStore> = inner.clone();
        let blocker = PushLock::acquire_internal(
            &setup_store,
            "org/repo",
            GIT_MANIFEST_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let metered_store: Arc<dyn ObjectStore> = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let mut context = PushLockAcquireContext::new(metered_store);

        let blocked = context
            .try_acquire_internal("org/repo", GIT_MANIFEST_RESOURCE, Duration::from_secs(60))
            .await;
        assert!(matches!(
            blocked,
            Err(CoordinationError::PushLockHeld { .. })
        ));
        assert_eq!(requests.load(Ordering::Relaxed), 2);

        let clock_path = Path::from(format!("{}/clock", blocker.path()));
        assert!(matches!(
            setup_store.head(&clock_path).await,
            Err(object_store::Error::NotFound { .. })
        ));
        blocker.release().await.unwrap();
    }

    #[tokio::test]
    async fn try_internal_contention_retries_transient_probe() {
        let inner = Arc::new(InMemory::new());
        let setup_store: Arc<dyn ObjectStore> = inner.clone();
        let blocker = PushLock::acquire_internal(
            &setup_store,
            "org/repo",
            GIT_MANIFEST_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let metered = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let metered_store: Arc<dyn ObjectStore> = metered.clone();
        let mut context = PushLockAcquireContext::new(metered_store);
        metered.fail_next_get();

        let blocked = context
            .try_acquire_internal("org/repo", GIT_MANIFEST_RESOURCE, Duration::from_secs(60))
            .await;

        assert!(matches!(
            blocked,
            Err(CoordinationError::PushLockHeld { .. })
        ));
        assert!(requests.load(Ordering::Relaxed) >= 3);
        blocker.release().await.unwrap();
    }

    #[tokio::test]
    async fn try_internal_contention_retries_transient_create_probe() {
        let inner = Arc::new(InMemory::new());
        let setup_store: Arc<dyn ObjectStore> = inner.clone();
        let blocker = PushLock::acquire_internal(
            &setup_store,
            "org/repo",
            GIT_MANIFEST_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let metered = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let metered_store: Arc<dyn ObjectStore> = metered.clone();
        let mut context = PushLockAcquireContext::new(metered_store);
        metered.fail_next_create();

        let blocked = context
            .try_acquire_internal("org/repo", GIT_MANIFEST_RESOURCE, Duration::from_secs(60))
            .await;

        assert!(matches!(
            blocked,
            Err(CoordinationError::PushLockHeld { .. })
        ));
        assert!(requests.load(Ordering::Relaxed) >= 2);
        blocker.release().await.unwrap();
    }

    #[tokio::test]
    async fn ref_successor_marker_is_scoped_to_the_observed_predecessor() {
        let store = memory_store();
        PushLock::announce_ref_successor(&store, "org/repo", "refs/heads/main", "holder-a")
            .await
            .unwrap();

        assert!(
            PushLock::ref_successor_was_announced(
                &store,
                "org/repo",
                "refs/heads/main",
                "holder-a",
            )
            .await
            .unwrap()
        );
        assert!(
            !PushLock::ref_successor_was_announced(
                &store,
                "org/repo",
                "refs/heads/main",
                "holder-b",
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn released_tombstone_reacquire_skips_backend_clock_probe() {
        let store = memory_store();
        let first = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        let lock_path = first.path().to_owned();
        first.release().await.unwrap();

        let second = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();

        let clock_path = Path::from(format!("{lock_path}/clock"));
        assert!(matches!(
            store.head(&clock_path).await,
            Err(object_store::Error::NotFound { .. })
        ));
        second.release().await.unwrap();
    }

    #[tokio::test]
    async fn backend_age_prevents_fast_client_from_reclaiming_live_lock() {
        let store = memory_store();
        let path = push_lock_path("org/repo", "refs/heads/main").unwrap();
        let body = serde_json::to_vec(&PushLockPayload::new("live-holder", 1, 60)).unwrap();
        create_strict(&store, &Path::from(path.as_str()), Bytes::from(body))
            .await
            .unwrap();

        let contender = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main").await;

        assert!(matches!(
            contender,
            Err(CoordinationError::PushLockHeld { holder, .. }) if holder == "live-holder"
        ));
    }

    #[tokio::test]
    async fn repeated_contender_reuses_existing_path_and_backend_clock() {
        let inner = Arc::new(InMemory::new());
        let setup_store: Arc<dyn ObjectStore> = inner.clone();
        let blocker = PushLock::acquire_ref_default(&setup_store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        let lock_path = Path::from(blocker.path());
        let requests = Arc::new(AtomicUsize::new(0));
        let metered_store: Arc<dyn ObjectStore> = Arc::new(RequestCountingStore {
            inner,
            requests: Arc::clone(&requests),
            fail_next_create: AtomicBool::new(false),
            fail_next_get: AtomicBool::new(false),
            fail_next_update: AtomicBool::new(false),
        });
        let mut context = PushLockAcquireContext::new(metered_store);

        for _ in 0..2 {
            assert!(matches!(
                context
                    .acquire_ref("org/repo", "refs/heads/main", Duration::from_secs(60),)
                    .await,
                Err(CoordinationError::PushLockHeld { .. })
            ));
        }

        assert_eq!(requests.load(Ordering::Relaxed), 5);
        blocker.release().await.unwrap();
        setup_store.delete(&lock_path).await.unwrap();
        context
            .acquire_ref("org/repo", "refs/heads/main", Duration::from_secs(60))
            .await
            .expect("known path may disappear between attempts")
            .release()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expired_lock_can_be_reacquired_and_repaired() {
        let store = memory_store();
        let path = push_lock_path("org/repo", "refs/heads/main").unwrap();
        let body = serde_json::to_vec(&PushLockPayload::new("dead-holder", 1, 1)).unwrap();
        create_strict(&store, &Path::from(path.as_str()), Bytes::from(body))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let lock = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        lock.release().await.unwrap();

        let repaired = PushLock::repair_expired(&store, &path).await.unwrap();
        assert!(!repaired, "released tombstones are not expired live leases");
    }

    #[tokio::test]
    async fn expired_legacy_payload_does_not_hold_ref_forever() {
        let store = memory_store();
        let path = push_lock_path("org/repo", "refs/heads/main").unwrap();
        create_strict(
            &store,
            &Path::from(path.as_str()),
            Bytes::from_static(br#"{"holder":"legacy-holder","expires_at":1}"#),
        )
        .await
        .unwrap();

        let lock = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();

        lock.release().await.unwrap();
    }

    #[tokio::test]
    async fn backend_clock_uses_one_reusable_object() {
        let store = memory_store();
        let anchor = Path::from("org/repo/locks/internal/push-admission/slots");

        backend_unix_time(&store, &anchor).await.unwrap();
        backend_unix_time(&store, &anchor).await.unwrap();

        let objects = store
            .list(Some(&anchor))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<object_store::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].location.as_ref(), format!("{anchor}/clock"));
    }

    #[tokio::test]
    async fn repair_marks_expired_live_lock_released() {
        let store = memory_store();
        let path = push_lock_path("org/repo", "refs/heads/main").unwrap();
        let body = serde_json::to_vec(&PushLockPayload::new("dead-holder", 1, 1)).unwrap();
        create_strict(&store, &Path::from(path.as_str()), Bytes::from(body))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;

        assert!(PushLock::repair_expired(&store, &path).await.unwrap());
        let (body, _) = get_with_version(&store, &Path::from(path.as_str()))
            .await
            .unwrap();
        let payload: PushLockPayload = serde_json::from_slice(&body).unwrap();
        assert!(payload.is_released());
    }

    #[test]
    fn payload_round_trips_json() {
        let payload = PushLockPayload::new("holder-a", 42, 30);

        let bytes = serde_json::to_vec(&payload).unwrap();
        let parsed: PushLockPayload = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(parsed, payload);
    }

    #[test]
    fn released_payload_uses_zero_expiry() {
        let payload = PushLockPayload::released("holder-a");

        assert_eq!(payload.holder, "holder-a");
        assert!(payload.is_released());
        assert!(!payload.is_expired_at(100));
    }

    #[test]
    fn expiry_ignores_released_tombstones() {
        let payload = PushLockPayload::new("holder-a", 99, 30);

        assert!(!payload.is_expired_at(98));
        assert!(payload.is_expired_at(99));
        assert!(payload.is_expired_at(100));
    }

    #[test]
    fn push_lock_key_layout_is_stable() {
        assert_eq!(
            push_lock_path("org/repo", "refs/heads/main").unwrap(),
            "org/repo/locks/refs/heads/main/lock"
        );
        assert_eq!(
            internal_lock_path("org/repo", GIT_OBJECT_LOCATOR_RESOURCE).unwrap(),
            "org/repo/locks/internal/git-object-locator/lock"
        );
        assert_eq!(
            internal_lock_path("org/repo", GIT_MANIFEST_RESOURCE).unwrap(),
            "org/repo/locks/internal/git-manifest/lock"
        );
        assert_eq!(
            internal_lock_path("org/repo", REPOSITORY_MAINTENANCE_RESOURCE).unwrap(),
            "org/repo/locks/internal/repository-maintenance/lock"
        );
        assert_eq!(push_locks_prefix("org/repo").unwrap(), "org/repo/locks");
    }

    #[test]
    fn scoped_repo_prefix_preserves_relative_lock_layout() {
        let prefix = "org/repo/acl-views/v1/scope/7-deadbeef";

        assert_eq!(
            push_lock_path(prefix, "refs/tags/releases/v1").unwrap(),
            "org/repo/acl-views/v1/scope/7-deadbeef/locks/refs/tags/releases/v1/lock"
        );
        assert_eq!(
            internal_lock_path(prefix, REPOSITORY_MAINTENANCE_RESOURCE).unwrap(),
            "org/repo/acl-views/v1/scope/7-deadbeef/locks/internal/repository-maintenance/lock"
        );
    }

    #[test]
    fn lock_target_validation_rejects_ambiguous_paths() {
        for ref_name in ["heads/main", "refs/refs/heads/main", "refs/heads/../main"] {
            assert!(push_lock_path("org/repo", ref_name).is_err(), "{ref_name}");
        }
        for resource in ["", "Git-Locator", "git_locator", "git--locator"] {
            assert!(
                internal_lock_path("org/repo", resource).is_err(),
                "{resource}"
            );
        }
        assert!(push_lock_path("org//repo", "refs/heads/main").is_err());
        assert!(push_locks_prefix("org//repo").is_err());
    }

    #[tokio::test]
    async fn ref_lock_writes_renews_and_releases_only_canonical_key() {
        let store = memory_store();
        let mut lock = PushLock::acquire_ref_default(&store, "org/repo", "refs/heads/main")
            .await
            .unwrap();
        let path = lock.path().to_owned();
        assert_eq!(path, "org/repo/locks/refs/heads/main/lock");
        let holder = lock.holder().to_owned();

        lock.renew().await.unwrap();
        let (body, _) = get_with_version(&store, &Path::from(path.as_str()))
            .await
            .unwrap();
        let payload: PushLockPayload = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.holder, holder);
        assert!(!payload.is_released());

        lock.release().await.unwrap();
        let (body, _) = get_with_version(&store, &Path::from(path.as_str()))
            .await
            .unwrap();
        let payload: PushLockPayload = serde_json::from_slice(&body).unwrap();
        assert!(payload.is_released());

        let retired = Path::from("org/repo/locks/refs/refs/heads/main/lock");
        assert!(matches!(
            store.get(&retired).await,
            Err(object_store::Error::NotFound { .. })
        ));
    }
}
