//! Bounded object-store admission for repository push pipelines.

use std::sync::Arc;
use std::time::{Duration, Instant};

use object_store::path::Path;
use object_store::{ObjectStore, UpdateVersion};
use tracing::warn;

use crate::error::{CoordinationError, Result};
use crate::push_lock::{
    PushLockPayload, backend_unix_time, create_strict, deserialize_payload,
    get_with_version_and_modified, push_locks_prefix, serialize_payload, store_error, unix_now,
    update,
};

/// Contender that becomes one of a fixed number of push-admission leases.
pub struct PushAdmissionTicket {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    holder: String,
    capacity: usize,
    lease_ttl: Duration,
    attempt: usize,
    occupied_slots: usize,
    backend_clock: Option<(i64, Instant)>,
    path: Option<String>,
    etag: Option<UpdateVersion>,
    released: bool,
}

impl PushAdmissionTicket {
    /// Creates a contender for one repository's bounded admission slots.
    pub fn new(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        capacity: usize,
        lease_ttl: Duration,
    ) -> Result<Self> {
        if capacity == 0 {
            return Err(CoordinationError::Configuration {
                key: capacity.to_string(),
                origin: "push admission capacity must be positive".to_owned(),
            });
        }
        validate_ttl(lease_ttl)?;

        Ok(Self {
            store: Arc::clone(store),
            prefix: push_admission_prefix(prefix)?,
            holder: crate::push_lock::generate_holder_id(),
            capacity,
            lease_ttl,
            attempt: 0,
            occupied_slots: 0,
            backend_clock: None,
            path: None,
            etag: None,
            released: false,
        })
    }

    /// Attempts to acquire one slot without listing repository objects.
    pub async fn try_admit(&mut self) -> Result<bool> {
        if self.path.is_some() {
            return Ok(true);
        }

        let body = serialize_payload(
            &self.prefix,
            &PushLockPayload::new(
                &self.holder,
                unix_now().saturating_add(self.lease_ttl.as_secs()),
                self.lease_ttl.as_secs(),
            ),
        )?;
        let offset = slot_offset(&self.holder, self.attempt, self.capacity);
        self.attempt = self.attempt.saturating_add(1);
        let mut live = Vec::with_capacity(self.capacity);

        for step in 0..self.capacity {
            let path = Path::from(push_admission_slot_path(
                &self.prefix,
                (offset + step) % self.capacity,
            ));
            let (existing_body, version, last_modified) =
                match get_with_version_and_modified(&self.store, &path).await {
                    Ok(existing) => existing,
                    Err(object_store::Error::NotFound { .. }) => {
                        match create_strict(&self.store, &path, body.clone()).await {
                            Ok(etag) => return Ok(self.admit(path, etag)),
                            Err(error) if is_cas_conflict(&error) => continue,
                            Err(source) => return Err(store_error(path.as_ref(), source)),
                        }
                    }
                    Err(source) => return Err(store_error(path.as_ref(), source)),
                };
            let payload = deserialize_payload(path.as_ref(), &existing_body)?;
            if payload.is_released() {
                match update(&self.store, &path, body.clone(), version).await {
                    Ok(etag) => return Ok(self.admit(path, etag)),
                    Err(error) if is_cas_conflict(&error) => continue,
                    Err(source) => return Err(store_error(path.as_ref(), source)),
                }
            }
            live.push((path, payload, version, last_modified));
        }

        self.occupied_slots = live.len();
        if live.is_empty() {
            return Ok(false);
        }
        let now = self.backend_now().await?;
        for (path, payload, version, last_modified) in live {
            if !lease_expired(&payload, last_modified, now) {
                continue;
            }
            match update(&self.store, &path, body.clone(), version).await {
                Ok(etag) => return Ok(self.admit(path, etag)),
                Err(error) if is_cas_conflict(&error) => {}
                Err(source) => return Err(store_error(path.as_ref(), source)),
            }
        }
        Ok(false)
    }

