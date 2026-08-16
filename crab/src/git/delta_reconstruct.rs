//! Delta-based file reconstruction for time-travel scenarios.
//!
//! When switching between file versions (e.g., `git checkout` to a different
//! commit), the naive approach reconstructs the target version from scratch
//! by fetching all chunks from object storage. For a 10 GB file where only
//! 45 MB changed, this wastes ~99.6% of the bandwidth.
//!
//! This module exploits crab's CDC (content-defined chunking) architecture
//! to reconstruct a target version by reusing unchanged chunks from a base
//! version that's already available locally. The algorithm:
//!
//! 1. Compare the base and target reconstruction term lists (metadata-only,
//!    zero data transfer) to identify which segments are unchanged vs. new.
//! 2. Copy unchanged segments directly from the base content (local I/O).
//! 3. Fetch only the new/changed segments from object storage.
//!
//! For typical ML workflows (model checkpoint with one layer updated), this
//! reduces reconstruction from minutes to seconds.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;

use crate::cache::ChunkCache;
use crate::core::error::{CrabError, Result};
use crate::git::smudge::{ReconstructionTerm, XorbFetcher};
use crab_xet::xorb::format::MerkleHash;

// ---------------------------------------------------------------------------
// Segment identity — the key used to match segments across versions
// ---------------------------------------------------------------------------

/// Identity of a reconstruction segment: `(xorb_hash, chunk_hash)`.
///
/// Two segments are considered identical when they reference the same xorb
/// range and produce the same chunk hash. This is the same equality
/// semantics used by `crab_diff::chunk_comparator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SegmentId {
    xorb_hash: [u8; 32],
    chunk_hash: [u8; 32],
}

impl SegmentId {
    fn from_term(term: &ReconstructionTerm) -> Self {
        Self {
            xorb_hash: term.xorb_hash,
            chunk_hash: term.chunk_hash,
        }
    }
}

// ---------------------------------------------------------------------------
// DeltaPlan — the result of comparing base and target term lists
// ---------------------------------------------------------------------------

/// A step in the delta reconstruction plan.
#[derive(Debug, Clone)]
enum ReconstructionStep {
    /// Copy bytes from the base content at the given byte range.
    CopyFromBase { offset: u64, length: u64 },
    /// Fetch this term from object storage (or chunk cache).
    FetchNew { term: ReconstructionTerm },
}

/// Plan for reconstructing a target version from a base version.
///
/// Produced by [`plan_delta`], consumed by [`execute_delta`].
#[derive(Debug)]
struct DeltaPlan {
    steps: Vec<ReconstructionStep>,
    /// Total bytes in the target file.
    target_size: u64,
    /// Bytes reused from the base (local copy, no network).
    reused_bytes: u64,
    /// Bytes that must be fetched from object storage.
    fetch_bytes: u64,
    /// Number of segments reused from base.
    reused_segments: u32,
    /// Number of segments that need fetching.
    fetch_segments: u32,
}

/// Build a delta reconstruction plan by comparing base and target term lists.
///
/// Builds a lookup table from the base terms (segment identity → byte offset),
/// then walks the target terms. For each target segment, if a matching base
/// segment exists, emit a `CopyFromBase` step; otherwise emit `FetchNew`.
///
/// Runs in O(base_len + target_len) time and O(base_len) space.
fn plan_delta(base_terms: &[ReconstructionTerm], target_terms: &[ReconstructionTerm]) -> DeltaPlan {
    // Build index: segment identity → (byte_offset, length) in the base content.
    // When the same segment appears multiple times, we keep the first occurrence.
    let mut base_index: HashMap<SegmentId, (u64, u64)> = HashMap::with_capacity(base_terms.len());
    let mut base_offset: u64 = 0;
    for term in base_terms {
        let id = SegmentId::from_term(term);
        base_index.entry(id).or_insert((base_offset, term.length));
        base_offset += term.length;
    }

    let mut steps = Vec::with_capacity(target_terms.len());
    let mut reused_bytes: u64 = 0;
    let mut fetch_bytes: u64 = 0;
    let mut reused_segments: u32 = 0;
    let mut fetch_segments: u32 = 0;

    for term in target_terms {
        let id = SegmentId::from_term(term);
        if let Some(&(offset, length)) = base_index.get(&id) {
            steps.push(ReconstructionStep::CopyFromBase { offset, length });
            reused_bytes += length;
            reused_segments += 1;
        } else {
            steps.push(ReconstructionStep::FetchNew { term: term.clone() });
            fetch_bytes += term.length;
            fetch_segments += 1;
        }
    }

    let target_size = reused_bytes + fetch_bytes;

    DeltaPlan {
        steps,
        target_size,
        reused_bytes,
        fetch_bytes,
        reused_segments,
        fetch_segments,
    }
}

