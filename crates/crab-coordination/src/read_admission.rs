//! Object-store admission for concurrent repository readers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use object_store::ObjectStore;

use crate::error::{CoordinationError, Result};
use crate::push_lock::{PushLock, PushLockAcquireContext};

/// Default number of concurrent upload-pack sessions admitted per repository.
pub const DEFAULT_READ_ADMISSION_CAPACITY: usize = 16;

/// Default lease lifetime for one upload-pack admission slot.
pub const DEFAULT_READ_ADMISSION_TTL: Duration = Duration::from_secs(300);

const READ_ADMISSION_RESOURCE_PREFIX: &str = "git-read-admission";

/// A crash-reclaimable slot that bounds concurrent repository readers across
/// independent Crab processes and hosts.
pub struct ReadAdmissionTicket {
    prefix: String,
    capacity: usize,
    lease_ttl: Duration,
    holder: String,
    attempt: usize,
    acquire_context: PushLockAcquireContext,
    lock: Option<PushLock>,
}

impl ReadAdmissionTicket {
    /// Creates a reader for one repository's fixed admission slots.
    pub fn new(
        store: &Arc<dyn ObjectStore>,
        prefix: &str,
        capacity: usize,
        lease_ttl: Duration,
    ) -> Result<Self> {
        if capacity == 0 {
            return Err(CoordinationError::Configuration {
                key: capacity.to_string(),
                origin: "read admission capacity must be positive".to_owned(),
            });
        }
        if lease_ttl.as_secs() == 0 {
            return Err(CoordinationError::Configuration {
                key: lease_ttl.as_secs().to_string(),
                origin: "read admission TTL must be at least one second".to_owned(),
            });
        }
        crate::push_lock::push_locks_prefix(prefix)?;

        Ok(Self {
            prefix: prefix.to_owned(),
            capacity,
            lease_ttl,
            holder: admission_holder(),
            attempt: 0,
            acquire_context: PushLockAcquireContext::new(Arc::clone(store)),
            lock: None,
        })
    }

    /// Tries one rotated slot without waiting behind an active reader.
    pub async fn try_admit(&mut self) -> Result<bool> {
        if self.lock.is_some() {
            return Ok(true);
        }

        let slot = slot_offset(&self.holder, self.attempt, self.capacity);
        self.attempt = self.attempt.saturating_add(1);
        let resource = format!("{READ_ADMISSION_RESOURCE_PREFIX}-{slot}");
        match self
            .acquire_context
            .try_acquire_internal(&self.prefix, &resource, self.lease_ttl)
            .await
        {
            Ok(lock) => {
                self.lock = Some(lock);
                Ok(true)
            }
            Err(CoordinationError::PushLockHeld { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Extends the active slot lease.
    pub async fn renew(&mut self) -> Result<()> {
        self.lock
            .as_mut()
            .ok_or_else(|| CoordinationError::Configuration {
                key: self.prefix.clone(),
                origin: "cannot renew an unadmitted repository reader".to_owned(),
            })?
            .renew()
            .await
    }

    /// Releases the active slot with a holder-checked CAS tombstone.
    pub async fn release(mut self) -> Result<()> {
        match self.lock.take() {
            Some(lock) => lock.release().await,
            None => Ok(()),
        }
    }

    /// Returns the active lease lifetime used by the renewal loop.
    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.lease_ttl
    }
}

fn admission_holder() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    format!(
        "read-admission-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn slot_offset(holder: &str, attempt: usize, capacity: usize) -> usize {
    let digest = blake3::hash(holder.as_bytes());
    let mut bytes = [0_u8; std::mem::size_of::<u64>()];
    bytes.copy_from_slice(&digest.as_bytes()[..std::mem::size_of::<u64>()]);
    (u64::from_le_bytes(bytes) as usize + attempt) % capacity
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    use super::*;

    fn memory_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    async fn admit(ticket: &mut ReadAdmissionTicket, capacity: usize) -> bool {
        for _ in 0..capacity {
            if ticket.try_admit().await.unwrap() {
                return true;
            }
        }
        false
    }

    #[tokio::test]
    async fn capacity_is_bounded_and_reusable() {
        let store = memory_store();
        let ttl = Duration::from_secs(60);
        let mut first = ReadAdmissionTicket::new(&store, "org/repo", 2, ttl).unwrap();
        let mut second = ReadAdmissionTicket::new(&store, "org/repo", 2, ttl).unwrap();
        let mut blocked = ReadAdmissionTicket::new(&store, "org/repo", 2, ttl).unwrap();

        assert!(admit(&mut first, 2).await);
        assert!(admit(&mut second, 2).await);
        assert!(!admit(&mut blocked, 2).await);

        first.release().await.unwrap();
        assert!(admit(&mut blocked, 2).await);
        second.release().await.unwrap();
        blocked.release().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_readers_never_exceed_capacity() {
        const CAPACITY: usize = 4;
        let store = memory_store();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let workers = (0..20)
            .map(|_| {
                let store = Arc::clone(&store);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                tokio::spawn(async move {
                    let mut ticket = ReadAdmissionTicket::new(
                        &store,
                        "org/repo",
                        CAPACITY,
                        Duration::from_secs(60),
                    )
                    .unwrap();
                    while !ticket.try_admit().await.unwrap() {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    let readers = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(readers, Ordering::SeqCst);
                    assert!(readers <= CAPACITY);
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
        .expect("all readers should eventually acquire a slot");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), CAPACITY);
    }
}
