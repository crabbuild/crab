//! Error classification and structured error info for JSON output.
//!
use schemars::JsonSchema;
use serde::Serialize;

use crate::core::error::CrabError;
use crab_types::error::ErrorCategory;

/// Structured error representation for the JSON error envelope.
///
/// Populated from a `&CrabError` via the `From` impl. Contains the
/// stable error code, classification, human message, retry hint,
/// variant-specific details, and the `std::error::Error::source()` chain.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ErrorInfo {
    pub code: String,
    pub category: ErrorCategory,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_chain: Vec<ErrorSource>,
}

/// One link in the `std::error::Error::source()` chain.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ErrorSource {
    pub message: String,
}

/// Maximum depth when walking the error source chain.
const MAX_SOURCE_CHAIN_DEPTH: usize = 8;

impl From<&CrabError> for ErrorInfo {
    fn from(err: &CrabError) -> Self {
        let mut source_chain = Vec::new();
        let mut current: Option<&dyn std::error::Error> = std::error::Error::source(err);
        for _ in 0..MAX_SOURCE_CHAIN_DEPTH {
            match current {
                Some(src) => {
                    source_chain.push(ErrorSource {
                        message: src.to_string(),
                    });
                    current = src.source();
                }
                None => break,
            }
        }

        Self {
            code: err.code().to_owned(),
            category: err.category(),
            message: err.to_string(),
            retryable: err.is_retryable(),
            details: err.details_json(),
            source_chain,
        }
    }
}