// ---------------------------------------------------------------------------
// Execution — run the plan against base content + xorb fetcher
// ---------------------------------------------------------------------------

/// Execute a delta plan, producing the target file content.
///
/// `base_content` is the fully materialized base version (e.g., the file
/// currently on disk). For `CopyFromBase` steps, bytes are copied directly
/// from this buffer. For `FetchNew` steps, chunks are fetched via the
/// `xorb_fetcher` (with chunk cache as a first-level lookup).
fn execute_delta(
    plan: &DeltaPlan,
    base_content: &[u8],
    xorb_fetcher: &dyn XorbFetcher,
    chunk_cache: Option<&ChunkCache>,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(plan.target_size as usize);

    for step in &plan.steps {
        match step {
            ReconstructionStep::CopyFromBase { offset, length } => {
                let start = *offset as usize;
                let end = start + *length as usize;
                if end > base_content.len() {
                    return Err(CrabError::Internal(format!(
                        "delta copy out of bounds: offset={offset}, length={length}, \
                         base_len={}",
                        base_content.len()
                    )));
                }
                output.extend_from_slice(&base_content[start..end]);
            }
            ReconstructionStep::FetchNew { term } => {
                let chunk_data = fetch_chunk(term, xorb_fetcher, chunk_cache)?;
                output.extend_from_slice(&chunk_data);
            }
        }
    }

    Ok(output)
}

/// Fetch a single chunk, checking the cache first.
///
/// On cache miss, fetches from the xorb via Range GET, verifies the blake3
/// hash, and stores the result in the cache for future reuse.
fn fetch_chunk(
    term: &ReconstructionTerm,
    xorb_fetcher: &dyn XorbFetcher,
    chunk_cache: Option<&ChunkCache>,
) -> Result<Vec<u8>> {
    let chunk_hash = MerkleHash::from_slice(&term.chunk_hash).ok();

    // Check chunk cache first.
    if let (Some(cache), Some(hash)) = (chunk_cache, &chunk_hash)
        && let Some(cached) = cache.get(hash)
    {
        return Ok(cached.to_vec());
    }

    // Cache miss — fetch from object storage.
    let range: Range<u64> = term.offset..term.offset + term.length;
    let chunk_data = xorb_fetcher.fetch_range(&term.xorb_hash, range)?;

    // Per-chunk blake3 verification.
    let actual_hash = *blake3::hash(&chunk_data).as_bytes();
    if actual_hash != term.chunk_hash {
        return Err(CrabError::HashMismatch {
            requested: hex_encode(&term.chunk_hash),
            actual: hex_encode(&actual_hash),
        });
    }

    // Store in cache for reuse.
    if let (Some(cache), Some(hash)) = (chunk_cache, chunk_hash) {
        cache.put(hash, Bytes::from(chunk_data.clone()));
    }

    Ok(chunk_data)
}

/// Encode a 32-byte hash as lowercase hex.
fn hex_encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of a delta reconstruction, including the content and metrics.
#[derive(Debug)]
pub struct DeltaResult {
    /// The fully reconstructed target file content.
    pub content: Vec<u8>,
    /// Bytes reused from the base version (zero network I/O).
    pub reused_bytes: u64,
    /// Bytes fetched from object storage.
    pub fetched_bytes: u64,
    /// Number of segments reused from base.
    pub reused_segments: u32,
    /// Number of segments fetched from storage.
    pub fetched_segments: u32,
}

