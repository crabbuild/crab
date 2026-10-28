//! Renewal and draining ownership for captured reads.

use std::sync::{Arc, Mutex};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::{TaskTracker, task_tracker::TaskTrackerToken};

use crate::read_retention::{READ_RETENTION_TTL, ReadRetentionLease};
use crate::{CoordinationError, Result};

/// Owns capture admission and all renewal/release tasks for one read session.
///
/// Call shutdown after backend requests stop. Drop requests shutdown, but the
/// Tokio runtime must remain alive until admitted captures and reads drain.
pub struct ReadRetentionSession(Arc<Session>);

struct Session {
    accepting: Mutex<bool>,
    stop: CancellationToken,
    tasks: TaskTracker,
    failure: Mutex<Option<Arc<CoordinationError>>>,
}

impl Default for ReadRetentionSession {
    fn default() -> Self {
        let tasks = TaskTracker::new();
        tasks.close();
        Self(Arc::new(Session {
            accepting: Mutex::new(true),
            stop: CancellationToken::new(),
            tasks,
            failure: Mutex::new(None),
        }))
    }
}

impl ReadRetentionSession {
    /// Admit a capture before its first asynchronous storage operation.
    pub fn capture(&self) -> Result<ReadRetentionCapture> {
        let accepting = self
            .0
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*accepting {
            return Err(CoordinationError::Configuration {
                key: "captured-read admission is closed".into(),
                origin: "read retention session".into(),
            });
        }
        Ok(ReadRetentionCapture {
            session: Arc::clone(&self.0),
            _admission: self.0.tasks.token(),
        })
    }

    /// Close admission, cancel reads, and await every admitted capture and release.
    /// Cancellation of this wait does not interrupt cleanup.
    pub async fn shutdown(&self) -> Result<()> {
        self.close();
        self.0.tasks.wait().await;
        match self
            .0
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            Some(error) => Err(CoordinationError::ReadRetentionFailed(Arc::clone(error))),
            None => Ok(()),
        }
    }

    fn close(&self) {
        let mut accepting = self
            .0
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *accepting = false;
        self.0.stop.cancel();
    }
}

impl Drop for ReadRetentionSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Keeps shutdown pending while a capture can still register a claim worker.
pub struct ReadRetentionCapture {
    session: Arc<Session>,
    _admission: TaskTrackerToken,
}

impl ReadRetentionCapture {
    /// Transfer a created claim to its renewal owner before validating GC barriers.
    /// The returned view must not escape until both barriers validate.
    pub fn retain(self, lease: ReadRetentionLease) -> Result<RetainedRead> {
        let deadline = lease.deadline().unwrap_or_else(|_| Instant::now());
        let reads = TaskTracker::new();
        reads.close();
        let health = Arc::new(Health {
            path: lease.path().to_string(),
            state: Mutex::new(State {
                deadline,
                closed: false,
                failure: None,
            }),
            stop: self.session.stop.child_token(),
            reads,
        });
        let owner = RetainedRead(Arc::new(Owner {
            health: Arc::clone(&health),
            dropped: CancellationToken::new(),
        }));
        let dropped = owner.0.dropped.clone();
        let session = Arc::clone(&self.session);
        // The capture token keeps the closed tracker nonempty until the worker
        // is registered, including when shutdown raced with claim creation.
        self.session.tasks.spawn(async move {
            let outcome = renew_and_release(lease, &health, &dropped).await;
            if let Err(error) = outcome {
                let error = Arc::new(error);
                health.fail(Arc::clone(&error));
                session
                    .failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert(error);
            }
        });
        owner.0.health.check()?;
        Ok(owner)
    }
}

/// Retains a captured view through clones, open files and generation replacement.
#[derive(Clone)]
pub struct RetainedRead(Arc<Owner>);

struct Owner {
    health: Arc<Health>,
    dropped: CancellationToken,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.dropped.cancel();
    }
}

