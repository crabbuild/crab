//! Fair object-store admission for repository push pipelines.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt};
use tracing::warn;

use crate::error::{CoordinationError, Result};
use crate::push_lock::{
    create_strict, generate_holder_id, get_with_version, push_locks_prefix, store_error, unix_now,
    update,
};

const MAX_TICKET_CREATE_ATTEMPTS: usize = 8;
const MAX_ADMISSION_RETRIES: usize = 16;
const MAX_EXPIRED_CLEANUP_PER_OBSERVATION: usize = 2;

/// Durable FIFO ticket that becomes a bounded push-admission permit.
pub struct PushAdmissionTicket {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    path: String,
    holder: String,
    capacity: usize,
    lease_ttl: Duration,
    writers_ahead: usize,
    admitted: bool,
    released: bool,
}

impl PushAdmissionTicket {
    /// Creates one independently writable ticket in the repository queue.
    pub async fn enqueue(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        capacity: usize,
        active_ttl: Duration,
        queued_ttl: Duration,
    ) -> Result<Self> {
        if capacity == 0 {
            return Err(CoordinationError::Configuration {
                key: capacity.to_string(),
                origin: "push admission capacity must be positive".to_owned(),
            });
        }
        validate_ttl(active_ttl, "active")?;
        validate_ttl(queued_ttl, "queued")?;

        let prefix = push_admission_prefix(prefix)?;
        let lease_ttl = active_ttl.min(queued_ttl);
        for _ in 0..MAX_TICKET_CREATE_ATTEMPTS {
            let holder = generate_holder_id();
            let path = format!("{prefix}/{}", uuid::Uuid::now_v7());
            match create_strict(
                store,
                &Path::from(path.as_str()),
                Bytes::from(holder.clone()),
            )
            .await
            {
                Ok(_) => {
                    return Ok(Self {
                        store: Arc::clone(store),
                        prefix,
                        path,
                        holder,
                        capacity,
                        lease_ttl,
                        writers_ahead: usize::MAX,
                        admitted: false,
                        released: false,
                    });
                }
                Err(error) if is_cas_conflict(&error) => {}
                Err(source) => return Err(store_error(&path, source)),
            }
        }
        Err(CoordinationError::CasConflict {
            path: prefix,
            expected_etag: None,
        })
    }

    /// Attempts to convert this FIFO ticket into an active bounded permit.
    pub async fn try_admit(&mut self) -> Result<bool> {
        if self.admitted {
            return Ok(true);
        }
        for _ in 0..MAX_ADMISSION_RETRIES {
            let now = unix_now();
            let mut live = Vec::new();
            let mut expired = Vec::new();
            let mut stream = self.store.list(Some(&Path::from(self.prefix.as_str())));
            while let Some(item) = stream.next().await {
                let meta = item.map_err(|source| store_error(&self.prefix, source))?;
                let modified = u64::try_from(meta.last_modified.timestamp()).unwrap_or(0);
                if modified.saturating_add(self.lease_ttl.as_secs().max(1)) <= now {
                    expired.push(meta);
                } else {
                    live.push(meta);
                }
            }
            // Renewal changes object metadata, so queue order must live in the
            // immutable UUIDv7 key rather than the mutable last-modified time.
            live.sort_unstable_by(|left, right| left.location.cmp(&right.location));
            self.cleanup_expired(expired).await;

            let Some(position) = live
                .iter()
                .position(|meta| meta.location.as_ref() == self.path)
            else {
                return Err(expired_ticket(&self.path));
            };
            self.writers_ahead = position;
            let admitted = position < self.capacity;
            let own = &live[position];
            let modified = u64::try_from(own.last_modified.timestamp()).unwrap_or(0);
            let refresh_margin = self.lease_ttl.as_secs().max(1).div_ceil(4).min(30);
            let needs_refresh = admitted
                || modified.saturating_add(self.lease_ttl.as_secs().max(1))
                    <= now.saturating_add(refresh_margin);
            if !needs_refresh {
                return Ok(false);
            }
            let (body, version) = get_with_version(&self.store, &own.location)
                .await
                .map_err(|source| store_error(&self.path, source))?;
            if body.as_ref() != self.holder.as_bytes() {
                return Err(expired_ticket(&self.path));
            }
            match update(
                &self.store,
                &own.location,
                Bytes::from(self.holder.clone()),
                version,
            )
            .await
            {
                Ok(_) => {
                    self.admitted = admitted;
                    return Ok(admitted);
                }
                Err(error) if is_cas_conflict(&error) => continue,
                Err(source) => return Err(store_error(&self.path, source)),
            }
        }
        Err(CoordinationError::CasConflict {
            path: self.path.clone(),
            expected_etag: None,
        })
    }