    /// Extends an active permit's lease.
    pub async fn renew(&mut self) -> Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Err(CoordinationError::Configuration {
                key: self.prefix.clone(),
                origin: "cannot renew an unadmitted push contender".to_owned(),
            });
        };
        let object_path = Path::from(path);
        let (body, version, _) = get_with_version_and_modified(&self.store, &object_path)
            .await
            .map_err(|source| store_error(path, source))?;
        let payload = deserialize_payload(path, &body)?;
        if payload.holder != self.holder || payload.is_released() {
            return Err(expired_ticket(path));
        }
        let body = serialize_payload(
            path,
            &PushLockPayload::new(
                &self.holder,
                unix_now().saturating_add(self.lease_ttl.as_secs()),
                self.lease_ttl.as_secs(),
            ),
        )?;
        self.etag = Some(
            update(&self.store, &object_path, body, version)
                .await
                .map_err(|source| store_error(path, source))?,
        );
        Ok(())
    }

    /// Releases this writer's slot for another push.
    pub async fn release(mut self) -> Result<()> {
        let result = match self.path.as_deref() {
            Some(path) => {
                crate::push_lock::release_with_known_etag(
                    &self.store,
                    path,
                    &self.holder,
                    self.etag.clone(),
                )
                .await
            }
            None => Ok(()),
        };
        self.released = true;
        result
    }

    /// Returns the permit lifetime used for active renewal.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.lease_ttl
    }

    /// Returns the number of live slots observed in the last attempt.
    #[must_use]
    pub fn occupied_slots(&self) -> usize {
        self.occupied_slots
    }

    fn admit(&mut self, path: Path, etag: UpdateVersion) -> bool {
        self.path = Some(path.as_ref().to_owned());
        self.etag = Some(etag);
        self.occupied_slots = self.capacity;
        true
    }

    async fn backend_now(&mut self) -> Result<i64> {
        if let Some((sample, sampled_at)) = self.backend_clock {
            return Ok(sample.saturating_add(
                i64::try_from(sampled_at.elapsed().as_secs()).unwrap_or(i64::MAX),
            ));
        }
        let sample = backend_unix_time(&self.store, &Path::from(self.prefix.as_str())).await?;
        self.backend_clock = Some((sample, Instant::now()));
        Ok(sample)
    }
}

impl Drop for PushAdmissionTicket {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let holder = self.holder.clone();
        let etag = self.etag.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) =
                    crate::push_lock::release_with_known_etag(&store, &path, &holder, etag).await
                {
                    warn!(path, %error, "failed to release push admission slot on drop");
                }
            });
        }
    }
}

/// Canonical object-key prefix for one repository's fixed admission slots.
pub fn push_admission_prefix(prefix: &str) -> Result<String> {
    Ok(format!(
        "{}/internal/push-admission/slots",
        push_locks_prefix(prefix)?
    ))
}

fn push_admission_slot_path(prefix: &str, slot: usize) -> String {
    format!("{prefix}/{slot}")
}

fn slot_offset(holder: &str, attempt: usize, capacity: usize) -> usize {
    let digest = blake3::hash(holder.as_bytes());
    let mut bytes = [0_u8; std::mem::size_of::<u64>()];
    bytes.copy_from_slice(&digest.as_bytes()[..std::mem::size_of::<u64>()]);
    (usize::try_from(u64::from_le_bytes(bytes)).unwrap_or(0) + attempt) % capacity
}

fn lease_expired(payload: &PushLockPayload, last_modified: i64, backend_now: i64) -> bool {
    if payload.lease_secs == 0 {
        return false;
    }
    let Ok(lease_secs) = i64::try_from(payload.lease_secs) else {
        return false;
    };
    last_modified.saturating_add(lease_secs) <= backend_now
}

fn validate_ttl(ttl: Duration) -> Result<()> {
    if ttl.as_secs() > 0 {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: ttl.as_secs().to_string(),
        origin: "push admission TTL must be at least one second".to_owned(),
    })
}

fn expired_ticket(path: &str) -> CoordinationError {
    CoordinationError::NotFound {
        path: path.to_owned(),
    }
}

