use std::future::{Future, IntoFuture};
use std::pin::Pin;

use crate::Result;

type RequestFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// A lazy SDK operation with defaults that can be configured before awaiting.
///
/// Constructing or dropping an unpolled request performs no storage access or
/// worker admission. Once polled,
/// dropping its future signals cancellation through the SDK worker lifecycle.
#[must_use = "requests perform work only when awaited"]
pub struct Request<'a, T, O> {
    options: O,
    start: Box<dyn FnOnce(O) -> RequestFuture<'a, T> + Send + 'a>,
}

impl<'a, T, O: Default> Request<'a, T, O> {
    pub(crate) fn new(start: impl FnOnce(O) -> RequestFuture<'a, T> + Send + 'a) -> Self {
        Self {
            options: O::default(),
            start: Box::new(start),
        }
    }
}

impl<T, O> Request<'_, T, O> {
    /// Replace the operation's default options before it starts.
    pub fn with_options(mut self, options: O) -> Self {
        self.options = options;
        self
    }
}

impl<'a, T, O> IntoFuture for Request<'a, T, O> {
    type Output = Result<T>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        (self.start)(self.options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn requests_start_only_when_polled_with_the_final_options() {
        let entered = Arc::new(AtomicUsize::new(0));
        let request = || {
            let entered = entered.clone();
            Request::new(move |options: usize| {
                Box::pin(async move {
                    entered.fetch_add(1, Ordering::SeqCst);
                    Ok(options)
                })
            })
        };
        drop(request());
        drop(request().into_future());
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        let selected = request().with_options(1).with_options(2).await.unwrap();
        assert_eq!((selected, entered.load(Ordering::SeqCst)), (2, 1));
    }
}
