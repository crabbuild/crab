use std::fmt;

/// Result returned by SDK operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Stable categories for programmatic error handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    InvalidInput,
    LocalSetupRequired,
    UnsupportedCapability,
    Authentication,
    Authorization,
    NotFound,
    Conflict,
    Indexing,
    Corruption,
    LimitExceeded,
    Timeout,
    Cancelled,
    Io,
    Transport,
}

/// Process-local identity allocated for one SDK operation attempt.
///
/// Identities are diagnostic correlation values, not persistent recovery tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(u64);

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

impl OperationId {
    #[cfg(feature = "remote")]
    pub(crate) fn allocate() -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map(Self)
        .map_err(|_| Error::new(ErrorKind::LimitExceeded, "operation identities exhausted"))
    }
}

/// An SDK failure with safe context and a preserved diagnostic source.
#[derive(thiserror::Error)]
#[error("{context}")]
pub struct Error {
    kind: ErrorKind,
    operation: Option<OperationId>,
    context: &'static str,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    cleanup: Option<Box<Error>>,
}

impl Error {
    /// Return the stable category without inspecting diagnostic strings.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Return the operation identity, or None for validation before admission.
    #[must_use]
    pub fn operation_id(&self) -> Option<OperationId> {
        self.operation
    }

    #[cfg(feature = "remote")]
    pub(crate) fn with_operation(mut self, operation: OperationId) -> Self {
        self.operation = Some(operation);
        if let Some(cleanup) = self.cleanup.take() {
            self.cleanup = Some(Box::new(cleanup.with_operation(operation)));
        }
        self
    }

    /// Return a secondary cleanup failure without replacing the operation failure.
    #[must_use]
    pub fn cleanup_error(&self) -> Option<&Error> {
        self.cleanup.as_deref()
    }

    #[cfg(feature = "remote")]
    pub(crate) fn with_cleanup(mut self, cleanup: Error) -> Self {
        self.cleanup = Some(Box::new(cleanup));
        self
    }

    #[cfg(feature = "remote")]
    pub(crate) fn with_deadline_kind(mut self) -> Self {
        self.kind = ErrorKind::Timeout;
        self.context = "operation deadline expired";
        self
    }

    pub(crate) fn new(kind: ErrorKind, context: &'static str) -> Self {
        Self {
            kind,
            operation: None,
            context,
            source: None,
            cleanup: None,
        }
    }

    pub(crate) fn with_source(
        kind: ErrorKind,
        context: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            operation: None,
            context,
            source: Some(Box::new(source)),
            cleanup: None,
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Dependency diagnostics can contain input or placement details. Keep
        // ordinary logs bounded; callers explicitly opt into the source chain.
        formatter
            .debug_struct("Error")
            .field("kind", &self.kind)
            .field("operation", &self.operation)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}
