//! Audit seam for tier, xorb optimization, and class-aware GC call sites.
//!
//! Call sites:
//! - `tier::apply` — on successful lifecycle apply.
//! - `tier::apply::rollback` — on rollback.
//! - `cmd/gc` — on `--force-early-delete`.
//! - `optimize::xorbs::executor` — on start and finalize.

use std::path::PathBuf;

use serde::Serialize;
use tracing::{debug, warn};

#[cfg(not(test))]
use crate::audit::default_log_path;
use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event};

/// Operations that the audit subsystem records.
#[derive(Debug, Clone, Copy)]
pub enum AuditOp {
    /// Lifecycle rules applied to a bucket.
    TierApply,
    /// Lifecycle rules rolled back from a backup.
    TierRollback,
    /// GC force-deleted an object inside its minimum-retention window.
    ForceEarlyDelete,
    /// An xorb optimization run started.
    OptimizeXorbsStart,
    /// An xorb optimization run finalized (reconciliation complete).
    OptimizeXorbsFinalize,
}

impl AuditOp {
    fn operation(self) -> &'static str {
        match self {
            Self::TierApply => "tier.apply",
            Self::TierRollback => "tier.rollback",
            Self::ForceEarlyDelete => "gc.force_early_delete",
            Self::OptimizeXorbsStart => "optimize.xorbs.start",
            Self::OptimizeXorbsFinalize => "optimize.xorbs.finalize",
        }
    }
}

/// Record an auditable operation.
pub fn record(op: AuditOp, payload: &impl Serialize) {
    let Some(path) = audit_log_path() else {
        debug!(?op, "audit_shim: test audit path not configured");
        return;
    };
    let details = serde_json::to_value(payload).unwrap_or_else(|err| {
        serde_json::json!({
            "serialization_error": err.to_string()
        })
    });
    let event = AuditEvent::new(NewAuditEvent {
        operation: op.operation().to_owned(),
        outcome: AuditOutcome::Success,
        actor: None,
        repository: None,
        details,
    });

    if let Err(err) = append_event(&path, &event) {
        warn!(?op, path = %path.display(), %err, "failed to append audit event");
    }
}

#[cfg(not(test))]
fn audit_log_path() -> Option<PathBuf> {
    Some(default_log_path())
}

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static TEST_AUDIT_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn audit_log_path() -> Option<PathBuf> {
    TEST_AUDIT_PATH.with(|path| path.borrow().clone())
}

#[cfg(test)]
pub(crate) struct TestAuditPathGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for TestAuditPathGuard {
    fn drop(&mut self) {
        TEST_AUDIT_PATH.with(|path| *path.borrow_mut() = self.previous.take());
    }
}

#[cfg(test)]
pub(crate) fn set_test_audit_path(path: PathBuf) -> TestAuditPathGuard {
    let previous = TEST_AUDIT_PATH.with(|current| current.borrow_mut().replace(path));
    TestAuditPathGuard { previous }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::audit::read_events;

    #[test]
    fn record_appends_real_audit_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let _guard = set_test_audit_path(path.clone());

        record(
            AuditOp::TierApply,
            &serde_json::json!({
                "rule_count": 2,
                "access_token": "secret-token",
            }),
        );

        let events = read_events(&path).expect("read audit events");
        let event = events
            .iter()
            .find(|event| event.operation == "tier.apply")
            .expect("tier.apply audit event");
        assert_eq!(event.outcome, AuditOutcome::Success);
        assert_eq!(event.details["rule_count"], 2);
        assert_eq!(event.details["access_token"], "[REDACTED]");
        assert!(event.digest_valid());
    }
}
