//! Durable multipart-upload recovery contracts.
//!
//! The storage crate owns provider transport while the staging crate owns the
//! local SQLite journal. [`MultipartJournal`] is the narrow composition seam
//! between them: the transport never mutates a recorded upload unless it holds
//! the current lease token, and the journal never performs provider I/O.

use std::time::Duration;

/// Error returned by a multipart journal implementation.
pub type JournalError = Box<dyn std::error::Error + Send + Sync>;

/// Result returned by a multipart journal implementation.
pub type JournalResult<T> = std::result::Result<T, JournalError>;

/// Exact physical destination of one multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartTarget {
    pub provider: String,
    pub host: String,
    pub container: String,
    pub key: String,
}

/// One successfully uploaded provider part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalPart {
    pub part_idx: usize,
    pub content_id: String,
    pub size: u64,
}

/// Opaque proof that one process currently owns a journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLease {
    pub entry_id: String,
    pub owner_token: String,
}

/// State returned after atomically acquiring a new or expired lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalClaim {
    pub lease: JournalLease,
    pub upload_id: Option<String>,
    pub payload_hash: Vec<u8>,
    pub expected_hash: [u8; 32],
    pub size: u64,
    pub part_size: usize,
    pub parts: Vec<JournalPart>,
}

/// Result of attempting to acquire an exact-destination upload lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalClaimOutcome {
    Acquired(JournalClaim),
    Busy,
}

/// Result of a durable multipart upload request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumableUploadOutcome {
    Uploaded,
    Resumed,
    AlreadyPresent,
}

/// Persistence boundary for in-flight multipart uploads.
///
/// Implementations must make `claim` and every ownership-checked mutation
/// atomic across processes. Returning `false` from a mutation means the lease
/// was lost; the caller must stop using the provider session immediately.
#[async_trait::async_trait]
pub trait MultipartJournal: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn claim(
        &self,
        target: &MultipartTarget,
        payload_hash: &[u8],
        expected_hash: &[u8; 32],
        size: u64,
        part_size: usize,
        owner_token: &str,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> JournalResult<JournalClaimOutcome>;

    async fn bind_upload(
        &self,
        lease: &JournalLease,
        upload_id: &str,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> JournalResult<bool>;

    async fn renew(
        &self,
        lease: &JournalLease,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> JournalResult<bool>;

    async fn record_part(
        &self,
        lease: &JournalLease,
        part: &JournalPart,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> JournalResult<bool>;

    #[allow(clippy::too_many_arguments)]
    async fn reset_owned(
        &self,
        lease: &JournalLease,
        payload_hash: &[u8],
        expected_hash: &[u8; 32],
        size: u64,
        part_size: usize,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> JournalResult<bool>;

    async fn complete_owned(
        &self,
        lease: &JournalLease,
        now_unix_seconds: i64,
    ) -> JournalResult<bool>;

    async fn abandon_owned(
        &self,
        lease: &JournalLease,
        now_unix_seconds: i64,
    ) -> JournalResult<bool>;

    /// Expire the caller's lease without deleting resumable provider state.
    async fn release_owned(
        &self,
        lease: &JournalLease,
        now_unix_seconds: i64,
    ) -> JournalResult<bool>;
}

/// Places recorded parts into their exact plan slots.
///
/// A row is compatible only when every part index and byte length matches the
/// current fixed-size plan. This rejects boundary drift before provider
/// completion can assemble a different object.
#[must_use]
pub(crate) fn compatible_parts(
    claim: &JournalClaim,
    size: u64,
    part_size: usize,
) -> Option<Vec<Option<JournalPart>>> {
    if part_size == 0 || claim.size != size || claim.part_size != part_size {
        return None;
    }
    let total_parts = usize::try_from(size.div_ceil(part_size as u64)).ok()?;
    let mut slots = vec![None; total_parts];
    for part in &claim.parts {
        if part.part_idx >= total_parts || slots[part.part_idx].is_some() {
            return None;
        }
        let offset = u64::try_from(part.part_idx).ok()? * part_size as u64;
        let expected_size = (size - offset).min(part_size as u64);
        if part.size != expected_size {
            return None;
        }
        slots[part.part_idx] = Some(part.clone());
    }
    Some(slots)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(parts: Vec<JournalPart>) -> JournalClaim {
        JournalClaim {
            lease: JournalLease {
                entry_id: "entry".into(),
                owner_token: "owner".into(),
            },
            upload_id: Some("upload".into()),
            payload_hash: vec![1; 32],
            expected_hash: [2; 32],
            size: 10,
            part_size: 4,
            parts,
        }
    }

    #[test]
    fn compatible_parts_require_exact_indexes_and_lengths() {
        let parts = vec![
            JournalPart {
                part_idx: 0,
                content_id: "a".into(),
                size: 4,
            },
            JournalPart {
                part_idx: 2,
                content_id: "c".into(),
                size: 2,
            },
        ];

        let slots = compatible_parts(&claim(parts), 10, 4).unwrap();
        assert!(slots[0].is_some());
        assert!(slots[1].is_none());
        assert!(slots[2].is_some());
    }

    #[test]
    fn compatible_parts_reject_wrong_final_length() {
        let parts = vec![JournalPart {
            part_idx: 2,
            content_id: "c".into(),
            size: 4,
        }];

        assert!(compatible_parts(&claim(parts), 10, 4).is_none());
    }
}