/// Reconstruct a target file version by reusing unchanged chunks from a
/// base version.
///
/// This is the primary entry point for delta-based reconstruction. Given:
/// - `base_terms`: reconstruction terms for the version we already have
/// - `base_content`: the fully materialized base file (bytes on disk)
/// - `target_terms`: reconstruction terms for the version we want
/// - `xorb_fetcher`: fetches byte ranges from xorbs on object storage
/// - `chunk_cache`: optional shared chunk cache
///
/// Returns the reconstructed target content along with transfer metrics.
///
/// When the base and target share most chunks (typical for consecutive
/// versions of large files), this avoids fetching the shared chunks
/// entirely — only the delta is transferred.
///
/// # Errors
///
/// Returns `CrabError::HashMismatch` if any fetched chunk fails blake3
/// verification. Returns `CrabError::Internal` if the base content is
/// shorter than expected by the reconstruction terms.
pub fn reconstruct_from_delta(
    base_terms: &[ReconstructionTerm],
    base_content: &[u8],
    target_terms: &[ReconstructionTerm],
    xorb_fetcher: &dyn XorbFetcher,
    chunk_cache: Option<&Arc<ChunkCache>>,
) -> Result<DeltaResult> {
    let plan = plan_delta(base_terms, target_terms);

    tracing::debug!(
        reused_segments = plan.reused_segments,
        fetch_segments = plan.fetch_segments,
        reused_bytes = plan.reused_bytes,
        fetch_bytes = plan.fetch_bytes,
        target_size = plan.target_size,
        "delta reconstruction plan"
    );

    let cache_ref = chunk_cache.map(std::convert::AsRef::as_ref);
    let content = execute_delta(&plan, base_content, xorb_fetcher, cache_ref)?;

    Ok(DeltaResult {
        content,
        reused_bytes: plan.reused_bytes,
        fetched_bytes: plan.fetch_bytes,
        reused_segments: plan.reused_segments,
        fetched_segments: plan.fetch_segments,
    })
}

