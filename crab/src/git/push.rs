//! Shared push policy, outcomes, and metadata maintenance.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crab_auth::{CrabAuthProvider, PushRefUpdate};
use crab_coordination::active_active::ActiveActiveReplicationConfig;
use crab_coordination::write_coordinator::{CommitOutcome, PushTransactionState, WriteCoordinator};
use crab_coordination::{PushLock, PushLockAcquireContext};
use crab_metadata::commit_graph::CommitEntry;
use crab_metadata::pack_metadata::PackMetadata;
use crab_metadata::split_commit_graph::{
    CommitGraphInput, CommitGraphWrite, SplitCommitGraph, append_split_commit_graph,
    decode_commit_graph_descriptor, encode_commit_graph_descriptor, load_split_commit_graph,
    load_split_commit_graph_descriptor, upload_split_commit_graph,
};
use crab_remote_git::{
    Error as RemoteGitError, ObjectLimits as RemoteGitObjectLimits,
    OperationContext as RemoteGitOperationContext, OperationKind as RemoteGitOperationKind,
    OperationLimits as RemoteGitOperationLimits, RemoteGitObject, RemoteGitRepository,
    RemoteGitRuntime, RepositoryIdentity as RemoteGitRepositoryIdentity,
    RepositoryOptions as RemoteGitRepositoryOptions,
};
use crab_write::catalog::{GitObjectKindMap, LocatorPackEvidence};
use crab_write::generation::{
    CommittedManifestAnchor, committed_manifest_anchor, uncovered_locator_object_rows,
};
use crab_xet::hash::MerkleHash;
use futures_util::stream::{StreamExt, TryStreamExt};
use object_store::path::Path as ObjectPath;
use rand::Rng;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::perf_phase::PerfPhaseSink;
use crate::git::remote_helper::PushSpec;
use crate::metadata::manifest::{
    Manifest, PackManifestEntry, read_bulk_pack_list, read_manifest, write_manifest_cas,
};
use crate::replication::ReplicationConfig;
use crate::storage::StoreLayout;
use crate::storage::store::Store;

pub(crate) async fn upsert_pack_metadata(
    store: &Store,
    path: &ObjectPath,
    pack_id: &str,
    object_count: u64,
    ref_tips: Vec<String>,
    max_retries: u32,
) -> Result<PackMetadata> {
    let mut requested_tips = ref_tips.into_iter().collect::<BTreeSet<_>>();
    if store.staging_write_prefix().is_some() {
        let (metadata, body) =
            pack_metadata_for_write(pack_id, object_count, requested_tips.into_iter().collect())?;
        store.put_exact(path, Bytes::from(body)).await?;
        return Ok(metadata);
    }

    // A newly generated content-addressed pack normally has no sidecar yet;
    // create-first avoids a guaranteed read on the first publication.
    let (metadata, body) = pack_metadata_for_write(
        pack_id,
        object_count,
        requested_tips.iter().cloned().collect(),
    )?;
    match store.create_strict(path, Bytes::from(body)).await {
        Ok(()) => return Ok(metadata),
        Err(CrabError::CasConflict { .. }) => {}
        Err(error) => return Err(error),
    }

    for _ in 0..max_retries.max(1) {
        match store
            .get_with_etag_bounded(path, crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES)
            .await
        {
            Ok((body, etag)) => {
                let mut metadata =
                    crab_metadata::pack_metadata::parse_pack_metadata(&body, path.as_ref())
                        .map_err(CrabError::from)?;
                if metadata.pack_id != pack_id || metadata.object_count != object_count {
                    return Err(CrabError::CorruptObject {
                        path: path.to_string(),
                        reason: "pack metadata conflicts with content-addressed pack identity"
                            .to_owned(),
                    });
                }
                let before = metadata.ref_tips.iter().cloned().collect::<BTreeSet<_>>();
                if before.is_empty() && !requested_tips.is_empty() {
                    // An empty hint may be a deliberate legacy fallback after a
                    // previous union exceeded the bound. It cannot be enriched
                    // without proving the complete ref-tip set again.
                    return Ok(pack_metadata_without_ref_tips(pack_id, object_count));
                }
                requested_tips.extend(before.iter().cloned());
                if requested_tips == before {
                    metadata.ref_tips = before.into_iter().collect();
                    return Ok(metadata);
                }
                metadata.ref_tips = requested_tips.iter().cloned().collect();
                let Some(next) =
                    crab_metadata::pack_metadata::serialize_pack_metadata_bounded(&metadata)
                        .map_err(CrabError::from)?
                else {
                    tracing::warn!(
                        pack_id,
                        maximum_bytes = crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
                        "pack metadata ref-tip union exceeded its bound; using legacy pack selection"
                    );
                    return Ok(pack_metadata_without_ref_tips(pack_id, object_count));
                };
                match store.update(path, Bytes::from(next), etag).await {
                    Ok(_) => return Ok(metadata),
                    Err(CrabError::CasConflict { .. }) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(CrabError::NotFound { .. }) => {
                let (metadata, body) = pack_metadata_for_write(
                    pack_id,
                    object_count,
                    requested_tips.iter().cloned().collect(),
                )?;
                match store.create_strict(path, Bytes::from(body)).await {
                    Ok(()) => return Ok(metadata),
                    Err(CrabError::CasConflict { .. }) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => {
                if matches!(&error, CrabError::CorruptObject { .. })
                    && existing_pack_metadata_is_oversized(store, path).await
                {
                    tracing::warn!(
                        pack_id,
                        maximum_bytes = crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
                        "existing oversized pack metadata is treated as a legacy hint"
                    );
                    return Ok(pack_metadata_without_ref_tips(pack_id, object_count));
                }
                return Err(error);
            }
        }
    }

    Err(CrabError::CasConflict {
        path: path.to_string(),
        expected_etag: None,
    })
}

fn pack_metadata_without_ref_tips(pack_id: &str, object_count: u64) -> PackMetadata {
    PackMetadata {
        pack_id: pack_id.to_owned(),
        ref_tips: Vec::new(),
        object_count,
    }
}

fn pack_metadata_for_write(
    pack_id: &str,
    object_count: u64,
    ref_tips: Vec<String>,
) -> Result<(PackMetadata, Vec<u8>)> {
    let metadata = PackMetadata {
        pack_id: pack_id.to_owned(),
        ref_tips,
        object_count,
    };
    if let Some(body) = crab_metadata::pack_metadata::serialize_pack_metadata_bounded(&metadata)
        .map_err(CrabError::from)?
    {
        return Ok((metadata, body));
    }

    tracing::warn!(
        pack_id,
        maximum_bytes = crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
        "pack metadata ref-tip hint exceeded its bound; publishing an empty hint"
    );
    let fallback = pack_metadata_without_ref_tips(pack_id, object_count);
    let body = crab_metadata::pack_metadata::serialize_pack_metadata_bounded(&fallback)
        .map_err(CrabError::from)?
        .ok_or_else(|| {
            CrabError::Internal("empty pack metadata hint exceeded its size bound".to_owned())
        })?;
    Ok((fallback, body))
}

async fn existing_pack_metadata_is_oversized(store: &Store, path: &ObjectPath) -> bool {
    match store.head(path).await {
        Ok(meta) => meta.size > crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
        Err(error) => {
            tracing::debug!(
                path = %path,
                error = %error,
                "could not confirm oversized pack metadata during legacy fallback"
            );
            false
        }
    }
}

/// Parameters for a single ref update during a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name, e.g. `refs/heads/main`.
    pub ref_name: String,
    /// SHA of the current remote ref (`None` for new refs).
    pub old_sha: Option<String>,
    /// SHA the ref should point to after the push.
    pub new_sha: String,
    /// Whether the `+` force prefix was used.
    pub force: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CommittedPackIndex<'a> {
    pub(crate) pack: &'a PackManifestEntry,
    pub(crate) idx_path: &'a Path,
    pub(crate) rev_path: &'a Path,
    pub(crate) git_sha1: &'a str,
    pub(crate) kind_by_oid: Option<&'a GitObjectKindMap>,
}

const MAX_GIT_OBJECT_CANDIDATES: usize = 2_000_000;
const MAX_GIT_OBJECT_ENUM_BYTES: usize = 256 * 1024 * 1024;

fn enumerate_git_object_candidates_blocking(
    git_dir: Option<PathBuf>,
    tips: Vec<String>,
    excluded_tips: Vec<String>,
) -> Result<Option<Vec<gix_hash::ObjectId>>> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    command
        .arg("rev-list")
        .arg("--objects")
        .arg("--no-object-names")
        .args(&tips);
    if !excluded_tips.is_empty() {
        command.arg("--not").args(&excluded_tips);
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(CrabError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git rev-list stdout unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CrabError::Internal("git rev-list stderr unavailable".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr)
            .take(1024 * 1024)
            .read_to_end(&mut bytes);
        bytes
    });

    let mut objects = Vec::new();
    let mut bytes_read = 0usize;
    let mut exceeded = false;
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(CrabError::Io)?;
        bytes_read = bytes_read.saturating_add(line.len() + 1);
        if bytes_read > MAX_GIT_OBJECT_ENUM_BYTES || objects.len() >= MAX_GIT_OBJECT_CANDIDATES {
            exceeded = true;
            let _ = child.kill();
            break;
        }
        let oid = gix_hash::ObjectId::from_hex(line.as_bytes()).map_err(|error| {
            CrabError::Internal(format!("git rev-list returned invalid object id: {error}"))
        })?;
        objects.push(oid);
    }
    let status = child.wait().map_err(CrabError::Io)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CrabError::Internal("git rev-list stderr reader panicked".to_owned()))?;
    if exceeded {
        return Ok(None);
    }
    if !status.success() {
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    objects.sort_unstable();
    objects.dedup();
    Ok(Some(objects))
}

/// Outcome of the ref-update precondition check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdateDecision {
    /// Proceed with a conditional write using the given `ETag`.
    Proceed {
        /// `ETag` from the current ref object (for CAS). `None` when
        /// creating a new ref (uses `PutMode::Create`).
        etag: Option<String>,
    },
    /// Rejected for a structured reason. Carries the full
    /// [`PushRejectReason`] so pass-1 evaluation can surface policy
    /// violations, non-fast-forwards, lock contention, etc. without
    /// losing detail by the time the outcome map lands in the remote
    /// helper response.
    Reject(PushRejectReason),
}

pub(crate) fn duplicate_destination_result(specs: &[PushSpec]) -> Option<PushResult> {
    let mut destinations = BTreeSet::new();
    let duplicate = specs
        .iter()
        .find(|spec| !destinations.insert(spec.dst.as_str()))?
        .dst
        .clone();
    let reason = PushRejectReason::DuplicateDestination {
        ref_name: duplicate,
    };
    Some(PushResult::new(
        specs
            .iter()
            .map(|spec| (spec.dst.clone(), RefPushOutcome::Rejected(reason.clone())))
            .collect(),
    ))
}

/// Check whether a ref update should proceed.
///
/// `is_fast_forward` indicates whether `new_sha` is a descendant of
/// `old_sha`. For new refs (`old_sha` is `None`), this is always
/// considered a fast-forward.
///
/// Returns [`RefUpdateDecision::Proceed`] with the `ETag` when the push
/// should go ahead, or
/// [`RefUpdateDecision::Reject`] carrying a
/// [`PushRejectReason::NonFastForward`] when a non-force push fails
/// the ancestry check.
#[must_use]
pub fn check_ref_update(
    update: &RefUpdate,
    is_fast_forward: bool,
    current_etag: Option<String>,
) -> RefUpdateDecision {
    // New ref creation is always allowed.
    if update.old_sha.is_none() {
        return RefUpdateDecision::Proceed { etag: current_etag };
    }

    // Force push skips the fast-forward check entirely.
    if update.force {
        return RefUpdateDecision::Proceed { etag: current_etag };
    }

    // Normal push requires fast-forward.
    if is_fast_forward {
        RefUpdateDecision::Proceed { etag: current_etag }
    } else {
        // Preserve the have/want SHAs so downstream formatters can
        // render a useful error line without re-plumbing `update`
        // back through the call chain.
        let have = update
            .old_sha
            .clone()
            .unwrap_or_else(|| "(none)".to_owned());
        let want = update.new_sha.clone();
        RefUpdateDecision::Reject(PushRejectReason::NonFastForward { have, want })
    }
}

/// Recognize stderr signatures that `git merge-base --is-ancestor`
/// emits when the queried SHA exists only on the remote. Shallow
/// clients that don't have the old tip locally hit this path; the
/// caller should fall back to the commit-graph summary ancestry.
#[must_use]
pub(super) fn is_missing_object_error(stderr: &str) -> bool {
    stderr.contains("Not a valid commit name")
        || stderr.contains("not our ref")
        || stderr.contains("bad revision")
        || stderr.contains("bad object")
}

// --- Push pipeline configuration and types ---

/// Default maximum concurrent xorb uploads.
const DEFAULT_UPLOAD_CONCURRENCY: usize = 16;

/// Default concurrency for HEAD-check resume (step 6).

/// Default retry budget for transient HEAD/LIST failures in step 6.
///
/// Matches the `Config::max_retries` default; a standalone constant keeps
/// `PushConfig::default()` self-contained for tests.
const DEFAULT_MAX_RETRIES: u32 = 5;

/// Default batch size for staging chunk reads in step 5.
///
/// Matches `StagingConfig::batch_read_size` default and keeps
/// `PushConfig::default()` self-contained for tests.
const DEFAULT_BATCH_READ_SIZE: usize = 256;

/// Default maximum CAS retry attempts for the unified manifest pointer.
const DEFAULT_MAX_CAS_RETRIES: u32 = 64;

/// Base delay for opt-in push-lock wait polling.
const PUSH_LOCK_WAIT_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Cap for opt-in push-lock wait polling.
const PUSH_LOCK_WAIT_BACKOFF_CAP: Duration = Duration::from_secs(2);
const GIT_LOCATOR_LOCK_WAIT_TTL_MULTIPLIER: u32 = 2;
const GIT_VISIBILITY_LOCK_WAIT_TTL_MULTIPLIER: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GitVisibilityCapacity {
    observed: u64,
    maximum: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitVisibilityPublication {
    Published,
    CatalogBound,
    CompletePackOnly(GitVisibilityCapacity),
}

fn git_visibility_capacity_exceeded_at_limit<'a>(
    packs: impl IntoIterator<Item = &'a PackManifestEntry>,
    maximum: u64,
) -> Result<Option<GitVisibilityCapacity>> {
    let mut seen = HashSet::new();
    let packed_objects = packs
        .into_iter()
        .filter(|pack| seen.insert(pack.pack_id.as_str()))
        .try_fold(0u64, |total, pack| total.checked_add(pack.object_count))
        .ok_or_else(|| CrabError::Internal("Git pack object count overflow".to_owned()))?;
    // V5 stores one shared catalog dictionary and ordinal closures per ref.
    // Bound the unique dictionary here; the serialized proof byte limit remains
    // the protection against an unbounded number of repeated memberships.
    Ok((packed_objects > maximum).then_some(GitVisibilityCapacity {
        observed: packed_objects,
        maximum,
    }))
}

#[derive(Clone)]
pub struct ActiveActiveWriteCoordinator {
    inner: Arc<dyn WriteCoordinator>,
}

impl ActiveActiveWriteCoordinator {
    #[must_use]
    pub fn new(inner: Arc<dyn WriteCoordinator>) -> Self {
        Self { inner }
    }
}

impl AsRef<dyn WriteCoordinator + 'static> for ActiveActiveWriteCoordinator {
    fn as_ref(&self) -> &(dyn WriteCoordinator + 'static) {
        self.inner.as_ref()
    }
}

impl fmt::Debug for ActiveActiveWriteCoordinator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActiveActiveWriteCoordinator")
            .finish_non_exhaustive()
    }
}

/// Configuration for the push pipeline.
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// Maximum concurrent uploads; Git pack transactions are additionally capped at four.
    pub upload_concurrency: usize,
    /// Maximum number of concurrent HEAD/LIST requests for the HEAD-check
    /// resume step (step 6). Defaults to 64.
    pub head_check_concurrency: usize,
    /// Maximum retry attempts for transient HEAD/LIST failures in the
    /// resume step. Mirrors `Config::max_retries`; defaults to 5.
    pub max_retries: u32,
    /// Heartbeat interval for the push lock, or `None` to disable.
    ///
    /// Should be obtained from `Config::clamped_heartbeat_interval(ttl)`.
    /// When set, step 12 spawns a `LockHeartbeat` that extends the lock
    /// TTL at this cadence.
    pub heartbeat_interval: Option<Duration>,
    /// TTL for the push lock. Defaults to 5 minutes.
    pub lock_ttl: Duration,
    /// How long to wait for contested push locks before returning lock contention.
    pub lock_wait: Duration,
    /// When `true`, use adaptive compression (BG4 + entropy probe).
    /// When `false`, use fixed compression with the configured algorithm.
    pub compression_adaptive: bool,
    /// Reserved for adaptive upload concurrency; currently logs and uses a
    /// bounded fixed semaphore because the pinned xet-core API keeps final
    /// transfer reporting crate-private.
    pub adaptive_concurrency: bool,
    /// Lower bound for the adaptive concurrency controller.
    pub min_concurrency: usize,
    /// Upper bound for the adaptive concurrency controller.
    pub max_concurrency: usize,
    /// When `true`, dynamically adjust xorb target size based on upload throughput.
    pub adaptive_xorb_size: bool,
    /// Target xorb size in bytes for the packer before adaptive updates.
    pub xorb_target_size: u64,
    /// Minimum xorb target size in bytes for adaptive sizing.
    pub min_xorb_size: u64,
    /// Maximum xorb target size in bytes for adaptive sizing.
    pub max_xorb_size: u64,
    /// Minimum remaining run bytes to trigger xorb extension.
    pub min_run_tail: u64,
    /// Maximum xorb overshoot as a fraction of target size.
    pub max_xorb_overshoot_percent: f64,
    /// Prefer file boundary within this distance when splitting.
    pub split_prefer_boundary: u64,
    /// Rolling window size for the `DefragPrevention` estimator.
    pub defrag_window_size: usize,
    /// Minimum chunks-per-range threshold for `DefragPrevention`.
    pub min_chunks_per_range: f64,
    /// Batch size for staging chunk reads in step 5.
    pub batch_read_size: usize,
    /// Memory ceiling for the in-memory ChunkIndex HashMap (bytes).
    /// When exceeded, new shards spill to on-disk `MDBShardFile` handles.
    pub shard_chunk_index_table_max_size: u64,
    /// Maximum CAS retry attempts for the unified manifest pointer.
    /// On each conflict the pipeline re-reads the current manifest,
    /// checks for ref conflicts, and retries if non-conflicting.
    pub max_cas_retries: u32,
    /// When `true`, multi-ref pushes commit atomically: if any ref is
    /// rejected in pass 1, the entire batch aborts with zero manifest
    /// writes. When `false` (default), rejected refs are skipped and
    /// the rest of the batch commits in a single manifest generation.
    /// Flipped by the remote-helper `option atomic true` from git's
    /// `push --atomic` invocation.
    pub atomic: bool,
    /// Exact ref values required by the caller, rechecked after locking and on retry.
    /// A present key with `None` requires that the ref does not exist.
    pub expected_refs: BTreeMap<String, Option<String>>,
    /// Maximum size of each generated pack, in bytes. Mirrors git's
    /// `receive.maxInputSize`. Aggregate closures are split into bounded
    /// packs; a single object that cannot fit is rejected. `0` disables
    /// the bound.
    pub receive_max_input_size: u64,
    /// When `true`, a `git push :ref` delete-refspec is rejected with
    /// [`PushRejectReason::DenyDeletes`]. Mirrors git's
    /// `receive.denyDeletes`. Server-side policy knob, independent of
    /// any client-side flag.
    pub receive_deny_deletes: bool,
    /// When `true`, non-fast-forward updates are rejected with
    /// [`PushRejectReason::DenyNonFastForward`] even when the client
    /// sent `force`. Mirrors git's `receive.denyNonFastForwards` —
    /// admin policy takes precedence over the pusher's `+refspec`.
    pub receive_deny_non_fast_forwards: bool,
    /// Behavior when a push targets the remote's HEAD symref. One of
    /// `"refuse"` (reject with [`PushRejectReason::DenyCurrentBranch`]),
    /// `"warn"` (allow with a `tracing::warn!` event), or `"ignore"`
    /// (no check). Any other value degrades to `"ignore"` with a log
    /// line at construction time. Mirrors git's
    /// `receive.denyCurrentBranch`.
    pub receive_deny_current_branch: String,
    /// Upper bound on commits walked by the commit-graph
    /// ancestry fallback used by the FF precomputation when
    /// `git merge-base --is-ancestor` can't resolve the question
    /// (shallow/sparse clients, missing local objects). `0` disables
    /// the fallback and conservatively rejects the push as
    /// non-fast-forward.
    pub receive_ff_summary_window_commits: u64,
    /// `[metadb]` TOML surface, used when constructing the push
    /// pipeline's `MetaDbGuard`. Defaults to an empty struct (all
    /// fields `None`), preserving the derived [`MetaDbConfig::for_repo`]
    /// defaults when the operator hasn't tuned anything.
    pub metadb: crate::core::config::MetaDbTomlConfig,
    /// Active-active replication settings copied from the resolved config.
    ///
    /// When set, the pipeline must commit through a linearizable
    /// coordinator instead of falling back to the object-store manifest CAS.
    pub active_active_replication: Option<ReplicationConfig>,
    /// Preferred active-active writer name, selected by the caller.
    pub active_active_writer: Option<String>,
    /// Linearizable active-active coordinator used for ref commits.
    pub active_active_coordinator: Option<ActiveActiveWriteCoordinator>,
    /// Optional JSONL sink for stable `perf.phase` events.
    pub perf_phase_sink: Option<PerfPhaseSink>,
    /// Explicit git directory for callers that publish a repository
    /// other than the process current directory.
    pub git_dir: Option<PathBuf>,
    /// Include the complete outgoing Git and LFS closure instead of excluding
    /// objects reachable from current remote tips.
    pub force_full_graph: bool,
    /// Validated internal mirror-plan identity for durable commit attribution.
    pub mirror_plan_id: Option<String>,
    pub protected_push: Option<ProtectedPushSession>,
}

#[derive(Debug, Clone)]
pub struct ProtectedPushSession {
    pub ref_updates: Vec<PushRefUpdate>,
    pub push_id: String,
    pub upload_prefix: String,
    pub active_active_writer: Option<String>,
    pub backend: ProtectedPushBackend,
}

#[derive(Debug, Clone)]
pub enum ProtectedPushBackend {
    CrabAuth {
        auth: Arc<CrabAuthProvider>,
        bucket: String,
        prefix: String,
        active_active_replication: Option<ActiveActiveReplicationConfig>,
    },
    Managed {
        token_cache_directory: PathBuf,
        repository: crab_git::ManagedRepository,
        push_id: uuid::Uuid,
        request: crab_auth::managed::PushFinalizeRequest,
    },
}

/// Default push lock TTL: 5 minutes.
const DEFAULT_LOCK_TTL: Duration = Duration::from_secs(300);

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            upload_concurrency: DEFAULT_UPLOAD_CONCURRENCY,
            head_check_concurrency: crate::core::config::DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY,
            max_retries: DEFAULT_MAX_RETRIES,
            heartbeat_interval: None,
            lock_ttl: DEFAULT_LOCK_TTL,
            lock_wait: Duration::ZERO,
            compression_adaptive: true,
            adaptive_concurrency: false,
            min_concurrency: 4,
            max_concurrency: 64,
            adaptive_xorb_size: false,
            xorb_target_size: 64 * 1024 * 1024,
            min_xorb_size: 16 * 1024 * 1024,
            max_xorb_size: 256 * 1024 * 1024,
            min_run_tail: 1024 * 1024,
            max_xorb_overshoot_percent: 0.10,
            split_prefer_boundary: 2 * 1024 * 1024,
            defrag_window_size: 10,
            min_chunks_per_range: 16.0,
            batch_read_size: DEFAULT_BATCH_READ_SIZE,
            shard_chunk_index_table_max_size: 64 * 1024 * 1024,
            max_cas_retries: DEFAULT_MAX_CAS_RETRIES,
            atomic: false,
            expected_refs: BTreeMap::new(),
            receive_max_input_size: 2 * 1024 * 1024 * 1024,
            receive_deny_deletes: false,
            receive_deny_non_fast_forwards: false,
            receive_deny_current_branch: String::from("ignore"),
            receive_ff_summary_window_commits: 1000,
            metadb: crate::core::config::MetaDbTomlConfig::default(),
            active_active_replication: None,
            active_active_writer: None,
            active_active_coordinator: None,
            perf_phase_sink: None,
            git_dir: None,
            force_full_graph: false,
            mirror_plan_id: None,
            protected_push: None,
        }
    }
}

impl PushConfig {
    /// Build a `PushConfig` from the resolved [`Config`].
    ///
    /// Maps the top-level config fields into the push-specific struct,
    /// including the clamped heartbeat interval.
    #[must_use]
    pub fn from_config(config: &crate::core::config::Config) -> Self {
        Self {
            upload_concurrency: config.upload_concurrency,
            head_check_concurrency: config.push_head_check_concurrency,
            max_retries: config.max_retries,
            lock_ttl: std::time::Duration::from_secs(config.push_lock_ttl_secs),
            lock_wait: std::time::Duration::from_secs(config.push_lock_wait_secs),
            heartbeat_interval: config.clamped_heartbeat_interval(config.push_lock_ttl_secs),
            compression_adaptive: config.compression_adaptive,
            adaptive_concurrency: config.push_adaptive_concurrency,
            min_concurrency: config.push_min_concurrency,
            max_concurrency: config.push_max_concurrency,
            adaptive_xorb_size: config.adaptive_xorb_size,
            xorb_target_size: config.xorb_target_size,
            min_xorb_size: config.min_xorb_size,
            max_xorb_size: config.max_xorb_size,
            min_run_tail: config.min_run_tail,
            max_xorb_overshoot_percent: config.max_xorb_overshoot_percent,
            split_prefer_boundary: config.split_prefer_boundary,
            defrag_window_size: config.defrag_window_size,
            min_chunks_per_range: config.min_chunks_per_range,
            batch_read_size: config.staging.batch_read_size,
            shard_chunk_index_table_max_size: config.shard_chunk_index_table_max_size,
            max_cas_retries: config.push_max_cas_retries,
            atomic: false,
            expected_refs: BTreeMap::new(),
            receive_max_input_size: config.receive_max_input_size,
            receive_deny_deletes: config.receive_deny_deletes,
            receive_deny_non_fast_forwards: config.receive_deny_non_fast_forwards,
            receive_deny_current_branch: config.receive_deny_current_branch.clone(),
            receive_ff_summary_window_commits: config.receive_ff_summary_window_commits,
            metadb: config.metadb.clone(),
            active_active_replication: config
                .replication
                .as_ref()
                .filter(|replication| replication.is_active_active())
                .cloned(),
            active_active_writer: None,
            active_active_coordinator: None,
            perf_phase_sink: None,
            git_dir: None,
            force_full_graph: false,
            mirror_plan_id: None,
            protected_push: None,
        }
    }

