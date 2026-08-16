//! `crab errors` — look up error codes.
//!
//! Lists all known error codes or shows the full explanation for a
//! single code. With `--json`, emits structured payloads under the
//! `"errors"` schema.

use serde::Serialize;

use crate::core::error_catalog::{self, ALL_CODES};
use crate::core::output::{OutputMode, emit_json};

/// Payload for `crab errors --json` (no code argument).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ErrorCatalogPayload {
    pub codes: Vec<ErrorDocEntry>,
}

/// One row in the error catalog listing.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ErrorDocEntry {
    pub code: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    pub retryable: bool,
    pub message_template: &'static str,
    pub remediation: &'static str,
}

/// Payload for `crab errors <code> --json` (single-code lookup).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ErrorDocPayload {
    pub code: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    pub retryable: bool,
    pub message_template: &'static str,
    pub remediation: &'static str,
}

/// Map an error code to its category string.
///
/// Mirrors the `CrabError::category()` mapping but works from the
/// static code string rather than requiring an error instance.
fn category_for_code(code: &str) -> &'static str {
    match code {
        "CRAB-E0001" | "CRAB-E0002" => "transient",
        "CRAB-E0010" | "CRAB-E0011" | "CRAB-E0012" | "CRAB-E0017" | "CRAB-E0088" | "CRAB-E0089"
        | "CRAB-E0097" => "conflict",
        "CRAB-E0020" | "CRAB-E0021" | "CRAB-E0082" | "CRAB-E0083" | "CRAB-E0084" | "CRAB-E0101" => {
            "integrity"
        }
        "CRAB-E0030" | "CRAB-E0031" | "CRAB-E0040" | "CRAB-E0041" | "CRAB-E0042" | "CRAB-E0043"
        | "CRAB-E0091" => "permanent",
        "CRAB-E0050" | "CRAB-E0051" | "CRAB-E0052" => "config",
        "CRAB-E0060" | "CRAB-E0070" | "CRAB-E0071" | "CRAB-E0110" => "transport",
        "CRAB-E0080" | "CRAB-E0081" => "staging",
        "CRAB-E0100" | "CRAB-E0102" | "CRAB-E0103" | "CRAB-E0104" | "CRAB-E0105" => "lfs",
        "CRAB-E0090" => "cancelled",
        // CRAB-E0099 and any unknown codes default to internal.
        _ => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_integration_failed_catalog_entry_is_non_retryable_conflict() {
        let entry = doc_entry_for_code("CRAB-E0097").expect("catalog entry");

        assert_eq!(entry.category, "conflict");
        assert!(!entry.retryable);
    }
}

/// Whether the error code represents a retryable condition.
///
/// Mirrors `CrabError::is_retryable()`.
fn is_retryable_code(code: &str) -> bool {
    matches!(
        code,
        "CRAB-E0001" | "CRAB-E0002" | "CRAB-E0010" | "CRAB-E0081"
    )
}

/// Build an [`ErrorDocEntry`] from a catalog code.
fn doc_entry_for_code(code: &'static str) -> Option<ErrorDocEntry> {
    let exp = error_catalog::lookup(code)?;
    Some(ErrorDocEntry {
        code: exp.code,
        name: exp.summary,
        category: category_for_code(code),
        retryable: is_retryable_code(code),
        message_template: exp.summary,
        remediation: exp.remediation,
    })
}

/// Run `crab errors [code]`.
///
/// Returns `true` when the command produced output successfully,
/// `false` when the requested code was not found.
pub fn run_errors(
    mode: OutputMode,
    code: Option<&str>,
) -> std::result::Result<bool, crate::core::error::CrabError> {
    match (mode, code) {
        (OutputMode::Json, None) => {
            let codes: Vec<ErrorDocEntry> = ALL_CODES
                .iter()
                .filter_map(|c| doc_entry_for_code(c))
                .collect();
            emit_json("errors", "1.0", ErrorCatalogPayload { codes });
            Ok(true)
        }
        (OutputMode::Json, Some(raw)) => {
            let normalized = raw.to_uppercase();
            let static_code = ALL_CODES.iter().find(|c| **c == normalized).copied();
            if let Some(entry) = static_code.and_then(doc_entry_for_code) {
                emit_json(
                    "errors",
                    "1.0",
                    ErrorDocPayload {
                        code: entry.code,
                        name: entry.name,
                        category: entry.category,
                        retryable: entry.retryable,
                        message_template: entry.message_template,
                        remediation: entry.remediation,
                    },
                );
                Ok(true)
            } else {
                eprintln!("unknown error code: {raw}");
                eprintln!("run `crab errors` to list all codes");
                Ok(false)
            }
        }
        (_, Some(raw)) => {
            let normalized = raw.to_uppercase();
            if error_catalog::print_explanation(&normalized) {
                Ok(true)
            } else {
                eprintln!("unknown error code: {raw}");
                eprintln!("run `crab errors` to list all codes");
                Ok(false)
            }
        }
        (_, None) => {
            error_catalog::print_all_codes();
            Ok(true)
        }
    }
}