fn is_cas_conflict(error: &object_store::Error) -> bool {
    matches!(
        error,
        object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn memory_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    fn contender(
        store: &Arc<dyn ObjectStore>,
        capacity: usize,
        ttl: Duration,
    ) -> PushAdmissionTicket {
        PushAdmissionTicket::new(store, "org/repo", capacity, ttl).unwrap()
    }

    #[tokio::test]
    async fn capacity_is_bounded_without_per_writer_objects() {
        let store = memory_store();
        let mut admitted = Vec::new();
        for _ in 0..5 {
            let mut ticket = contender(&store, 5, Duration::from_secs(60));
            assert!(ticket.try_admit().await.unwrap());
            admitted.push(ticket);
        }
        let mut blocked = contender(&store, 5, Duration::from_secs(60));

        assert!(!blocked.try_admit().await.unwrap());
        assert_eq!(blocked.occupied_slots(), 5);
        assert_eq!(
            store
                .list(Some(&Path::from(
                    push_admission_prefix("org/repo").unwrap()
                )))
                .count()
                .await,
            6
        );

        admitted.pop().unwrap().release().await.unwrap();
        assert!(blocked.try_admit().await.unwrap());
        blocked.release().await.unwrap();
        for ticket in admitted {
            ticket.release().await.unwrap();
        }
    }

    #[tokio::test]
    async fn blocked_contender_samples_backend_clock_once() {
        let store = memory_store();
        let mut active = contender(&store, 1, Duration::from_secs(60));
        assert!(active.try_admit().await.unwrap());
        let mut blocked = contender(&store, 1, Duration::from_secs(60));
        assert!(!blocked.try_admit().await.unwrap());
        let clock = Path::from(format!(
            "{}/clock",
            push_admission_prefix("org/repo").unwrap()
        ));
        store.delete(&clock).await.unwrap();

        assert!(!blocked.try_admit().await.unwrap());
        assert!(matches!(
            store.head(&clock).await,
            Err(object_store::Error::NotFound { .. })
        ));

        active.release().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_contenders_never_exceed_capacity() {
        const CAPACITY: usize = 5;
        let store = memory_store();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let workers = (0..40)
            .map(|_| {
                let store = Arc::clone(&store);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                tokio::spawn(async move {
                    let mut ticket = contender(&store, CAPACITY, Duration::from_secs(60));
                    while !ticket.try_admit().await.unwrap() {
                        tokio::task::yield_now().await;
                    }
                    let admitted = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(admitted, Ordering::SeqCst);
                    assert!(admitted <= CAPACITY);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    ticket.release().await.unwrap();
                })
            })
            .collect::<Vec<_>>();

        tokio::time::timeout(Duration::from_secs(10), async {
            for worker in workers {
                worker.await.unwrap();
            }
        })
        .await
        .expect("all contenders should eventually acquire a slot");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), CAPACITY);
    }

    #[tokio::test]
    async fn expired_slot_does_not_reduce_capacity() {
        let store = memory_store();
        let mut abandoned = contender(&store, 1, Duration::from_secs(1));
        assert!(abandoned.try_admit().await.unwrap());
        std::mem::forget(abandoned);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let mut successor = contender(&store, 1, Duration::from_secs(1));

        assert!(successor.try_admit().await.unwrap());
        successor.release().await.unwrap();
    }

    #[tokio::test]
    async fn release_leaves_reusable_tombstone() {
        let store = memory_store();
        let mut ticket = contender(&store, 1, Duration::from_secs(60));
        assert!(ticket.try_admit().await.unwrap());
        let path = Path::from(push_admission_slot_path(
            &push_admission_prefix("org/repo").unwrap(),
            0,
        ));

        ticket.release().await.unwrap();

        let body = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert!(
            deserialize_payload(path.as_ref(), &body)
                .unwrap()
                .is_released()
        );
    }

    #[tokio::test]
    async fn previous_holder_cannot_renew_reacquired_slot() {
        let store = memory_store();
        let mut previous = contender(&store, 1, Duration::from_secs(60));
        assert!(previous.try_admit().await.unwrap());
        let path = previous.path.clone().unwrap();
        crate::push_lock::release_if_holder(&store, &path, &previous.holder)
            .await
            .unwrap();
        let mut current = contender(&store, 1, Duration::from_secs(60));
        assert!(current.try_admit().await.unwrap());

        assert!(matches!(
            previous.renew().await,
            Err(CoordinationError::NotFound { .. })
        ));

        previous.released = true;
        current.release().await.unwrap();
    }

    #[tokio::test]
    async fn stale_release_does_not_clear_reacquired_slot() {
        let store = memory_store();
        let mut previous = contender(&store, 1, Duration::from_secs(60));
        assert!(previous.try_admit().await.unwrap());
        let path = previous.path.clone().unwrap();
        crate::push_lock::release_if_holder(&store, &path, &previous.holder)
            .await
            .unwrap();
        let mut current = contender(&store, 1, Duration::from_secs(60));
        assert!(current.try_admit().await.unwrap());

        previous.release().await.unwrap();
        current.renew().await.unwrap();
        current.release().await.unwrap();
    }

    #[test]
    fn slot_prefix_is_repository_scoped() {
        assert_eq!(
            push_admission_prefix("org/repo").unwrap(),
            "org/repo/locks/internal/push-admission/slots"
        );
    }
}
