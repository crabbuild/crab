use super::{active_session, shared_auth};
use crate::auth::{Authentication, Session, key};
use crab_storage::Store;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::Semaphore;

struct Reads {
    requests: AtomicUsize,
    pending: AtomicUsize,
    block_first: usize,
    release: Semaphore,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Reads {
    fn new(block_first: usize) -> Self {
        Self {
            requests: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            block_first,
            release: Semaphore::new(0),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }
}

struct Pending<'a>(&'a AtomicUsize);

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crab_storage::ReadAdmission for Reads {
    fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancellation
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = self.requests.fetch_add(1, Ordering::SeqCst);
        if request < self.block_first {
            self.pending.fetch_add(1, Ordering::SeqCst);
            let _pending = Pending(&self.pending);
            self.release.acquire().await?.forget();
        }
        Ok(())
    }

    async fn bytes(&self, _: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

struct Fixture {
    auth: Authentication,
    session: Arc<Session>,
    store: Store,
    reads: Arc<Reads>,
    backend: Weak<object_store::memory::InMemory>,
}

async fn fixture(block_first: usize) -> Fixture {
    let backend = Arc::new(object_store::memory::InMemory::new());
    let store = Store::new(backend.clone());
    let mut auth = shared_auth(store.clone());
    let session = active_session(key("queued-check"));
    auth.store_session(session.clone()).await.unwrap();
    auth.load_session(session.session_key).await.unwrap();
    let reads = Arc::new(Reads::new(block_first));
    auth.state.store = auth.state.store.clone().with_read_admission(reads.clone());
    Fixture {
        auth,
        session,
        store,
        reads,
        backend: Arc::downgrade(&backend),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_session_checks_share_one_fresh_verification() {
    let Fixture {
        auth,
        session,
        reads,
        ..
    } = fixture(0).await;
    let (first, second) = tokio::join!(
        auth.load_session(session.session_key),
        auth.load_session(session.session_key)
    );
    assert_eq!(
        (
            first.is_some(),
            second.is_some(),
            reads.requests.load(Ordering::SeqCst)
        ),
        (true, true, 2)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_arriving_after_dispatch_observes_current_revocation() {
    let Fixture {
        auth,
        session,
        store,
        reads,
        ..
    } = fixture(2).await;
    let mut first = Box::pin(auth.load_session(session.session_key));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    assert_eq!(reads.pending.load(Ordering::SeqCst), 2);
    let marker = auth.state.identity_session_path(
        &session.identity.issuer,
        &session.identity.subject,
        &session.session_key,
    );
    store.delete(&marker).await.unwrap();
    let late = tokio::time::timeout(
        Duration::from_secs(5),
        auth.load_session(session.session_key),
    )
    .await
    .unwrap();
    reads.release.add_permits(2);
    assert_eq!((late.is_none(), first.await.is_none()), (true, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_one_enrolled_check_preserves_its_peer() {
    let Fixture {
        auth,
        session,
        reads,
        ..
    } = fixture(2).await;
    let mut first = Box::pin(auth.load_session(session.session_key));
    let mut second = Box::pin(auth.load_session(session.session_key));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    drop(first);
    reads.release.add_permits(2);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_registry_does_not_retain_an_abandoned_verification() {
    for polls in [1, 2] {
        let Fixture {
            auth,
            session,
            store,
            reads,
            backend,
        } = fixture(2).await;
        let mut pending = Box::pin(auth.load_session(session.session_key));
        for _ in 0..polls {
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
        }
        assert_eq!(reads.pending.load(Ordering::SeqCst), (polls - 1) * 2);
        drop(pending);
        drop(auth);
        drop(store);
        assert_eq!(
            (
                reads.pending.load(Ordering::SeqCst),
                backend.upgrade().is_none()
            ),
            (0, true)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_shared_result_cannot_delete_newer_session_state() {
    let Fixture {
        auth,
        session,
        store,
        reads,
        ..
    } = fixture(1).await;
    auth.sessions.lock().await.clear();
    let mut first = Box::pin(auth.load_session(session.session_key));
    let mut late = Box::pin(auth.load_session(session.session_key));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    assert!(futures_util::poll!(late.as_mut()).is_pending());
    reads.release.add_permits(1);
    let initial = first.await.unwrap();
    let path = auth.state.path("sessions", &session.session_key);
    let (body, _) = store.get_with_etag_bounded(&path, 64 * 1024).await.unwrap();
    let mut current: serde_json::Value = serde_json::from_slice(&body).unwrap();
    current["csrf"] = serde_json::json!("replacement");
    store
        .put_overwrite(
            &path,
            bytes::Bytes::from(serde_json::to_vec(&current).unwrap()),
        )
        .await
        .unwrap();
    let refreshed = auth.load_session(session.session_key).await.unwrap();
    assert!(!initial.active() && refreshed.active());
    let _ = late.await;
    assert_eq!(
        auth.load_session(session.session_key)
            .await
            .map(|session| session.csrf.clone()),
        Some("replacement".to_owned())
    );
}