impl RetainedRead {
    /// Admit a read that must remain pinned through all asynchronous workers.
    pub fn pin(&self) -> Result<Arc<ReadRetentionPin>> {
        let health = &self.0.health;
        let state = health
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        health.check_state(&state)?;
        Ok(Arc::new(ReadRetentionPin {
            health: Arc::clone(health),
            _admission: health.reads.token(),
        }))
    }
}

/// Prevents release while admitted reconstruction or its child workers remain.
pub struct ReadRetentionPin {
    health: Arc<Health>,
    _admission: TaskTrackerToken,
}

impl ReadRetentionPin {
    /// Reject closed, expired or failed retention before exposing read output.
    pub fn ensure_valid(&self) -> Result<()> {
        self.health.check()
    }

    /// Signals shutdown or lease loss to admitted read operations.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.health.stop
    }
}

struct Health {
    path: String,
    state: Mutex<State>,
    stop: CancellationToken,
    reads: TaskTracker,
}

struct State {
    deadline: Instant,
    closed: bool,
    failure: Option<Arc<CoordinationError>>,
}

impl Health {
    fn check(&self) -> Result<()> {
        self.check_state(
            &self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn check_state(&self, state: &State) -> Result<()> {
        if let Some(error) = &state.failure {
            return Err(CoordinationError::ReadRetentionFailed(Arc::clone(error)));
        }
        if state.closed || self.stop.is_cancelled() || Instant::now() >= state.deadline {
            self.stop.cancel();
            return Err(self.lost());
        }
        Ok(())
    }

    fn lost(&self) -> CoordinationError {
        CoordinationError::ReadRetentionLost {
            path: self.path.clone(),
        }
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.stop.cancel();
    }

    fn fail(&self, error: Arc<CoordinationError>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
            .get_or_insert(error);
        self.close();
    }
}

async fn renew_and_release(
    mut lease: ReadRetentionLease,
    health: &Health,
    dropped: &CancellationToken,
) -> Result<()> {
    let mut draining = false;
    let mut failure = None;
    loop {
        let deadline = match lease.deadline() {
            Ok(deadline) => deadline,
            Err(error) => {
                let error = Arc::new(error);
                health.fail(Arc::clone(&error));
                failure = Some(error);
                health.reads.wait().await;
                break;
            }
        };
        tokio::select! {
            biased;
            () = health.reads.wait(), if draining => break,
            () = health.stop.cancelled(), if !draining => {
                health.close();
                draining = true;
            }
            () = dropped.cancelled(), if !draining => {
                health.close();
                draining = true;
            }
            () = tokio::time::sleep_until(deadline - READ_RETENTION_TTL / 3) => {
                let renewed = match tokio::time::timeout_at(deadline, lease.renew()).await {
                    Ok(result) => result,
                    Err(_) => Err(health.lost()),
                };
                if let Err(error) = renewed {
                    let error = Arc::new(error);
                    health.fail(Arc::clone(&error));
                    failure = Some(error);
                    health.reads.wait().await;
                    break;
                }
                health.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).deadline = lease.deadline()?;
            }
        }
    }
    // A failed/uncertain renewal may have committed. Release only the last
    // acknowledged provider version; a conflict leaves conservative GC retention.
    let released = match tokio::time::timeout(READ_RETENTION_TTL / 2, lease.release()).await {
        Ok(result) => result,
        Err(_) => Err(health.lost()),
    };
    match failure {
        Some(error) => Err(CoordinationError::ReadRetentionFailed(error)),
        None => released,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_retention::{RetainedReadRoot, load_read_retention_root, retained_read_root};
    use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path};
    use std::time::Duration;

    async fn fixture() -> (Arc<dyn ObjectStore>, Path, ReadRetentionLease) {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("repo/v2/readers/owner/claim");
        let root = RetainedReadRoot::new(blake3::hash(b"capture").to_hex().to_string(), 7).unwrap();
        let lease = ReadRetentionLease::create(store.clone(), path.clone(), root)
            .await
            .unwrap();
        (store, path, lease)
    }

    #[tokio::test]
    async fn last_view_drop_drains_pins_before_release() {
        let (store, path, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let view = session.capture().unwrap().retain(lease).unwrap();
        let second_view = view.clone();
        let pin = view.pin().unwrap();
        drop(view);
        pin.ensure_valid().unwrap();
        drop(second_view);
        pin.cancellation().cancelled().await;
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_some()
        );
        drop(pin);
        session.0.tasks.wait().await;
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_none()
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_revokes_views_and_waits_for_existing_pins() {
        let (store, path, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let view = session.capture().unwrap().retain(lease).unwrap();
        let pin = view.pin().unwrap();
        let shutdown = session.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        assert!(view.pin().is_err());
        assert!(session.capture().is_err());
        assert!(pin.ensure_valid().is_err());
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_some()
        );
        drop(pin);
        shutdown.await.unwrap();
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_a_late_capture_to_register_and_release() {
        let (store, path, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let capture = session.capture().unwrap();
        let shutdown = session.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        assert!(capture.retain(lease).is_err());
        shutdown.await.unwrap();
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn abandoning_shutdown_wait_does_not_abandon_release() {
        let (store, path, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let view = session.capture().unwrap().retain(lease).unwrap();
        let pin = view.pin().unwrap();
        {
            let shutdown = session.shutdown();
            tokio::pin!(shutdown);
            assert!(futures_util::poll!(&mut shutdown).is_pending());
        }
        drop(session);
        pin.cancellation().cancelled().await;
        let health = Arc::clone(&pin.health);
        drop(pin);
        // The view can outlive the closed backend; it cannot keep its claim alive.
        assert!(view.pin().is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(health.stop.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn renewals_continue_while_cancelled_reads_drain() {
        let (store, path, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let view = session.capture().unwrap().retain(lease).unwrap();
        let pin = view.pin().unwrap();
        let before = store.get(&path).await.unwrap().bytes().await.unwrap();
        drop(view);
        pin.cancellation().cancelled().await;
        tokio::time::advance(READ_RETENTION_TTL / 6).await;
        tokio::task::yield_now().await;
        let after = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert_ne!(before, after);
        drop(pin);
        session.shutdown().await.unwrap();
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_renewal_revokes_reads_and_preserves_the_provider_error() {
        let (store, path, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let view = session.capture().unwrap().retain(lease).unwrap();
        let pin = view.pin().unwrap();
        let expired = store.head(&path).await.unwrap().last_modified.timestamp()
            + READ_RETENTION_TTL.as_secs() as i64;
        retained_read_root(&store, &path, expired).await.unwrap();
        pin.cancellation().cancelled().await;
        let error = pin.ensure_valid().unwrap_err();
        assert!(matches!(error, CoordinationError::ReadRetentionFailed(_)));
        drop(pin);
        let error = session.shutdown().await.unwrap_err();
        let mut source: &dyn std::error::Error = &error;
        let mut provider_error = false;
        loop {
            provider_error |= source.downcast_ref::<object_store::Error>().is_some();
            let Some(next) = source.source() else { break };
            source = next;
        }
        assert!(provider_error);
        // A stale release must never overwrite GC's retirement.
        assert!(
            load_read_retention_root(&store, &path)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn expired_local_deadline_rejects_reads_before_the_worker_runs() {
        let (_, _, lease) = fixture().await;
        let session = ReadRetentionSession::default();
        let view = session.capture().unwrap().retain(lease).unwrap();
        view.0.health.state.lock().unwrap().deadline = Instant::now();
        assert!(matches!(
            view.pin(),
            Err(CoordinationError::ReadRetentionLost { .. })
        ));
        session.shutdown().await.unwrap();
    }
}