    /// Extends an active permit's lease.
    pub async fn renew(&self) -> Result<()> {
        if !self.admitted {
            return Err(CoordinationError::Configuration {
                key: self.path.clone(),
                origin: "cannot renew a queued push admission ticket".to_owned(),
            });
        }
        let path = Path::from(self.path.as_str());
        let (body, version) = get_with_version(&self.store, &path)
            .await
            .map_err(|source| store_error(&self.path, source))?;
        if body.as_ref() != self.holder.as_bytes() {
            return Err(expired_ticket(&self.path));
        }
        update(
            &self.store,
            &path,
            Bytes::from(self.holder.clone()),
            version,
        )
        .await
        .map(|_| ())
        .map_err(|source| store_error(&self.path, source))
    }

    /// Removes this writer from the queue and hands capacity to its successor.
    pub async fn release(mut self) -> Result<()> {
        let result = delete_ticket(&self.store, &self.path).await;
        self.released = true;
        result
    }

    /// Returns the permit lifetime used for active renewal.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.lease_ttl
    }

    /// Returns the last observed number of live writers ahead in FIFO order.
    #[must_use]
    pub fn writers_ahead(&self) -> usize {
        self.writers_ahead
    }

    async fn cleanup_expired(&self, expired: Vec<object_store::ObjectMeta>) {
        for meta in expired
            .into_iter()
            .filter(|meta| meta.location.as_ref() != self.path)
            .take(MAX_EXPIRED_CLEANUP_PER_OBSERVATION)
        {
            let version = object_store::UpdateVersion {
                e_tag: meta.e_tag,
                version: meta.version,
            };
            match update(
                &self.store,
                &meta.location,
                Bytes::from_static(b"expired"),
                version,
            )
            .await
            {
                Ok(_) => {
                    if let Err(error) = self.store.delete(&meta.location).await {
                        warn!(path = %meta.location, %error, "failed to delete expired push admission ticket");
                    }
                }
                Err(error) if is_cas_conflict(&error) => {}
                Err(error) => {
                    warn!(path = %meta.location, %error, "failed to claim expired push admission ticket");
                }
            }
        }
    }
}

impl Drop for PushAdmissionTicket {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let store = Arc::clone(&self.store);
        let path = self.path.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = delete_ticket(&store, &path).await {
                    warn!(path, %error, "failed to release push admission ticket on drop");
                }
            });
        }
    }
}

/// Canonical object-key prefix for one repository's FIFO admission tickets.
pub fn push_admission_prefix(prefix: &str) -> Result<String> {
    Ok(format!(
        "{}/internal/push-admission/tickets",
        push_locks_prefix(prefix)?
    ))
}

async fn delete_ticket(store: &Arc<dyn ObjectStore>, path: &str) -> Result<()> {
    match store.delete(&Path::from(path)).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(source) => Err(store_error(path, source)),
    }
}

