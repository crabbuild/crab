//! Shared structured result shapes for preview/apply command flows.

use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanApplyOperation {
    Preview,
    Apply,
    Inspect,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanApplyStatus {
    NotImplemented,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PlanApplyResult {
    pub command: String,
    pub operation: PlanApplyOperation,
    pub status: PlanApplyStatus,
    pub mutates: bool,
    pub idempotent_apply: bool,
    pub message: String,
}

impl PlanApplyResult {
    #[cfg(test)]
    pub(crate) fn not_implemented(
        command: impl Into<String>,
        operation: PlanApplyOperation,
        idempotent_apply: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            command: command.into(),
            operation,
            status: PlanApplyStatus::NotImplemented,
            mutates: false,
            idempotent_apply,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_apply_reports_no_mutation_and_idempotency_contract() {
        let payload = PlanApplyResult::not_implemented(
            "recover.apply",
            PlanApplyOperation::Apply,
            true,
            "not ready",
        );

        assert_eq!(payload.operation, PlanApplyOperation::Apply);
        assert_eq!(payload.status, PlanApplyStatus::NotImplemented);
        assert!(!payload.mutates);
        assert!(payload.idempotent_apply);
    }

    #[test]
    fn serializes_enums_as_snake_case() -> std::result::Result<(), serde_json::Error> {
        let payload = PlanApplyResult::not_implemented(
            "recover.plan",
            PlanApplyOperation::Preview,
            false,
            "not ready",
        );

        let value = serde_json::to_value(&payload)?;

        assert_eq!(value["operation"], "preview");
        assert_eq!(value["status"], "not_implemented");
        Ok(())
    }
}
