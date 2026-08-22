//! Bounded object-store admission for repository push pipelines.

use std::sync::Arc;
use std::time::Duration;

use object_store::path::Path;
use object_store::{ObjectStore, UpdateVersion};
use tracing::warn;

use crate::error::{CoordinationError, Result};
use crate::push_lock::{
    BackendClock, PushLockPayload, create_strict, deserialize_payload,
    get_with_version_and_modified, push_locks_prefix, serialize_payload, store_error, unix_now,
    update,
};

/// Contender that reserves one or more fixed push-admission slots.
pub struct PushAdmissionTicket {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    holder: String,
    capacity: usize,
    required_slots: usize,
    lease_ttl: Duration,
    attempt: usize,
    occupied_slots: usize,
    backend_clock: BackendClock,
    leases: Vec<(String, Option<UpdateVersion>)>,
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
        Self::new_weighted(store, prefix, capacity, 1, lease_ttl)
    }

    /// Creates a contender that must reserve `required_slots` together.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid capacity, reservation
    /// width, repository prefix, or lease lifetime.
    pub fn new_weighted(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        capacity: usize,
        required_slots: usize,
        lease_ttl: Duration,
    ) -> Result<Self> {
        if capacity == 0 {
            return Err(CoordinationError::Configuration {
                key: capacity.to_string(),
                origin: "push admission capacity must be positive".to_owned(),
            });
        }
        if required_slots == 0 || required_slots > capacity {
            return Err(CoordinationError::Configuration {
                key: required_slots.to_string(),
                origin: format!("push admission required slots must be between 1 and {capacity}"),
            });
        }
        validate_ttl(lease_ttl)?;

        Ok(Self {
            store: Arc::clone(store),
            prefix: push_admission_prefix(prefix)?,
            holder: crate::push_lock::generate_holder_id(),
            capacity,
            required_slots,
            lease_ttl,
            attempt: 0,
            occupied_slots: 0,
            backend_clock: BackendClock::default(),
            leases: Vec::with_capacity(required_slots),
            released: false,
        })
    }

    /// Attempts to acquire the required slots without listing repository objects.
    /// Partial reservations are released before returning `false` or an error.
    pub async fn try_admit(&mut self) -> Result<bool> {
        if self.leases.len() == self.required_slots {
            return Ok(true);
        }
        if !self.leases.is_empty() {
            return Err(CoordinationError::Configuration {
                key: self.prefix.clone(),
                origin: "push admission contender retained a partial reservation".to_owned(),
            });
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
                            Ok(etag) => {
                                self.admit(path, etag);
                                if self.is_admitted() {
                                    return Ok(true);
                                }
                            }
                            Err(error) if is_cas_conflict(&error) => continue,
                            Err(source) => {
                                return self
                                    .abort_partial(store_error(path.as_ref(), source))
                                    .await;
                            }
                        }
                        continue;
                    }
                    Err(source) => {
                        return self.abort_partial(store_error(path.as_ref(), source)).await;
                    }
                };
            let payload = match deserialize_payload(path.as_ref(), &existing_body) {
                Ok(payload) => payload,
                Err(error) => return self.abort_partial(error).await,
            };
            if payload.is_released() {
                match update(&self.store, &path, body.clone(), version).await {
                    Ok(etag) => {
                        self.admit(path, etag);
                        if self.is_admitted() {
                            return Ok(true);
                        }
                    }
                    Err(error) if is_cas_conflict(&error) => continue,
                    Err(source) => {
                        return self.abort_partial(store_error(path.as_ref(), source)).await;
                    }
                }
                continue;
            }
            live.push((path, payload, version, last_modified));
        }

        self.occupied_slots = live.len().saturating_add(self.leases.len());
        if live.is_empty() {
            self.release_acquired().await?;
            return Ok(false);
        }
        let now = match self.backend_now().await {
            Ok(now) => now,
            Err(error) => return self.abort_partial(error).await,
        };
        for (path, payload, version, last_modified) in live {
            if !lease_expired(&payload, last_modified, now) {
                continue;
            }
            match update(&self.store, &path, body.clone(), version).await {
                Ok(etag) => {
                    self.admit(path, etag);
                    if self.is_admitted() {
                        return Ok(true);
                    }
                }
                Err(error) if is_cas_conflict(&error) => {}
                Err(source) => {
                    return self.abort_partial(store_error(path.as_ref(), source)).await;
                }
            }
        }
        self.release_acquired().await?;
        Ok(false)
    }

    /// Extends an active permit's lease.
    pub async fn renew(&mut self) -> Result<()> {
        if !self.is_admitted() {
            return Err(CoordinationError::Configuration {
                key: self.prefix.clone(),
                origin: "cannot renew an unadmitted push contender".to_owned(),
            });
        }
        for (path, etag) in &mut self.leases {
            let object_path = Path::from(path.as_str());
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
            *etag = Some(
                update(&self.store, &object_path, body, version)
                    .await
                    .map_err(|source| store_error(path, source))?,
            );
        }
        Ok(())
    }

    /// Releases this writer's slots for other pushes.
    pub async fn release(mut self) -> Result<()> {
        let result = self.release_acquired().await;
        self.released = result.is_ok();
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

    fn admit(&mut self, path: Path, etag: UpdateVersion) {
        self.leases.push((path.as_ref().to_owned(), Some(etag)));
        self.occupied_slots = self.capacity;
    }

    fn is_admitted(&self) -> bool {
        self.leases.len() == self.required_slots
    }

    async fn release_acquired(&mut self) -> Result<()> {
        let mut failed = Vec::new();
        let mut first_error = None;
        for (path, etag) in std::mem::take(&mut self.leases) {
            if let Err(error) = crate::push_lock::release_with_known_etag(
                &self.store,
                &path,
                &self.holder,
                etag.clone(),
            )
            .await
            {
                failed.push((path, etag));
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        self.leases = failed;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn abort_partial<T>(&mut self, error: CoordinationError) -> Result<T> {
        self.release_acquired().await?;
        Err(error)
    }

    async fn backend_now(&mut self) -> Result<i64> {
        self.backend_clock
            .now(&self.store, &Path::from(self.prefix.as_str()))
            .await
    }
}

impl Drop for PushAdmissionTicket {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if self.leases.is_empty() {
            return;
        }
        let store = Arc::clone(&self.store);
        let holder = self.holder.clone();
        let leases = std::mem::take(&mut self.leases);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for (path, etag) in leases {
                    if let Err(error) =
                        crate::push_lock::release_with_known_etag(&store, &path, &holder, etag)
                            .await
                    {
                        warn!(path, %error, "failed to release push admission slot on drop");
                    }
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
    async fn weighted_contender_releases_partial_reservation() {
        let store = memory_store();
        let mut first =
            PushAdmissionTicket::new_weighted(&store, "org/repo", 5, 3, Duration::from_secs(60))
                .unwrap();
        assert!(first.try_admit().await.unwrap());
        let mut blocked =
            PushAdmissionTicket::new_weighted(&store, "org/repo", 5, 3, Duration::from_secs(60))
                .unwrap();
        assert!(!blocked.try_admit().await.unwrap());
        let mut light = contender(&store, 5, Duration::from_secs(60));

        assert!(light.try_admit().await.unwrap());

        light.release().await.unwrap();
        first.release().await.unwrap();
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

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_weighted_contenders_never_exceed_capacity() {
        const CAPACITY: usize = 5;
        let store = memory_store();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let workers = [1, 2, 3, 4, 5, 1, 2, 3]
            .into_iter()
            .map(|required_slots| {
                let store = Arc::clone(&store);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                tokio::spawn(async move {
                    let mut ticket = PushAdmissionTicket::new_weighted(
                        &store,
                        "org/repo",
                        CAPACITY,
                        required_slots,
                        Duration::from_secs(60),
                    )
                    .unwrap();
                    while !ticket.try_admit().await.unwrap() {
                        tokio::task::yield_now().await;
                    }
                    let admitted =
                        active.fetch_add(required_slots, Ordering::SeqCst) + required_slots;
                    peak.fetch_max(admitted, Ordering::SeqCst);
                    assert!(admitted <= CAPACITY);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    active.fetch_sub(required_slots, Ordering::SeqCst);
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
        .expect("all weighted contenders should eventually acquire their slots");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(peak.load(Ordering::SeqCst) <= CAPACITY);
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
        let path = previous.leases[0].0.clone();
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
        let path = previous.leases[0].0.clone();
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