fn validate_ttl(ttl: Duration, kind: &str) -> Result<()> {
    if !ttl.is_zero() {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: ttl.as_secs().to_string(),
        origin: format!("push admission {kind} TTL must be positive"),
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
    use object_store::memory::InMemory;

    fn memory_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    #[tokio::test]
    async fn one_hundred_tickets_complete_in_fifo_capacity_windows() {
        let store = memory_store();
        let mut tickets = Vec::new();
        for _ in 0..100 {
            tickets.push(
                PushAdmissionTicket::enqueue(
                    &store,
                    "org/repo",
                    5,
                    Duration::from_secs(60),
                    Duration::from_secs(60),
                )
                .await
                .unwrap(),
            );
        }
        let expected = tickets
            .iter()
            .map(|ticket| ticket.path.clone())
            .collect::<Vec<_>>();
        let (admitted_tx, mut admitted_rx) = tokio::sync::mpsc::channel(100);
        let workers = tickets
            .into_iter()
            .map(|mut ticket| {
                let admitted_tx = admitted_tx.clone();
                tokio::spawn(async move {
                    loop {
                        if ticket.try_admit().await.unwrap() {
                            admitted_tx.send(ticket).await.unwrap();
                            return;
                        }
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect::<Vec<_>>();
        drop(admitted_tx);

        for window in 0..20 {
            let mut admitted = Vec::new();
            for _ in 0..5 {
                admitted.push(admitted_rx.recv().await.unwrap());
            }
            tokio::task::yield_now().await;
            assert!(
                admitted_rx.try_recv().is_err(),
                "more than five tickets entered in window {window}"
            );
            admitted.sort_by(|left, right| left.path.cmp(&right.path));
            assert_eq!(
                admitted
                    .iter()
                    .map(|ticket| ticket.path.clone())
                    .collect::<Vec<_>>(),
                expected[(window * 5)..(window * 5 + 5)]
            );
            for ticket in admitted {
                ticket.release().await.unwrap();
            }
        }
        for worker in workers {
            worker.await.unwrap();
        }
    }

    #[tokio::test]
    async fn expired_front_ticket_does_not_block_its_successor() {
        let store = memory_store();
        let abandoned = PushAdmissionTicket::enqueue(
            &store,
            "org/repo",
            1,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        std::mem::forget(abandoned);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let mut successor = PushAdmissionTicket::enqueue(
            &store,
            "org/repo",
            1,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert!(successor.try_admit().await.unwrap());
        successor.release().await.unwrap();
    }

    #[tokio::test]
    async fn release_removes_ticket_object() {
        let store = memory_store();
        let ticket = PushAdmissionTicket::enqueue(
            &store,
            "org/repo",
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let path = Path::from(ticket.path.as_str());

        ticket.release().await.unwrap();

        assert!(matches!(
            store.head(&path).await,
            Err(object_store::Error::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn stale_cleanup_does_not_delete_a_renewed_ticket() {
        let store = memory_store();
        let mut active = PushAdmissionTicket::enqueue(
            &store,
            "org/repo",
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(active.try_admit().await.unwrap());
        let cleaner = PushAdmissionTicket::enqueue(
            &store,
            "org/repo",
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let path = Path::from(active.path.as_str());
        let stale = store.head(&path).await.unwrap();

        active.renew().await.unwrap();
        cleaner.cleanup_expired(vec![stale]).await;

        store.head(&path).await.unwrap();
        active.release().await.unwrap();
        cleaner.release().await.unwrap();
    }

    #[tokio::test]
    async fn claimed_expired_ticket_cannot_be_renewed() {
        let store = memory_store();
        let mut ticket = PushAdmissionTicket::enqueue(
            &store,
            "org/repo",
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(ticket.try_admit().await.unwrap());
        let path = Path::from(ticket.path.as_str());
        let (_, version) = get_with_version(&store, &path).await.unwrap();
        update(&store, &path, Bytes::from_static(b"expired"), version)
            .await
            .unwrap();

        assert!(matches!(
            ticket.renew().await,
            Err(CoordinationError::NotFound { .. })
        ));
        ticket.release().await.unwrap();
    }

    #[test]
    fn queue_prefix_is_repository_scoped() {
        assert_eq!(
            push_admission_prefix("org/repo").unwrap(),
            "org/repo/locks/internal/push-admission/tickets"
        );
    }
}