    /// Build the compression policy determined by the config.
    ///
    /// When `compression_adaptive` is `true`, returns an [`AdaptiveCompression`]
    /// policy (BG4 + entropy probe). Otherwise returns [`FixedCompression`].
    #[must_use]
    pub fn compression_policy(&self) -> Box<dyn crab_xet::xorb::builder::CompressionPolicy> {
        use crab_xet::xorb::builder::{AdaptiveCompression, FixedCompression};

        if self.compression_adaptive {
            Box::new(AdaptiveCompression::default())
        } else {
            Box::new(FixedCompression::default())
        }
    }

    /// Applies split-decision parameters from this config to the builder.
    ///
    /// Wires size bounds, the configured target size, overshoot budget,
    /// and `DefragPrevention` parameters into the builder. The target is
    /// clamped to `[min_xorb_size, max_xorb_size]`, and overshoot is
    /// derived from that clamped target so the knobs move together.
    #[must_use]
    pub fn configure_builder(
        &self,
        builder: crab_xet::xorb::builder::XorbBuilder,
    ) -> crab_xet::xorb::builder::XorbBuilder {
        let min_size = self.min_xorb_size as usize;
        let max_size = self.max_xorb_size as usize;
        let max_size = max_size.max(min_size);
        let target_size = (self.xorb_target_size as usize).clamp(min_size, max_size);
        let overshoot_percent = if self.max_xorb_overshoot_percent.is_finite()
            && self.max_xorb_overshoot_percent > 0.0
        {
            self.max_xorb_overshoot_percent
        } else {
            0.0
        };
        let max_overshoot = (target_size as f64 * overshoot_percent) as usize;

        #[expect(
            clippy::cast_possible_truncation,
            reason = "min_chunks_per_range is a small float, safe to narrow to f32"
        )]
        let min_cpr = self.min_chunks_per_range as f32;

        let mut builder = builder
            .with_size_bounds(min_size, max_size)
            .with_defrag_params(self.defrag_window_size, min_cpr, 0.5);
        builder.set_target_size(target_size);
        builder.with_max_overshoot(max_overshoot)
    }
}

/// Wires the configured active-active coordinator into a push config.
///
/// Active-active writes fail closed unless the remote URL maps to one enabled
/// writer and the configured coordinator backend can prove write admission.
pub async fn configure_active_active_push_coordinator(
    config: &crate::core::config::Config,
    remote_url: Option<&str>,
    repo_prefix: &str,
    push_config: &mut PushConfig,
) -> Result<()> {
    let Some(replication) = config
        .replication
        .as_ref()
        .filter(|replication| replication.is_active_active())
    else {
        return Ok(());
    };

    crate::replication::validate_active_active_config(replication)?;
    push_config.active_active_writer = Some(
        crate::replication::active_active_writer_name_for_remote(replication, remote_url)?,
    );
    if matches!(
        config.auth.provider,
        crate::core::config::AuthProvider::CrabAuth
    ) {
        return Ok(());
    }

    let coordinator =
        crate::replication::active_active_write_coordinator_for_repo(config, repo_prefix).await?;
    push_config.active_active_coordinator = Some(ActiveActiveWriteCoordinator::new(coordinator));
    Ok(())
}

///
/// `expires_at_unix` is the lease expiry read from the lock payload.
/// Returns `0` when the lease has already passed, when no expiry was
/// recorded (payload unreadable), or when the system clock is behind
/// the lease's absolute timestamp by more than `u64::MAX` seconds
/// (not physically possible on wall-clock hardware). The caller can
/// interpret `0` as "retry immediately and let the next acquire()
/// reclaim the expired lease".
#[must_use]
pub fn lock_ttl_remaining(expires_at_unix: Option<u64>) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let Some(expires_at) = expires_at_unix else {
        return 0;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    expires_at.saturating_sub(now)
}

fn push_lock_wait_delay(attempt: u32, remaining: Duration) -> Duration {
    push_lock_wait_delay_with_cap(attempt, remaining, PUSH_LOCK_WAIT_BACKOFF_CAP)
}

fn push_lock_wait_delay_with_cap(attempt: u32, remaining: Duration, cap: Duration) -> Duration {
    let shift = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let exp = PUSH_LOCK_WAIT_BACKOFF_BASE.saturating_mul(shift);
    let bound = exp.min(cap).min(remaining);
    let bound_nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if bound_nanos == 0 {
        return Duration::ZERO;
    }
    let pick = rand::rng().random_range(1..=bound_nanos);
    Duration::from_nanos(pick)
}

/// Structured reject reason for a per-ref push failure.
///
/// Each variant maps to a stable short protocol string the remote helper
/// emits (via [`PushRejectReason::protocol_tag`]) so git clients and
/// scripts can parse outcomes reliably, and to a human-readable detail
/// via its [`fmt::Display`] impl. Modeled on smart-HTTP's
/// `report-status` reasons (non-fast-forward, stale info, etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRejectReason {
    /// New tip is not a descendant of the current ref.
    NonFastForward {
        /// Current SHA on the remote.
        have: String,
        /// SHA the push attempted to write.
        want: String,
    },
    /// A delete refspec failed.
    Deleted(String),
    /// The push failed because the local view of the ref was stale
    /// (CAS conflict with no matching ref conflict).
    StaleInfo,
    /// More than one refspec targeted the same destination in one batch.
    DuplicateDestination {
        /// Destination ref named by multiple refspecs.
        ref_name: String,
    },
    /// The ref passed its own checks but the transaction did not commit
    /// because another ref aborted the indivisible batch.
    AtomicAbort {
        /// Destination ref whose failure caused the batch abort.
        blocked_by: String,
    },
    /// Server policy prohibits deleting refs.
    DenyDeletes,
    /// Server policy prohibits non-fast-forward updates.
    DenyNonFastForward,
    /// Server policy prohibits updating the current branch.
    DenyCurrentBranch,
    /// Connectivity check failed: an object reachable from the new tip
    /// is missing from the local ODB, so the pack would have landed
    /// incomplete.
    ConnectivityMissing {
        /// Hex OID of the first missing object.
        oid: String,
    },
    /// The local repository does not contain enough history to prove
    /// or package the requested push.
    ShallowBoundary {
        /// Tip or boundary OID that could not be traversed locally.
        oid: String,
    },
    /// Pack generation or upload failed.
    UnpackFailed(String),
    /// The ref name is malformed.
    BadRefname(String),
    /// The push lock was held by another pusher. `holder` identifies
    /// the competing session (usually `pid-{pid}-{uuid}`) so scripts
    /// can distinguish retries from truly stuck locks;
    /// `ttl_remaining_secs` is the number of seconds until the lease
    /// expires so clients can back off for at least that long. A
    /// remaining TTL of `0` either means the lock just expired (retry
    /// immediately) or the lease payload could not be read.
    LockContention {
        /// Holder identity from the lease payload.
        holder: String,
        /// Seconds until the existing lease expires. Saturates at
        /// `u64::MAX`; truncated to `0` when the lease has already
        /// passed or the payload could not be parsed.
        ttl_remaining_secs: u64,
    },
    /// One generated pack exceeded the `receive.maxInputSize` limit.
    /// Aggregate closures are split; this normally means one object
    /// cannot fit in a bounded pack.
    PackTooLarge {
        /// Actual pack size in bytes.
        size_bytes: u64,
        /// Configured limit in bytes (0 means disabled, so this
        /// variant is never emitted for that case).
        limit_bytes: u64,
    },
    /// Server-side hook declined the push. `name` identifies which
    /// hook fired (`"branch-protection"`, `"webhook"`, etc.) so
    /// clients can distinguish policy enforcement from validation
    /// hooks; `message` is the human-readable reason surfaced by the
    /// hook implementation.
    HookDeclined {
        /// Hook identifier (e.g. `"webhook"`, `"branch-protection"`).
        name: String,
        /// Hook-provided message explaining the decline.
        message: String,
    },
    /// A push-cert was required by server policy but none was
    /// provided by the client.
    NotSigned,
    /// The push-cert's signer fingerprint was not on the repo's
    /// allowlist. Carrying the fingerprint lets admins diagnose
    /// which key was rejected without re-running the push.
    SignerNotAllowed {
        /// Fingerprint of the signing key (GPG / SSH). Diagnostic
        /// only; never used to bypass the allowlist on retry.
        fingerprint: String,
    },
    /// Ref name parsed cleanly but refers to something that does not
    /// exist or should not exist on the remote — a peeled tag-target
    /// ref, a ref hidden by `transfer.hideRefs`, or any other name
    /// that passes lexical validation yet isn't addressable. Distinct
    /// from [`Self::BadRefname`] (which rejects malformed names
    /// before they reach the manifest).
    UnknownRefname {
        /// Offending ref name.
        name: String,
    },
    /// Object-level fsck caught a malformed object in the push pack.
    /// Surfaces before the manifest CAS so the bad object never
    /// lands in the reachable graph.
    MalformedObject {
        /// Hex object id of the failing object.
        oid: String,
        /// Validation scope reported by Git. Pack-index validation uses
        /// `"pack"` because index-pack owns object decoding and diagnostics.
        kind: String,
        /// Short explanation of the failure mode (e.g. `"tree
        /// entries out of order"`).
        detail: String,
    },
    /// Command-layer integration failed while trying to make an
    /// otherwise retryable current-branch push fast-forward.
    IntegrationFailed {
        /// Integration command Crab ran.
        command: String,
        /// Diagnostic output from the integration command.
        message: String,
    },
    /// A publication may have committed; reconcile before any explicit retry.
    CommitIndeterminate { commit_identity: String },
    /// Object-store transport failed after its bounded retry policy.
    NetworkTransient(String),
    /// Object-store throttling persisted after bounded retries.
    Throttled {
        /// Provider retry hint rounded up to whole seconds.
        retry_after_secs: Option<u64>,
    },
    /// Everything else — kept as a last resort so we always have a
    /// variant for uncategorized errors during pipeline failures.
    Internal(String),
}

impl PushRejectReason {
    /// Stable short string suitable for the remote-helper
    /// `error {ref} {tag}` response line. Parseable by scripts and
    /// clients that key off the tag.
    #[must_use]
    pub fn protocol_tag(&self) -> &'static str {
        match self {
            Self::NonFastForward { .. } => "non-fast-forward",
            Self::Deleted(_) => "delete-failed",
            Self::StaleInfo => "stale info",
            Self::DuplicateDestination { .. } => "duplicate-destination",
            Self::AtomicAbort { .. } => "atomic-abort",
            Self::DenyDeletes => "deny-deletes",
            Self::DenyNonFastForward => "deny-non-fast-forward",
            Self::DenyCurrentBranch => "deny-current-branch",
            Self::ConnectivityMissing { .. } => "missing-object",
            Self::ShallowBoundary { .. } => "shallow-boundary",
            Self::UnpackFailed(_) => "unpack-failed",
            Self::BadRefname(_) => "bad-refname",
            Self::LockContention { .. } => "lock-contention",
            Self::PackTooLarge { .. } => "pack-too-large",
            Self::HookDeclined { .. } => "hook-declined",
            Self::NotSigned => "not-signed",
            Self::SignerNotAllowed { .. } => "signer-not-allowed",
            Self::UnknownRefname { .. } => "unknown-refname",
            Self::MalformedObject { .. } => "malformed-object",
            Self::IntegrationFailed { .. } => "integration-failed",
            Self::CommitIndeterminate { .. } => "indeterminate",
            Self::NetworkTransient(_) | Self::Throttled { .. } => "transient",
            Self::Internal(_) => "internal",
        }
    }

    /// Whether retrying the same push can reasonably succeed without user
    /// edits or policy changes.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::LockContention { .. }
                | Self::StaleInfo
                | Self::NetworkTransient(_)
                | Self::Throttled { .. }
        )
    }

    /// Suggested retry delay in seconds, when the reason carries one.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::LockContention {
                ttl_remaining_secs, ..
            } => Some(*ttl_remaining_secs),
            Self::Throttled {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    /// Classify a [`CrabError`] into a structured reject reason.
    ///
    /// Errors that carry ref-level context or retry semantics map to specific
    /// variants; every other error falls back to `Internal(e.to_string())` so
    /// diagnostic detail is not dropped.
    #[must_use]
    pub fn from_error(err: &CrabError) -> Self {
        if let Some(commit_identity) = uncertain_commit_identity(err) {
            return Self::CommitIndeterminate {
                commit_identity: commit_identity.to_owned(),
            };
        }
        match err {
            CrabError::NonFastForward { have, want, .. } => Self::NonFastForward {
                have: have.clone(),
                want: want.clone(),
            },
            CrabError::PushConnectivityMissing { oid, .. } => {
                Self::ConnectivityMissing { oid: oid.clone() }
            }
            CrabError::BeyondShallowBoundary { oid } => Self::ShallowBoundary { oid: oid.clone() },
            CrabError::PushLockHeld {
                holder,
                expires_at_unix,
                ..
            } => Self::LockContention {
                holder: holder.clone(),
                ttl_remaining_secs: lock_ttl_remaining(*expires_at_unix),
            },
            CrabError::PackTooLarge { size, limit } => Self::PackTooLarge {
                size_bytes: *size,
                limit_bytes: *limit,
            },
            CrabError::PushMalformedObject { oid, kind, detail } => Self::MalformedObject {
                oid: oid.clone(),
                kind: kind.clone(),
                detail: detail.clone(),
            },
            CrabError::NetworkTransient(_) => Self::NetworkTransient(err.to_string()),
            CrabError::Throttled { retry_after, .. } => Self::Throttled {
                retry_after_secs: retry_after.map(|delay| {
                    delay
                        .as_secs()
                        .saturating_add(u64::from(delay.subsec_nanos() != 0))
                }),
            },
            // CAS conflicts mean the pusher's view of the manifest
            // diverged from the remote's current state; `stale info`
            // is the git `report-status` convention for that case.
            CrabError::CasConflict { .. } => Self::StaleInfo,
            _ => Self::Internal(err.to_string()),
        }
    }
}

fn uncertain_commit_identity(error: &CrabError) -> Option<&str> {
    let CrabError::Io(io_error) = error else {
        return None;
    };
    let source = io_error.get_ref()?;
    if let Some(write_error) = source.downcast_ref::<crab_write::WriteError>() {
        return match write_error {
            crab_write::WriteError::CapsuleCommitUncertain { transaction_id, .. } => {
                Some(transaction_id)
            }
            crab_write::WriteError::CapsuleCheckpointCommitUncertain {
                checkpoint_hash, ..
            } => Some(checkpoint_hash),
            crab_write::WriteError::CapsuleMaintenanceCommitUncertain { fence_id, .. } => {
                Some(fence_id)
            }
            _ => None,
        };
    }
    match source.downcast_ref::<crab_metadata::error::MetadataError>()? {
        crab_metadata::error::MetadataError::RefJournalCommitUncertain {
            transaction_id, ..
        } => Some(transaction_id),
        crab_metadata::error::MetadataError::ManifestCommitUncertain {
            candidate_digest, ..
        } => Some(candidate_digest),
        _ => None,
    }
}

impl fmt::Display for PushRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFastForward { have, want } => {
                write!(f, "non-fast-forward (have {have}, want {want})")
            }
            Self::Deleted(msg) => write!(f, "delete failed: {msg}"),
            Self::StaleInfo => write!(f, "stale info"),
            Self::DuplicateDestination { ref_name } => {
                write!(f, "multiple refspecs target {ref_name}")
            }
            Self::AtomicAbort { blocked_by } => {
                write!(f, "batch aborted because {blocked_by} was rejected")
            }
            Self::DenyDeletes => write!(f, "deletion not allowed by server policy"),
            Self::DenyNonFastForward => {
                write!(f, "non-fast-forward update not allowed by server policy")
            }
            Self::DenyCurrentBranch => {
                write!(f, "updates to the current branch are not allowed")
            }
            Self::ConnectivityMissing { oid } => {
                write!(f, "missing object {oid}; pack would be incomplete")
            }
            Self::ShallowBoundary { oid } => write!(
                f,
                "local history is incomplete at {oid}; run `git fetch --deepen=N` or `git fetch --unshallow` and retry"
            ),
            Self::UnpackFailed(msg) => write!(f, "unpack failed: {msg}"),
            Self::BadRefname(name) => write!(f, "invalid ref name: {name}"),
            Self::LockContention {
                holder,
                ttl_remaining_secs,
            } => write!(
                f,
                "lock held by {holder}; retry after {ttl_remaining_secs}s"
            ),
            Self::PackTooLarge {
                size_bytes,
                limit_bytes,
            } => write!(
                f,
                "pack size {size_bytes} bytes exceeds {limit_bytes} byte limit"
            ),
            Self::HookDeclined { name, message } => {
                write!(f, "hook {name} declined: {message}")
            }
            Self::NotSigned => {
                write!(f, "push requires signing (push-cert not provided)")
            }
            Self::SignerNotAllowed { fingerprint } => {
                write!(f, "signer fingerprint {fingerprint} not on allowlist")
            }
            Self::UnknownRefname { name } => write!(f, "unknown ref name: {name}"),
            Self::MalformedObject { oid, kind, detail } => {
                write!(f, "malformed {kind} {oid}: {detail}")
            }
            Self::IntegrationFailed { command, message } => {
                write!(f, "{command} failed: {message}")
            }
            Self::CommitIndeterminate { commit_identity } => write!(
                f,
                "commit outcome indeterminate for {commit_identity}; reconcile durable evidence before retrying"
            ),
            Self::NetworkTransient(message) => write!(f, "transient network failure: {message}"),
            Self::Throttled {
                retry_after_secs: Some(delay),
            } => write!(f, "storage throttled; retry after {delay}s"),
            Self::Throttled {
                retry_after_secs: None,
            } => write!(f, "storage throttled"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

/// Outcome of a single ref push within a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefPushOutcome {
    /// The ref was updated successfully.
    Ok,
    /// The ref push failed with a structured reject reason.
    Rejected(PushRejectReason),
}

impl RefPushOutcome {
    /// Returns the protocol tag for this outcome — `"ok"` on success
    /// or the reject reason's tag on failure. Used by the remote
    /// helper to emit stable `ok {ref}` / `error {ref} {tag}` lines.
    #[must_use]
    pub fn protocol_tag(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected(reason) => reason.protocol_tag(),
        }
    }
}

impl fmt::Display for RefPushOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::Rejected(reason) => write!(f, "error: {reason}"),
        }
    }
}

/// Stable push-pipeline stage attached to a rejected batch for diagnostics.
///
/// This is deliberately separate from [`PushRejectReason::protocol_tag`]: Git
/// consumes the reason tag, while operators need the stage to attribute a
/// transient failure without parsing provider-specific error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PushFailureStage {
    /// Repository admission ticket acquisition or renewal.
    Admission,
    /// Write-store, control-plane, or protected-push setup.
    StoreResolve,
    /// Native Git and remote-frontier discovery before pipeline delegation.
    Discovery,
    /// Canonical manifest or repository snapshot read.
    RemoteState,
    /// Ref policy, ancestry, and base-bound decision planning.
    Preflight,
    /// Pointer, staging, and chunk classification.
    Classify,
    /// Per-ref push-lock acquisition or renewal.
    Lock,
    /// Xorb payload packing.
    XorbPack,
    /// Xorb payload upload or durability verification.
    XorbUpload,
    /// Git reachability and pack preparation.
    GitPackPrepare,
    /// Reconstruction shard construction.
    ShardBuild,
    /// Reconstruction shard and file-index upload.
    ShardUpload,
    /// Git pack upload.
    GitPackUpload,
    /// Reachability verification before refs move.
    Connectivity,
    /// Commit-graph summary publication.
    CommitGraph,
    /// Manifest/ref transaction commit.
    RefCommit,
}

impl PushFailureStage {
    /// Return the stable name used by audit and structured push telemetry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::StoreResolve => "store-resolve",
            Self::Discovery => "discovery",
            Self::RemoteState => "remote-state",
            Self::Preflight => "preflight",
            Self::Classify => "classify",
            Self::Lock => "lock",
            Self::XorbPack => "xorb-pack",
            Self::XorbUpload => "xorb-upload",
            Self::GitPackPrepare => "git-pack-prepare",
            Self::ShardBuild => "shard-build",
            Self::ShardUpload => "shard-upload",
            Self::GitPackUpload => "git-pack-upload",
            Self::Connectivity => "connectivity",
            Self::CommitGraph => "commit-graph",
            Self::RefCommit => "ref-commit",
        }
    }
}

/// Result of a push batch: per-ref outcomes keyed by destination ref name.
#[derive(Debug, Clone)]
pub struct PushResult {
    /// Per-ref outcomes, keyed by the destination ref name (e.g. `refs/heads/main`).
    pub outcomes: HashMap<String, RefPushOutcome>,
    /// Coordinator commit metadata when active-active mode accepted this push.
    pub active_active_commit: Option<PushCommitMetadata>,
    /// Pipeline stage responsible for a rejected batch, when known.
    pub failure_stage: Option<PushFailureStage>,
    /// Immutable payloads uploaded by this push. `None` for rejected pushes
    /// that did not enter the transfer pipeline.
    pub transfer_stats: Option<PushTransferStats>,
}

/// Counts of newly uploaded immutable payloads for one push.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PushTransferStats {
    /// Number of xorb payloads written to the origin.
    pub xorbs_uploaded: u64,
    /// Number of shard payloads written to the origin.
    pub shards_uploaded: u64,
    /// Bytes in newly uploaded xorb payloads.
    pub xorb_bytes_uploaded: u64,
}

/// Coordinator metadata attached to a successful active-active push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCommitMetadata {
    pub operation_id: String,
    pub coordinator_epoch: u64,
    pub writer: String,
    pub writer_region: String,
    pub manifest_generation: u64,
    pub commit_state: PushTransactionState,
}

impl From<CommitOutcome> for PushCommitMetadata {
    fn from(outcome: CommitOutcome) -> Self {
        Self {
            operation_id: outcome.operation_id,
            coordinator_epoch: outcome.coordinator_epoch,
            writer: outcome.writer,
            writer_region: outcome.region,
            manifest_generation: outcome.manifest_generation,
            commit_state: outcome.state,
        }
    }
}

impl PushCommitMetadata {
    #[must_use]
    pub fn commit_state_name(&self) -> &'static str {
        match self.commit_state {
            PushTransactionState::Pending => "pending",
            PushTransactionState::ObjectsUploaded => "objects_uploaded",
            PushTransactionState::Committed => "committed",
            PushTransactionState::Materialized => "materialized",
            PushTransactionState::Aborted => "aborted",
        }
    }
}

impl PushResult {
    #[must_use]
    pub fn new(outcomes: HashMap<String, RefPushOutcome>) -> Self {
        Self {
            outcomes,
            active_active_commit: None,
            failure_stage: None,
            transfer_stats: None,
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(HashMap::new())
    }

    #[must_use]
    pub fn with_active_active_commit(mut self, commit: PushCommitMetadata) -> Self {
        self.active_active_commit = Some(commit);
        self
    }

    #[must_use]
    pub fn with_failure_stage(mut self, stage: PushFailureStage) -> Self {
        self.failure_stage = Some(stage);
        self
    }

    #[must_use]
    pub fn with_transfer_stats(mut self, stats: PushTransferStats) -> Self {
        self.transfer_stats = Some(stats);
        self
    }

    /// Returns `true` when every ref in the batch succeeded.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.outcomes
            .values()
            .all(|o| matches!(o, RefPushOutcome::Ok))
    }
}

#[derive(Debug, Serialize)]
struct PushAuditPayload<'a> {
    repo_prefix: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_stage: Option<PushFailureStage>,
    refs: Vec<PushAuditRef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_active: Option<PushAuditActiveActive<'a>>,
}

#[derive(Debug, Serialize)]
struct PushAuditRef<'a> {
    dst: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    src: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    force: Option<bool>,
    status: &'static str,
    reason_tag: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct PushAuditActiveActive<'a> {
    operation_id: &'a str,
    coordinator_epoch: u64,
    writer: &'a str,
    writer_region: &'a str,
    manifest_generation: u64,
    commit_state: &'static str,
}

pub fn record_push_audit_event(
    audit_path: &Path,
    repository: Option<&str>,
    repo_prefix: &str,
    specs: &[PushSpec],
    result: &PushResult,
    duration_ms: Option<u64>,
) -> Result<()> {
    if specs.is_empty() && result.outcomes.is_empty() {
        return Ok(());
    }
    let details = PushAuditPayload {
        repo_prefix,
        duration_ms,
        failure_stage: result.failure_stage,
        refs: push_audit_refs(specs, result),
        active_active: result
            .active_active_commit
            .as_ref()
            .map(|commit| PushAuditActiveActive {
                operation_id: &commit.operation_id,
                coordinator_epoch: commit.coordinator_epoch,
                writer: &commit.writer,
                writer_region: &commit.writer_region,
                manifest_generation: commit.manifest_generation,
                commit_state: commit.commit_state_name(),
            }),
    };
    let details = serde_json::to_value(details)
        .map_err(|err| CrabError::Internal(format!("serialize push audit details: {err}")))?;
    let event = AuditEvent::new(NewAuditEvent {
        operation: "push".to_owned(),
        outcome: if result.all_ok() {
            AuditOutcome::Success
        } else {
            AuditOutcome::Failure
        },
        actor: None,
        repository: repository.map(str::to_owned),
        details,
    });
    append_event(audit_path, &event)
}