/// Check whether delta reconstruction is worthwhile for the given term lists.
///
/// Returns the fraction of target segments that can be reused from the base
/// (0.0 = no overlap, 1.0 = identical). Callers can use this to decide
/// whether to take the delta path or fall back to full reconstruction.
///
/// This is a metadata-only check — no data is fetched.
pub fn estimate_reuse_ratio(
    base_terms: &[ReconstructionTerm],
    target_terms: &[ReconstructionTerm],
) -> f64 {
    if target_terms.is_empty() {
        return 0.0;
    }

    let base_set: std::collections::HashSet<SegmentId> =
        base_terms.iter().map(SegmentId::from_term).collect();

    let reusable = target_terms
        .iter()
        .filter(|t| base_set.contains(&SegmentId::from_term(t)))
        .count();

    reusable as f64 / target_terms.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::smudge::XorbFetcher;

    /// Test xorb fetcher that serves pre-loaded chunk data keyed by
    /// `(xorb_hash, offset, length)`.
    struct TestXorbFetcher {
        data: HashMap<([u8; 32], u64, u64), Vec<u8>>,
    }

    impl TestXorbFetcher {
        fn new() -> Self {
            Self {
                data: HashMap::new(),
            }
        }

        fn insert(&mut self, xorb_hash: [u8; 32], offset: u64, data: Vec<u8>) {
            let length = data.len() as u64;
            self.data.insert((xorb_hash, offset, length), data);
        }
    }

    impl XorbFetcher for TestXorbFetcher {
        fn fetch_range(
            &self,
            xorb_hash: &[u8; 32],
            range: Range<u64>,
        ) -> crab_vfs::Result<Vec<u8>> {
            let key = (*xorb_hash, range.start, range.end - range.start);
            self.data
                .get(&key)
                .cloned()
                .ok_or_else(|| crab_vfs::VfsError::NotFound {
                    path: format!("xorb/{}", hex_encode(xorb_hash)),
                })
        }
    }

    fn make_term(xorb_seed: u8, offset: u64, data: &[u8]) -> ReconstructionTerm {
        let mut xorb_hash = [0u8; 32];
        xorb_hash[0] = xorb_seed;
        let chunk_hash = *blake3::hash(data).as_bytes();
        ReconstructionTerm {
            xorb_hash,
            offset,
            length: data.len() as u64,
            chunk_hash,
        }
    }

    #[test]
    fn identical_versions_reuse_everything() {
        let chunk_a = b"hello world chunk A";
        let chunk_b = b"another chunk B here";

        let terms = vec![make_term(1, 0, chunk_a), make_term(2, 0, chunk_b)];

        let base_content: Vec<u8> = [&chunk_a[..], &chunk_b[..]].concat();
        let fetcher = TestXorbFetcher::new();

        let result = reconstruct_from_delta(&terms, &base_content, &terms, &fetcher, None).unwrap();

        assert_eq!(result.content, base_content);
        assert_eq!(result.reused_segments, 2);
        assert_eq!(result.fetched_segments, 0);
        assert_eq!(result.fetched_bytes, 0);
        assert_eq!(result.reused_bytes, base_content.len() as u64);
    }

    #[test]
    fn completely_different_versions_fetch_everything() {
        let chunk_a = b"base chunk A";
        let chunk_b = b"base chunk B";
        let chunk_c = b"new chunk C!!";
        let chunk_d = b"new chunk D!!";

        let base_terms = vec![make_term(1, 0, chunk_a), make_term(2, 0, chunk_b)];
        let target_terms = vec![make_term(3, 0, chunk_c), make_term(4, 0, chunk_d)];

        let base_content: Vec<u8> = [&chunk_a[..], &chunk_b[..]].concat();

        let mut fetcher = TestXorbFetcher::new();
        fetcher.insert(target_terms[0].xorb_hash, 0, chunk_c.to_vec());
        fetcher.insert(target_terms[1].xorb_hash, 0, chunk_d.to_vec());

        let result =
            reconstruct_from_delta(&base_terms, &base_content, &target_terms, &fetcher, None)
                .unwrap();

        let expected: Vec<u8> = [&chunk_c[..], &chunk_d[..]].concat();
        assert_eq!(result.content, expected);
        assert_eq!(result.reused_segments, 0);
        assert_eq!(result.fetched_segments, 2);
    }

    #[test]
    fn partial_overlap_reuses_shared_chunks() {
        let chunk_a = b"shared chunk A stays the same";
        let chunk_b = b"old chunk B will be replaced";
        let chunk_c = b"shared chunk C stays the same";
        let chunk_new = b"brand new chunk replacing B";

        let base_terms = vec![
            make_term(1, 0, chunk_a),
            make_term(2, 0, chunk_b),
            make_term(3, 0, chunk_c),
        ];
        let target_terms = vec![
            make_term(1, 0, chunk_a),   // same as base
            make_term(9, 0, chunk_new), // new
            make_term(3, 0, chunk_c),   // same as base
        ];

        let base_content: Vec<u8> = [&chunk_a[..], &chunk_b[..], &chunk_c[..]].concat();

        let mut fetcher = TestXorbFetcher::new();
        fetcher.insert(target_terms[1].xorb_hash, 0, chunk_new.to_vec());

        let result =
            reconstruct_from_delta(&base_terms, &base_content, &target_terms, &fetcher, None)
                .unwrap();

        let expected: Vec<u8> = [&chunk_a[..], &chunk_new[..], &chunk_c[..]].concat();
        assert_eq!(result.content, expected);
        assert_eq!(result.reused_segments, 2);
        assert_eq!(result.fetched_segments, 1);
        assert_eq!(result.reused_bytes, (chunk_a.len() + chunk_c.len()) as u64);
        assert_eq!(result.fetched_bytes, chunk_new.len() as u64);
    }

    #[test]
    fn hash_mismatch_returns_error() {
        let chunk_a = b"chunk A";
        let base_terms = vec![make_term(1, 0, chunk_a)];
        let base_content = chunk_a.to_vec();

        // Target has a new chunk.
        let chunk_new = b"new chunk";
        let target_terms = vec![make_term(5, 0, chunk_new)];

        // Fetcher returns data with the right length but wrong content.
        let mut corrupted = chunk_new.to_vec();
        corrupted[0] = b'X';
        let mut fetcher = TestXorbFetcher::new();
        fetcher.insert(target_terms[0].xorb_hash, 0, corrupted);

        let result =
            reconstruct_from_delta(&base_terms, &base_content, &target_terms, &fetcher, None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CrabError::HashMismatch { .. }),
            "expected HashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn empty_target_produces_empty_output() {
        let chunk_a = b"some data";
        let base_terms = vec![make_term(1, 0, chunk_a)];
        let base_content = chunk_a.to_vec();
        let fetcher = TestXorbFetcher::new();

        let result =
            reconstruct_from_delta(&base_terms, &base_content, &[], &fetcher, None).unwrap();

        assert!(result.content.is_empty());
        assert_eq!(result.reused_segments, 0);
        assert_eq!(result.fetched_segments, 0);
    }

    #[test]
    fn empty_base_fetches_everything() {
        let chunk_a = b"target chunk A";
        let chunk_b = b"target chunk B";
        let target_terms = vec![make_term(1, 0, chunk_a), make_term(2, 0, chunk_b)];

        let mut fetcher = TestXorbFetcher::new();
        fetcher.insert(target_terms[0].xorb_hash, 0, chunk_a.to_vec());
        fetcher.insert(target_terms[1].xorb_hash, 0, chunk_b.to_vec());

        let result = reconstruct_from_delta(&[], &[], &target_terms, &fetcher, None).unwrap();

        let expected: Vec<u8> = [&chunk_a[..], &chunk_b[..]].concat();
        assert_eq!(result.content, expected);
        assert_eq!(result.reused_segments, 0);
        assert_eq!(result.fetched_segments, 2);
    }

    #[test]
    fn estimate_reuse_ratio_identical() {
        let terms = vec![make_term(1, 0, b"chunk A"), make_term(2, 0, b"chunk B")];
        let ratio = estimate_reuse_ratio(&terms, &terms);
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_reuse_ratio_no_overlap() {
        let base = vec![make_term(1, 0, b"chunk A")];
        let target = vec![make_term(2, 0, b"chunk B")];
        let ratio = estimate_reuse_ratio(&base, &target);
        assert!(ratio.abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_reuse_ratio_partial() {
        let shared = make_term(1, 0, b"shared");
        let base = vec![shared.clone(), make_term(2, 0, b"old")];
        let target = vec![shared, make_term(3, 0, b"new")];
        let ratio = estimate_reuse_ratio(&base, &target);
        assert!((ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn estimate_reuse_ratio_empty_target() {
        let base = vec![make_term(1, 0, b"chunk")];
        let ratio = estimate_reuse_ratio(&base, &[]);
        assert!(ratio.abs() < f64::EPSILON);
    }

    #[test]
    fn plan_delta_handles_duplicate_segments_in_base() {
        // Base has the same segment twice (can happen with repeated data).
        let chunk_a = b"repeated chunk";
        let term_a = make_term(1, 0, chunk_a);

        let base_terms = vec![term_a.clone(), term_a.clone()];
        let target_terms = vec![term_a.clone()];

        let base_content: Vec<u8> = [&chunk_a[..], &chunk_a[..]].concat();
        let fetcher = TestXorbFetcher::new();

        let result =
            reconstruct_from_delta(&base_terms, &base_content, &target_terms, &fetcher, None)
                .unwrap();

        assert_eq!(result.content, chunk_a);
        assert_eq!(result.reused_segments, 1);
        assert_eq!(result.fetched_segments, 0);
    }

    #[test]
    fn plan_delta_preserves_target_segment_order() {
        let chunk_a = b"chunk A content";
        let chunk_b = b"chunk B content";
        let chunk_c = b"chunk C content";

        let base_terms = vec![
            make_term(1, 0, chunk_a),
            make_term(2, 0, chunk_b),
            make_term(3, 0, chunk_c),
        ];
        // Target reverses the order.
        let target_terms = vec![
            make_term(3, 0, chunk_c),
            make_term(2, 0, chunk_b),
            make_term(1, 0, chunk_a),
        ];

        let base_content: Vec<u8> = [&chunk_a[..], &chunk_b[..], &chunk_c[..]].concat();
        let fetcher = TestXorbFetcher::new();

        let result =
            reconstruct_from_delta(&base_terms, &base_content, &target_terms, &fetcher, None)
                .unwrap();

        let expected: Vec<u8> = [&chunk_c[..], &chunk_b[..], &chunk_a[..]].concat();
        assert_eq!(result.content, expected);
        assert_eq!(result.reused_segments, 3);
        assert_eq!(result.fetched_segments, 0);
    }
}
