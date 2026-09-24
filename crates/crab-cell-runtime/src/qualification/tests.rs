use super::profile::{
    MAX_QUALIFICATION_CELLS, MAX_QUALIFICATION_CONCURRENCY, MAX_QUALIFICATION_DURATION_SECS,
    MAX_QUALIFICATION_OPERATIONS, MAX_RECEIPT_BYTES,
};
use super::workload::qualification_run_outcome_digest;
use super::*;

// Capability modules keep the receipt suite navigable; the shared imports
// stay here.
mod evidence;
mod matrix;
mod profile;
mod receipt;
mod workload;

struct ContractExecutor {
    calls: u64,
    case_coverage: bool,
}

impl QualificationOperationExecutor for ContractExecutor {
    type Future<'a> = std::future::Ready<Result<QualificationExecution>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        self.calls = self.calls.saturating_add(1);
        let execution = if operation.rejection_hint() {
            QualificationExecution::rejected()
        } else if operation.ambiguous_hint() {
            QualificationExecution::ambiguous(u64::from(operation.retry_hint()))
        } else {
            QualificationExecution::acknowledged(true)
                .with_retries(u64::from(operation.retry_hint()))
        };
        let execution = if self.case_coverage {
            execution.with_case(operation.case())
        } else {
            execution
        };
        std::future::ready(Ok(execution))
    }
}

#[derive(Clone)]
struct ConcurrentExecutor {
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    maximum: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    fail_at: Option<u64>,
}

impl QualificationOperationExecutor for ConcurrentExecutor {
    type Future<'a> =
        std::pin::Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let active = std::sync::Arc::clone(&self.active);
        let maximum = std::sync::Arc::clone(&self.maximum);
        let fail_at = self.fail_at;
        Box::pin(async move {
            let current = active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            maximum.fetch_max(current, std::sync::atomic::Ordering::AcqRel);
            tokio::task::yield_now().await;
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            if Some(operation.index()) == fail_at {
                return Err(Error::Control("qualification executor failed"));
            }
            let execution = if operation.rejection_hint() {
                QualificationExecution::rejected()
            } else if operation.ambiguous_hint() {
                QualificationExecution::ambiguous(u64::from(operation.retry_hint()))
            } else {
                QualificationExecution::acknowledged(true)
                    .with_retries(u64::from(operation.retry_hint()))
            };
            Ok(execution)
        })
    }
}
