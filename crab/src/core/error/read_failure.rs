use std::error::Error;
use std::fmt;

use crab_read::ReadError;
use crab_types::error::ErrorCategory;

use super::CrabError;
use crate::storage::retry::{RetryClass, retry_class};

/// Source-preserving diagnostics for failures returned through shared reconstruction.
#[derive(Debug)]
pub struct ReadFailure(pub(super) ReadError);

enum Cause<'a> {
    Product(&'a CrabError),
    OriginIntegrity {
        path: &'a str,
        source: &'a crab_cache::CacheError,
    },
    Io(&'a std::io::Error),
    Other,
}

impl ReadFailure {
    fn cause(&self) -> Cause<'_> {
        let mut current: Option<&(dyn Error + 'static)> = Some(&self.0);
        while let Some(error) = current {
            if let Some(error) = error.downcast_ref::<CrabError>() {
                return Cause::Product(error);
            }
            if let Some(crab_cache_store::CacheStoreError::OriginIntegrity { path, source }) =
                error.downcast_ref::<crab_cache_store::CacheStoreError>()
            {
                return Cause::OriginIntegrity { path, source };
            }
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                return Cause::Io(error);
            }
            current = error.source();
        }
        Cause::Other
    }

    pub(crate) fn code(&self) -> &'static str {
        match self.cause() {
            Cause::Product(error) => error.code(),
            Cause::OriginIntegrity { .. } => "CRAB-E0020",
            Cause::Io(_) => "CRAB-E0070",
            Cause::Other => "CRAB-E0099",
        }
    }

    pub(super) fn exit_code(&self) -> u8 {
        match self.cause() {
            Cause::Product(error) => error.exit_code(),
            Cause::OriginIntegrity { .. } => 4,
            Cause::Io(_) => 5,
            Cause::Other => 9,
        }
    }

    pub(super) fn category(&self) -> ErrorCategory {
        match self.cause() {
            Cause::Product(error) => error.category(),
            Cause::OriginIntegrity { .. } => ErrorCategory::Integrity,
            Cause::Io(_) => ErrorCategory::Transport,
            Cause::Other => ErrorCategory::Internal,
        }
    }

    pub(super) fn is_retryable(&self) -> bool {
        matches!(self.cause(), Cause::Product(error) if error.is_retryable())
    }

    pub(crate) fn retry_class(&self) -> RetryClass {
        match self.cause() {
            Cause::Product(error) => match retry_class(error) {
                RetryClass::InspectErrno => RetryClass::Fatal,
                class => class,
            },
            // Origin verification exhausted cache repair. Writer failures can
            // follow partial output; replaying that writer is not safe.
            Cause::OriginIntegrity { .. } | Cause::Io(_) | Cause::Other => RetryClass::Fatal,
        }
    }

    pub(super) fn details_json(&self) -> serde_json::Value {
        match self.cause() {
            Cause::Product(error) => error.details_json(),
            Cause::OriginIntegrity { path, source } => serde_json::json!({
                "path": path,
                "reason": source.to_string(),
                "origin": "object-store",
            }),
            Cause::Io(error) => serde_json::json!({ "message": error.to_string() }),
            Cause::Other => serde_json::json!({ "message": self.0.to_string() }),
        }
    }

    pub(super) fn hint(&self) -> Option<&'static str> {
        match self.cause() {
            Cause::Product(error) => error.hint(),
            _ => None,
        }
    }

    pub(super) fn docs_anchor(&self) -> Option<&'static str> {
        match self.cause() {
            Cause::Product(error) => error.docs_anchor(),
            _ => None,
        }
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.cause() {
            Cause::Product(error) => error.fmt(f),
            Cause::OriginIntegrity { path, source } => write!(
                f,
                "origin object at {path} failed integrity verification [{}]: {source}",
                self.code()
            ),
            Cause::Io(error) => write!(f, "I/O error [{}]: {error}", self.code()),
            Cause::Other => write!(f, "{} [{}]", self.0, self.code()),
        }
    }
}

impl Error for ReadFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}
