//! Storage-backed short-TTL lease for serializing repository mutations.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// Internal resource serializing unified manifest publication.
pub const GIT_MANIFEST_RESOURCE: &str = "git-manifest";
/// Internal resource serializing repository repacks.
pub const REPACK_RESOURCE: &str = "repack";
/// Internal resource used when a push has no destination ref.
pub const BATCH_RESOURCE: &str = "batch";
/// Internal resource serializing history recovery without existing refs.
pub const HISTORY_RECOVERY_RESOURCE: &str = "history-recovery";
/// Internal resource serializing destructive repository maintenance.
pub const REPOSITORY_MAINTENANCE_RESOURCE: &str = "repository-maintenance";

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
        let holder = generate_holder_id();
        let expires_at = unix_now() + ttl.as_secs();
        let etag = acquire_one(store, &path, target, &holder, expires_at, ttl).await?;
        Ok(Self {
            store: Arc::clone(store),
            path,
            ttl,
            holder,
            etag: Some(etag),
            released: false,
        })
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
        self.etag = Some(renew_one(&self.store, &self.path, &self.holder, self.ttl).await?);
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
) -> Result<UpdateVersion> {
    let object_path = Path::from(path);
    let body = serialize_payload(
        path,
        &PushLockPayload::new(holder, expires_at, ttl.as_secs()),
    )?;
    let etag = match create_strict(store, &object_path, body.clone()).await {
        Ok(etag) => etag,
        Err(object_store::Error::AlreadyExists { .. })
        | Err(object_store::Error::Precondition { .. }) => {
            match acquire_contended(store, &object_path, target, body).await? {
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
            }
        }
        Err(source) => return Err(store_error(path, source)),
    };
    debug!(lock_path = %path, holder, ttl_secs = ttl.as_secs(), "push lock acquired");
    Ok(etag)
}

async fn renew_one(
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
        let now = backend_unix_time(store, object_path).await?;
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

pub(crate) async fn release_with_known_etag(
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
    release_if_holder_checked(store, path, holder)
        .await
        .map(|_| ())
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
    use object_store::memory::InMemory;
    use std::sync::Arc;
    use std::time::Duration;

    fn memory_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
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
            internal_lock_path(prefix, REPACK_RESOURCE).unwrap(),
            "org/repo/acl-views/v1/scope/7-deadbeef/locks/internal/repack/lock"
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