fn push_audit_refs<'a>(specs: &'a [PushSpec], result: &'a PushResult) -> Vec<PushAuditRef<'a>> {
    let mut by_dst: HashMap<&str, &PushSpec> = HashMap::new();
    let mut keys = BTreeSet::new();
    for spec in specs {
        by_dst.insert(&spec.dst, spec);
        keys.insert(spec.dst.as_str());
    }
    for dst in result.outcomes.keys() {
        keys.insert(dst.as_str());
    }

    keys.into_iter()
        .map(|dst| {
            let outcome = result.outcomes.get(dst);
            let spec = by_dst.get(dst).copied();
            PushAuditRef {
                dst,
                src: spec
                    .map(|spec| spec.src.as_str())
                    .filter(|src| !src.is_empty()),
                force: spec.map(|spec| spec.force),
                status: match outcome {
                    Some(RefPushOutcome::Ok) => "ok",
                    Some(_) => "rejected",
                    None => "missing",
                },
                reason_tag: outcome.map_or("missing", RefPushOutcome::protocol_tag),
                reason: outcome
                    .filter(|outcome| !matches!(outcome, RefPushOutcome::Ok))
                    .map(ToString::to_string),
            }
        })
        .collect()
}

pub(crate) async fn compact_ref_journal_for_owner(
    store: &Store,
    router: &StoreLayout,
    lock_ttl: Duration,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let storage_router = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    crab_write::journal::compact_for_owner(
        store.as_storage(),
        &storage_router,
        lock_ttl,
        pusher,
        cancel,
    )
    .await
    .map_err(CrabError::from)
}

pub(crate) async fn compact_ref_journal_for_reader(
    store: &Store,
    router: &StoreLayout,
    lock_ttl: Duration,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let storage_router = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    crab_write::journal::compact_for_reader(
        store.as_storage(),
        &storage_router,
        lock_ttl,
        pusher,
        cancel,
    )
    .await
    .map_err(CrabError::from)
}

