use crate::error::{MetadataError, Result};

const MAX_DECODERS: usize = 4;
static DECODERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_DECODERS);

pub(super) async fn run<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let runtime = tokio::runtime::Handle::try_current().map_err(|source| MetadataError::Io {
        source: std::io::Error::other(source),
    })?;
    let permit = DECODERS
        .acquire()
        .await
        .map_err(|source| MetadataError::CapsuleDecodeAdmission { source })?;
    let span = tracing::Span::current();
    // Only immutable decode inputs cross this boundary. Cancellation may drop
    // the await, so the worker owns capacity until its bounded CPU work finishes;
    // it must never retain a store, publication fence or lookup session.
    runtime
        .spawn_blocking(move || {
            let _permit = permit;
            span.in_scope(operation)
        })
        .await
        .map_err(|source| MetadataError::CapsuleDecodeWorker { source })?
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::sync::{Arc, Condvar, Mutex};

    #[test]
    fn missing_runtime_returns_a_typed_error() {
        use std::future::Future;

        let mut future = Box::pin(super::run(|| Ok(())));
        let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        let std::task::Poll::Ready(Err(crate::error::MetadataError::Io { source })) =
            future.as_mut().poll(&mut context)
        else {
            panic!("expected missing runtime error");
        };
        assert!(
            source
                .get_ref()
                .is_some_and(|source| source.is::<tokio::runtime::TryCurrentError>())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn complete_decoding_does_not_occupy_the_async_worker() {
        let executor = std::thread::current().id();
        let decoder = super::run(|| Ok(std::thread::current().id()))
            .await
            .unwrap();
        assert_ne!(decoder, executor);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abandoned_decoders_retain_capacity_until_their_cpu_work_finishes() {
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                let (ready, wake) = self.0.as_ref();
                *ready.lock().unwrap() = true;
                wake.notify_all();
            }
        }
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(gate.clone());
        let (entered, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let tasks = (0..super::MAX_DECODERS)
            .map(|_| {
                let gate = gate.clone();
                let entered = entered.clone();
                tokio::spawn(super::run(move || {
                    entered.send(()).unwrap();
                    let mut ready = gate.0.lock().unwrap();
                    while !*ready {
                        ready = gate.1.wait(ready).unwrap();
                    }
                    Ok(())
                }))
            })
            .collect::<Vec<_>>();
        for _ in 0..super::MAX_DECODERS {
            observed.recv().await.unwrap();
        }
        for task in tasks {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert_eq!(super::DECODERS.available_permits(), 0);
        drop(release);
        super::run(|| Ok(())).await.unwrap();
    }

    #[tokio::test]
    async fn decoder_worker_failure_retains_its_typed_source() {
        let error = super::run::<(), _>(|| panic!("decoder worker fixture"))
            .await
            .unwrap_err();
        let crate::error::MetadataError::CapsuleDecodeWorker { source } = error else {
            panic!("expected decoder worker error");
        };
        assert!(source.is_panic());
    }
}
