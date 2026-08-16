//! Lazy, async, retryable one-shot initializer for SlateDB session handles.
//!
//! Wraps [`tokio::sync::OnceCell`] with the specific `Arc<T>` + retry-on-error
//! semantics the metadb session needs. Multiple concurrent tasks calling
//! [`OnceAsync::get_or_init`] observe exactly one successful initialization;
//! once a handle is installed, subsequent calls return the shared reference
//! without re-running the initializer. If the initializer fails, the cell
//! stays empty so the next caller can retry — a transient S3 error during
//! SlateDB open should not poison the handle for the rest of the session.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::core::error::Result;

/// Lazy, async, retryable one-shot initializer.
///
/// Wraps [`tokio::sync::OnceCell`] with the specific `Arc<T>` + retry-on-error
/// semantics the metadb session needs: successful inits are memoized, failed
/// inits leave the cell empty so the next caller can retry.
pub struct OnceAsync<T> {
    cell: OnceCell<Arc<T>>,
}

impl<T> OnceAsync<T>
where
    T: Send + Sync + 'static,
{
    /// Construct an uninitialized cell.
    pub fn new() -> Self {
        Self {
            cell: OnceCell::new(),
        }
    }

    /// Return the installed value, or run `f` to produce one.
    ///
    /// Concurrent callers synchronize on the underlying [`OnceCell`]: exactly
    /// one task's future runs to completion; the rest observe the installed
    /// `Arc<T>` once it lands. If the initializer returns `Err`, the cell is
    /// left empty and the next caller may retry.
    pub async fn get_or_init<F, Fut>(&self, f: F) -> Result<&Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<T>>>,
    {
        self.cell.get_or_try_init(f).await
    }

    /// Return the installed value without initializing.
    ///
    /// Used by `close_all` to skip handles that were never opened in this
    /// session.
    pub fn get(&self) -> Option<&Arc<T>> {
        self.cell.get()
    }

    /// Install a value directly, skipping lazy initialization.
    ///
    /// Returns `true` if the value was accepted and `false` if the cell
    /// had already been populated by a concurrent `get_or_init` (in
    /// which case the passed `value` is dropped). Used by the push
    /// pipeline to share an already-opened `PersistentChunkIndex`
    /// handle with a fresh `MetaDb` session so a second `open_or_create`
    /// on the same warm-tier path never fires.
    pub fn set(&self, value: Arc<T>) -> bool {
        self.cell.set(value).is_ok()
    }
}

impl<T> Default for OnceAsync<T>
where
    T: Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::core::error::CrabError;

    #[tokio::test]
    async fn once_async_initializes_exactly_once() {
        let once: Arc<OnceAsync<String>> = Arc::new(OnceAsync::new());
        let init_count = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let once = Arc::clone(&once);
            let init_count = Arc::clone(&init_count);
            handles.push(tokio::spawn(async move {
                let value = once
                    .get_or_init(|| async {
                        init_count.fetch_add(1, Ordering::SeqCst);
                        // Yield so concurrent callers actually race rather
                        // than running the initializer back-to-back.
                        tokio::task::yield_now().await;
                        Ok(Arc::new(String::from("handle")))
                    })
                    .await
                    .expect("initializer succeeds");

                Arc::clone(value)
            }));
        }

        let mut pointers = Vec::new();
        for handle in handles {
            pointers.push(handle.await.expect("task joins"));
        }

        assert_eq!(
            init_count.load(Ordering::SeqCst),
            1,
            "initializer should run exactly once across concurrent callers"
        );

        // Every caller sees the same Arc (same allocation).
        let first = Arc::as_ptr(&pointers[0]);
        for p in &pointers[1..] {
            assert_eq!(
                Arc::as_ptr(p),
                first,
                "all callers should observe the same shared Arc"
            );
        }

        // And the cached accessor returns it too.
        let cached = once.get().expect("value is installed");
        assert_eq!(Arc::as_ptr(cached), first);
    }

    #[tokio::test]
    async fn once_async_allows_retry_after_failure() {
        let once: OnceAsync<u32> = OnceAsync::new();

        let first = once
            .get_or_init(|| async { Err::<Arc<u32>, _>(CrabError::Internal("boom".into())) })
            .await;
        assert!(
            first.is_err(),
            "first call propagates the initializer error"
        );
        assert!(once.get().is_none(), "failed init must not install a value");

        let second = once
            .get_or_init(|| async { Ok(Arc::new(42u32)) })
            .await
            .expect("retry succeeds");
        assert_eq!(**second, 42);

        // Third call short-circuits — the initializer is not invoked.
        let third = once
            .get_or_init(|| async { panic!("should not re-run after success") })
            .await
            .expect("cached value returned");
        assert!(Arc::ptr_eq(second, third));
    }
}