async fn acquire_current_git_locator_lock(
    store: &Store,
    router: &StoreLayout,
    anchor: CommittedManifestAnchor,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<PushLock>> {
    let deadline = Instant::now() + lock_ttl.saturating_mul(GIT_LOCATOR_LOCK_WAIT_TTL_MULTIPLIER);
    let mut attempt = 0;
    let mut acquire_context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    loop {
        check_cancelled(cancel)?;
        let (current, _) = read_manifest(store, router).await?;
        if current.generation != anchor.generation
            || current.pack_index_hash != anchor.pack_index_hash.hex()
        {
            return Ok(None);
        }
        match acquire_context
            .acquire_internal(
                router.repo_prefix(),
                crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
                lock_ttl,
            )
            .await
            .map_err(CrabError::from)
        {
            Ok(lock) => return Ok(Some(lock)),
            Err(error @ CrabError::PushLockHeld { .. }) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error);
                }
                let delay = push_lock_wait_delay(attempt, deadline.saturating_duration_since(now));
                attempt = attempt.saturating_add(1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis(),
                    generation = anchor.generation,
                    "current Git locator generation is waiting for the derived writer"
                );
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = cancel.cancelled() => return Err(CrabError::Cancelled),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn try_acquire_current_git_locator_lock(
    store: &Store,
    router: &StoreLayout,
    anchor: CommittedManifestAnchor,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<PushLock>> {
    check_cancelled(cancel)?;
    let (current, _) = read_manifest(store, router).await?;
    if current.generation != anchor.generation
        || current.pack_index_hash != anchor.pack_index_hash.hex()
    {
        return Ok(None);
    }
    let mut acquire_context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    match acquire_context
        .try_acquire_internal(
            router.repo_prefix(),
            crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
            lock_ttl,
        )
        .await
        .map_err(CrabError::from)
    {
        Ok(lock) => Ok(Some(lock)),
        Err(CrabError::PushLockHeld { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

async fn planned_locator_object_rows(
    store: &Store,
    router: &StoreLayout,
    current_packs: &[PackManifestEntry],
) -> Result<u64> {
    let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
        Arc::clone(store.inner()),
        router.repo_prefix(),
    )
    .await
    .map_err(CrabError::from)?;
    let coverage = session.coverage();
    let bindings = session.pack_bindings().await.map_err(CrabError::from);
    let close = session.close().await.map_err(CrabError::from);
    let bindings = match (bindings, close) {
        (Ok(bindings), Ok(())) => bindings,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(close_error)) => {
            warn!(
                error = %close_error,
                "Git locator budget session close also failed after reading pack bindings"
            );
            return Err(error);
        }
    };
    Ok(uncovered_locator_object_rows(
        coverage,
        &bindings,
        current_packs,
    ))
}

pub(crate) async fn publish_committed_pack_locators(
    store: &Store,
    router: &StoreLayout,
    packs: &[CommittedPackIndex<'_>],
    anchor: CommittedManifestAnchor,
    pinned_inventory: Option<&[PackManifestEntry]>,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<crab_metadata::git_object_locator::LocatorWriteStats> {
    publish_committed_pack_locators_with_mode(
        store,
        router,
        packs,
        anchor,
        pinned_inventory,
        lock_ttl,
        cancel,
        true,
        true,
    )
    .await
}

async fn publish_committed_pack_locators_with_mode(
    store: &Store,
    router: &StoreLayout,
    packs: &[CommittedPackIndex<'_>],
    anchor: CommittedManifestAnchor,
    pinned_inventory: Option<&[PackManifestEntry]>,
    lock_ttl: Duration,
    cancel: &CancellationToken,
    wait_for_lock: bool,
    allow_compaction: bool,
) -> Result<crab_metadata::git_object_locator::LocatorWriteStats> {
    if git_generation_owner_is_active(store, router).await? {
        debug!(
            generation = anchor.generation,
            "durable Git generation owner will publish locator coverage"
        );
        return Ok(crab_metadata::git_object_locator::LocatorWriteStats::default());
    }
    let mut local_evidence = HashMap::with_capacity(packs.len());
    for published in packs {
        let pack = published.pack;
        let pack_id = MerkleHash::from_hex(&pack.pack_id).map_err(|error| {
            CrabError::Internal(format!(
                "committed pack id is invalid for locator publication: {error}"
            ))
        })?;
        if local_evidence
            .insert(
                pack_id,
                LocatorPackEvidence::from_local(
                    pack,
                    published.idx_path,
                    published.rev_path,
                    published.git_sha1,
                    published.kind_by_oid.cloned(),
                )?,
            )
            .is_some()
        {
            return Err(CrabError::Internal(
                "locator publication received duplicate pack evidence".to_owned(),
            ));
        }
    }

    let lock = if wait_for_lock {
        acquire_current_git_locator_lock(store, router, anchor, lock_ttl, cancel).await?
    } else {
        try_acquire_current_git_locator_lock(store, router, anchor, lock_ttl, cancel).await?
    };
    let Some(mut lock) = lock else {
        return Ok(crab_metadata::git_object_locator::LocatorWriteStats::default());
    };
    let publication_started = Instant::now();
    let write_result = Box::pin(crab_coordination::while_renewing(&mut lock, None, async {
        let (current, _) = read_manifest(store, router).await?;
        if current.generation != anchor.generation
            || current.pack_index_hash != anchor.pack_index_hash.hex()
        {
            return Ok(crab_metadata::git_object_locator::LocatorWriteStats::default());
        }
        let loaded_inventory;
        let current_packs = if let Some(pinned) = pinned_inventory {
            let (pinned_hash, _, _) =
                crate::metadata::manifest::compact_pack_index(anchor.generation, pinned)?;
            if pinned_hash != current.pack_index_hash {
                return Err(CrabError::Internal(
                    "pinned locator inventory does not match its committed manifest".to_owned(),
                ));
            }
            pinned
        } else {
            loaded_inventory = read_bulk_pack_list(store, router, &current.pack_index_hash).await?;
            &loaded_inventory
        };

        let mut writer = if allow_compaction {
            // Owner publication can amortize locator maintenance over the
            // write, using the uncovered-pack budget to decide when to compact.
            let planned_object_rows =
                planned_locator_object_rows(store, router, current_packs).await?;
            crab_metadata::git_object_locator::GitObjectLocatorWriter::open_for_publication(
                Arc::clone(store.inner()),
                router.repo_prefix(),
                planned_object_rows,
            )
            .await?
        } else {
            // A read admission must not turn a one-object repair into a
            // repository-sized compaction; the owner handles that maintenance.
            crab_metadata::git_object_locator::GitObjectLocatorWriter::open_for_incremental_publication(
                Arc::clone(store.inner()),
                router.repo_prefix(),
            )
            .await?
        };
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(), router.repo_prefix().to_owned());
        let operation = crab_write::catalog::publish_inventory(
            &mut writer,
            store.as_storage(),
            &storage_router,
            &mut local_evidence,
            crab_metadata::git_object_locator::GitLocatorCoverage {
                generation: anchor.generation,
                pack_index_hash: anchor.pack_index_hash,
            },
            current_packs,
            wait_for_lock,
            cancel,
        )
        .await
        .map_err(CrabError::from);
        let close_result = writer.close().await.map_err(CrabError::from);
        match (operation, close_result) {
            (Ok(_), Ok(stats)) => Ok(stats),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(close_error)) => {
                warn!(
                    error = %close_error,
                    "Git locator close also failed after publication error"
                );
                Err(error)
            }
        }
    }))
    .await;
    let release_result = lock.release().await.map_err(CrabError::from);
    info!(
        elapsed_ms = publication_started.elapsed().as_millis(),
        succeeded = write_result.is_ok() && release_result.is_ok(),
        "Git locator publication phase finished"
    );
    let stats = write_result?;
    release_result?;
    Ok(stats)
}

pub(crate) async fn repair_git_object_locator_if_current(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    repair_git_object_locator_if_current_with_mode(
        store,
        router,
        required_generation,
        lock_ttl,
        cancel,
        true,
        true,
    )
    .await
}

pub(crate) async fn repair_git_object_locator_if_current_for_reader(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<bool> {
    repair_git_object_locator_if_current_with_mode(
        store,
        router,
        required_generation,
        lock_ttl,
        cancel,
        false,
        false,
    )
    .await
}

async fn repair_git_object_locator_if_current_with_mode(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
    wait_for_lock: bool,
    allow_compaction: bool,
) -> Result<bool> {
    let (manifest, _) = read_manifest(store, router).await?;
    // An active ref-journal transaction asks readers for the next generation.
    // Repair only an already-canonical generation or a read could bless stale state.
    if manifest.generation != required_generation {
        return Ok(false);
    }
    let Some(anchor) = committed_manifest_anchor(&manifest)? else {
        return Ok(false);
    };
    let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
        Arc::clone(store.inner()),
        router.repo_prefix(),
    )
    .await?;
    let already_covered = session.coverage()
        == Some(crab_metadata::git_object_locator::GitLocatorCoverage {
            generation: anchor.generation,
            pack_index_hash: anchor.pack_index_hash,
        });
    session.close().await?;
    if already_covered {
        return Ok(false);
    }
    let stats = publish_committed_pack_locators_with_mode(
        store,
        router,
        &[],
        anchor,
        None,
        lock_ttl,
        cancel,
        wait_for_lock,
        allow_compaction,
    )
    .await?;
    Ok(stats.coverage_updated)
}

pub(crate) async fn git_generation_owner_is_active(
    store: &Store,
    router: &StoreLayout,
) -> Result<bool> {
    PushLock::internal_lease_is_active(
        store.inner(),
        router.repo_prefix(),
        crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
    )
    .await
    .map_err(CrabError::from)
}

pub(crate) async fn repair_git_visibility_after_locator_if_current_with_limit(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    maximum_logical_objects: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<GitVisibilityPublication>> {
    Box::pin(repair_git_visibility_if_current_with_options(
        store,
        router,
        required_generation,
        maximum_logical_objects,
        false,
        lock_ttl,
        cancel,
        true,
        true,
    ))
    .await
}

pub(crate) async fn repair_git_visibility_after_locator_if_current_with_limit_for_reader(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    maximum_logical_objects: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<GitVisibilityPublication>> {
    Box::pin(repair_git_visibility_if_current_with_options(
        store,
        router,
        required_generation,
        maximum_logical_objects,
        false,
        lock_ttl,
        cancel,
        true,
        false,
    ))
    .await
}

async fn repair_git_visibility_if_current_with_options(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    maximum_logical_objects: u64,
    repair_locator: bool,
    lock_ttl: Duration,
    cancel: &CancellationToken,
    allow_catalog_materialization: bool,
    wait_for_lock: bool,
) -> Result<Option<GitVisibilityPublication>> {
    if maximum_logical_objects == 0
        || maximum_logical_objects > crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
    {
        return Err(CrabError::Configuration {
            key: "Git visibility logical object limit".to_owned(),
            origin: format!(
                "limit must be between 1 and {}",
                crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
            ),
        });
    }
    let (manifest, _) = read_manifest(store, router).await?;
    if manifest.generation != required_generation {
        return Ok(None);
    }
    let visibility_available = if allow_catalog_materialization {
        git_visibility_index_exists_for_manifest(store, router, &manifest).await?
    } else {
        git_visibility_proof_available_for_manifest(store, router, &manifest).await?
    };
    if visibility_available {
        return Ok(Some(GitVisibilityPublication::Published));
    }
    if manifest.refs.is_empty() {
        return Ok(Some(GitVisibilityPublication::Published));
    }
    if allow_catalog_materialization {
        let storage = store.as_storage().clone();
        let storage_router =
            crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
        if crab_metadata::git_visibility::ensure_catalog_bound(&storage, &storage_router, &manifest)
            .await
            .map_err(CrabError::from)?
        {
            debug!(
                generation = manifest.generation,
                "completed pending Git catalog visibility handoff"
            );
            return Ok(Some(GitVisibilityPublication::CatalogBound));
        }
    }
    let packs = read_bulk_pack_list(store, router, &manifest.pack_index_hash).await?;
    if let Some(capacity) =
        git_visibility_capacity_exceeded_at_limit(packs.iter(), maximum_logical_objects)?
    {
        return Ok(Some(GitVisibilityPublication::CompletePackOnly(capacity)));
    }

    // The generation owner publishes locator coverage before taking the
    // visibility lock. Keep that ordering even though cold visibility repair
    // now walks bulk-materialized packs instead of issuing per-object reads.
    if repair_locator {
        if wait_for_lock {
            repair_git_object_locator_if_current(
                store,
                router,
                required_generation,
                lock_ttl,
                cancel,
            )
            .await?;
        } else {
            repair_git_object_locator_if_current_for_reader(
                store,
                router,
                required_generation,
                lock_ttl,
                cancel,
            )
            .await?;
        }
    }
    let lock = if wait_for_lock {
        acquire_current_git_manifest_lock(store, router, required_generation, lock_ttl, cancel)
            .await?
    } else {
        try_acquire_current_git_manifest_lock(store, router, required_generation, lock_ttl, cancel)
            .await?
    };
    let Some(mut lock) = lock else {
        return Ok(None);
    };
    let repair = Box::pin(crab_coordination::while_renewing(&mut lock, None, async {
        let (current, _) = read_manifest(store, router).await?;
        if current.generation != required_generation
            || current.pack_index_hash != manifest.pack_index_hash
            || current.git_validation_digest != manifest.git_validation_digest
        {
            return Ok(None);
        }
        let visibility_available = if allow_catalog_materialization {
            git_visibility_index_exists_for_manifest(store, router, &current).await?
        } else {
            git_visibility_proof_available_for_manifest(store, router, &current).await?
        };
        if visibility_available {
            return Ok(Some(GitVisibilityPublication::Published));
        }

        let storage = store.as_storage().clone();
        let storage_router =
            crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
        let index = build_git_visibility_index_from_remote_packs(
            &storage,
            &storage_router,
            &current,
            &packs,
            cancel,
        )
        .await?;
        let (before_upload, _) = read_manifest(store, router).await?;
        if before_upload.generation != required_generation
            || before_upload.pack_index_hash != current.pack_index_hash
            || before_upload.git_validation_digest != current.git_validation_digest
        {
            return Ok(None);
        }
        if allow_catalog_materialization {
            crab_metadata::git_visibility::upload_if_absent(&storage, &storage_router, &index)
                .await
                .map_err(CrabError::from)?;
        } else {
            crab_metadata::git_visibility::upload_digest_bound_if_absent(
                &storage,
                &storage_router,
                &index,
            )
            .await
            .map_err(CrabError::from)?;
        }
        let (after_upload, _) = read_manifest(store, router).await?;
        if after_upload.generation != required_generation
            || after_upload.pack_index_hash != current.pack_index_hash
            || after_upload.git_validation_digest != current.git_validation_digest
        {
            return Ok(None);
        }
        Ok(Some(GitVisibilityPublication::Published))
    }))
    .await;
    let release = lock.release().await.map_err(CrabError::from);
    match (repair, release) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => {
            warn!(
                error = %release_error,
                "Git visibility lock release also failed after repair error"
            );
            Err(error)
        }
    }
}

async fn acquire_current_git_manifest_lock(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<PushLock>> {
    let deadline =
        Instant::now() + lock_ttl.saturating_mul(GIT_VISIBILITY_LOCK_WAIT_TTL_MULTIPLIER);
    let mut attempt = 0;
    let mut acquire_context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    loop {
        check_cancelled(cancel)?;
        let (current, _) = read_manifest(store, router).await?;
        if current.generation != required_generation {
            return Ok(None);
        }
        match acquire_context
            .acquire_internal(
                router.repo_prefix(),
                crab_coordination::GIT_MANIFEST_RESOURCE,
                lock_ttl,
            )
            .await
            .map_err(CrabError::from)
        {
            Ok(lock) => return Ok(Some(lock)),
            Err(error @ CrabError::PushLockHeld { .. }) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error);
                }
                let delay = push_lock_wait_delay(attempt, deadline.saturating_duration_since(now));
                attempt = attempt.saturating_add(1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis(),
                    generation = required_generation,
                    "current Git visibility generation is waiting for the manifest owner"
                );
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = cancel.cancelled() => return Err(CrabError::Cancelled),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn try_acquire_current_git_manifest_lock(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<PushLock>> {
    check_cancelled(cancel)?;
    let (current, _) = read_manifest(store, router).await?;
    if current.generation != required_generation {
        return Ok(None);
    }
    let mut acquire_context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    match acquire_context
        .try_acquire_internal(
            router.repo_prefix(),
            crab_coordination::GIT_MANIFEST_RESOURCE,
            lock_ttl,
        )
        .await
        .map_err(CrabError::from)
    {
        Ok(lock) => Ok(Some(lock)),
        Err(CrabError::PushLockHeld { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) async fn git_visibility_index_exists_for_manifest(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<bool> {
    let storage = store.as_storage();
    let storage_router =
        crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
    crab_metadata::git_visibility::catalog_bound_available(
        storage,
        &storage_router,
        manifest.generation,
        &manifest.pack_index_hash,
        &manifest.git_validation_digest,
    )
    .await
    .map_err(CrabError::from)
}

pub(crate) async fn git_visibility_proof_available_for_manifest(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<bool> {
    if git_visibility_index_exists_for_manifest(store, router, manifest).await? {
        return Ok(true);
    }
    let storage = store.as_storage();
    let storage_router =
        crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
    crab_metadata::git_visibility::digest_bound_available(
        storage,
        &storage_router,
        &manifest.git_validation_digest,
    )
    .await
    .map_err(CrabError::from)
}

/// Publish the generation-bound Git object visibility proof from a local ODB.
pub(crate) async fn publish_git_visibility_index_from_git_dir(
    git_dir: &Path,
    manifest: &Manifest,
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
) -> Result<()> {
    let storage = store.as_storage();
    let storage_router =
        crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
    publish_git_visibility_index_from_storage_git_dir(git_dir, manifest, &storage, &storage_router)
        .await
}

/// Publish the same visibility proof for callers that already own the shared
/// storage-domain store, such as metadata rebuild and maintenance commands.
pub(crate) async fn publish_git_visibility_index_from_storage_git_dir(
    git_dir: &Path,
    manifest: &Manifest,
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<()> {
    let index = build_git_visibility_index_from_storage_git_dir(git_dir, manifest).await?;
    crab_metadata::git_visibility::upload_if_absent(store, router, &index)
        .await
        .map_err(CrabError::from)
}

/// Build and upload a complete split commit graph from a fully materialized ODB.
pub(crate) async fn rebuild_split_commit_graph_from_storage_git_dir(
    git_dir: &Path,
    manifest: &Manifest,
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<String> {
    let refs = manifest
        .refs
        .iter()
        .map(|(name, oid)| {
            (
                name.clone(),
                manifest.peeled_refs.get(name).unwrap_or(oid).clone(),
            )
        })
        .collect::<Vec<_>>();
    let git_dir = git_dir.to_owned();
    let git_dir_display = git_dir.display().to_string();
    let shallow_git_dir = git_dir.clone();
    let (entries, inputs) = tokio::task::spawn_blocking(move || {
        let entries = collect_commit_entries(&git_dir, &refs)?;
        let inputs = collect_split_commit_graph_inputs(&git_dir, &entries)?;
        Ok::<_, CrabError>((entries, inputs))
    })
    .await
    .map_err(|error| CrabError::Internal(format!("commit graph rebuild join: {error}")))??;
    let roots = manifest
        .refs
        .iter()
        .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
        .map(|oid| parse_sha1_array(oid, "commit graph rebuild root"))
        .collect::<Result<Vec<_>>>()?;
    let write = append_split_commit_graph(
        None,
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        &roots,
        inputs,
    )?
    .ok_or_else(|| CrabError::CorruptObject {
        path: git_dir_display,
        reason: "materialized Git ODB does not contain every manifest ref commit".to_owned(),
    })?;
    upload_split_commit_graph(store, router, &write).await?;
    let _ = rebuild_shallow_closure_index_from_storage_git_dir(
        shallow_git_dir.as_path(),
        manifest,
        store,
        router,
    )
    .await?;
    info!(
        generation = manifest.generation,
        commits = entries.len(),
        layers = write.layers.len(),
        descriptor_hash = %write.descriptor_hash,
        "rebuilt complete split commit graph"
    );
    Ok(write.descriptor_hash)
}

const MAX_SHALLOW_CLOSURE_TIPS: usize = 128;

/// Build the exact shallow object closures used by the generation-bound
/// upload-pack accelerator.
pub(crate) async fn rebuild_shallow_closure_index_from_storage_git_dir(
    git_dir: &Path,
    manifest: &Manifest,
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<bool> {
    if manifest.refs.is_empty() {
        return Ok(false);
    }
    let roots = shallow_closure_roots(manifest)?;
    if roots.len() > MAX_SHALLOW_CLOSURE_TIPS {
        warn!(
            roots = roots.len(),
            maximum = MAX_SHALLOW_CLOSURE_TIPS,
            "skipping shallow closure index because the repository has too many advertised tips"
        );
        return Ok(false);
    }
    let git_dir = git_dir.to_owned();
    let entries = tokio::task::spawn_blocking(move || {
        let refs = roots
            .iter()
            .map(|(tip, _)| ("refs/heads/crab-tip".to_owned(), tip.clone()))
            .collect::<Vec<_>>();
        let commits = collect_commit_entries(&git_dir, &refs)?;
        build_shallow_closure_entries(&git_dir, &commits, &roots)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("shallow closure rebuild join: {error}")))??;
    let Some(entries) = entries else {
        return Ok(false);
    };
    let write = crab_metadata::shallow_closure::build_shallow_closure_write(
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        entries,
    )
    .map_err(CrabError::from)?;
    crab_metadata::shallow_closure::upload_shallow_closure(
        store,
        router,
        &manifest.git_validation_digest,
        &write,
    )
    .await
    .map_err(CrabError::from)?;
    info!(
        generation = manifest.generation,
        entries = write.entries.len(),
        descriptor_bytes = write.descriptor_bytes.len(),
        "published generation-bound shallow closure index"
    );
    Ok(true)
}

fn shallow_closure_roots(manifest: &Manifest) -> Result<Vec<(String, [u8; 20])>> {
    let mut roots = BTreeMap::new();
    for (name, oid) in &manifest.refs {
        let peeled = manifest.peeled_refs.get(name).unwrap_or(oid);
        let parsed = parse_sha1_array(peeled, "shallow closure root")?;
        roots.entry(parsed).or_insert_with(|| peeled.clone());
    }
    Ok(roots.into_iter().map(|(oid, hex)| (hex, oid)).collect())
}

fn build_shallow_closure_entries(
    git_dir: &Path,
    commits: &[CommitEntry],
    roots: &[(String, [u8; 20])],
) -> Result<Option<Vec<crab_metadata::shallow_closure::ShallowClosureEntry>>> {
    let mut parents = HashMap::<[u8; 20], Vec<[u8; 20]>>::with_capacity(commits.len());
    for commit in commits {
        let oid = parse_sha1_array(&commit.oid, "shallow closure commit")?;
        let commit_parents = commit
            .parents
            .iter()
            .map(|parent| parse_sha1_array(parent, "shallow closure parent"))
            .collect::<Result<Vec<_>>>()?;
        parents.insert(oid, commit_parents);
    }

    let mut entries = Vec::with_capacity(
        roots
            .len()
            .saturating_mul(crab_metadata::shallow_closure::DEFAULT_SHALLOW_CLOSURE_DEPTHS.len()),
    );
    for (tip_hex, tip) in roots {
        for &depth in crab_metadata::shallow_closure::DEFAULT_SHALLOW_CLOSURE_DEPTHS {
            let shallow = shallow_closure_boundaries(*tip, depth, &parents)?;
            let repository = create_shallow_enumeration_repository(git_dir, tip_hex, &shallow)?;
            let objects = enumerate_git_object_candidates_blocking(
                Some(repository.path().join("repo.git")),
                vec![tip_hex.clone()],
                Vec::new(),
            )?;
            let Some(objects) = objects else {
                warn!(
                    tip = %tip_hex,
                    depth,
                    "skipping shallow closure index because object enumeration exceeded its bound"
                );
                return Ok(None);
            };
            let object_ids = objects
                .into_iter()
                .map(|oid| {
                    oid.as_bytes().try_into().map_err(|_| {
                        CrabError::Internal(
                            "shallow closure enumeration returned a non-SHA-1 object".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<[u8; 20]>>>()?;
            entries.push(crab_metadata::shallow_closure::ShallowClosureEntry {
                tip: *tip,
                depth,
                object_ids,
                shallow,
            });
        }
    }
    Ok(Some(entries))
}

fn shallow_closure_boundaries(
    tip: [u8; 20],
    depth: u32,
    parents: &HashMap<[u8; 20], Vec<[u8; 20]>>,
) -> Result<Vec<[u8; 20]>> {
    let mut queue = VecDeque::from([(tip, 1_u32)]);
    let mut visited = HashMap::<[u8; 20], u32>::new();
    let mut shallow = BTreeSet::new();
    while let Some((oid, current_depth)) = queue.pop_front() {
        if visited
            .get(&oid)
            .is_some_and(|previous| *previous <= current_depth)
        {
            continue;
        }
        visited.insert(oid, current_depth);
        if current_depth >= depth {
            shallow.insert(oid);
            continue;
        }
        let commit_parents = parents.get(&oid).ok_or_else(|| CrabError::CorruptObject {
            path: oid.iter().map(|byte| format!("{byte:02x}")).collect(),
            reason: "shallow closure commit parent is absent from the complete commit graph"
                .to_owned(),
        })?;
        queue.extend(
            commit_parents
                .iter()
                .copied()
                .map(|parent| (parent, current_depth.saturating_add(1))),
        );
    }
    Ok(shallow.into_iter().collect())
}

fn create_shallow_enumeration_repository(
    source_git_dir: &Path,
    tip: &str,
    shallow: &[[u8; 20]],
) -> Result<tempfile::TempDir> {
    let temporary = tempfile::tempdir().map_err(CrabError::Io)?;
    let repository = temporary.path().join("repo.git");
    let output = std::process::Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&repository)
        .output()
        .map_err(CrabError::Io)?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git init for shallow closure enumeration failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let source_objects = crate::git::discover::resolve_common_dir(source_git_dir).join("objects");
    let alternates = repository.join("objects/info/alternates");
    std::fs::create_dir_all(alternates.parent().ok_or_else(|| {
        CrabError::Internal("shallow closure alternates path has no parent".to_owned())
    })?)
    .map_err(CrabError::Io)?;
    std::fs::write(&alternates, format!("{}\n", source_objects.display()))
        .map_err(CrabError::Io)?;
    let reference = repository.join("refs/heads/crab-tip");
    std::fs::create_dir_all(
        reference.parent().ok_or_else(|| {
            CrabError::Internal("shallow closure ref path has no parent".to_owned())
        })?,
    )
    .map_err(CrabError::Io)?;
    std::fs::write(reference, format!("{tip}\n")).map_err(CrabError::Io)?;
    if !shallow.is_empty() {
        let mut body = String::new();
        for oid in shallow {
            body.push_str(&sha1_hex(oid));
            body.push('\n');
        }
        std::fs::write(repository.join("shallow"), body).map_err(CrabError::Io)?;
    }
    Ok(temporary)
}

const GENERATION_OWNER_REMOTE_GRAPH_BATCH_SIZE: usize = 512;
const GENERATION_OWNER_REMOTE_GRAPH_MAX_DURATION: Duration = Duration::from_hours(2);

#[derive(Debug)]
pub(crate) struct SplitCommitGraphRebuild {
    pub(crate) hash: String,
    pub(crate) incremental: bool,
    pub(crate) layers: u64,
    pub(crate) bytes: u64,
    pub(crate) bytes_read: u64,
    pub(crate) bytes_written: u64,
}

#[derive(Debug)]
struct RemoteCommitGraphInputs {
    inputs: Vec<CommitGraphInput>,
    bytes_read: u64,
}

fn commit_graph_write_stats(write: &CommitGraphWrite) -> Result<(u64, u64, u64)> {
    let descriptor = decode_commit_graph_descriptor(&write.descriptor_bytes, "commit graph")
        .map_err(CrabError::from)?;
    let graph_bytes = descriptor.layers.iter().map(|layer| layer.bytes).sum();
    let written_bytes = write
        .layers
        .iter()
        .map(|layer| layer.bytes.len() as u64)
        .sum::<u64>()
        .saturating_add(write.descriptor_bytes.len() as u64);
    Ok((
        u64::try_from(descriptor.layers.len()).unwrap_or(u64::MAX),
        graph_bytes,
        written_bytes,
    ))
}

fn commit_graph_descriptor_stats(
    descriptor: &crab_metadata::split_commit_graph::CommitGraphDescriptor,
) -> Result<(u64, u64, u64)> {
    let descriptor_bytes = encode_commit_graph_descriptor(descriptor).map_err(CrabError::from)?;
    let graph_bytes = descriptor.layers.iter().map(|layer| layer.bytes).sum();
    Ok((
        u64::try_from(descriptor.layers.len()).unwrap_or(u64::MAX),
        graph_bytes,
        graph_bytes.saturating_add(descriptor_bytes.len() as u64),
    ))
}

async fn load_previous_split_commit_graph(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
    maximum_bytes: u64,
) -> Result<Option<SplitCommitGraph>> {
    let Some(previous_generation) = manifest.generation.checked_sub(1) else {
        return Ok(None);
    };
    let history = crate::metadata::manifest::list_manifest_history_for_generation(
        store,
        router,
        previous_generation,
    )
    .await?;
    let Some(base_manifest) = history
        .iter()
        .find(|entry| entry.manifest.commit_graph_hash.is_some())
        .map(|entry| &entry.manifest)
    else {
        return Ok(None);
    };
    let Some(hash) = base_manifest.commit_graph_hash.as_deref() else {
        return Ok(None);
    };
    let storage = store.as_storage();
    let storage_router = crab_storage::StoreLayout::with_global_prefix(
        storage.clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let graph = load_split_commit_graph(storage, &storage_router, hash, maximum_bytes).await?;
    let roots = base_manifest
        .refs
        .iter()
        .map(|(name, oid)| base_manifest.peeled_refs.get(name).unwrap_or(oid))
        .map(|oid| parse_sha1_array(oid, "base commit graph root"))
        .collect::<Result<Vec<_>>>()?;
    if graph.descriptor.generation != base_manifest.generation
        || graph.descriptor.pack_index_hash != base_manifest.pack_index_hash
        || graph.descriptor.git_validation_digest != base_manifest.git_validation_digest
        || roots.iter().any(|root| !graph.contains(root))
    {
        return Err(CrabError::CorruptObject {
            path: storage_router
                .bulk_manifest_path("commit-graph", hash)
                .to_string(),
            reason: "historical commit graph does not match its committed Git state".to_owned(),
        });
    }
    Ok(Some(graph))
}

fn generation_owner_remote_graph_options(maximum_bytes: u64) -> Result<RemoteGitRepositoryOptions> {
    let maximum_bytes = maximum_bytes.max(1);
    let maximum_objects = maximum_bytes
        .checked_div(60)
        .and_then(|value| value.checked_add(1))
        .unwrap_or(u64::MAX)
        .max(1);
    let object = RemoteGitObjectLimits {
        max_packed_entry_bytes: 128 * 1024 * 1024,
        max_inflated_entry_bytes: 128 * 1024 * 1024,
        max_object_bytes: 128 * 1024 * 1024,
        ..RemoteGitObjectLimits::default()
    };
    let operation = RemoteGitOperationLimits {
        max_duration: GENERATION_OWNER_REMOTE_GRAPH_MAX_DURATION,
        max_logical_objects: maximum_objects,
        max_storage_requests: maximum_objects.saturating_mul(4).max(1),
        max_fetched_bytes: maximum_bytes,
        max_inflated_bytes: maximum_bytes,
        max_history_commits: maximum_objects,
        max_response_bytes: maximum_bytes,
        ..RemoteGitOperationLimits::default()
    };
    RemoteGitRepositoryOptions::new(object, operation).map_err(remote_git_graph_error)
}

fn remote_git_graph_error(error: impl fmt::Display) -> CrabError {
    CrabError::Protocol(format!(
        "remote Git commit-graph maintenance failed: {error}"
    ))
}

async fn append_split_commit_graph_from_remote(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
    base: SplitCommitGraph,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Option<(CommitGraphWrite, u64)>> {
    let roots = manifest
        .refs
        .iter()
        .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
        .map(|oid| parse_sha1_array(oid, "remote commit graph root"))
        .collect::<Result<Vec<_>>>()?;
    let bucket = store.bucket_identity();
    let provider = format!("{:?}:{}:{}", bucket.cloud, bucket.host, bucket.container);
    let identity = RemoteGitRepositoryIdentity::new(provider, router.repo_prefix().to_owned(), 1)
        .map_err(remote_git_graph_error)?;
    let remote_router = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let repository = RemoteGitRepository::open(
        store.as_storage().clone(),
        remote_router,
        identity,
        Arc::new(RemoteGitRuntime::default()),
        generation_owner_remote_graph_options(maximum_bytes)?,
        cancel,
    )
    .await
    .map_err(remote_git_graph_error)?;
    if repository.generation() != manifest.generation {
        return Ok(None);
    }
    let operation = repository
        .operation(RemoteGitOperationKind::History, cancel)
        .await
        .map_err(remote_git_graph_error)?;
    let additions = collect_remote_commit_graph_inputs(&operation, &base, &roots).await;
    let additions = operation
        .finish(additions)
        .await
        .map_err(remote_git_graph_error)?;
    if !repository
        .is_current(cancel)
        .await
        .map_err(remote_git_graph_error)?
    {
        return Ok(None);
    }
    let write = append_split_commit_graph(
        Some(base),
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        &roots,
        additions.inputs,
    )
    .map_err(CrabError::from)?;
    Ok(write.map(|write| (write, additions.bytes_read)))
}

async fn collect_remote_commit_graph_inputs(
    operation: &RemoteGitOperationContext,
    base: &SplitCommitGraph,
    roots: &[[u8; 20]],
) -> std::result::Result<RemoteCommitGraphInputs, RemoteGitError> {
    let mut pending = VecDeque::new();
    let mut queued = HashSet::new();
    for root in roots {
        if !base.contains(root) && queued.insert(*root) {
            pending.push_back(*root);
        }
    }
    let mut additions = Vec::new();
    let mut bytes_read = 0_u64;
    while !pending.is_empty() {
        let mut requested = Vec::with_capacity(GENERATION_OWNER_REMOTE_GRAPH_BATCH_SIZE);
        while requested.len() < GENERATION_OWNER_REMOTE_GRAPH_BATCH_SIZE {
            let Some(oid) = pending.pop_front() else {
                break;
            };
            if !base.contains(&oid) {
                requested.push(gix_hash::ObjectId::from(oid));
            }
        }
        if requested.is_empty() {
            continue;
        }
        let objects = operation.read_objects(&requested).await?;
        if objects.len() != requested.len() {
            return Err(RemoteGitError::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            });
        }
        for (expected, object) in requested.into_iter().zip(objects) {
            if object.oid != expected {
                return Err(RemoteGitError::Corrupt {
                    stage: crab_remote_git::CorruptionStage::Commit,
                });
            }
            bytes_read = bytes_read.saturating_add(object.data.len() as u64);
            let input = remote_commit_graph_input(&object)?;
            for parent in &input.parents {
                if !base.contains(parent) && queued.insert(*parent) {
                    pending.push_back(*parent);
                }
            }
            additions.push(input);
        }
    }
    Ok(RemoteCommitGraphInputs {
        inputs: additions,
        bytes_read,
    })
}

fn remote_commit_graph_input(
    object: &RemoteGitObject,
) -> std::result::Result<CommitGraphInput, RemoteGitError> {
    if object.kind != gix_object::Kind::Commit {
        return Err(RemoteGitError::ObjectKind {
            oid: object.oid,
            expected: gix_object::Kind::Commit,
            actual: object.kind,
        });
    }
    let parsed = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|source| RemoteGitError::CommitParse {
            oid: object.oid,
            source,
        })?
        .into_owned()
        .map_err(|source| RemoteGitError::CommitParse {
            oid: object.oid,
            source,
        })?;
    let parents = parsed.parents.into_vec();
    Ok(CommitGraphInput {
        oid: commit_graph_oid_bytes(&object.oid),
        tree_oid: commit_graph_oid_bytes(&parsed.tree),
        commit_time: parsed.committer.time.seconds,
        parents: parents
            .iter()
            .map(|parent| commit_graph_oid_bytes(parent))
            .collect::<Vec<_>>(),
    })
}

async fn attach_split_commit_graph_if_current(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
    hash: &str,
) -> Result<Option<String>> {
    for _ in 0..3 {
        let (mut current, etag) = read_manifest(store, router).await?;
        if current.generation != manifest.generation
            || current.pack_index_hash != manifest.pack_index_hash
            || current.git_validation_digest != manifest.git_validation_digest
        {
            return Ok(None);
        }
        if let Some(existing) = current.commit_graph_hash.as_ref() {
            return Ok(Some(existing.clone()));
        }
        current.commit_graph_hash = Some(hash.to_owned());
        match write_manifest_cas(store, router, &current, &etag).await {
            Ok(_) => return Ok(Some(hash.to_owned())),
            Err(CrabError::CasConflict { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Err(CrabError::CasConflict {
        path: router.manifest_path().to_string(),
        expected_etag: None,
    })
}

/// Rebuild and attach a missing split graph from one pinned remote pack set.
pub(crate) async fn rebuild_split_commit_graph_from_remote_packs_if_current(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Option<SplitCommitGraphRebuild>> {
    let (manifest, _) = read_manifest(store, router).await?;
    if manifest.generation != required_generation || manifest.refs.is_empty() {
        return Ok(None);
    }
    if let Some(hash) = manifest.commit_graph_hash.as_ref() {
        return Ok(Some(SplitCommitGraphRebuild {
            hash: hash.clone(),
            incremental: false,
            layers: 0,
            bytes: 0,
            bytes_read: 0,
            bytes_written: 0,
        }));
    }
    if let Some(base) =
        load_previous_split_commit_graph(store, router, &manifest, maximum_bytes).await?
    {
        if let Some((write, bytes_read)) = append_split_commit_graph_from_remote(
            store,
            router,
            &manifest,
            base,
            maximum_bytes,
            cancel,
        )
        .await?
        {
            let (layers, bytes, bytes_written) = commit_graph_write_stats(&write)?;
            let descriptor_hash = write.descriptor_hash.clone();
            let storage = store.as_storage();
            let storage_router = crab_storage::StoreLayout::with_global_prefix(
                storage.clone(),
                router.repo_prefix().to_owned(),
                router.global_prefix().to_owned(),
            );
            upload_split_commit_graph(storage, &storage_router, &write).await?;
            let Some(hash) =
                attach_split_commit_graph_if_current(store, router, &manifest, &descriptor_hash)
                    .await?
            else {
                return Ok(None);
            };
            let incremental = hash == descriptor_hash;
            return Ok(Some(SplitCommitGraphRebuild {
                incremental,
                hash,
                layers,
                bytes,
                bytes_read: if incremental { bytes_read } else { 0 },
                bytes_written: if incremental { bytes_written } else { 0 },
            }));
        }
        let (current, _) = read_manifest(store, router).await?;
        if current.generation != manifest.generation
            || current.pack_index_hash != manifest.pack_index_hash
            || current.git_validation_digest != manifest.git_validation_digest
        {
            return Ok(None);
        }
    }
    let packs = read_bulk_pack_list(store, router, &manifest.pack_index_hash).await?;
    let storage = store.as_storage().clone();
    let storage_router =
        crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
    let materialized = materialize_remote_git_packs(
        &storage,
        &storage_router,
        &packs,
        maximum_bytes,
        MAX_BOUNDED_REMOTE_GIT_PACKS,
        cancel,
    )
    .await?;
    let (before_build, _) = read_manifest(store, router).await?;
    if before_build.generation != manifest.generation
        || before_build.pack_index_hash != manifest.pack_index_hash
        || before_build.git_validation_digest != manifest.git_validation_digest
    {
        return Ok(None);
    }
    let hash = rebuild_split_commit_graph_from_storage_git_dir(
        materialized.git_dir.path(),
        &manifest,
        &storage,
        &storage_router,
    )
    .await?;
    let Some(hash) = attach_split_commit_graph_if_current(store, router, &manifest, &hash).await?
    else {
        return Ok(None);
    };
    let descriptor =
        load_split_commit_graph_descriptor(&storage, &storage_router, &hash, maximum_bytes)
            .await
            .map_err(CrabError::from)?;
    let (layers, bytes, bytes_written) = commit_graph_descriptor_stats(&descriptor)?;
    Ok(Some(SplitCommitGraphRebuild {
        hash,
        incremental: false,
        layers,
        bytes,
        bytes_read: materialized.fetched_bytes,
        bytes_written,
    }))
}

/// Rebuild a missing shallow-closure index from one pinned remote pack set.
pub(crate) async fn rebuild_shallow_closure_index_from_remote_packs_if_current(
    store: &Store,
    router: &StoreLayout,
    required_generation: u64,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Option<bool>> {
    let (manifest, _) = read_manifest(store, router).await?;
    if manifest.generation != required_generation {
        return Ok(None);
    }
    if manifest.refs.is_empty() || manifest.pack_index_hash.is_empty() {
        return Ok(Some(false));
    }
    let storage = store.as_storage();
    let storage_router =
        crab_storage::StoreLayout::new(storage.clone(), router.repo_prefix().to_owned());
    match crab_metadata::shallow_closure::load_shallow_closure_descriptor(
        storage,
        &storage_router,
        &manifest.git_validation_digest,
        manifest.generation,
        &manifest.pack_index_hash,
        crab_metadata::shallow_closure::DEFAULT_MAX_SHALLOW_CLOSURE_DESCRIPTOR_BYTES,
    )
    .await
    {
        Ok(Some(_)) => return Ok(Some(false)),
        Ok(None) => {}
        Err(error) => return Err(error.into()),
    }
    let packs = read_bulk_pack_list(store, router, &manifest.pack_index_hash).await?;
    let materialized = materialize_remote_git_packs(
        storage,
        &storage_router,
        &packs,
        maximum_bytes,
        MAX_BOUNDED_REMOTE_GIT_PACKS,
        cancel,
    )
    .await?;
    let (current, _) = read_manifest(store, router).await?;
    if current.generation != manifest.generation
        || current.pack_index_hash != manifest.pack_index_hash
        || current.git_validation_digest != manifest.git_validation_digest
    {
        return Ok(None);
    }
    let rebuilt = rebuild_shallow_closure_index_from_storage_git_dir(
        materialized.git_dir.path(),
        &manifest,
        storage,
        &storage_router,
    )
    .await?;
    Ok(Some(rebuilt))
}

async fn build_git_visibility_index_from_remote_packs(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    manifest: &Manifest,
    packs: &[PackManifestEntry],
    cancel: &CancellationToken,
) -> Result<crab_metadata::git_visibility::GitVisibilityIndex> {
    let started = Instant::now();
    let materialized =
        materialize_remote_git_packs(store, router, packs, u64::MAX, usize::MAX, cancel).await?;
    let index =
        build_git_visibility_index_from_storage_git_dir(materialized.git_dir.path(), manifest)
            .await?;
    let logical_objects = index.membership_count();
    info!(
        target: "crab::git::visibility",
        telemetry_event = "operation_summary",
        operation = "visibility",
        outcome = "Success",
        duration_ms = started.elapsed().as_millis() as u64,
        logical_objects,
        storage_requests = materialized.pack_count,
        fetched_bytes = materialized.fetched_bytes,
        inflated_bytes = 0u64,
        response_bytes = 0u64,
        "remote Git operation summary"
    );
    Ok(index)
}

struct MaterializedRemoteGitPacks {
    git_dir: tempfile::TempDir,
    pack_count: u64,
    fetched_bytes: u64,
}

const REMOTE_PACK_MATERIALIZATION_CONCURRENCY: usize = 4;
const MAX_BOUNDED_REMOTE_GIT_PACKS: usize = 4_096;

async fn run_bounded_pack_materialization<F, Fut>(
    packs: Vec<PackManifestEntry>,
    concurrency: usize,
    cancel: &CancellationToken,
    materialize: F,
) -> Result<Vec<u64>>
where
    F: Fn(PackManifestEntry) -> Fut,
    Fut: Future<Output = Result<u64>>,
{
    check_cancelled(cancel)?;
    let results = futures_util::stream::iter(packs.into_iter().map(materialize))
        .buffer_unordered(concurrency.max(1))
        .try_collect::<Vec<_>>()
        .await?;
    check_cancelled(cancel)?;
    Ok(results)
}

async fn materialize_remote_git_packs(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
    packs: &[PackManifestEntry],
    maximum_bytes: u64,
    maximum_requests: usize,
    cancel: &CancellationToken,
) -> Result<MaterializedRemoteGitPacks> {
    let mut seen_pack_ids = HashSet::with_capacity(packs.len());
    let unique_packs = packs
        .iter()
        .filter(|pack| seen_pack_ids.insert(pack.pack_id.clone()))
        .cloned()
        .collect::<Vec<_>>();
    let unique_pack_count = unique_packs.len();
    if unique_pack_count > maximum_requests {
        return Err(CrabError::Internal(format!(
            "remote Git pack materialization requires {unique_pack_count} requests, budget is {maximum_requests}"
        )));
    }
    let planned_bytes = unique_packs.iter().try_fold(0_u64, |total, pack| {
        total.checked_add(pack.size).ok_or_else(|| {
            CrabError::Internal("remote Git pack materialization byte count overflow".to_owned())
        })
    })?;
    if planned_bytes > maximum_bytes {
        return Err(CrabError::Internal(format!(
            "remote Git pack materialization requires {planned_bytes} bytes, budget is {maximum_bytes}"
        )));
    }
    let git_dir = tempfile::tempdir()?;
    let pack_dir = git_dir.path().join("objects/pack");
    tokio::fs::create_dir_all(&pack_dir).await?;
    let git_dir_path = git_dir.path().to_owned();
    let pack_dir_for_tasks = pack_dir.clone();
    let store = store.clone();
    let router = router.clone();
    let cancel_for_tasks = cancel.clone();
    let downloaded = run_bounded_pack_materialization(
        unique_packs,
        REMOTE_PACK_MATERIALIZATION_CONCURRENCY,
        cancel,
        move |pack| {
            let store = store.clone();
            let router = router.clone();
            let cancel = cancel_for_tasks.clone();
            let git_dir = git_dir_path.clone();
            let pack_dir = pack_dir_for_tasks.clone();
            async move {
                check_cancelled(&cancel)?;
                let remote_path = router.pack_path(&pack.pack_id);
                let download = tempfile::Builder::new()
                    .prefix(".crab-visibility-pack-")
                    .suffix(".pack")
                    .tempfile_in(&git_dir)?
                    .into_temp_path();
                let downloaded = tokio::select! {
                    result = store.download_to_path_bounded(
                        &remote_path,
                        download.as_ref(),
                        pack.size,
                    ) => result?,
                    () = cancel.cancelled() => return Err(CrabError::Cancelled),
                };
                if downloaded != pack.size {
                    return Err(CrabError::CorruptObject {
                        path: remote_path.to_string(),
                        reason: format!(
                            "visibility repair downloaded {downloaded} bytes, expected {}",
                            pack.size
                        ),
                    });
                }

                let download_for_hash = download.to_path_buf();
                let (actual_hash, actual_size) =
                    tokio::task::spawn_blocking(move || hash_file_blake3(&download_for_hash))
                        .await
                        .map_err(|error| {
                            CrabError::Internal(format!(
                                "visibility pack hash join failed: {error}"
                            ))
                        })??;
                if actual_size != pack.size
                    || blake3::Hash::from(actual_hash).to_hex().as_str() != pack.pack_id
                {
                    return Err(CrabError::CorruptObject {
                        path: remote_path.to_string(),
                        reason: "visibility repair pack content hash does not match the manifest"
                            .to_owned(),
                    });
                }

                let installed = crate::git::pack::install_pack_file_locally(
                    &pack_dir,
                    download.as_ref(),
                    &pack.pack_id,
                    pack.size,
                    false,
                )
                .await?;
                check_cancelled(&cancel)?;
                let index = crab_git::pack_locator::PackLocationIter::open(
                    &installed.idx_path,
                    &installed.rev_path,
                    pack.size,
                )
                .map_err(crab_git::pack::PackError::from)?;
                if index.object_count() != pack.object_count {
                    return Err(CrabError::CorruptObject {
                        path: installed.idx_path.display().to_string(),
                        reason: format!(
                            "visibility repair index contains {} objects, expected {}",
                            index.object_count(),
                            pack.object_count
                        ),
                    });
                }
                if index.pack_checksum().to_string() != installed.git_sha1 {
                    return Err(CrabError::CorruptObject {
                        path: installed.idx_path.display().to_string(),
                        reason: "visibility repair index checksum disagrees with the pack trailer"
                            .to_owned(),
                    });
                }
                Ok(downloaded)
            }
        },
    )
    .await?;
    let fetched_bytes = downloaded
        .into_iter()
        .fold(0_u64, |total, bytes| total.saturating_add(bytes));
    Ok(MaterializedRemoteGitPacks {
        git_dir,
        pack_count: u64::try_from(unique_pack_count).unwrap_or(u64::MAX),
        fetched_bytes,
    })
}

/// Build the generation-bound Git visibility proof from a local ODB.
pub(crate) async fn build_git_visibility_index_from_storage_git_dir(
    git_dir: &Path,
    manifest: &Manifest,
) -> Result<crab_metadata::git_visibility::GitVisibilityIndex> {
    if manifest.refs.is_empty() {
        return crab_metadata::git_visibility::GitVisibilityIndex::new(
            manifest.generation,
            manifest.pack_index_hash.clone(),
            manifest.git_validation_digest.clone(),
            BTreeMap::new(),
        )
        .map_err(CrabError::from);
    }
    let refs = manifest
        .refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let peeled_refs = manifest.peeled_refs.clone();
    // Linked worktrees keep objects in the shared Git directory while the
    // per-worktree directory only contains refs, an index, and metadata.
    // Visibility walks always need the object-bearing common directory.
    let git_dir = crate::git::discover::resolve_common_dir(git_dir);
    let closures = tokio::task::spawn_blocking(move || {
        crab_git::walk::walk_reachable_by_ref_bounded(
            &git_dir,
            &refs,
            &peeled_refs,
            crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS as usize,
        )
    })
    .await
    .map_err(|error| CrabError::Internal(format!("Git visibility walk join failed: {error}")))?
    .map_err(CrabError::from)?;

    let refs = closures
        .into_iter()
        .map(|(name, closure)| {
            let mut objects = BTreeSet::new();
            objects.extend(closure.commits.iter().map(sha1_hex));
            objects.extend(closure.trees.iter().map(sha1_hex));
            objects.extend(closure.blobs.iter().map(sha1_hex));
            objects.extend(closure.tags.iter().map(sha1_hex));
            (name, objects.into_iter().collect::<Vec<_>>())
        })
        .collect();
    let index = crab_metadata::git_visibility::GitVisibilityIndex::new(
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        refs,
    )
    .map_err(CrabError::from)?;
    Ok(index)
}

pub(crate) fn enumerate_visibility_difference(
    git_dir: &Path,
    include: &str,
    exclude: Option<&str>,
    maximum: usize,
) -> Result<Option<Vec<String>>> {
    let mut command = crab_git::raw_object_command(git_dir);
    // Visibility needs only object IDs. Optional pathname hints are ambiguous
    // and can make the text reader reject otherwise valid raw Git paths.
    command
        .args(["rev-list", "--objects", "--no-object-names"])
        .arg(include);
    if let Some(exclude) = exclude {
        command.arg("--not").arg(exclude);
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(CrabError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CrabError::Internal("git rev-list stdout unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CrabError::Internal("git rev-list stderr unavailable".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr)
            .take(1024 * 1024)
            .read_to_end(&mut bytes);
        bytes
    });

    let mut objects = Vec::new();
    let mut exceeded = false;
    let mut operation_error = None;
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                operation_error = Some(CrabError::Io(error));
                let _ = child.kill();
                break;
            }
        };
        if objects.len() == maximum {
            exceeded = true;
            let _ = child.kill();
            break;
        }
        let oid = match gix_hash::ObjectId::from_hex(line.as_bytes()) {
            Ok(oid) => oid,
            Err(error) => {
                operation_error = Some(CrabError::Internal(format!(
                    "git rev-list returned invalid visibility object id: {error}"
                )));
                let _ = child.kill();
                break;
            }
        };
        objects.push(oid.to_hex().to_string());
    }
    let status = child.wait().map_err(CrabError::Io)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CrabError::Internal("git rev-list stderr reader panicked".to_owned()))?;
    if let Some(error) = operation_error {
        return Err(error);
    }
    if exceeded {
        return Ok(None);
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        if is_missing_object_error(&stderr) {
            return Ok(None);
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed while building visibility evidence: {stderr}"
        )));
    }
    objects.sort_unstable();
    objects.dedup();
    Ok(Some(objects))
}

fn sha1_hex(oid: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(40);
    for byte in oid {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
///
/// Walks commits reachable from the tips using `gix-traverse`, recording each
/// commit's OID and parent OIDs. The split-graph builder derives corrected
/// commit dates after it topologically orders the complete parent closure.
fn collect_commit_entries(
    git_dir: &std::path::Path,
    refs: &[(String, String)],
) -> Result<Vec<CommitEntry>> {
    use gix_hash::ObjectId;

    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Ok(Vec::new());
    }

    let odb = gix_odb::at(&objects_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open git ODB at {}: {e}",
            objects_dir.display()
        ))
    })?;

    let tips: Vec<ObjectId> = refs
        .iter()
        .filter_map(|(ref_name, sha)| {
            match ObjectId::from_hex(sha.as_bytes()) {
                Ok(oid) => Some(oid),
                Err(e) => {
                    warn!(ref_name = %ref_name, sha = %sha, error = %e, "skipping ref with invalid SHA");
                    None
                }
            }
        })
        .collect();

    if tips.is_empty() {
        return Ok(Vec::new());
    }

    let commit_walk = gix_traverse::commit::Simple::new(tips, &odb);
    let mut entries = Vec::new();

    for info_result in commit_walk {
        let info =
            info_result.map_err(|e| CrabError::Internal(format!("commit walk error: {e}")))?;

        let oid_hex = info.id.to_hex().to_string();
        let parent_oids: Vec<String> = info
            .parent_ids
            .iter()
            .map(|p| p.to_hex().to_string())
            .collect();

        // gen_number is filled in after the walk via topological sort.
        // See S1-P1-6 — BFS visits children before parents, so the old
        // inline computation always produced gen_number=0.
        entries.push(CommitEntry {
            oid: oid_hex,
            gen_number: 0,
            parents: parent_oids,
        });
    }

    crab_metadata::commit_graph::fill_generation_numbers(&mut entries);

    Ok(entries)
}

fn collect_split_commit_graph_inputs(
    git_dir: &Path,
    entries: &[CommitEntry],
) -> Result<Vec<CommitGraphInput>> {
    use gix_hash::ObjectId;
    use gix_object::FindExt as _;

    let objects_dir = crate::git::discover::resolve_common_dir(git_dir).join("objects");
    let odb = gix_odb::at(&objects_dir).map_err(|error| {
        CrabError::Internal(format!(
            "failed to open Git ODB at {} for commit graph: {error}",
            objects_dir.display()
        ))
    })?;
    let mut inputs = Vec::with_capacity(entries.len());
    for entry in entries {
        let oid = ObjectId::from_hex(entry.oid.as_bytes()).map_err(|error| {
            CrabError::Internal(format!("invalid commit graph OID {}: {error}", entry.oid))
        })?;
        let mut tree_buffer = Vec::new();
        let mut tree_iter = odb
            .find_commit_iter(&oid, &mut tree_buffer)
            .map_err(|error| {
                CrabError::Internal(format!("failed to read commit {oid} for graph: {error}"))
            })?;
        let tree_oid = tree_iter.tree_id().map_err(|error| {
            CrabError::Internal(format!("failed to read tree for commit {oid}: {error}"))
        })?;
        let mut signature_buffer = Vec::new();
        let commit_time = odb
            .find_commit_iter(&oid, &mut signature_buffer)
            .map_err(|error| {
                CrabError::Internal(format!("failed to read commit {oid} for graph: {error}"))
            })?
            .committer()
            .map_err(|error| {
                CrabError::Internal(format!("failed to read committer for {oid}: {error}"))
            })?
            .time()
            .map(|time| time.seconds)
            .unwrap_or(0);
        inputs.push(CommitGraphInput {
            oid: commit_graph_oid_bytes(&oid),
            tree_oid: commit_graph_oid_bytes(&tree_oid),
            commit_time,
            parents: entry
                .parents
                .iter()
                .map(|parent| parse_sha1_array(parent, "commit graph parent"))
                .collect::<Result<Vec<_>>>()?,
        });
    }
    Ok(inputs)
}

fn parse_sha1_array(value: &str, context: &str) -> Result<[u8; 20]> {
    let oid = gix_hash::ObjectId::from_hex(value.as_bytes())
        .map_err(|error| CrabError::Internal(format!("invalid {context} {value}: {error}")))?;
    Ok(commit_graph_oid_bytes(&oid))
}

fn commit_graph_oid_bytes(oid: &gix_hash::oid) -> [u8; 20] {
    let mut bytes = [0; 20];
    bytes.copy_from_slice(oid.as_bytes());
    bytes
}

/// Compute a short per-repo hash for local cache namespacing.
///
/// Returns the first 16 hex characters of `blake3(repo_prefix)`, giving
/// a 64-bit collision-resistant namespace for per-repo state directories.
pub(crate) fn compute_repo_hash(repo_prefix: &str) -> String {
    let digest = blake3::hash(repo_prefix.as_bytes());
    let hex = digest.to_hex();
    hex[..16].to_string()
}

fn hash_file_blake3(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok((*hasher.finalize().as_bytes(), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::chunk_reuse::{AddRemoteChunkClassifier, revalidate_staged_remote_chunks};
    use crate::git::xorb_proof::{RemoteXorbIndex, committed_origin_receipt_matches_remote};
    use crate::metadata::manifest::{BulkData, upload_segmented_bulk};
    use crab_storage::canonical_global_content_path;
    use crab_xet::reconstruction::ChunkPlacementMap;
    use crab_xet::xorb::builder::RunId;
    use crab_xet::xorb::format::{Chunk, ChunkPlacement, XorbRef};
    use futures_util::stream::BoxStream;
    use object_store::ObjectStoreExt;
    use object_store::path::Path;
    use object_store::{
        GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    fn test_store_router(prefix: &str) -> (Store, StoreLayout) {
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        (store, router)
    }

    async fn initialize_test_repository(store: &Store, router: &StoreLayout) {
        crate::core::remote_layout::initialize(store, router)
            .await
            .expect("initialize canonical test repository");
        crate::metadata::manifest::create_manifest(
            store,
            router,
            &Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .expect("initialize canonical v1 test manifest");
    }

    async fn ensure_test_layout(store: &Store, router: &StoreLayout) {
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        crab_metadata::layout_descriptor::ensure_canonical_layout(
            store.as_storage(),
            &storage_router,
        )
        .await
        .expect("publish canonical test layout");
    }

    fn locator_pack_fixture_for(
        content: &[u8],
    ) -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        String,
        gix_hash::ObjectId,
        u64,
    ) {
        fn run(args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
            let mut child = Command::new("git")
                .args(args)
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn git");
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(input)
                    .expect("write git stdin");
            }
            let output = child.wait_with_output().expect("wait for git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        let temp = tempfile::tempdir().expect("pack fixture tempdir");
        let git_dir = temp.path().join("repo.git");
        run(
            &["init", "--bare", git_dir.to_str().expect("git dir UTF-8")],
            None,
        );
        let oid_text = String::from_utf8(run(
            &[
                "--git-dir",
                git_dir.to_str().expect("git dir UTF-8"),
                "hash-object",
                "-w",
                "--stdin",
            ],
            Some(content),
        ))
        .expect("object ID UTF-8");
        let base = temp.path().join("fixture");
        let pack_hash = String::from_utf8(run(
            &[
                "--git-dir",
                git_dir.to_str().expect("git dir UTF-8"),
                "pack-objects",
                "--index-version=2",
                base.to_str().expect("pack base UTF-8"),
            ],
            Some(oid_text.as_bytes()),
        ))
        .expect("pack hash UTF-8");
        let pack_path = temp
            .path()
            .join(format!("fixture-{}.pack", pack_hash.trim()));
        let idx_path = temp.path().join("fixture.idx");
        run(
            &[
                "index-pack",
                "-o",
                idx_path.to_str().expect("idx path UTF-8"),
                pack_path.to_str().expect("pack path UTF-8"),
            ],
            None,
        );
        let rev_path = idx_path.with_extension("rev");
        crab_git::pack_locator::write_pack_reverse_index(&idx_path, &rev_path)
            .expect("write reverse index");
        let index =
            gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1).expect("open fixture index");
        let git_sha1 = index.pack_checksum().to_string();
        let oid = gix_hash::ObjectId::from_hex(oid_text.trim().as_bytes()).expect("fixture oid");
        let pack_size = std::fs::metadata(&pack_path)
            .expect("fixture pack metadata")
            .len();
        (
            temp, pack_path, idx_path, rev_path, git_sha1, oid, pack_size,
        )
    }

    #[test]
    fn remote_commit_graph_input_preserves_commit_edges_and_metadata() {
        let parent = gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .expect("parent oid");
        let tree = gix_hash::ObjectId::from_hex(b"2222222222222222222222222222222222222222")
            .expect("tree oid");
        let oid = gix_hash::ObjectId::from_hex(b"3333333333333333333333333333333333333333")
            .expect("commit oid");
        let data = format!(
            "tree {tree}\nparent {parent}\nauthor Crab Test <crab@example.invalid> 1 +0000\ncommitter Crab Test <crab@example.invalid> 7 +0000\n\nmessage\n"
        );
        let object = RemoteGitObject {
            oid,
            kind: gix_object::Kind::Commit,
            data: Bytes::from(data),
        };

        let input = remote_commit_graph_input(&object).expect("parse remote commit");

        assert_eq!(input.oid, commit_graph_oid_bytes(&oid));
        assert_eq!(input.tree_oid, commit_graph_oid_bytes(&tree));
        assert_eq!(input.commit_time, 7);
        assert_eq!(input.parents, vec![commit_graph_oid_bytes(&parent)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_appends_remote_commit_graph_from_prior_generation() {
        fn run_git(git_dir: &std::path::Path, args: &[&str], input: &[u8]) -> Vec<u8> {
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(git_dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Crab Test")
                .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
                .env("GIT_COMMITTER_NAME", "Crab Test")
                .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
                .env("GIT_AUTHOR_DATE", "@1 +0000")
                .env("GIT_COMMITTER_DATE", "@1 +0000")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().expect("spawn git");
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input)
                .expect("write git input");
            let output = child.wait_with_output().expect("wait for git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        let temporary = tempfile::tempdir().expect("graph fixture tempdir");
        let git_dir = temporary.path().join("repo.git");
        let init = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&git_dir)
            .output()
            .expect("initialize graph fixture");
        assert!(init.status.success());

        let blob = String::from_utf8(run_git(
            &git_dir,
            &["hash-object", "-w", "--stdin"],
            b"graph fixture\n",
        ))
        .expect("blob oid UTF-8")
        .trim()
        .to_owned();
        let tree = String::from_utf8(run_git(
            &git_dir,
            &["mktree"],
            format!("100644 blob {blob}\tfile.txt\n").as_bytes(),
        ))
        .expect("tree oid UTF-8")
        .trim()
        .to_owned();
        let first = String::from_utf8(run_git(
            &git_dir,
            &["commit-tree", &tree, "-m", "first"],
            &[],
        ))
        .expect("first commit oid UTF-8")
        .trim()
        .to_owned();
        let second = String::from_utf8(run_git(
            &git_dir,
            &["commit-tree", &tree, "-p", &first, "-m", "second"],
            &[],
        ))
        .expect("second commit oid UTF-8")
        .trim()
        .to_owned();
        let pack_base = temporary.path().join("fixture");
        let pack_hash = String::from_utf8(run_git(
            &git_dir,
            &[
                "pack-objects",
                "--index-version=2",
                pack_base.to_str().expect("pack base UTF-8"),
            ],
            format!("{second}\n{first}\n{tree}\n{blob}\n").as_bytes(),
        ))
        .expect("pack hash UTF-8")
        .trim()
        .to_owned();
        let pack_path = temporary.path().join(format!("fixture-{pack_hash}.pack"));
        let idx_path = temporary.path().join("fixture.idx");
        let index_output = Command::new("git")
            .args(["index-pack", "-o"])
            .arg(&idx_path)
            .arg(&pack_path)
            .output()
            .expect("index graph fixture pack");
        assert!(index_output.status.success());
        let rev_path = temporary.path().join("fixture.rev");
        crab_git::pack_locator::write_pack_reverse_index(&idx_path, &rev_path)
            .expect("write graph fixture reverse index");
        let pack_bytes = std::fs::read(&pack_path).expect("read graph fixture pack");
        let index = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1)
            .expect("open graph fixture index");
        let pack_size = pack_bytes.len() as u64;
        let object_count = u64::from(index.num_objects());
        let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let first_oid = gix_hash::ObjectId::from_hex(first.as_bytes()).expect("first oid");
        let second_oid = gix_hash::ObjectId::from_hex(second.as_bytes()).expect("second oid");
        let tree_oid = gix_hash::ObjectId::from_hex(tree.as_bytes()).expect("tree oid");
        let pack = PackManifestEntry {
            pack_id: pack_id.clone(),
            size: pack_size,
            content_hash: pack_id.clone(),
            ref_tips: vec![first.clone(), second.clone()],
            object_count,
        };
        let (pack_index_hash, _, pack_write) =
            crate::metadata::manifest::compact_pack_index(1, &[pack.clone()])
                .expect("build graph fixture pack index");
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let router = StoreLayout::new(store.clone(), "owner-remote-graph".to_owned());
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: crab_metadata::segmented::SegmentWrite::default(),
                pack_index: pack_write,
            },
        )
        .await
        .expect("upload graph fixture pack index");
        store
            .put(&router.pack_path(&pack_id), Bytes::from(pack_bytes))
            .await
            .expect("upload graph fixture pack");
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from(std::fs::read(&idx_path).expect("read graph fixture index")),
            )
            .await
            .expect("upload graph fixture index");
        store
            .put(
                &router.pack_reverse_index_path(&pack_id),
                Bytes::from(std::fs::read(&rev_path).expect("read graph fixture reverse index")),
            )
            .await
            .expect("upload graph fixture reverse index");

        let first_input = CommitGraphInput {
            oid: commit_graph_oid_bytes(&first_oid),
            tree_oid: commit_graph_oid_bytes(&tree_oid),
            commit_time: 1,
            parents: Vec::new(),
        };
        let mut base_manifest = Manifest::default_for_repo("refs/heads/main");
        base_manifest.generation = 1;
        base_manifest
            .refs
            .insert("refs/heads/main".to_owned(), first.clone());
        base_manifest.pack_index_hash = pack_index_hash.clone();
        base_manifest.seal_git_validation();
        let base_storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let base_write = append_split_commit_graph(
            None,
            base_manifest.generation,
            base_manifest.pack_index_hash.clone(),
            base_manifest.git_validation_digest.clone(),
            &[commit_graph_oid_bytes(&first_oid)],
            vec![first_input],
        )
        .expect("build base graph")
        .expect("base graph write");
        upload_split_commit_graph(store.as_storage(), &base_storage_router, &base_write)
            .await
            .expect("upload base graph");
        base_manifest.commit_graph_hash = Some(base_write.descriptor_hash);
        ensure_test_layout(&store, &router).await;
        crate::metadata::manifest::create_manifest(&store, &router, &base_manifest)
            .await
            .expect("publish base graph manifest");
        let mut current_manifest = base_manifest.clone();
        current_manifest.generation = 2;
        current_manifest
            .refs
            .insert("refs/heads/main".to_owned(), second.clone());
        current_manifest.commit_graph_hash = None;
        current_manifest.session_id = "second-generation".to_owned();
        current_manifest.seal_git_validation();
        let (_, etag) = read_manifest(&store, &router)
            .await
            .expect("read base graph manifest");
        write_manifest_cas(&store, &router, &current_manifest, &etag)
            .await
            .expect("publish second graph generation");

        crab_write::generation::maintain_catalog(
            store.as_storage(),
            &base_storage_router,
            &current_manifest,
            &[pack],
            Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .expect("publish graph fixture catalog")
        .expect("current fixture generation");

        let rebuild = rebuild_split_commit_graph_from_remote_packs_if_current(
            &store,
            &router,
            current_manifest.generation,
            64 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .await
        .expect("append remote graph");
        let rebuild = rebuild.expect("remote graph rebuild result");
        assert!(rebuild.incremental);
        assert_eq!(rebuild.layers, 2);
        assert!(rebuild.bytes_read > 0);
        assert!(rebuild.bytes_written > 0);
        let (attached_manifest, _) = read_manifest(&store, &router)
            .await
            .expect("read appended graph manifest");
        let graph_hash = attached_manifest
            .commit_graph_hash
            .as_deref()
            .expect("appended graph hash");
        let graph = load_split_commit_graph(
            store.as_storage(),
            &base_storage_router,
            graph_hash,
            64 * 1024 * 1024,
        )
        .await
        .expect("load appended graph");
        assert_eq!(graph.descriptor.commit_count, 2);
        assert_eq!(
            graph.is_ancestor(
                &commit_graph_oid_bytes(&first_oid),
                &commit_graph_oid_bytes(&second_oid)
            ),
            Some(true)
        );
    }

    #[test]
    fn shallow_closure_matches_git_depth_one_object_selection() {
        fn git(git_dir: &std::path::Path, args: &[&str], input: Option<&[u8]>) -> String {
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(git_dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Crab Test")
                .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
                .env("GIT_COMMITTER_NAME", "Crab Test")
                .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().expect("spawn git");
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(input)
                    .expect("write git stdin");
            }
            let output = child.wait_with_output().expect("wait for git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("git output UTF-8")
                .trim()
                .to_owned()
        }

        let temp = tempfile::tempdir().expect("repository tempdir");
        let git_dir = temp.path().join("repo.git");
        let initialized = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&git_dir)
            .status()
            .expect("initialize repository");
        assert!(initialized.success());

        let mut commits = Vec::new();
        let mut parent = None;
        for value in [
            b"one\n".as_slice(),
            b"two\n".as_slice(),
            b"three\n".as_slice(),
        ] {
            let blob = git(&git_dir, &["hash-object", "-w", "--stdin"], Some(value));
            let tree = git(
                &git_dir,
                &["mktree"],
                Some(format!("100644 blob {blob}\tfile.txt\n").as_bytes()),
            );
            let mut args = vec!["commit-tree", &tree];
            if let Some(parent) = parent.as_deref() {
                args.extend(["-p", parent]);
            }
            let commit = git(&git_dir, &args, Some(b"commit\n"));
            parent = Some(commit.clone());
            commits.push(commit);
        }
        let head = commits.last().cloned().expect("head commit");
        let _ = git(&git_dir, &["update-ref", "refs/heads/main", &head], None);
        let commit_entries =
            collect_commit_entries(&git_dir, &[("refs/heads/main".to_owned(), head.clone())])
                .expect("collect commits");
        let head_oid = parse_sha1_array(&head, "test head").expect("head object ID");
        let roots = vec![(head.clone(), head_oid)];
        let entries = build_shallow_closure_entries(&git_dir, &commit_entries, &roots)
            .expect("build shallow closures")
            .expect("closures within test bounds");
        let depth_one = entries
            .iter()
            .find(|entry| entry.depth == 1)
            .expect("depth one entry");
        assert_eq!(depth_one.shallow, vec![head_oid]);
        let parent_map = commit_entries
            .iter()
            .map(|entry| {
                (
                    parse_sha1_array(&entry.oid, "test commit").expect("commit object ID"),
                    entry
                        .parents
                        .iter()
                        .map(|parent| {
                            parse_sha1_array(parent, "test parent").expect("parent object ID")
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            shallow_closure_boundaries(head_oid, 2, &parent_map).expect("depth two boundary"),
            vec![parse_sha1_array(&commits[1], "middle").expect("middle object ID")]
        );
        assert!(
            entries
                .iter()
                .find(|entry| entry.depth == 10)
                .expect("deep entry")
                .shallow
                .is_empty()
        );

        let clone = temp.path().join("clone");
        let source_url = format!("file://{}", git_dir.display());
        let cloned = Command::new("git")
            .args([
                "clone",
                "--no-local",
                "--no-tags",
                "--depth=1",
                "--branch",
                "main",
            ])
            .arg(&source_url)
            .arg(&clone)
            .status()
            .expect("clone shallow repository");
        assert!(cloned.success());
        let shallow =
            std::fs::read_to_string(clone.join(".git/shallow")).expect("read Git shallow boundary");
        assert_eq!(shallow.trim(), head);
        let output = Command::new("git")
            .args(["--git-dir"])
            .arg(clone.join(".git"))
            .args(["rev-list", "--objects", "--no-object-names", "HEAD"])
            .output()
            .expect("enumerate cloned objects");
        assert!(output.status.success());
        let mut expected = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                gix_hash::ObjectId::from_hex(line)
                    .expect("cloned object ID")
                    .as_bytes()
                    .try_into()
                    .expect("cloned SHA-1 object ID")
            })
            .collect::<Vec<[u8; 20]>>();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(depth_one.object_ids, expected);
    }

    #[test]
    fn shallow_boundaries_match_git_for_an_octopus_merge_history() {
        fn git(git_dir: &std::path::Path, args: &[&str], input: Option<&[u8]>) -> String {
            let mut command = Command::new("git");
            command
                .arg("--git-dir")
                .arg(git_dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Crab Test")
                .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
                .env("GIT_COMMITTER_NAME", "Crab Test")
                .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().expect("spawn git");
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .expect("piped stdin")
                    .write_all(input)
                    .expect("write git stdin");
            }
            let output = child.wait_with_output().expect("wait for git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("git output UTF-8")
                .trim()
                .to_owned()
        }

        let temp = tempfile::tempdir().expect("repository tempdir");
        let git_dir = temp.path().join("repo.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&git_dir)
                .status()
                .expect("initialize repository")
                .success()
        );
        let root_blob = git(&git_dir, &["hash-object", "-w", "--stdin"], Some(b"root\n"));
        let root_tree = git(
            &git_dir,
            &["mktree"],
            Some(format!("100644 blob {root_blob}\tfile.txt\n").as_bytes()),
        );
        let root = git(&git_dir, &["commit-tree", &root_tree], Some(b"root\n"));
        let branches = [b"a\n", b"b\n", b"c\n"]
            .into_iter()
            .map(|content| {
                let blob = git(&git_dir, &["hash-object", "-w", "--stdin"], Some(content));
                let tree = git(
                    &git_dir,
                    &["mktree"],
                    Some(format!("100644 blob {blob}\tfile.txt\n").as_bytes()),
                );
                git(
                    &git_dir,
                    &["commit-tree", &tree, "-p", &root],
                    Some(b"branch\n"),
                )
            })
            .collect::<Vec<_>>();
        let merge_tree_blob = git(
            &git_dir,
            &["hash-object", "-w", "--stdin"],
            Some(b"merge\n"),
        );
        let merge_tree = git(
            &git_dir,
            &["mktree"],
            Some(format!("100644 blob {merge_tree_blob}\tfile.txt\n").as_bytes()),
        );
        let merge = git(
            &git_dir,
            &[
                "commit-tree",
                &merge_tree,
                "-p",
                &branches[0],
                "-p",
                &branches[1],
                "-p",
                &branches[2],
            ],
            Some(b"octopus\n"),
        );
        let _ = git(&git_dir, &["update-ref", "refs/heads/main", &merge], None);
        let commit_entries =
            collect_commit_entries(&git_dir, &[("refs/heads/main".to_owned(), merge.clone())])
                .expect("collect merge commits");
        let merge_oid = parse_sha1_array(&merge, "merge").expect("merge object ID");
        let parent_map = commit_entries
            .iter()
            .map(|entry| {
                (
                    parse_sha1_array(&entry.oid, "commit").expect("commit object ID"),
                    entry
                        .parents
                        .iter()
                        .map(|parent| parse_sha1_array(parent, "parent").expect("parent object ID"))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        let expected = shallow_closure_boundaries(merge_oid, 2, &parent_map)
            .expect("compute merge shallow boundaries");

        let clone = temp.path().join("clone");
        let source_url = format!("file://{}", git_dir.display());
        assert!(
            Command::new("git")
                .args([
                    "clone",
                    "--no-local",
                    "--no-tags",
                    "--depth=2",
                    "--branch",
                    "main"
                ])
                .arg(&source_url)
                .arg(&clone)
                .status()
                .expect("clone merge history")
                .success()
        );
        let mut actual = std::fs::read_to_string(clone.join(".git/shallow"))
            .expect("read merge shallow boundary")
            .lines()
            .map(|line| parse_sha1_array(line, "Git shallow boundary").expect("boundary object ID"))
            .collect::<Vec<_>>();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    fn test_deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut counter = 0u64;
        while out.len() < len {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&seed.to_le_bytes());
            hasher.update(&counter.to_le_bytes());
            out.extend_from_slice(hasher.finalize().as_bytes());
            counter += 1;
        }
        out.truncate(len);
        out
    }

    #[derive(Debug)]
    struct RecordingReadStore {
        inner: Arc<object_store::memory::InMemory>,
        reads: Arc<Mutex<Vec<String>>>,
    }

    impl std::fmt::Display for RecordingReadStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "RecordingReadStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for RecordingReadStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.reads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(location.to_string());
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            if let Some(prefix) = prefix {
                self.reads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(prefix.to_string());
            }
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[derive(Debug)]
    struct RangeCountingStore {
        inner: object_store::memory::InMemory,
        range_gets: Arc<AtomicUsize>,
        full_xorb_gets: AtomicUsize,
    }

    impl RangeCountingStore {
        fn new(range_gets: Arc<AtomicUsize>) -> Self {
            Self {
                inner: object_store::memory::InMemory::new(),
                range_gets,
                full_xorb_gets: AtomicUsize::new(0),
            }
        }
    }

    impl std::fmt::Display for RangeCountingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "RangeCountingStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for RangeCountingStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            let is_head = options.head;
            if !is_head && options.range.is_none() && location.as_ref().contains("/xorbs/") {
                self.full_xorb_gets.fetch_add(1, Ordering::Relaxed);
            }
            if options.range.is_some() {
                self.range_gets.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[derive(Debug)]
    struct GatedPutStore {
        inner: object_store::memory::InMemory,
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
        head_started: Option<Arc<tokio::sync::Semaphore>>,
        head_release: Option<Arc<tokio::sync::Semaphore>>,
        pack_only: bool,
        fail_next_put: AtomicBool,
    }

    impl std::fmt::Display for GatedPutStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "GatedPutStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for GatedPutStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            if self.pack_only && !location.as_ref().ends_with(".pack") {
                return self.inner.put_opts(location, payload, opts).await;
            }
            self.started.add_permits(1);
            let permit =
                self.release
                    .acquire()
                    .await
                    .map_err(|error| object_store::Error::Generic {
                        store: "gated",
                        source: Box::new(error),
                    })?;
            permit.forget();
            if self.fail_next_put.swap(false, Ordering::SeqCst) {
                return Err(object_store::Error::NotFound {
                    path: location.to_string(),
                    source: "injected pack upload failure".into(),
                });
            }
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if !options.head {
                return self.inner.get_opts(location, options).await;
            }
            if let (Some(started), Some(release)) = (&self.head_started, &self.head_release) {
                started.add_permits(1);
                let permit =
                    release
                        .acquire()
                        .await
                        .map_err(|error| object_store::Error::Generic {
                            store: "gated",
                            source: Box::new(error),
                        })?;
                permit.forget();
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    mod capsule_chunk_tests {
        use super::*;

        struct FirstCandidateOnly(Arc<AddRemoteChunkClassifier>);

        #[async_trait::async_trait]
        impl crab_staging::push_plan::ExistingChunkLookup for FirstCandidateOnly {
            async fn lookup_existing_candidates(
                &self,
                chunks: &[(MerkleHash, u64)],
            ) -> crab_staging::Result<Vec<Option<crab_staging::push_plan::ExistingChunkCandidate>>>
            {
                let candidates =
                    crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
                        self.0.as_ref(),
                        chunks,
                    )
                    .await?;
                Ok(candidates
                    .into_iter()
                    .enumerate()
                    .map(
                        |(index, candidate)| {
                            if index == 0 { candidate } else { None }
                        },
                    )
                    .collect())
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn capsule_push_exposes_committed_chunks_to_cold_cross_repository_add() {
            use crab_staging::push_plan::ExistingChunkLookup;
            use crab_staging::stream::{StreamStageProgress, stage_reader_streaming};

            let _git_env = CleanGitEnvGuard::new();
            let content = test_deterministic_bytes(0x4cab, 2 * 1024 * 1024);
            let source = tempfile::tempdir().unwrap();
            let git = |args: &[&str]| {
                let output = Command::new("git")
                    .args(args)
                    .current_dir(source.path())
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init", "-b", "main"]);
            let staging_root = source.path().join("staging");
            let staging = crab_staging::StagingArea::open(staging_root.clone())
                .await
                .unwrap();
            let staged = stage_reader_streaming(
                &mut std::io::Cursor::new(content.as_slice()),
                std::path::Path::new("model.bin"),
                &staging,
                StreamStageProgress::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            let chunks = staging
                .recipe_page(&staged.recipe, 0)
                .unwrap()
                .chunks
                .into_iter()
                .map(|chunk| (chunk.chunk_hash, chunk.len))
                .collect::<Vec<_>>();
            staging
                .delegate_publication(&[(&staged.batch_id, std::path::Path::new("model.bin"))])
                .unwrap();
            let pointer = crab_types::pointer::Pointer {
                file_hash: staged.file_hash,
                size: staged.size,
                shard_hint: None,
            };
            std::fs::write(source.path().join("model.bin"), pointer.serialize()).unwrap();
            git(&["add", "model.bin"]);
            git(&["commit", "-m", "shared payload"]);
            staging.close().await.unwrap();
            let staging = Arc::new(
                crab_staging::StagingAreaReadOnly::open(staging_root)
                    .await
                    .unwrap(),
            );
            let reads = Arc::new(RangeCountingStore::new(Arc::new(AtomicUsize::new(0))));
            let store = Store::new(reads.clone());
            let source_router = StoreLayout::new(store.clone(), "repos/source".to_owned());
            let layout = crab_storage::StoreLayout::new(
                store.as_storage().clone(),
                "repos/source".to_owned(),
            );
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
            let config = PushConfig {
                git_dir: Some(source.path().join(".git")),
                ..Default::default()
            };
            let (result, _) = crate::git::capsule_push::run(
                &config,
                &[crate::git::remote_helper::PushSpec {
                    force: false,
                    src: "refs/heads/main".to_owned(),
                    dst: "refs/heads/main".to_owned(),
                }],
                &store,
                &source_router,
                None,
                &[],
                Some(&staging),
                None,
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(result.all_ok(), "source publication failed: {result:?}");
            drop(staging);

            let router = StoreLayout::new(store.clone(), "repos/consumer".to_owned());
            let cache_root = tempfile::tempdir().unwrap();
            // No advisory caches or source-repository state can manufacture a hit.
            // This exercises the real publisher-to-classifier authority boundary.
            let classifier = Arc::new(AddRemoteChunkClassifier::with_test_cache(
                store.clone(),
                router.clone(),
                None,
            ));
            let candidates = classifier.lookup_existing_candidates(&chunks).await;
            let consumer_staging_root = cache_root.path().join("consumer-staging");
            let consumer_staging = crab_staging::StagingArea::open(consumer_staging_root.clone())
                .await
                .unwrap();
            let remote_staged = stage_reader_streaming(
                &mut std::io::Cursor::new(content.as_slice()),
                std::path::Path::new("model.bin"),
                &consumer_staging,
                StreamStageProgress {
                    existing_lookup: Some(classifier.clone()),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await;
            let partial_root = cache_root.path().join("partial-staging");
            let partial_staging = crab_staging::StagingArea::open(partial_root.clone())
                .await
                .unwrap();
            let partial_staged = stage_reader_streaming(
                &mut std::io::Cursor::new(content.as_slice()),
                std::path::Path::new("model.bin"),
                &partial_staging,
                StreamStageProgress {
                    existing_lookup: Some(Arc::new(FirstCandidateOnly(classifier.clone()))),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await;
            classifier.report_cache_stats();
            let partial_staged = partial_staged.unwrap();
            partial_staging
                .delegate_publication(&[(
                    &partial_staged.batch_id,
                    std::path::Path::new("model.bin"),
                )])
                .unwrap();
            partial_staging.close().await.unwrap();
            let remote_staged = remote_staged.unwrap();
            consumer_staging
                .delegate_publication(&[(
                    &remote_staged.batch_id,
                    std::path::Path::new("model.bin"),
                )])
                .unwrap();
            consumer_staging.close().await.unwrap();
            let candidates = candidates.unwrap();
            assert!(
                !candidates.is_empty() && candidates.iter().all(Option::is_some),
                "committed source chunks must be reusable from a cold consumer: {candidates:?}"
            );

            let planned = chunks
                .iter()
                .zip(&candidates)
                .filter_map(|((hash, _), candidate)| candidate.map(|candidate| (*hash, candidate)))
                .collect::<HashMap<_, _>>();
            let root = crab_metadata::capsule_protocol::load_root(&layout)
                .await
                .unwrap();
            let catalog =
                crab_metadata::capsule_protocol::load_pointer_catalog_from_root(&layout, &root)
                    .await
                    .unwrap();
            let mut origins = HashMap::new();
            for candidate in planned.values() {
                let path = layout.xorb_path(&candidate.xorb_ref.xorb_hash);
                let (bytes, identity) = layout.store().get_with_etag(&path).await.unwrap();
                let parser = crab_xet::xorb::parser::XorbParser::parse(bytes.clone()).unwrap();
                let mut origin = crab_metadata::receipts::OriginReceipt::new(
                    "canonical-origin".to_owned(),
                    path.to_string(),
                    candidate.xorb_ref.xorb_hash.into(),
                    parser.payload_digest(),
                    bytes.len() as u64,
                    identity.e_tag,
                    identity.version,
                );
                // Change the head's proof identity while retaining the actual committed source.
                origin.proven_at_unix_secs += 10;
                origins.insert(candidate.xorb_ref.xorb_hash, origin);
            }
            crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
                &layout,
                &catalog,
                &origins,
                &|| false,
            )
            .await
            .unwrap();
            let consumer_layout = crab_storage::StoreLayout::new(
                store.as_storage().clone(),
                "repos/consumer".to_owned(),
            );
            let verified = revalidate_staged_remote_chunks(
                &consumer_layout,
                &planned,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                verified.candidates.len(),
                planned.len(),
                "old exact proof survives a newer shared head"
            );

            for field in ["placement", "origin", "position"] {
                let mut forged = planned.clone();
                for candidate in forged.values_mut() {
                    match field {
                        "placement" => candidate.placement_id = [9; 32],
                        "origin" => candidate.origin_proof_id = [9; 32],
                        "position" => candidate.xorb_ref.chunk_index += 1,
                        _ => unreachable!(),
                    }
                }
                assert!(
                    revalidate_staged_remote_chunks(
                        &consumer_layout,
                        &forged,
                        &CancellationToken::new()
                    )
                    .await
                    .unwrap()
                    .candidates
                    .is_empty(),
                    "accepted forged {field}"
                );
            }

            let consumer_base = crab_write::capsule_protocol::initialize(
                &consumer_layout,
                &"2".repeat(64),
                "refs/heads/main",
            )
            .await
            .unwrap();
            let consumer_staging = Arc::new(
                crab_staging::StagingAreaReadOnly::open(consumer_staging_root)
                    .await
                    .unwrap(),
            );
            reads.full_xorb_gets.store(0, Ordering::Relaxed);
            let (delta, stats) = crate::git::xet_publication::prepare_delta(
                &consumer_layout,
                &consumer_base,
                &[crab_types::pointer::Pointer {
                    file_hash: remote_staged.file_hash,
                    size: remote_staged.size,
                    shard_hint: None,
                }],
                Some(&consumer_staging),
                None,
                None,
                1,
                true,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                (delta.files().len(), stats.xorbs_uploaded),
                (1, 0),
                "remote-only staging must publish without repacking"
            );
            assert_eq!(
                reads.full_xorb_gets.load(Ordering::Relaxed),
                0,
                "an exact, current source/origin proof must not download the reused xorb body"
            );
            drop(consumer_staging);
            let partial_staging = Arc::new(
                crab_staging::StagingAreaReadOnly::open(partial_root)
                    .await
                    .unwrap(),
            );
            reads.full_xorb_gets.store(0, Ordering::Relaxed);
            let (_, partial_stats) = crate::git::xet_publication::prepare_delta(
                &consumer_layout,
                &consumer_base,
                &[crab_types::pointer::Pointer {
                    file_hash: partial_staged.file_hash,
                    size: partial_staged.size,
                    shard_hint: None,
                }],
                Some(&partial_staging),
                None,
                None,
                1,
                true,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                (
                    partial_stats.xorbs_uploaded,
                    reads.full_xorb_gets.load(Ordering::Relaxed)
                ),
                (0, 0),
                "partial add-time hints must reuse the authenticated requested closure without packing or downloading"
            );
            drop(partial_staging);

            let source_key = layout.capsule_root_path();
            let (source_bytes, _) = layout.store().get_with_etag(&source_key).await.unwrap();
            layout.store().delete(&source_key).await.unwrap();
            let unrooted = revalidate_staged_remote_chunks(
                &consumer_layout,
                &planned,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            layout.store().put(&source_key, source_bytes).await.unwrap();
            assert!(
                unrooted.candidates.is_empty(),
                "physical bytes alone cannot replace a source root"
            );

            for candidate in planned.values() {
                let key = layout.xorb_path(&candidate.xorb_ref.xorb_hash);
                let (bytes, previous) = layout.store().get_with_etag(&key).await.unwrap();
                // Bypass the immutable adapter only to inject a provider-side replacement.
                let replaced = layout
                    .store()
                    .inner()
                    .put(&key, bytes.into())
                    .await
                    .unwrap();
                assert_ne!(
                    (previous.e_tag, previous.version),
                    (replaced.e_tag, replaced.version),
                    "the fault must actually change the provider identity"
                );
            }
            assert!(
                revalidate_staged_remote_chunks(
                    &consumer_layout,
                    &planned,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
                .candidates
                .is_empty(),
                "a replacement identity cannot reuse the original proof"
            );
            let verified_refs = verified.into_refs();
            let cached = classifier
                .verifier_for_test()
                .verify_origins(&verified_refs, &origins)
                .await
                .unwrap();
            assert!(
                cached.is_empty(),
                "warm proofs cannot certify the replacement identity"
            );

            // A fresh attestation for one occurrence must not certify another
            // occurrence's stale provider identity in the same immutable xorb.
            let mut fresh_origins = HashMap::new();
            for hash in planned
                .values()
                .map(|candidate| candidate.xorb_ref.xorb_hash)
                .collect::<HashSet<_>>()
            {
                let key = layout.xorb_path(&hash);
                let (bytes, identity) = layout.store().get_with_etag(&key).await.unwrap();
                let parser = crab_xet::xorb::parser::XorbParser::parse(bytes.clone()).unwrap();
                parser.verify_payload_digest().unwrap();
                parser.verify_all_chunks().unwrap();
                fresh_origins.insert(
                    hash,
                    crab_metadata::receipts::OriginReceipt::new(
                        "canonical-origin".to_owned(),
                        key.to_string(),
                        hash.into(),
                        parser.payload_digest(),
                        bytes.len() as u64,
                        identity.e_tag,
                        identity.version,
                    ),
                );
            }
            crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
                &layout,
                &catalog,
                &fresh_origins,
                &|| false,
            )
            .await
            .unwrap();
            let index = crab_metadata::capsule_protocol::CapsuleChunkIndex::new(&layout);
            let hashes = planned.keys().copied().collect::<Vec<_>>();
            let fresh = classifier
                .verifier_for_test()
                .lookup_capsule_chunks_for_add(&hashes, &index)
                .await;
            let fresh = fresh.unwrap();
            assert_eq!(
                fresh.len(),
                planned.len(),
                "replacement proof fixture must be independently valid"
            );
            assert!(
                fresh.len() > 1,
                "mixed-origin coverage needs distinct chunks in one xorb"
            );
            let mut mixed = planned.clone();
            let (hash, candidate) = fresh.into_iter().next().unwrap();
            mixed.insert(hash, candidate);
            assert!(
                matches!(
                    revalidate_staged_remote_chunks(
                        &consumer_layout,
                        &mixed,
                        &CancellationToken::new()
                    )
                    .await,
                    Err(CrabError::CorruptObject { .. })
                ),
                "conflicting immutable-xorb identities must not certify one another"
            );
        }
    }

    fn add_classifier_with_private_cache(
        store: &Store,
        router: &StoreLayout,
        cache_root: &std::path::Path,
    ) -> (
        AddRemoteChunkClassifier,
        Arc<crate::cache::add_remote_candidates::AddRemoteCandidateCache>,
    ) {
        let cache_root = cache_root.join("cache");
        crab_cache::ensure_private_cache_directory(&cache_root).expect("private cache root");
        let candidate_cache = Arc::new(
            crate::cache::add_remote_candidates::AddRemoteCandidateCache::open(
                &cache_root.join("candidates.sqlite"),
            )
            .expect("open candidate cache"),
        );
        let classifier = AddRemoteChunkClassifier::with_test_cache(
            store.clone(),
            router.clone(),
            Some(Arc::clone(&candidate_cache)),
        );
        (classifier, candidate_cache)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_classifier_ignores_candidates_cached_under_manifest_authority() {
        let root = tempfile::tempdir().unwrap();
        let _cache = crate::test::git_repo::CacheDirGuard::new(root.path());
        let (store, router) = test_store_router("source");
        let bucket_cache =
            crate::cache::chunk_index_cache_path(root.path(), &store.bucket_identity());
        let mut key = blake3::Hasher::new();
        key.update(b"crab-add-remote-candidates-v1\0");
        key.update(router.global_prefix().as_bytes());
        let path = bucket_cache.parent().unwrap().join(format!(
            "remote-candidates-{}.sqlite",
            &key.finalize().to_hex()[..16]
        ));
        let hash = MerkleHash::from([1; 32]);
        let candidate = crab_staging::push_plan::ExistingChunkCandidate {
            xorb_ref: XorbRef {
                xorb_hash: MerkleHash::from([2; 32]),
                chunk_index: 0,
                uncompressed_size: 4,
            },
            placement_id: [3; 32],
            origin_proof_id: [4; 32],
        };
        let old =
            crate::cache::add_remote_candidates::AddRemoteCandidateCache::open(&path).unwrap();
        old.persist_results(&[(hash, Some(candidate))]).unwrap();
        drop(old);

        let classifier = AddRemoteChunkClassifier::new(
            crate::core::config::DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY,
            store,
            None,
            router,
            CancellationToken::new(),
        );
        let candidates = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            &classifier,
            &[(hash, 4)],
        )
        .await
        .unwrap();
        classifier.report_cache_stats();
        assert_eq!(candidates, vec![None]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_classifier_does_not_open_manifest_metadata() {
        let cache_root = tempfile::tempdir().unwrap();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let store = Store::new(Arc::new(RecordingReadStore {
            inner: Arc::new(object_store::memory::InMemory::new()),
            reads: Arc::clone(&reads),
        }));
        let router = StoreLayout::new(store.clone(), "source".to_owned());
        initialize_test_repository(&store, &router).await;
        let (classifier, _) = add_classifier_with_private_cache(&store, &router, cache_root.path());
        reads.lock().unwrap().clear();

        let candidates = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            &classifier,
            &[(MerkleHash::from([1; 32]), 4)],
        )
        .await;
        classifier.report_cache_stats();
        let legacy = reads.lock().unwrap().iter().any(|path| {
            path == router.layout_descriptor_path().as_ref()
                || path == router.manifest_path().as_ref()
                || path.contains("chunk_index_db/")
        });
        assert_eq!((candidates.unwrap(), legacy), (vec![None], false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn staged_remote_chunks_do_not_probe_manifest_receipts() {
        let reads = Arc::new(Mutex::new(Vec::new()));
        let store = Store::new(Arc::new(RecordingReadStore {
            inner: Arc::new(object_store::memory::InMemory::new()),
            reads: Arc::clone(&reads),
        }));
        let router = StoreLayout::new(store.clone(), "source".to_owned());
        initialize_test_repository(&store, &router).await;
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let planned = HashMap::from([(
            MerkleHash::from([1; 32]),
            crab_staging::push_plan::ExistingChunkCandidate {
                xorb_ref: XorbRef {
                    xorb_hash: MerkleHash::from([2; 32]),
                    chunk_index: 0,
                    uncompressed_size: 4,
                },
                placement_id: [3; 32],
                origin_proof_id: [4; 32],
            },
        )]);
        reads.lock().unwrap().clear();

        let candidates =
            revalidate_staged_remote_chunks(&layout, &planned, &CancellationToken::new())
                .await
                .unwrap();
        let legacy = reads
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains("chunk_index_db/"));
        assert_eq!((candidates.candidates.len(), legacy), (0, false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_classifier_keeps_uninitialized_index_absence_session_scoped() {
        let cache_root = tempfile::tempdir().expect("private candidate cache");
        let inner = Arc::new(object_store::memory::InMemory::new());
        let reads = Arc::new(Mutex::new(Vec::new()));
        let recording_store: Arc<dyn ObjectStore> = Arc::new(RecordingReadStore {
            inner,
            reads: Arc::clone(&reads),
        });
        let store = Store::new(recording_store);
        let router = StoreLayout::new(store.clone(), "repo-add-uninitialized-index".to_owned());
        let (classifier, candidate_cache) =
            add_classifier_with_private_cache(&store, &router, cache_root.path());
        let first = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            &classifier,
            &[(MerkleHash::from([1; 32]), 4)],
        )
        .await
        .expect("first absent-index lookup");
        assert_eq!(first, vec![None]);
        let index_probes = || {
            reads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter(|path| path.ends_with("/capsule-chunk-index"))
                .count()
        };
        let initial_probes = index_probes();
        assert!(initial_probes > 0, "the missing index must be checked once");

        let second = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            &classifier,
            &[(MerkleHash::from([2; 32]), 4)],
        )
        .await
        .expect("second absent-index lookup");
        assert_eq!(second, vec![None]);
        assert_eq!(index_probes(), initial_probes);

        classifier.verifier_for_test().cancel.cancel();
        let cancelled = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            &classifier,
            &[(MerkleHash::from([3; 32]), 4)],
        )
        .await
        .expect_err("cancelled add must not return cached absence");
        assert!(matches!(cancelled, crab_staging::StagingError::Cancelled));
        assert_eq!(index_probes(), initial_probes);
        classifier.report_cache_stats();
        assert!(
            candidate_cache
                .load_persistent(&[MerkleHash::from([1; 32]), MerkleHash::from([2; 32])])
                .expect("inspect persisted candidates")
                .is_empty(),
            "an absent index must not write a per-chunk negative cache"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_classifier_persists_misses_from_an_initialized_index() {
        let cache_root = tempfile::tempdir().expect("private candidate cache");
        let (store, router) = test_store_router("repo-add-initialized-index");
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let xorb = MerkleHash::from([7; 32]);
        let mut catalog = crab_metadata::capsule_protocol::PointerCatalog::new();
        catalog
            .insert_xorb(
                xorb.hex(),
                crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                    80,
                    "8".repeat(64),
                    vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                        MerkleHash::from([6; 32]).hex(),
                        4,
                    )],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(
                "9".repeat(64),
                crab_metadata::capsule_protocol::ShardCatalogEntry::new(10, vec![xorb.hex()]),
            )
            .unwrap();
        let origins = HashMap::from([(
            xorb,
            crab_metadata::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                layout.xorb_path(&xorb).to_string(),
                xorb.into(),
                [8; 32],
                80,
                Some("etag".to_owned()),
                None,
            ),
        )]);
        crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
            &layout,
            &catalog,
            &origins,
            &|| false,
        )
        .await
        .unwrap();
        let (classifier, candidate_cache) =
            add_classifier_with_private_cache(&store, &router, cache_root.path());
        let hash = MerkleHash::from([4; 32]);
        let lookup = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            &classifier,
            &[(hash, 4)],
        )
        .await;
        classifier.report_cache_stats();
        lookup.expect("initialized-index lookup");
        assert_eq!(
            candidate_cache
                .load_persistent(&[hash])
                .expect("inspect persisted miss")
                .get(&hash),
            Some(&None)
        );
    }

    use crate::test::git_repo::CleanGitEnvGuard;

    fn pack_manifest_entry_with_tips(ref_tips: Vec<String>) -> PackManifestEntry {
        let pack_id = "a".repeat(64);
        PackManifestEntry {
            pack_id: pack_id.clone(),
            size: 1,
            content_hash: pack_id,
            ref_tips,
            object_count: 1,
        }
    }

    #[test]
    fn git_visibility_capacity_honors_owner_limit() {
        let mut pack = pack_manifest_entry_with_tips(Vec::new());
        pack.object_count = 101;

        let capacity = git_visibility_capacity_exceeded_at_limit([&pack], 100)
            .expect("count owner proof")
            .expect("owner limit should be enforced");

        assert_eq!(capacity.observed, 101);
        assert_eq!(capacity.maximum, 100);
        assert!(
            git_visibility_capacity_exceeded_at_limit([&pack], 101)
                .expect("count exact owner proof")
                .is_none()
        );
    }

    #[test]
    fn git_visibility_capacity_does_not_multiply_shared_refs() {
        let mut pack = pack_manifest_entry_with_tips(Vec::new());
        pack.object_count = 100;

        assert!(
            git_visibility_capacity_exceeded_at_limit([&pack], 100)
                .expect("count shared-history proof")
                .is_none(),
            "shared ref closures use one catalog object budget"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_pack_materialization_limits_concurrent_work() {
        let packs = (0..12)
            .map(|index| PackManifestEntry {
                pack_id: index.to_string(),
                size: 1,
                content_hash: index.to_string(),
                ref_tips: Vec::new(),
                object_count: 1,
            })
            .collect();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();

        let results = run_bounded_pack_materialization(packs, 3, &cancel, {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move |pack| {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                async move {
                    let in_flight = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(in_flight, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok::<u64, CrabError>(pack.size)
                }
            }
        })
        .await
        .expect("bounded materialization should complete");

        assert_eq!(results.len(), 12);
        assert_eq!(results.into_iter().sum::<u64>(), 12);
        assert_eq!(maximum.load(Ordering::SeqCst), 3);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_pack_materialization_installs_multiple_verified_packs() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let router = StoreLayout::new(store.clone(), "parallel-pack-materialization".to_owned());
        let mut fixtures = Vec::new();
        let mut packs = Vec::new();
        let mut expected_bytes = 0_u64;

        for index in 0..4 {
            let (fixture, pack_path, idx_path, _rev_path, _git_sha1, _oid, pack_size) =
                locator_pack_fixture_for(format!("parallel pack fixture {index}\n").as_bytes());
            let pack_bytes = std::fs::read(&pack_path).expect("read parallel pack fixture");
            let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
            let index = gix_pack::index::File::at(&idx_path, gix_hash::Kind::Sha1)
                .expect("open parallel pack fixture index");
            let pack = PackManifestEntry {
                pack_id: pack_id.clone(),
                size: pack_size,
                content_hash: pack_id.clone(),
                ref_tips: Vec::new(),
                object_count: u64::from(index.num_objects()),
            };
            store
                .put(&router.pack_path(&pack_id), Bytes::from(pack_bytes))
                .await
                .expect("upload parallel pack fixture");
            expected_bytes = expected_bytes.saturating_add(pack_size);
            fixtures.push(fixture);
            packs.push(pack);
        }

        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let materialized = materialize_remote_git_packs(
            store.as_storage(),
            &storage_router,
            &packs,
            expected_bytes,
            packs.len(),
            &CancellationToken::new(),
        )
        .await
        .expect("materialize parallel pack fixtures");

        assert_eq!(materialized.pack_count, 4);
        assert_eq!(materialized.fetched_bytes, expected_bytes);
        assert!(packs.iter().all(|pack| {
            materialized
                .git_dir
                .path()
                .join(format!("objects/pack/pack-{}.pack", pack.pack_id))
                .is_file()
        }));
        drop(fixtures);
    }

    #[test]
    fn visibility_and_candidates_preserve_raw_path_reachability() {
        let _guard = CleanGitEnvGuard::new();
        let repo = tempfile::tempdir().expect("bare repository");
        let git = |args: &[&str], input: Option<&[u8]>| {
            let mut child = Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn Git");
            if let Some(input) = input {
                child.stdin.take().unwrap().write_all(input).unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--bare", "--quiet"], None);
        let mut entries = Vec::new();
        let mut expected = Vec::new();
        for path in [
            b"raw-\xff".as_slice(),
            b"space name",
            b"line\nname",
            b"tab\tname",
        ] {
            let blob = git(&["hash-object", "-w", "--stdin"], Some(path));
            entries.extend_from_slice(format!("100644 blob {blob}\t").as_bytes());
            entries.extend_from_slice(path);
            entries.push(0);
            expected.push(blob);
        }
        let tree = git(&["mktree", "-z"], Some(&entries));
        let commit = git(&["commit-tree", &tree], Some(b"raw paths\n"));
        let candidates = enumerate_git_object_candidates_blocking(
            Some(repo.path().to_path_buf()),
            vec![commit.clone()],
            Vec::new(),
        )
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|oid| oid.to_string())
        .collect::<Vec<_>>();
        let visible = enumerate_visibility_difference(repo.path(), &commit, None, 100)
            .unwrap()
            .unwrap();
        expected.extend([tree, commit]);
        expected.sort_unstable();
        assert_eq!((candidates, visible), (expected.clone(), expected));
    }

    fn make_update(force: bool, old_sha: Option<&str>) -> RefUpdate {
        RefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_sha: old_sha.map(str::to_owned),
            new_sha: "new111".to_owned(),
            force,
        }
    }

    fn active_active_replication() -> ReplicationConfig {
        ReplicationConfig {
            mode: crate::replication::ReplicationMode::ActiveActive,
            coordinator: Some(crate::replication::ReplicationCoordinatorConfig {
                kind: crate::replication::ReplicationCoordinatorKind::Managed,
                url: "dynamodb://crab-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: vec!["us-west-2".into()],
                consistency: crate::replication::ReplicationCoordinatorConsistency::Linearizable,
            }),
            writers: vec![crate::replication::WriterConfig {
                name: "east".into(),
                url: "crab://primary/org/repo".into(),
                region: "us-east-1".into(),
                enabled: true,
            }],
            ..ReplicationConfig::default()
        }
    }

    // --- Force push skips fast-forward check ---

    #[test]
    fn force_push_proceeds_even_when_not_fast_forward() {
        let update = make_update(true, Some("old000"));
        let decision = check_ref_update(&update, false, Some("etag-abc".to_owned()));
        assert_eq!(
            decision,
            RefUpdateDecision::Proceed {
                etag: Some("etag-abc".to_owned()),
            }
        );
    }

    #[test]
    fn force_push_proceeds_when_fast_forward() {
        let update = make_update(true, Some("old000"));
        let decision = check_ref_update(&update, true, Some("etag-abc".to_owned()));
        assert_eq!(
            decision,
            RefUpdateDecision::Proceed {
                etag: Some("etag-abc".to_owned()),
            }
        );
    }

    // --- Normal push requires fast-forward ---

    #[test]
    fn normal_push_rejected_when_not_fast_forward() {
        let update = make_update(false, Some("old000"));
        let decision = check_ref_update(&update, false, Some("etag-abc".to_owned()));
        assert_eq!(
            decision,
            RefUpdateDecision::Reject(PushRejectReason::NonFastForward {
                have: "old000".to_owned(),
                want: "new111".to_owned(),
            })
        );
    }

    #[test]
    fn normal_push_proceeds_when_fast_forward() {
        let update = make_update(false, Some("old000"));
        let decision = check_ref_update(&update, true, Some("etag-abc".to_owned()));
        assert_eq!(
            decision,
            RefUpdateDecision::Proceed {
                etag: Some("etag-abc".to_owned()),
            }
        );
    }

    // --- Both use conditional write (ETag-based CAS) ---

    #[test]
    fn force_push_preserves_etag_for_cas() {
        let update = make_update(true, Some("old000"));
        let etag = Some("etag-xyz".to_owned());
        let decision = check_ref_update(&update, false, etag.clone());
        assert_eq!(decision, RefUpdateDecision::Proceed { etag });
    }

    #[test]
    fn normal_push_preserves_etag_for_cas() {
        let update = make_update(false, Some("old000"));
        let etag = Some("etag-xyz".to_owned());
        let decision = check_ref_update(&update, true, etag.clone());
        assert_eq!(decision, RefUpdateDecision::Proceed { etag });
    }

    // --- New ref creation ---

    #[test]
    fn new_ref_always_proceeds_without_force() {
        let update = make_update(false, None);
        let decision = check_ref_update(&update, true, None);
        assert_eq!(decision, RefUpdateDecision::Proceed { etag: None });
    }

    #[test]
    fn new_ref_always_proceeds_with_force() {
        let update = make_update(true, None);
        let decision = check_ref_update(&update, false, None);
        assert_eq!(decision, RefUpdateDecision::Proceed { etag: None });
    }

    // --- FF check reliability (P1-6) ---

    #[test]
    fn is_missing_object_error_recognizes_git_stderr_patterns() {
        // Shallow client without the old tip locally: git prints one
        // of these when `merge-base --is-ancestor` can't resolve the
        // SHA.
        assert!(is_missing_object_error(
            "fatal: Not a valid commit name old000\n"
        ));
        assert!(is_missing_object_error("error: not our ref old000"));
        assert!(is_missing_object_error("fatal: bad revision 'old000^@'"));
        assert!(is_missing_object_error("fatal: bad object old000"));
    }

    #[test]
    fn is_missing_object_error_rejects_unrelated_stderr() {
        // These should surface as HardError, not a summary fallback.
        assert!(!is_missing_object_error(
            "fatal: unable to access repository"
        ));
        assert!(!is_missing_object_error(
            "error: cannot spawn git: No such file or directory"
        ));
        assert!(!is_missing_object_error(""));
    }

    // --- Sub-task 2.3 status: is_fast_forward collapse ---
    //
    // The `merge-base --is-ancestor` shellout from earlier revisions
    // of crab has already been removed. `check_ref_update` now
    // takes `is_fast_forward: bool` as a parameter — the caller
    // supplies the bool. There is no in-tree function named
    // `is_fast_forward` that does the ancestor comparison itself;
    // `rg -n 'fn is_fast_forward|merge-base.*is-ancestor'
    //   crab/src` returns no matches.
    //
    // The two tests below mirror the spec-mandated
    // `fast_forward_detects_strict_ancestor` /
    // `fast_forward_rejects_divergent_history` test names and
    // cover the dispatch decision given a known bool. They are
    // not `#[ignore]` because they actually exercise what crab
    // has today; the `#[ignore]` case would apply if the spec had
    // asked us to add a new in-process `is_ancestor(...)` function
    // via `gix_revwalk::graph::Graph`, which it explicitly does not
    // per sub-task 2.3's "Do NOT invent a new fast-forward check
    // if none is currently called" guidance.

    #[test]
    fn fast_forward_detects_strict_ancestor() {
        // When the caller determined the new commit is a strict
        // descendant of the old (i.e. is_fast_forward = true),
        // a non-force push proceeds with its ETag intact.
        let update = make_update(false, Some("old000"));
        let decision = check_ref_update(&update, true, Some("etag-abc".to_owned()));
        assert_eq!(
            decision,
            RefUpdateDecision::Proceed {
                etag: Some("etag-abc".to_owned()),
            }
        );
    }

    #[test]
    fn fast_forward_rejects_divergent_history() {
        // When the caller determined histories are divergent (i.e.
        // is_fast_forward = false), a non-force push is rejected.
        let update = make_update(false, Some("old000"));
        let decision = check_ref_update(&update, false, Some("etag-abc".to_owned()));
        assert_eq!(
            decision,
            RefUpdateDecision::Reject(PushRejectReason::NonFastForward {
                have: "old000".to_owned(),
                want: "new111".to_owned(),
            })
        );
    }

    #[test]
    fn failure_stage_names_match_serialized_audit_values() {
        let stages = [
            PushFailureStage::Admission,
            PushFailureStage::StoreResolve,
            PushFailureStage::Discovery,
            PushFailureStage::RemoteState,
            PushFailureStage::Preflight,
            PushFailureStage::Classify,
            PushFailureStage::Lock,
            PushFailureStage::XorbPack,
            PushFailureStage::XorbUpload,
            PushFailureStage::GitPackPrepare,
            PushFailureStage::ShardBuild,
            PushFailureStage::ShardUpload,
            PushFailureStage::GitPackUpload,
            PushFailureStage::Connectivity,
            PushFailureStage::CommitGraph,
            PushFailureStage::RefCommit,
        ];

        for stage in stages {
            assert_eq!(
                serde_json::to_value(stage).expect("serialize failure stage"),
                serde_json::Value::String(stage.as_str().to_owned())
            );
        }
    }

    // --- PushConfig defaults ---

    #[test]
    fn push_config_default_concurrency() {
        let config = PushConfig::default();
        assert_eq!(config.upload_concurrency, 16);
    }

    #[test]
    fn push_config_from_config_uses_manifest_cas_retries() {
        let mut config = crate::core::config::Config::default();
        config.push_max_cas_retries = 123;

        let push_config = PushConfig::from_config(&config);

        assert_eq!(push_config.max_cas_retries, 123);
    }

    #[test]
    fn push_config_configures_xorb_target_size() {
        use crab_xet::xorb::builder::{
            CompressionPolicy, FixedCompression, PackOutcome, XorbBuilder,
        };
        use crab_xet::xorb::format::CompressionScheme;

        let mut config = crate::core::config::Config::default();
        config.xorb_target_size = 2 * 1024;
        config.min_xorb_size = 1024;
        config.max_xorb_size = 8 * 1024;
        config.max_xorb_overshoot_percent = 0.0;

        let push_config = PushConfig::from_config(&config);
        let policy: Arc<dyn CompressionPolicy> =
            Arc::new(FixedCompression::new(CompressionScheme::None));
        let mut builder = push_config.configure_builder(XorbBuilder::with_policy(policy));

        assert_eq!(
            builder
                .push(&Chunk::new(Bytes::from(vec![0xA1; 1500])), RunId(0))
                .expect("push first chunk"),
            PackOutcome::Packed
        );
        assert_eq!(
            builder
                .push(&Chunk::new(Bytes::from(vec![0xB2; 1500])), RunId(0))
                .expect("push second chunk"),
            PackOutcome::RolledOver
        );
        assert!(
            builder.take_completed().is_some(),
            "configured target should seal the first xorb before the second chunk"
        );
    }

    // --- PushResult ---

    #[test]
    fn push_result_all_ok_when_all_succeed() {
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert("refs/heads/dev".to_owned(), RefPushOutcome::Ok);
        let result = PushResult::new(outcomes);
        assert!(result.all_ok());
    }

    #[test]
    fn push_result_not_all_ok_when_one_fails() {
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert(
            "refs/heads/dev".to_owned(),
            RefPushOutcome::Rejected(PushRejectReason::Internal("rejected".to_owned())),
        );
        let result = PushResult::new(outcomes);
        assert!(!result.all_ok());
    }

    #[test]
    fn push_result_all_ok_on_empty() {
        let result = PushResult::empty();
        assert!(result.all_ok());
    }

    #[test]
    fn push_result_retains_transfer_stats() {
        let stats = PushTransferStats {
            xorbs_uploaded: 2,
            shards_uploaded: 3,
            xorb_bytes_uploaded: 4096,
        };
        let result = PushResult::empty().with_transfer_stats(stats);
        assert_eq!(result.transfer_stats, Some(stats));
    }

    #[test]
    fn push_audit_event_records_per_ref_outcomes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let specs = vec![
            PushSpec {
                force: false,
                src: "main".to_owned(),
                dst: "refs/heads/main".to_owned(),
            },
            PushSpec {
                force: true,
                src: "feature".to_owned(),
                dst: "refs/heads/feature".to_owned(),
            },
        ];
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert(
            "refs/heads/feature".to_owned(),
            RefPushOutcome::Rejected(PushRejectReason::NonFastForward {
                have: "old".to_owned(),
                want: "new".to_owned(),
            }),
        );
        let result = PushResult::new(outcomes).with_failure_stage(PushFailureStage::Preflight);

        record_push_audit_event(
            &path,
            Some("crab://bucket/repo"),
            "repo",
            &specs,
            &result,
            Some(42),
        )
        .expect("record push audit");

        let events = crate::audit::read_events(&path).expect("read audit events");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.operation, "push");
        assert_eq!(event.outcome, AuditOutcome::Failure);
        assert_eq!(event.repository.as_deref(), Some("crab://bucket/repo"));
        assert_eq!(event.details["repo_prefix"], "repo");
        assert_eq!(event.details["duration_ms"], 42);
        assert_eq!(event.details["failure_stage"], "preflight");
        assert_eq!(event.details["refs"][0]["status"], "rejected");
        assert_eq!(event.details["refs"][0]["reason_tag"], "non-fast-forward");
        assert_eq!(event.details["refs"][1]["status"], "ok");
        assert!(event.digest_valid());
    }

    // --- RefPushOutcome display ---

    #[test]
    fn ref_push_outcome_display_ok() {
        assert_eq!(RefPushOutcome::Ok.to_string(), "ok");
    }

    #[test]
    fn ref_push_outcome_display_error() {
        let outcome =
            RefPushOutcome::Rejected(PushRejectReason::Internal("non-fast-forward".to_owned()));
        assert_eq!(
            outcome.to_string(),
            "error: internal error: non-fast-forward"
        );
    }

    // --- Step 10: PackMetadata generation ---

    #[tokio::test]
    async fn pack_metadata_upsert_is_idempotent_and_unions_ref_tips() {
        let (store, router) = test_store_router("pack-metadata-union");
        let pack_id = "a".repeat(64);
        let path = router.pack_metadata_path(&pack_id);

        let first = upsert_pack_metadata(&store, &path, &pack_id, 7, vec!["1".repeat(40)], 4)
            .await
            .expect("create metadata");
        let second = upsert_pack_metadata(
            &store,
            &path,
            &pack_id,
            7,
            vec!["2".repeat(40), "1".repeat(40)],
            4,
        )
        .await
        .expect("union metadata");

        assert_eq!(first.ref_tips, vec!["1".repeat(40)]);
        assert_eq!(second.ref_tips, vec!["1".repeat(40), "2".repeat(40)]);
        let (body, _) = store.get_with_etag(&path).await.expect("read metadata");
        let stored: PackMetadata = serde_json::from_slice(&body).expect("parse metadata");
        assert_eq!(stored.ref_tips, second.ref_tips);
    }

    #[tokio::test]
    async fn pack_metadata_upsert_publishes_empty_hint_when_union_is_oversized() {
        let (store, router) = test_store_router("pack-metadata-oversized");
        let pack_id = "a".repeat(64);
        let path = router.pack_metadata_path(&pack_id);
        let ref_tips = (0..200_000)
            .map(|tip| format!("{tip:040x}"))
            .collect::<Vec<_>>();

        let metadata = upsert_pack_metadata(&store, &path, &pack_id, 7, ref_tips, 4)
            .await
            .expect("oversized hint should degrade to legacy selection");

        assert!(metadata.ref_tips.is_empty());
        let (body, _) = store
            .get_with_etag_bounded(&path, crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES)
            .await
            .expect("fallback metadata should remain bounded");
        let stored = crab_metadata::pack_metadata::parse_pack_metadata(&body, path.as_ref())
            .expect("fallback metadata should remain valid");
        assert!(stored.ref_tips.is_empty());
        assert_eq!(stored.object_count, 7);

        let follow_up = upsert_pack_metadata(&store, &path, &pack_id, 7, vec!["b".repeat(40)], 4)
            .await
            .expect("legacy fallback should not be enriched incompletely");
        assert!(follow_up.ref_tips.is_empty());
        let (body, _) = store
            .get_with_etag_bounded(&path, crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES)
            .await
            .expect("legacy fallback metadata should remain readable");
        let stored = crab_metadata::pack_metadata::parse_pack_metadata(&body, path.as_ref())
            .expect("legacy fallback metadata should remain valid");
        assert!(stored.ref_tips.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn current_git_locator_generation_waits_for_writer_handoff() {
        let (store, router) = test_store_router("locator-writer-handoff");
        let pack_index_hash = MerkleHash::from([7_u64, 8, 9, 10]);
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.pack_index_hash = pack_index_hash.hex();
        manifest.seal_git_validation();
        ensure_test_layout(&store, &router).await;
        crate::metadata::manifest::create_manifest(&store, &router, &manifest)
            .await
            .expect("publish current manifest");
        let blocker = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(1),
        )
        .await
        .expect("acquire first locator writer");
        let anchor = CommittedManifestAnchor {
            generation: manifest.generation,
            shard_index_hash: MerkleHash::default(),
            pack_index_hash,
        };
        let waiting_store = store.clone();
        let waiting_router = router.clone();
        let waiting = tokio::spawn(async move {
            acquire_current_git_locator_lock(
                &waiting_store,
                &waiting_router,
                anchor,
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished());
        blocker
            .release()
            .await
            .expect("release first locator writer");
        let acquired = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("waiting locator writer should acquire")
            .expect("locator wait task should complete")
            .expect("locator wait should succeed")
            .expect("current generation should still need publication");
        acquired
            .release()
            .await
            .expect("release waiting locator writer");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reader_locator_and_visibility_repairs_do_not_wait_for_writers() {
        let (store, router) = test_store_router("reader-repair-handoff");
        let pack_index_hash = MerkleHash::from([17_u64, 18, 19, 20]);
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.pack_index_hash = pack_index_hash.hex();
        manifest.seal_git_validation();
        ensure_test_layout(&store, &router).await;
        crate::metadata::manifest::create_manifest(&store, &router, &manifest)
            .await
            .expect("publish current manifest");
        let anchor = CommittedManifestAnchor {
            generation: manifest.generation,
            shard_index_hash: MerkleHash::default(),
            pack_index_hash,
        };

        let locator_blocker = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .expect("acquire locator writer");
        let locator = tokio::time::timeout(
            Duration::from_secs(1),
            try_acquire_current_git_locator_lock(
                &store,
                &router,
                anchor,
                Duration::from_secs(60),
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("reader locator repair should not wait")
        .expect("reader locator probe should succeed");
        assert!(locator.is_none());
        locator_blocker
            .release()
            .await
            .expect("release locator writer");

        let manifest_blocker = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::GIT_MANIFEST_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .expect("acquire visibility writer");
        let visibility = tokio::time::timeout(
            Duration::from_secs(1),
            try_acquire_current_git_manifest_lock(
                &store,
                &router,
                manifest.generation,
                Duration::from_secs(60),
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("reader visibility repair should not wait")
        .expect("reader visibility probe should succeed");
        assert!(visibility.is_none());
        manifest_blocker
            .release()
            .await
            .expect("release visibility writer");
    }

    #[tokio::test]
    async fn renewal_failure_cancels_long_lived_internal_owner() {
        let (store, router) = test_store_router("renewal-cancels-owner");
        let mut lock = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
            Duration::from_secs(1),
        )
        .await
        .expect("acquire owner");
        let replacement = serde_json::to_vec(&crab_coordination::PushLockPayload::new(
            "replacement-owner",
            u64::MAX,
            1,
        ))
        .expect("serialize replacement owner");
        store
            .inner()
            .put_opts(
                &Path::from(lock.path()),
                Bytes::from(replacement).into(),
                PutOptions::default(),
            )
            .await
            .expect("replace owner holder");
        let cancel = CancellationToken::new();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            crab_coordination::while_renewing(&mut lock, Some(&cancel), async {
                cancel.cancelled().await;
                Ok::<_, CrabError>(())
            }),
        )
        .await
        .expect("renewal failure should stop the owner");

        assert!(result.is_err());
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn completed_owner_does_not_wait_for_stalled_renewal() {
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(1));
        let store: Arc<dyn ObjectStore> = Arc::new(GatedPutStore {
            inner: object_store::memory::InMemory::new(),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            head_started: None,
            head_release: None,
            pack_only: false,
            fail_next_put: AtomicBool::new(false),
        });
        let mut lock = PushLock::acquire_internal(
            &store,
            "org/repo",
            crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
            Duration::from_secs(3),
        )
        .await
        .expect("acquire owner");
        started
            .acquire()
            .await
            .expect("observe owner acquisition")
            .forget();
        let operation_started = Arc::clone(&started);
        let cancel = CancellationToken::new();

        tokio::time::timeout(
            Duration::from_millis(1_500),
            crab_coordination::while_renewing(&mut lock, Some(&cancel), async move {
                operation_started
                    .acquire()
                    .await
                    .expect("observe stalled renewal")
                    .forget();
                Ok::<_, CrabError>(())
            }),
        )
        .await
        .expect("completed owner must win over a stalled renewal")
        .expect("owner operation succeeds");
        assert!(!cancel.is_cancelled());

        release.add_permits(1);
        lock.release().await.expect("release owner");
    }

    #[tokio::test]
    async fn git_object_locator_cli_publisher_advances_generations_without_new_packs() {
        let (store, router) = test_store_router("locator-publisher");
        let (_fixture, pack_path, idx_path, rev_path, git_sha1, oid, pack_size) =
            locator_pack_fixture_for(b"locator fixture object\n");
        let pack_id = "a".repeat(64);
        let pack = PackManifestEntry {
            pack_id: pack_id.clone(),
            size: pack_size,
            content_hash: pack_id.clone(),
            ref_tips: vec!["1".repeat(40)],
            object_count: 1,
        };
        let (pack_index_hash, _, pack_write) =
            crate::metadata::manifest::compact_pack_index(7, &[pack.clone()])
                .expect("build compact pack index");
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: crab_metadata::segmented::SegmentWrite::default(),
                pack_index: pack_write,
            },
        )
        .await
        .expect("upload pack index");
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 7;
        manifest.pack_index_hash = pack_index_hash.clone();
        manifest.seal_git_validation();
        ensure_test_layout(&store, &router).await;
        crate::metadata::manifest::create_manifest(&store, &router, &manifest)
            .await
            .expect("publish manifest");
        store
            .put(
                &router.pack_path(&pack_id),
                Bytes::from(std::fs::read(&pack_path).expect("read fixture pack")),
            )
            .await
            .expect("upload fixture pack");
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from(std::fs::read(&idx_path).expect("read fixture index")),
            )
            .await
            .expect("upload fixture index");
        store
            .put(
                &router.pack_reverse_index_path(&pack_id),
                Bytes::from(std::fs::read(&rev_path).expect("read fixture reverse index")),
            )
            .await
            .expect("upload fixture reverse index");

        let anchor = CommittedManifestAnchor {
            generation: 7,
            shard_index_hash: MerkleHash::default(),
            pack_index_hash: MerkleHash::from_hex(&pack_index_hash).expect("pack index hash"),
        };
        let stats = publish_committed_pack_locators(
            &store,
            &router,
            &[CommittedPackIndex {
                pack: &pack,
                idx_path: &idx_path,
                rev_path: &rev_path,
                git_sha1: &git_sha1,
                kind_by_oid: None,
            }],
            anchor,
            None,
            Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .expect("publish locators");
        assert_eq!(stats.object_rows_written, 1);
        assert_eq!(stats.flushes, 2);
        assert!(stats.coverage_updated);

        let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await
        .expect("open locator session");
        let oid_bytes: [u8; 20] = oid.as_bytes().try_into().expect("SHA-1 oid");
        let parsed_pack_id = MerkleHash::from_hex(&pack_id).expect("pack id");
        let inventory = HashMap::from([(
            parsed_pack_id,
            crab_metadata::git_object_locator::GitPackInventoryEntry {
                pack_id: parsed_pack_id,
                object_count: 1,
                pack_size,
            },
        )]);
        let lookup = session
            .lookup_batch(&[oid_bytes], &inventory)
            .await
            .expect("lookup locator");
        assert_eq!(
            session.coverage(),
            Some(crab_metadata::git_object_locator::GitLocatorCoverage {
                generation: 7,
                pack_index_hash: anchor.pack_index_hash,
            })
        );
        session.close().await.expect("close locator session");

        assert!(matches!(
            lookup.as_slice(),
            [crab_metadata::git_object_locator::GitObjectLookup::Hit(locator)]
                if locator.location.pack_offset >= 12
                    && locator.location.entry_len > 0
        ));

        let location = match lookup[0] {
            crab_metadata::git_object_locator::GitObjectLookup::Hit(locator) => locator.location,
            _ => unreachable!("initial locator must hit"),
        };
        let stale_pack_id = MerkleHash::from_hex(&"b".repeat(64)).expect("stale pack id");
        let mut stale_writer = crab_metadata::git_object_locator::GitObjectLocatorWriter::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await
        .expect("open stale locator writer");
        let stale_binding = stale_writer
            .bind_packs(&[crab_metadata::git_object_locator::GitPackLocatorRecord {
                pack_id: stale_pack_id,
                committed_generation: 8,
                pack_index_hash: MerkleHash::from([8, 9, 10, 11]),
                object_count: 1,
                pack_size,
            }])
            .await
            .expect("bind stale pack")[0];
        stale_writer
            .write_locations(
                stale_binding,
                &[crab_metadata::git_object_locator::GitObjectLocatorEntry {
                    oid: oid_bytes,
                    location,
                    metadata: Default::default(),
                }],
            )
            .await
            .expect("overwrite covered object with stale pack");
        stale_writer.flush_objects().await.expect("flush stale row");
        stale_writer.close().await.expect("close stale writer");

        let repaired = publish_committed_pack_locators(
            &store,
            &router,
            &[],
            anchor,
            None,
            Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .expect("repair stale duplicate row");
        assert_eq!(repaired.object_rows_written, 1);
        let repaired_session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await
        .expect("open repaired locator session");
        let repaired_lookup = repaired_session
            .lookup_batch(&[oid_bytes], &inventory)
            .await
            .expect("lookup repaired object");
        assert!(matches!(
            repaired_lookup.as_slice(),
            [crab_metadata::git_object_locator::GitObjectLookup::Hit(locator)]
                if locator.pack_id == parsed_pack_id
        ));
        repaired_session
            .close()
            .await
            .expect("close repaired locator session");

        let (mut next_manifest, etag) = read_manifest(&store, &router)
            .await
            .expect("read next manifest base");
        next_manifest.generation = 8;
        next_manifest.seal_git_validation();
        write_manifest_cas(&store, &router, &next_manifest, &etag)
            .await
            .expect("advance manifest without changing pack inventory");
        let next_anchor = CommittedManifestAnchor {
            generation: 8,
            ..anchor
        };

        let skipped = repair_git_object_locator_if_current(
            &store,
            &router,
            next_anchor.generation + 1,
            Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .expect("skip repair for a future generation");
        assert!(!skipped);

        let repaired = repair_git_object_locator_if_current(
            &store,
            &router,
            next_anchor.generation,
            Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .expect("repair current locator coverage without new packs");
        assert!(repaired);

        let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await
        .expect("open advanced locator session");
        assert_eq!(
            session.coverage(),
            Some(crab_metadata::git_object_locator::GitLocatorCoverage {
                generation: 8,
                pack_index_hash: next_anchor.pack_index_hash,
            })
        );
        session
            .close()
            .await
            .expect("close advanced locator session");

        let blocker = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
            Duration::from_secs(60),
        )
        .await
        .expect("hold locator writer");
        let already_covered = tokio::time::timeout(
            Duration::from_secs(1),
            repair_git_object_locator_if_current(
                &store,
                &router,
                next_anchor.generation,
                Duration::from_secs(60),
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("exact coverage should not wait for the writer")
        .expect("inspect exact coverage");
        assert!(!already_covered);
        blocker.release().await.expect("release locator writer");
    }

    #[tokio::test]
    async fn git_object_locator_cli_publisher_recovers_concurrent_sibling_evidence() {
        let (store, router) = test_store_router("locator-concurrent-sibling");
        let (
            _first_fixture,
            first_pack_path,
            first_idx_path,
            first_rev_path,
            _first_git_sha1,
            first_oid,
            first_pack_size,
        ) = locator_pack_fixture_for(b"first concurrent locator object\n");
        let (
            _second_fixture,
            _second_pack_path,
            second_idx_path,
            second_rev_path,
            second_git_sha1,
            second_oid,
            second_pack_size,
        ) = locator_pack_fixture_for(b"second concurrent locator object\n");
        let first_pack_bytes = std::fs::read(&first_pack_path).expect("read first pack");
        let first_pack_id = blake3::hash(&first_pack_bytes).to_hex().to_string();
        let second_pack_bytes = std::fs::read(&_second_pack_path).expect("read second pack");
        let second_pack_id = blake3::hash(&second_pack_bytes).to_hex().to_string();
        store
            .put(
                &router.pack_path(&first_pack_id),
                Bytes::from(first_pack_bytes),
            )
            .await
            .expect("upload first pack body");
        store
            .put(
                &router.pack_index_path(&first_pack_id),
                Bytes::from(std::fs::read(&first_idx_path).expect("read first index")),
            )
            .await
            .expect("upload first index evidence");
        store
            .put(
                &router.pack_reverse_index_path(&first_pack_id),
                Bytes::from(std::fs::read(&first_rev_path).expect("read first reverse index")),
            )
            .await
            .expect("upload first reverse-index evidence");

        let first_pack = PackManifestEntry {
            pack_id: first_pack_id.clone(),
            size: first_pack_size,
            content_hash: first_pack_id,
            ref_tips: vec!["1".repeat(40)],
            object_count: 1,
        };
        let second_pack = PackManifestEntry {
            pack_id: second_pack_id.clone(),
            size: second_pack_size,
            content_hash: second_pack_id,
            ref_tips: vec!["2".repeat(40)],
            object_count: 1,
        };
        let current_packs = vec![first_pack.clone(), second_pack.clone()];
        let (pack_index_hash, _, pack_write) =
            crate::metadata::manifest::compact_pack_index(9, &current_packs)
                .expect("build concurrent pack index");
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: crab_metadata::segmented::SegmentWrite::default(),
                pack_index: pack_write,
            },
        )
        .await
        .expect("upload concurrent pack index");
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 9;
        manifest.pack_index_hash = pack_index_hash.clone();
        manifest.seal_git_validation();
        ensure_test_layout(&store, &router).await;
        crate::metadata::manifest::create_manifest(&store, &router, &manifest)
            .await
            .expect("publish concurrent manifest");
        let anchor = CommittedManifestAnchor {
            generation: manifest.generation,
            shard_index_hash: MerkleHash::default(),
            pack_index_hash: MerkleHash::from_hex(&pack_index_hash).expect("pack index hash"),
        };

        let stats = publish_committed_pack_locators(
            &store,
            &router,
            &[CommittedPackIndex {
                pack: &second_pack,
                idx_path: &second_idx_path,
                rev_path: &second_rev_path,
                git_sha1: &second_git_sha1,
                kind_by_oid: None,
            }],
            anchor,
            Some(&current_packs),
            Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await
        .expect("publish complete concurrent generation");

        assert!(stats.coverage_updated);
        assert_eq!(stats.object_rows_written, 2);
        let inventory = current_packs
            .iter()
            .map(|pack| {
                let pack_id = MerkleHash::from_hex(&pack.pack_id).expect("pack id");
                (
                    pack_id,
                    crab_metadata::git_object_locator::GitPackInventoryEntry {
                        pack_id,
                        object_count: pack.object_count,
                        pack_size: pack.size,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let object_ids =
            [first_oid, second_oid].map(|oid| oid.as_bytes().try_into().expect("SHA-1 oid"));
        let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await
        .expect("open concurrent locator session");
        let lookups = session
            .lookup_batch(&object_ids, &inventory)
            .await
            .expect("lookup both concurrent objects");
        assert!(lookups.iter().all(|lookup| matches!(
            lookup,
            crab_metadata::git_object_locator::GitObjectLookup::Hit(_)
        )));
        session.close().await.expect("close locator session");
    }

    // --- Chunk placement map ---

    #[test]
    fn placement_map_tracks_all_chunks_with_correct_xorb_hashes_and_indices() {
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        fn make_test_chunk(seed: u32, size: usize) -> Chunk {
            let data: Vec<u8> = (0..size as u32)
                .map(|i| (i.wrapping_mul(seed.wrapping_mul(2654435761))) as u8)
                .collect();
            Chunk::new(Bytes::from(data))
        }

        // Create 3 "files" with known chunks.
        let file_a_chunks: Vec<Chunk> = (0..3u32).map(|i| make_test_chunk(i, 4096)).collect();
        let file_b_chunks: Vec<Chunk> = (10..13u32).map(|i| make_test_chunk(i, 2048)).collect();
        let file_c_chunks: Vec<Chunk> = (20..22u32).map(|i| make_test_chunk(i, 8192)).collect();

        let mut builder = XorbBuilder::new();
        for c in &file_a_chunks {
            builder.push(c, RunId(0)).unwrap();
        }
        for c in &file_b_chunks {
            builder.push(c, RunId(1)).unwrap();
        }
        for c in &file_c_chunks {
            builder.push(c, RunId(2)).unwrap();
        }

        let xorb_results = builder.finalize().unwrap();

        // Build placement map from inline placements.
        let mut placement_map: ChunkPlacementMap = HashMap::new();
        for result in &xorb_results {
            for p in &result.placements {
                placement_map.insert(
                    p.chunk_hash,
                    ChunkPlacement {
                        chunk_hash: p.chunk_hash,
                        xorb_hash: p.xorb_hash,
                        chunk_index: p.chunk_index,
                        uncompressed_size: p.uncompressed_size,
                    },
                );
            }
        }

        // All 8 chunks (3 + 3 + 2) should be in the map.
        let all_chunks: Vec<&Chunk> = file_a_chunks
            .iter()
            .chain(file_b_chunks.iter())
            .chain(file_c_chunks.iter())
            .collect();
        assert_eq!(placement_map.len(), all_chunks.len());

        for chunk in &all_chunks {
            let placement = placement_map
                .get(&chunk.hash)
                .unwrap_or_else(|| panic!("chunk {} missing from placement map", chunk.hash));

            // Verify the xorb hash matches a real xorb.
            assert!(
                xorb_results.iter().any(|x| x.hash == placement.xorb_hash),
                "placement xorb_hash should match a produced xorb"
            );
        }

        // Verify indices are sequential within each xorb (0, 1, ...).
        for result in &xorb_results {
            for (i, p) in result.placements.iter().enumerate() {
                assert_eq!(p.chunk_index, i as u32);
                assert_eq!(p.xorb_hash, result.hash);
            }
        }
    }

    // --- compute_repo_hash ---

    #[test]
    fn repo_hash_is_16_hex_chars() {
        let h = compute_repo_hash("org/models");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_hash_is_deterministic() {
        assert_eq!(
            compute_repo_hash("org/models"),
            compute_repo_hash("org/models")
        );
    }

    #[test]
    fn repo_hash_differs_for_different_prefixes() {
        assert_ne!(
            compute_repo_hash("org/models"),
            compute_repo_hash("org/datasets")
        );
    }

    #[tokio::test]
    async fn committed_origin_receipt_requires_current_object_identity() {
        let xorb_hash = MerkleHash::from([0xC01117, 0x5151, 0xA11, 0xD]);
        let path = canonical_global_content_path("xorbs", &xorb_hash.hex());
        let index = RemoteXorbIndex {
            hash: xorb_hash,
            payload_digest: [7; 32],
            chunks: Vec::new(),
        };
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        store
            .put(&path, Bytes::from_static(b"committed origin identity"))
            .await
            .expect("seed object identity");
        let meta = store.head(&path).await.expect("head object identity");
        let receipt = crab_metadata::receipts::OriginReceipt::new(
            "canonical-origin".to_owned(),
            path.to_string(),
            xorb_hash.into(),
            index.payload_digest,
            meta.size,
            meta.e_tag.clone(),
            meta.version.clone(),
        );

        assert!(committed_origin_receipt_matches_remote(
            &receipt, &xorb_hash, &path, &index, &meta,
        ));

        let mut changed_etag = meta.clone();
        changed_etag.e_tag = Some("etag-b".to_owned());
        assert!(!committed_origin_receipt_matches_remote(
            &receipt,
            &xorb_hash,
            &path,
            &index,
            &changed_etag,
        ));

        let mut missing_identity = receipt.clone();
        missing_identity.etag = None;
        missing_identity.object_version = None;
        assert!(!committed_origin_receipt_matches_remote(
            &missing_identity,
            &xorb_hash,
            &path,
            &index,
            &meta,
        ));

        let mut changed_digest = index.clone();
        changed_digest.payload_digest = [8; 32];
        assert!(!committed_origin_receipt_matches_remote(
            &receipt,
            &xorb_hash,
            &path,
            &changed_digest,
            &meta,
        ));
    }

    #[test]
    fn push_config_from_config_carries_active_active_to_pipeline() {
        let mut config = crate::core::config::Config::default();
        config.replication = Some(active_active_replication());

        let push_config = PushConfig::from_config(&config);

        assert!(
            push_config
                .active_active_replication
                .as_ref()
                .is_some_and(|replication| replication.is_active_active())
        );
    }

    #[test]
    fn active_active_writer_selection_uses_push_remote_url() {
        let mut replication = active_active_replication();
        replication.writers.push(crate::replication::WriterConfig {
            name: "west".into(),
            url: "crab://replica/org/repo".into(),
            region: "us-west-2".into(),
            enabled: true,
        });

        let writer = crate::replication::active_active_writer_name_for_remote(
            &replication,
            Some("crab://replica/org/repo"),
        )
        .unwrap();

        assert_eq!(writer, "west");
    }

    #[test]
    fn active_active_writer_selection_rejects_unknown_remote_url() {
        let err = crate::replication::active_active_writer_name_for_remote(
            &active_active_replication(),
            Some("crab://other/org/repo"),
        )
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(
            err.to_string()
                .contains("does not match any configured enabled writer")
        );
    }

    #[test]
    fn active_active_writer_selection_rejects_disabled_writer_url() {
        let mut replication = active_active_replication();
        replication.writers[0].enabled = false;
        replication.writers.push(crate::replication::WriterConfig {
            name: "west".into(),
            url: "crab://replica/org/repo".into(),
            region: "us-west-2".into(),
            enabled: true,
        });

        let err = crate::replication::active_active_writer_name_for_remote(
            &replication,
            Some("crab://primary/org/repo"),
        )
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("writer east is disabled"));
    }

    #[tokio::test]
    async fn active_active_push_coordinator_defers_crab_auth_commit_to_service() {
        let mut config = crate::core::config::Config::default();
        config.auth.provider = crate::core::config::AuthProvider::CrabAuth;
        config.replication = Some(active_active_replication());
        let mut push_config = PushConfig::from_config(&config);

        configure_active_active_push_coordinator(
            &config,
            Some("crab://primary/org/repo"),
            "org/repo",
            &mut push_config,
        )
        .await
        .unwrap();

        assert!(push_config.active_active_coordinator.is_none());
        assert_eq!(push_config.active_active_writer.as_deref(), Some("east"));
    }

    #[cfg(not(feature = "coordinator-dynamodb"))]
    #[tokio::test]
    async fn active_active_push_coordinator_fails_closed_without_dynamodb_feature() {
        let mut config = crate::core::config::Config::default();
        config.replication = Some(active_active_replication());
        let mut push_config = PushConfig::from_config(&config);

        let err = configure_active_active_push_coordinator(
            &config,
            Some("crab://primary/org/repo"),
            "org/repo",
            &mut push_config,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("coordinator-dynamodb feature"));
        assert!(push_config.active_active_coordinator.is_none());
        assert_eq!(push_config.active_active_writer.as_deref(), Some("east"));
    }

    /// `PushRejectReason::from_error` must map `PushLockHeld` to
    /// `LockContention` carrying the holder and a sensible TTL. This
    /// is the structured replacement for the old `Error(stringified)`
    /// collapse at the lock-acquire site.
    #[test]
    fn from_error_maps_lock_held_to_lock_contention() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + 30;
        let err = CrabError::PushLockHeld {
            ref_name: "refs/heads/main".to_owned(),
            holder: "pid-1234-1700000000".to_owned(),
            expires_at_unix: Some(future),
        };

        match PushRejectReason::from_error(&err) {
            PushRejectReason::LockContention {
                holder,
                ttl_remaining_secs,
            } => {
                assert_eq!(holder, "pid-1234-1700000000");
                assert!(
                    ttl_remaining_secs > 0 && ttl_remaining_secs <= 30,
                    "ttl must fall inside the lease window; got {ttl_remaining_secs}"
                );
            }
            other => panic!("expected LockContention, got {other:?}"),
        }
    }

    /// `lock_ttl_remaining` clamps to `0` on expired leases and on
    /// `None` payloads. The default must be "retry immediately" rather
    /// than a giant timer that could stall pushers after a crash.
    #[test]
    fn lock_ttl_remaining_clamps_to_zero_when_expired_or_missing() {
        assert_eq!(lock_ttl_remaining(None), 0);
        assert_eq!(lock_ttl_remaining(Some(0)), 0);
        // 1970 — far enough in the past that every reasonable wall
        // clock gets 0 back.
        assert_eq!(lock_ttl_remaining(Some(1)), 0);
    }

    /// `PushRejectReason::LockContention` emits the documented
    /// protocol tag and carries the holder in its Display output.
    /// Scripts key off the tag, so it must stay stable.
    #[test]
    fn lock_contention_protocol_tag_and_display() {
        let reason = PushRejectReason::LockContention {
            holder: "pid-42-1700000000".to_owned(),
            ttl_remaining_secs: 30,
        };
        assert_eq!(reason.protocol_tag(), "lock-contention");
        assert!(
            reason.to_string().contains("pid-42-1700000000"),
            "display must carry the holder identity for diagnostics: {reason}"
        );
        assert!(
            reason.to_string().contains("30s"),
            "display must carry the ttl remaining in seconds: {reason}"
        );
    }

    /// `HookDeclined` must expose its stable protocol tag and embed
    /// both the hook name and the hook-supplied message in its
    /// Display output so clients can distinguish declines from
    /// different hook implementations.
    #[test]
    fn hook_declined_protocol_tag_and_display() {
        let reason = PushRejectReason::HookDeclined {
            name: "branch-protection".to_owned(),
            message: "PR approval required".to_owned(),
        };
        assert_eq!(reason.protocol_tag(), "hook-declined");
        let rendered = reason.to_string();
        assert!(
            rendered.contains("branch-protection"),
            "display must carry the hook name: {rendered}"
        );
        assert!(
            rendered.contains("PR approval required"),
            "display must carry the hook message: {rendered}"
        );
    }

    /// `NotSigned` has no payload; just verify the tag and that
    /// Display mentions push-cert so users know what to provide.
    #[test]
    fn not_signed_protocol_tag_and_display() {
        let reason = PushRejectReason::NotSigned;
        assert_eq!(reason.protocol_tag(), "not-signed");
        assert!(
            reason.to_string().contains("push-cert"),
            "display must reference push-cert for diagnosis: {reason}"
        );
    }

    /// `SignerNotAllowed` carries the offending fingerprint so admins
    /// can diagnose which key was rejected without a retry.
    #[test]
    fn signer_not_allowed_protocol_tag_and_display() {
        let reason = PushRejectReason::SignerNotAllowed {
            fingerprint: "ABCD1234".to_owned(),
        };
        assert_eq!(reason.protocol_tag(), "signer-not-allowed");
        assert!(
            reason.to_string().contains("ABCD1234"),
            "display must carry the fingerprint for diagnosis: {reason}"
        );
    }

    /// `UnknownRefname` carries the offending name so the user can
    /// tell which ref was unresolvable on the remote.
    #[test]
    fn unknown_refname_protocol_tag_and_display() {
        let reason = PushRejectReason::UnknownRefname {
            name: "refs/hidden/internal".to_owned(),
        };
        assert_eq!(reason.protocol_tag(), "unknown-refname");
        assert!(
            reason.to_string().contains("refs/hidden/internal"),
            "display must carry the ref name for diagnosis: {reason}"
        );
    }

    /// `MalformedObject` embeds oid + kind + detail so fsck failures
    /// stay debuggable end-to-end.
    #[test]
    fn malformed_object_protocol_tag_and_display() {
        let reason = PushRejectReason::MalformedObject {
            oid: "deadbeef".to_owned(),
            kind: "tree".to_owned(),
            detail: "entries out of order".to_owned(),
        };
        assert_eq!(reason.protocol_tag(), "malformed-object");
        let rendered = reason.to_string();
        assert!(
            rendered.contains("deadbeef"),
            "display must carry the oid: {rendered}"
        );
        assert!(
            rendered.contains("tree"),
            "display must carry the object kind: {rendered}"
        );
        assert!(
            rendered.contains("entries out of order"),
            "display must carry the failure detail: {rendered}"
        );
    }

    #[test]
    fn shallow_boundary_from_error_protocol_tag_and_display() {
        let err = CrabError::BeyondShallowBoundary {
            oid: "abc123".to_owned(),
        };

        let reason = PushRejectReason::from_error(&err);
        assert_eq!(reason.protocol_tag(), "shallow-boundary");
        assert!(!reason.is_retryable());
        let rendered = reason.to_string();
        assert!(
            rendered.contains("abc123"),
            "display must carry the boundary oid: {rendered}"
        );
        assert!(
            rendered.contains("git fetch --unshallow"),
            "display must carry the remediation: {rendered}"
        );
    }

    #[test]
    fn integration_failed_protocol_tag_and_display() {
        let reason = PushRejectReason::IntegrationFailed {
            command: "git pull --rebase --autostash origin main".to_owned(),
            message: "CONFLICT (content): Merge conflict".to_owned(),
        };

        assert_eq!(reason.protocol_tag(), "integration-failed");
        assert!(!reason.is_retryable());
        let rendered = reason.to_string();
        assert!(
            rendered.contains("git pull --rebase --autostash origin main"),
            "display must carry the integration command: {rendered}"
        );
        assert!(
            rendered.contains("CONFLICT"),
            "display must carry the integration diagnostic: {rendered}"
        );
    }

    /// `PushRejectReason::from_error` must map `CasConflict` to
    /// `StaleInfo` so pusher-side manifest divergence surfaces with
    /// git's `report-status` convention rather than the generic
    /// `Internal` fallback.
    #[test]
    fn from_error_maps_cas_conflict_to_stale_info() {
        let err = CrabError::CasConflict {
            path: "repos/v2/manifest".to_owned(),
            expected_etag: None,
        };
        match PushRejectReason::from_error(&err) {
            PushRejectReason::StaleInfo => {}
            other => panic!("expected StaleInfo, got {other:?}"),
        }
    }

    #[test]
    fn from_error_preserves_retryable_transport_failures() {
        let network = CrabError::NetworkTransient(object_store::Error::Generic {
            store: "test",
            source: Box::new(std::io::Error::other("connection reset")),
        });
        let throttled = CrabError::Throttled {
            retry_after: Some(Duration::from_millis(1_500)),
            source: None,
        };

        let network_reason = PushRejectReason::from_error(&network);
        assert_eq!(network_reason.protocol_tag(), "transient");
        assert!(network_reason.is_retryable());
        assert_eq!(network_reason.retry_after_secs(), None);
        assert!(matches!(
            network_reason,
            PushRejectReason::NetworkTransient(message) if message.contains("connection reset")
        ));

        let throttled_reason = PushRejectReason::from_error(&throttled);
        assert_eq!(throttled_reason.protocol_tag(), "transient");
        assert!(throttled_reason.is_retryable());
        assert_eq!(throttled_reason.retry_after_secs(), Some(2));
        assert!(matches!(
            throttled_reason,
            PushRejectReason::Throttled {
                retry_after_secs: Some(2)
            }
        ));
    }

    #[test]
    fn from_error_preserves_uncertain_ref_journal_commit() {
        let metadata = crab_metadata::error::MetadataError::RefJournalCommitUncertain {
            transaction_id: "a".repeat(64),
            source: Box::new(crab_storage::StorageError::Throttled {
                retry_after: None,
                source: None,
            }),
            verification: None,
        };
        let error = CrabError::Io(std::io::Error::other(metadata));

        let reason = PushRejectReason::from_error(&error);
        assert_eq!(reason.protocol_tag(), "indeterminate");
        assert!(!reason.is_retryable());
    }
}
