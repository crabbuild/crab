use std::{future::Future, sync::Arc};

use crab_coordination::read_retention_runtime::{ReadRetentionPin, RetainedRead};
use tokio_util::sync::CancellationToken;

use crate::{ReadError, Result};

pub(crate) fn read<'a, T: Send + 'a, F>(
    retained: Option<&'a RetainedRead>,
    cancel: &'a CancellationToken,
    operation: impl FnOnce(Option<Arc<ReadRetentionPin>>, CancellationToken) -> F + Send + 'a,
) -> futures_util::future::BoxFuture<'a, Result<T>>
where
    F: Future<Output = Result<T>> + Send + 'a,
{
    // Keep provider future types behind the lifetime boundary. Mount callbacks
    // otherwise inherit the full transport and reconstruction state machines.
    Box::pin(async move {
        let Some(retained) = retained else {
            return operation(None, cancel.clone()).await;
        };
        let pin = retained.pin()?;
        let operation_cancel = cancel.child_token();
        let _cancel_on_drop = operation_cancel.clone().drop_guard();
        let operation = operation(Some(Arc::clone(&pin)), operation_cancel.clone());
        tokio::pin!(operation);
        let outcome = tokio::select! {
            biased;
            () = pin.cancellation().cancelled() => {
                // Cancellation requests a stop; it cannot release ownership while
                // the reconstructor or its blocking writer is still draining.
                operation_cancel.cancel();
                operation.await
            }
            outcome = &mut operation => outcome,
        };
        match outcome {
            Err(error) if !matches!(error, ReadError::Cancelled) => Err(error),
            outcome => {
                pin.ensure_valid()?;
                outcome
            }
        }
    })
}
