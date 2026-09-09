use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

/// Tracks verification jobs whose read futures can be cancelled independently.
///
/// Drop read futures/streams before closing. Close rejects further hash work and
/// waits for every admitted blocking verification job, including detached joins.
#[derive(Clone, Default)]
pub struct LfsReadSession(Arc<State>);

#[derive(Default)]
struct State {
    closed: Mutex<bool>,
    tasks: TaskTracker,
}

impl LfsReadSession {
    /// Stop verification admission and wait for already-started jobs.
    pub async fn close(&self) {
        {
            let mut closed = self
                .0
                .closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *closed = true;
            self.0.tasks.close();
        }
        self.0.tasks.wait().await;
    }

    pub(crate) fn spawn_blocking<F, T>(&self, job: F) -> std::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let closed = self
            .0
            .closed
            .lock()
            .map_err(|_| std::io::Error::other("LFS verification admission is unavailable"))?;
        if *closed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "LFS read session is closed",
            ));
        }
        Ok(self.0.tasks.spawn_blocking(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn close_waits_for_detached_verification_and_rejects_new_work() {
        let session = LfsReadSession::default();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, entered) = tokio::sync::oneshot::channel();
        let job = session
            .spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            })
            .unwrap();
        entered.await.unwrap();
        drop(job);
        let mut close = Box::pin(session.close());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut close)
                .await
                .is_err()
        );
        assert!(session.spawn_blocking(|| ()).is_err());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), close)
            .await
            .unwrap();
        session.close().await;
    }
}
