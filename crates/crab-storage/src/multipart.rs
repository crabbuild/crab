//! Resumable multipart upload support.
//!
//! [`object_store::multipart::MultipartStore`] exposes explicit
//! `MultipartId` + part indexes, which makes part-level resume possible
//! provider-neutrally. The pieces here are:
//!
//! - [`MultipartJournal`] — persistence boundary for in-flight upload
//!   state. Implemented by `crab-staging`'s SQLite-backed
//!   `MultipartRegistry` so a killed push can resume from its last good
//!   part on the next run.
//! - [`Store::put_multipart_file_resumable_retry`] — the canonical
//!   upload loop that consults the journal, uploads only missing parts,
//!   and keeps completed sessions recoverable across processes.

use tracing::warn;

use crate::error::Result;

/// One successfully uploaded part, as reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalPart {
    /// Zero-based part index within the upload.
    pub part_idx: usize,
    /// Provider-issued content id (S3 ETag) needed by `complete`.
    pub content_id: String,
    /// Part payload size in bytes.
    pub size: u64,
}

/// A resumable upload recorded by a previous process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeInfo {
    pub upload_id: String,
    pub parts: Vec<JournalPart>,
}

/// Persistence for in-flight multipart uploads.
///
/// Implementations must be safe to call from async contexts: every
/// method is a fast local write (single SQLite upsert scale), never a
/// network round-trip.
pub trait MultipartJournal: Send + Sync {
    /// Record a new upload before its first part PUT.
    ///
    /// Returns `false` when another active row already exists for
    /// `payload_hash` (a concurrent uploader owns it); callers must then
    /// proceed without journaling rather than clobbering the other row.
    fn begin(&self, payload_hash: &[u8], bucket: &str, key: &str, upload_id: &str) -> Result<bool>;

    /// Record one successfully uploaded part.
    fn record_part(
        &self,
        upload_id: &str,
        part_idx: usize,
        content_id: &str,
        size: u64,
    ) -> Result<()>;

    /// Drop the row after a successful `complete_multipart`.
    fn complete(&self, upload_id: &str) -> Result<()>;

    /// Drop the row because the upload is being abandoned or was found
    /// incompatible. Best-effort; backend cleanup is the caller's job.
    fn abort_stale(&self, upload_id: &str) -> Result<()>;

    /// Look up a recorded upload for `payload_hash`.
    fn resumable(&self, payload_hash: &[u8]) -> Result<Option<ResumeInfo>>;
}

/// Outcome of trying to claim journal ownership for one upload attempt.
#[derive(Debug, Clone)]
pub(crate) enum JournalLease {
    /// This attempt owns the journal row identified by `upload_id`
    /// (either freshly begun or resumed).
    Active { upload_id: String },
    /// No journaling for this attempt: either the caller passed no
    /// journal, or a concurrent row for the same payload hash exists.
    /// Failed attempts must abort the backend upload since nothing
    /// tracks the orphaned parts.
    StandDown,
}

impl JournalLease {
    pub(crate) fn upload_id(&self) -> Option<&str> {
        match self {
            Self::Active { upload_id } => Some(upload_id),
            Self::StandDown => None,
        }
    }
}

/// Validates a recorded upload against the current upload plan.
///
/// Parts are compatible when every index is inside the plan and every
/// non-final recorded part matches `part_size` exactly (providers such
/// as R2/S3 require fixed non-final part sizes, so a boundary drift
/// makes reuse impossible). Returns the cleaned-up part list indexed by
/// position, or `None` when the row must be discarded.
#[must_use]
pub(crate) fn compatible_parts(
    info: &ResumeInfo,
    total_parts: usize,
    part_size: usize,
) -> Option<Vec<Option<JournalPart>>> {
    let mut slots: Vec<Option<JournalPart>> = vec![None; total_parts];
    let mut boundary: Option<u64> = None;
    for part in &info.parts {
        if part.part_idx >= total_parts || slots[part.part_idx].is_some() {
            return None;
        }
        let is_last = part.part_idx + 1 == total_parts;
        if !is_last {
            match boundary {
                None => boundary = Some(part.size),
                Some(seen) if seen == part.size => {}
                Some(_) => return None,
            }
            if part.size != part_size as u64 {
                return None;
            }
        } else if part.size > part_size as u64 {
            return None;
        }
        slots[part.part_idx] = Some(part.clone());
    }
    Some(slots)
}

/// Logs a journal failure without failing the upload: durability of the
/// resume row is an optimization, never a correctness gate.
pub(crate) fn warn_journal_error(phase: &str, err: crate::error::StorageError) {
    warn!(phase, error = %err, "multipart journal operation failed");
}
