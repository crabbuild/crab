//! Workflow stage condition contracts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Condition that gates whether a stage executes.
///
/// Evaluated at execution time, not parse time. When false, upper runtime
/// adapters treat the stage as skipped for that run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StageCondition {
    /// Stage runs only if the named environment variable is set and non-empty.
    Env(String),
    /// Stage runs only if the file at the given path exists.
    FileExists(PathBuf),
    /// Simple equality expression, e.g. `"${param} == 'production'"`.
    Expr(String),
}

impl StageCondition {
    /// Evaluate the condition against the current environment and filesystem.
    #[must_use]
    pub fn evaluate(&self, repo_root: &Path) -> bool {
        match self {
            StageCondition::Env(var) => std::env::var(var).ok().is_some_and(|v| !v.is_empty()),
            StageCondition::FileExists(path) => {
                let effective = if path.is_absolute() {
                    path.clone()
                } else {
                    repo_root.join(path)
                };
                effective.exists()
            }
            StageCondition::Expr(expr) => evaluate_expr(expr),
        }
    }
}

/// Evaluate a simple equality expression of the form `LHS == 'RHS'` or
/// `LHS != 'RHS'`.
///
/// The left-hand side is treated as a literal because template substitution has
/// already resolved `${...}` before evaluation.
#[must_use]
pub fn evaluate_expr(expr: &str) -> bool {
    if let Some((lhs, rhs)) = expr.split_once("!=") {
        let lhs = lhs.trim().trim_matches('\'').trim_matches('"');
        let rhs = rhs.trim().trim_matches('\'').trim_matches('"');
        lhs != rhs
    } else if let Some((lhs, rhs)) = expr.split_once("==") {
        let lhs = lhs.trim().trim_matches('\'').trim_matches('"');
        let rhs = rhs.trim().trim_matches('\'').trim_matches('"');
        lhs == rhs
    } else {
        !expr.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_expr_evaluates_equality() {
        assert!(StageCondition::Expr("'prod' == \"prod\"".to_owned()).evaluate(Path::new(".")));
        assert!(!StageCondition::Expr("prod == dev".to_owned()).evaluate(Path::new(".")));
    }

    #[test]
    fn condition_expr_evaluates_inequality() {
        assert!(StageCondition::Expr("prod != dev".to_owned()).evaluate(Path::new(".")));
        assert!(!StageCondition::Expr("'prod' != \"prod\"".to_owned()).evaluate(Path::new(".")));
    }

    #[test]
    fn condition_expr_treats_unknown_non_empty_expression_as_truthy() {
        assert!(StageCondition::Expr("ready".to_owned()).evaluate(Path::new(".")));
        assert!(!StageCondition::Expr("   ".to_owned()).evaluate(Path::new(".")));
    }

    #[test]
    fn condition_file_exists_resolves_relative_to_repo_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("flag"), b"ok").unwrap();

        assert!(
            StageCondition::FileExists(PathBuf::from("flag")).evaluate(tmp.path()),
            "relative file condition should resolve inside repo root"
        );
        assert!(!StageCondition::FileExists(PathBuf::from("missing")).evaluate(tmp.path()));
    }

    #[test]
    fn condition_env_is_false_for_missing_variable() {
        let name = "CRAB_WORKFLOW_CONDITION_TEST_VARIABLE_SHOULD_NOT_EXIST";
        assert!(!StageCondition::Env(name.to_owned()).evaluate(Path::new(".")));
    }
}
