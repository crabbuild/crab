//! `crab fsck` — repository integrity checker.
//!
//! Checks Crab manifests, pack/index presence, data-chain metadata, and
//! coordination state. The production object-store checker does not yet run
//! full Git connectivity or enumerate provider-side multipart uploads.

use std::io::Stdout;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::core::error::{Result, check_cancelled};
use crate::core::output::event_payloads::WarningPayload;
use crate::core::output::{JsonlStream, OutputMode};

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

/// CLI arguments for `crab fsck`.
#[derive(Debug, Clone, Default)]
pub struct FsckArgs {
    /// Attempt safe repairs for detected issues.
    pub repair: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
}

// ---------------------------------------------------------------------------
// Issue taxonomy
// ---------------------------------------------------------------------------

/// Severity level for an fsck issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    /// Hard error — data integrity problem.
    Error,
    /// Informational — harmless but notable.
    Info,
}

/// A single inconsistency detected by fsck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckIssue {
    pub kind: IssueKind,
    pub severity: IssueSeverity,
    /// Whether this issue is repairable by `--repair`.
    pub repairable: bool,
}

/// The specific category of inconsistency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueKind {
    /// A ref points to a commit that doesn't exist.
    DanglingRef { ref_name: String, target: String },
    /// A commit references a tree that doesn't exist.
    MissingTree { oid: String, parent_commit: String },
    /// A tree references a blob that doesn't exist.
    MissingBlob { oid: String },
    /// A pointer blob references a file-index entry that doesn't exist.
    MissingFileIndex { file_hash: String },
    /// Git locator acceleration is unavailable, stale, or inconsistent.
    GitLocatorDamage { detail: String },
    /// Git upload-pack visibility proof is unavailable, stale, or inconsistent.
    GitVisibilityDamage { detail: String },
    /// A historical Git visibility proof is missing or needs an idempotent backfill.
    GitVisibilityBackfill {
        generation: u64,
        digest: String,
        detail: String,
    },
    /// A shard references a xorb that doesn't exist.
    MissingXorb { xorb_hash: String },
    /// A shard exists in storage but is not referenced by any xorb chain.
    OrphanShard { shard_key: String },
    /// Pack-list references a key not found in storage.
    PackListDivergence { key: String },
    /// A push lock whose TTL has elapsed.
    ExpiredPushLock { key: String, age: Duration },
    /// A multipart upload older than the grace period.
    AbandonedMultipart {
        upload_id: String,
        key: String,
        age: Duration,
    },
    /// PersistentChunkIndex and shard-list disagree.
    ShardListDivergence { key: String },
    /// A file-index entry exists but no pointer references it.
    /// Informational only — file-index entries are immutable, tiny, and
    /// content-addressed, so orphans are harmless.
    OrphanFileIndex { key: String },
}

impl FsckIssue {
    pub(crate) fn dangling_ref(ref_name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::DanglingRef {
                ref_name: ref_name.into(),
                target: target.into(),
            },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "kept for fsck issue rendering and severity tests")
    )]
    pub(crate) fn missing_tree(oid: impl Into<String>, parent_commit: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::MissingTree {
                oid: oid.into(),
                parent_commit: parent_commit.into(),
            },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "kept for fsck issue rendering and severity tests")
    )]
    pub(crate) fn missing_blob(oid: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::MissingBlob { oid: oid.into() },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    pub(crate) fn missing_file_index(file_hash: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::MissingFileIndex {
                file_hash: file_hash.into(),
            },
            severity: IssueSeverity::Error,
            repairable: true,
        }
    }

    pub(crate) fn git_locator_damage(detail: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::GitLocatorDamage {
                detail: detail.into(),
            },
            severity: IssueSeverity::Info,
            repairable: false,
        }
    }

    pub(crate) fn git_visibility_damage(detail: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::GitVisibilityDamage {
                detail: detail.into(),
            },
            severity: IssueSeverity::Info,
            repairable: false,
        }
    }

    pub(crate) fn git_visibility_backfill(
        generation: u64,
        digest: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind: IssueKind::GitVisibilityBackfill {
                generation,
                digest: digest.into(),
                detail: detail.into(),
            },
            severity: IssueSeverity::Info,
            repairable: true,
        }
    }

    pub(crate) fn missing_xorb(xorb_hash: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::MissingXorb {
                xorb_hash: xorb_hash.into(),
            },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    pub(crate) fn orphan_shard(shard_key: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::OrphanShard {
                shard_key: shard_key.into(),
            },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    pub(crate) fn pack_list_divergence(key: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::PackListDivergence { key: key.into() },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    pub(crate) fn expired_push_lock(key: impl Into<String>, age: Duration) -> Self {
        Self {
            kind: IssueKind::ExpiredPushLock {
                key: key.into(),
                age,
            },
            severity: IssueSeverity::Error,
            repairable: true,
        }
    }

    pub(crate) fn abandoned_multipart(
        upload_id: impl Into<String>,
        key: impl Into<String>,
        age: Duration,
    ) -> Self {
        Self {
            kind: IssueKind::AbandonedMultipart {
                upload_id: upload_id.into(),
                key: key.into(),
                age,
            },
            severity: IssueSeverity::Error,
            repairable: true,
        }
    }

    pub(crate) fn shard_list_divergence(key: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::ShardListDivergence { key: key.into() },
            severity: IssueSeverity::Error,
            repairable: false,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "kept for fsck issue rendering and severity tests")
    )]
    pub(crate) fn orphan_file_index(key: impl Into<String>) -> Self {
        Self {
            kind: IssueKind::OrphanFileIndex { key: key.into() },
            severity: IssueSeverity::Info,
            repairable: false,
        }
    }
}

impl std::fmt::Display for FsckIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = match self.severity {
            IssueSeverity::Error => "ERROR",
            IssueSeverity::Info => "INFO",
        };
        match &self.kind {
            IssueKind::DanglingRef { ref_name, target } => {
                write!(f, "{prefix}: dangling ref {ref_name} -> {target}")
            }
            IssueKind::MissingTree { oid, parent_commit } => {
                write!(
                    f,
                    "{prefix}: missing tree {oid} (parent commit {parent_commit})"
                )
            }
            IssueKind::MissingBlob { oid } => {
                write!(f, "{prefix}: missing blob {oid}")
            }
            IssueKind::MissingFileIndex { file_hash } => {
                write!(f, "{prefix}: missing file-index entry {file_hash}")
            }
            IssueKind::GitLocatorDamage { detail } => {
                write!(f, "{prefix}: Git locator acceleration damage: {detail}")
            }
            IssueKind::GitVisibilityDamage { detail } => {
                write!(f, "{prefix}: Git visibility proof damage: {detail}")
            }
            IssueKind::GitVisibilityBackfill {
                generation,
                digest,
                detail,
            } => {
                write!(
                    f,
                    "{prefix}: historical Git visibility proof {generation}/{digest} requires backfill: {detail}"
                )
            }
            IssueKind::MissingXorb { xorb_hash } => {
                write!(f, "{prefix}: missing xorb {xorb_hash}")
            }
            IssueKind::OrphanShard { shard_key } => {
                write!(f, "{prefix}: orphan shard {shard_key}")
            }
            IssueKind::PackListDivergence { key } => {
                write!(f, "{prefix}: pack-list object missing from storage: {key}")
            }
            IssueKind::ExpiredPushLock { key, age } => {
                write!(
                    f,
                    "{prefix}: expired push lock {key} (age: {}s)",
                    age.as_secs()
                )
            }
            IssueKind::AbandonedMultipart {
                upload_id,
                key,
                age,
            } => {
                write!(
                    f,
                    "{prefix}: abandoned multipart upload {upload_id} for {key} (age: {}s)",
                    age.as_secs()
                )
            }
            IssueKind::ShardListDivergence { key } => {
                write!(f, "{prefix}: shard-list divergence {key}")
            }
            IssueKind::OrphanFileIndex { key } => {
                write!(f, "{prefix}: orphan file-index entry {key}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fsck outcome
// ---------------------------------------------------------------------------

/// Structured outcome of an fsck run.
#[derive(Debug, Clone, Default)]
pub struct FsckOutcome {
    /// Total errors detected.
    pub errors: u64,
    /// Total informational issues detected.
    pub info_count: u64,
    /// Number of issues repaired (when `--repair` is used).
    pub repaired: u64,
    /// Number of repair attempts that failed.
    pub repair_failures: u64,
}

impl FsckOutcome {
    fn log(&self, repair_mode: bool) {
        if repair_mode {
            info!(
                errors = self.errors,
                info = self.info_count,
                repaired = self.repaired,
                repair_failures = self.repair_failures,
                "fsck complete (repair mode)"
            );
        } else {
            info!(
                errors = self.errors,
                info = self.info_count,
                "fsck complete"
            );
        }
    }

    /// Convert to the structured output summary payload.
    pub fn to_summary(&self) -> FsckSummary {
        FsckSummary {
            errors: self.errors,
            info_count: self.info_count,
            repaired: self.repaired,
            repair_failures: self.repair_failures,
            passed: self.errors == 0 || self.errors <= self.repaired,
        }
    }
}

/// Terminal result payload for `--json` / `--jsonl` structured output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FsckSummary {
    /// Total errors detected.
    pub errors: u64,
    /// Total informational issues detected.
    pub info_count: u64,
    /// Number of issues repaired (when `--repair` is used).
    pub repaired: u64,
    /// Number of repair attempts that failed.
    pub repair_failures: u64,
    /// `true` when the repository passed all checks (no unrepaired errors).
    pub passed: bool,
}

// ---------------------------------------------------------------------------
// Storage checker trait — abstraction for testability
// ---------------------------------------------------------------------------

/// Metadata for a push lock discovered during fsck.
#[derive(Debug, Clone)]
pub struct PushLockMeta {
    /// Storage key of the lock.
    pub key: String,
    /// When the lock was created.
    pub created: SystemTime,
    /// Lock TTL.
    pub ttl: Duration,
}

/// Metadata for a multipart upload discovered during fsck.
#[derive(Debug, Clone)]
pub struct MultipartMeta {
    /// Upload ID from the object store.
    pub upload_id: String,
    /// Target key for the upload.
    pub key: String,
    /// When the upload was initiated.
    pub initiated: SystemTime,
}

/// Trait abstracting the storage queries needed by fsck.
///
/// In production, this wraps the real `Store` and metadata stores.
/// In tests, a mock returns canned results for each check category.
pub trait FsckChecker: Send + Sync {
    /// Check git-object-level connectivity (dangling refs, missing trees/blobs).
    /// Returns issues found at the git object layer.
    fn check_git_objects(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>;

    /// Check pointer → file-index → shard → xorb chain integrity.
    /// Returns issues found in the crab data chain.
    fn check_data_chain(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>;

    /// Check that every manifest-selected pack and index exists in storage.
    fn check_pack_list(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>;

    /// List expired push locks.
    fn check_push_locks(
        &self,
        now: SystemTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<PushLockMeta>>> + Send + '_>>;

    /// List abandoned multipart uploads older than the grace period.
    fn check_multipart_uploads(
        &self,
        now: SystemTime,
        grace: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<MultipartMeta>>> + Send + '_>>;

    /// Check PersistentChunkIndex vs shard-list consistency.
    fn check_shard_list_divergence(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>;

    /// Find orphan file-index entries (file-index entries not referenced by any pointer).
    fn check_orphan_file_index(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>;
}

// ---------------------------------------------------------------------------
// Repair trait — abstraction for safe reversible operations
// ---------------------------------------------------------------------------

/// Trait abstracting the repair operations for `--repair`.
///
/// Each method performs a single safe, reversible repair and returns
/// `Ok(true)` on success, `Ok(false)` if the repair was skipped, or
/// `Err` on failure.
pub trait FsckRepairer: Send + Sync {
    /// Validate whether a reported file-index entry has become healthy.
    fn repair_file_index_entry(
        &self,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>;

    /// Rebuild one historical Git visibility proof from its verified packs.
    fn repair_git_visibility_history(
        &self,
        generation: u64,
        digest: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>;

    /// Repair an expired push lock.
    fn repair_push_lock(
        &self,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>;

    /// Abort an abandoned multipart upload.
    fn abort_multipart(
        &self,
        upload_id: &str,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>;
}

/// A no-op repairer for non-repair mode and tests.
pub struct NullRepairer;

impl FsckRepairer for NullRepairer {
    fn repair_file_index_entry(
        &self,
        _key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }

    fn repair_git_visibility_history(
        &self,
        _generation: u64,
        _digest: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }

    fn repair_push_lock(
        &self,
        _key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }

    fn abort_multipart(
        &self,
        _upload_id: &str,
        _key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }
}

// ---------------------------------------------------------------------------
// Main fsck entry point
// ---------------------------------------------------------------------------

/// Run repository integrity checks.
///
/// Orchestrates all detection categories, optionally repairs safe issues,
/// and returns a structured outcome.
///
/// # Errors
///
/// Returns [`CrabError::Cancelled`] on SIGINT, or propagates storage errors.
pub async fn run_fsck(
    args: &FsckArgs,
    checker: &dyn FsckChecker,
    repairer: &dyn FsckRepairer,
    cancel: &tokio_util::sync::CancellationToken,
    grace_period: Duration,
    jsonl_stream: Option<&std::sync::Mutex<JsonlStream<Stdout>>>,
) -> Result<(Vec<FsckIssue>, FsckOutcome)> {
    let now = SystemTime::now();
    let mut all_issues = Vec::new();
    let mut outcome = FsckOutcome::default();

    // Phase 1: Git-object-level connectivity checks.
    check_cancelled(cancel)?;
    debug!("checking git object connectivity");
    match checker.check_git_objects().await {
        Ok(issues) => all_issues.extend(issues),
        Err(e) => warn!(error = %e, "git object check failed"),
    }

    // Phase 2: Crab data chain (pointer → file-index → shard → xorb).
    check_cancelled(cancel)?;
    debug!("checking crab data chain");
    match checker.check_data_chain().await {
        Ok(issues) => all_issues.extend(issues),
        Err(e) => warn!(error = %e, "data chain check failed"),
    }

    // Phase 3: Pack-list vs storage divergence.
    check_cancelled(cancel)?;
    debug!("checking pack-list consistency");
    match checker.check_pack_list().await {
        Ok(issues) => all_issues.extend(issues),
        Err(e) => warn!(error = %e, "pack-list check failed"),
    }

    // Phase 4: Expired push locks.
    check_cancelled(cancel)?;
    debug!("checking push locks");
    match checker.check_push_locks(now).await {
        Ok(locks) => {
            for lock in locks {
                let age = now.duration_since(lock.created).unwrap_or(Duration::ZERO);
                all_issues.push(FsckIssue::expired_push_lock(&lock.key, age));
            }
        }
        Err(e) => warn!(error = %e, "push lock check failed"),
    }

    // Phase 5: Abandoned multipart uploads.
    check_cancelled(cancel)?;
    debug!("checking multipart uploads");
    match checker.check_multipart_uploads(now, grace_period).await {
        Ok(uploads) => {
            for upload in uploads {
                let age = now
                    .duration_since(upload.initiated)
                    .unwrap_or(Duration::ZERO);
                all_issues.push(FsckIssue::abandoned_multipart(
                    &upload.upload_id,
                    &upload.key,
                    age,
                ));
            }
        }
        Err(e) => warn!(error = %e, "multipart upload check failed"),
    }

    // Phase 6: PersistentChunkIndex / shard-list divergence.
    check_cancelled(cancel)?;
    debug!("checking shard-list divergence");
    match checker.check_shard_list_divergence().await {
        Ok(issues) => all_issues.extend(issues),
        Err(e) => warn!(error = %e, "shard-list divergence check failed"),
    }

    // Phase 7: Orphan file-index entries (informational).
    check_cancelled(cancel)?;
    debug!("checking orphan file-index entries");
    match checker.check_orphan_file_index().await {
        Ok(issues) => all_issues.extend(issues),
        Err(e) => warn!(error = %e, "orphan file-index check failed"),
    }

    // Tally issues by severity.
    for issue in &all_issues {
        match issue.severity {
            IssueSeverity::Error => outcome.errors += 1,
            IssueSeverity::Info => outcome.info_count += 1,
        }
    }

    // Report all issues.
    for issue in &all_issues {
        match issue.severity {
            IssueSeverity::Error => warn!(issue = %issue, "fsck issue"),
            IssueSeverity::Info => info!(issue = %issue, "fsck info"),
        }

        // Emit a warning event per issue in JSONL mode.
        if let Some(stream) = jsonl_stream {
            if let Ok(mut s) = stream.lock() {
                s.emit_warning(WarningPayload {
                    code: issue_code(&issue.kind),
                    message: issue.to_string(),
                    path: issue_path(&issue.kind),
                });
            }
        }
    }

    // Phase 8: Repair (if requested).
    if args.repair {
        check_cancelled(cancel)?;
        repair_issues(&all_issues, repairer, &mut outcome).await;
    }

    outcome.log(args.repair);
    Ok((all_issues, outcome))
}

/// Map an issue kind to a short code for the warning event.
fn issue_code(kind: &IssueKind) -> String {
    match kind {
        IssueKind::DanglingRef { .. } => "fsck-dangling-ref",
        IssueKind::MissingTree { .. } => "fsck-missing-tree",
        IssueKind::MissingBlob { .. } => "fsck-missing-blob",
        IssueKind::MissingFileIndex { .. } => "fsck-missing-file-index",
        IssueKind::GitLocatorDamage { .. } => "fsck-git-locator-damage",
        IssueKind::GitVisibilityDamage { .. } => "fsck-git-visibility-damage",
        IssueKind::GitVisibilityBackfill { .. } => "fsck-git-visibility-backfill",
        IssueKind::MissingXorb { .. } => "fsck-missing-xorb",
        IssueKind::OrphanShard { .. } => "fsck-orphan-shard",
        IssueKind::PackListDivergence { .. } => "fsck-pack-list-divergence",
        IssueKind::ExpiredPushLock { .. } => "fsck-expired-push-lock",
        IssueKind::AbandonedMultipart { .. } => "fsck-abandoned-multipart",
        IssueKind::ShardListDivergence { .. } => "fsck-shard-list-divergence",
        IssueKind::OrphanFileIndex { .. } => "fsck-orphan-file-index",
    }
    .to_owned()
}

/// Extract a path-like identifier from an issue kind, if applicable.
fn issue_path(kind: &IssueKind) -> Option<String> {
    match kind {
        IssueKind::DanglingRef { ref_name, .. } => Some(ref_name.clone()),
        IssueKind::MissingXorb { xorb_hash } => Some(xorb_hash.clone()),
        IssueKind::OrphanShard { shard_key } => Some(shard_key.clone()),
        IssueKind::PackListDivergence { key, .. }
        | IssueKind::ExpiredPushLock { key, .. }
        | IssueKind::AbandonedMultipart { key, .. }
        | IssueKind::ShardListDivergence { key }
        | IssueKind::OrphanFileIndex { key } => Some(key.clone()),
        IssueKind::GitVisibilityBackfill {
            generation, digest, ..
        } => Some(format!("{generation}/{digest}")),
        _ => None,
    }
}

/// Attempt repairs for all repairable issues.
async fn repair_issues(
    issues: &[FsckIssue],
    repairer: &dyn FsckRepairer,
    outcome: &mut FsckOutcome,
) {
    for issue in issues {
        if !issue.repairable {
            continue;
        }

        let result = match &issue.kind {
            IssueKind::MissingFileIndex { file_hash } => {
                info!(file_hash = %file_hash, "repairing: re-adding file-index manifest entry");
                repairer.repair_file_index_entry(file_hash).await
            }
            IssueKind::ExpiredPushLock { key, age } => {
                info!(key = %key, age_secs = age.as_secs(), "repairing: marking expired push lock released");
                repairer.repair_push_lock(key).await
            }
            IssueKind::AbandonedMultipart {
                upload_id,
                key,
                age,
            } => {
                info!(
                    upload_id = %upload_id,
                    key = %key,
                    age_secs = age.as_secs(),
                    "repairing: aborting abandoned multipart upload"
                );
                repairer.abort_multipart(upload_id, key).await
            }
            IssueKind::GitVisibilityBackfill {
                generation, digest, ..
            } => {
                info!(
                    generation,
                    digest = %digest,
                    "repairing: backfilling historical Git visibility proof"
                );
                repairer
                    .repair_git_visibility_history(*generation, digest)
                    .await
            }
            _ => continue,
        };

        match result {
            Ok(true) => {
                outcome.repaired += 1;
                debug!(issue = %issue, "repair succeeded");
            }
            Ok(false) => {
                debug!(issue = %issue, "repair skipped");
            }
            Err(e) => {
                outcome.repair_failures += 1;
                warn!(issue = %issue, error = %e, "repair failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::CrabError;
    use tokio_util::sync::CancellationToken;

    // --- Mock checker that returns configurable issues ---

    #[derive(Default)]
    struct MockChecker {
        git_issues: Vec<FsckIssue>,
        data_chain_issues: Vec<FsckIssue>,
        pack_list_issues: Vec<FsckIssue>,
        push_locks: Vec<PushLockMeta>,
        multipart_uploads: Vec<MultipartMeta>,
        shard_divergence_issues: Vec<FsckIssue>,
        orphan_file_index_issues: Vec<FsckIssue>,
    }

    impl FsckChecker for MockChecker {
        fn check_git_objects(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
        {
            let issues = self.git_issues.clone();
            Box::pin(async move { Ok(issues) })
        }

        fn check_data_chain(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
        {
            let issues = self.data_chain_issues.clone();
            Box::pin(async move { Ok(issues) })
        }

        fn check_pack_list(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
        {
            let issues = self.pack_list_issues.clone();
            Box::pin(async move { Ok(issues) })
        }

        fn check_push_locks(
            &self,
            _now: SystemTime,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<PushLockMeta>>> + Send + '_>,
        > {
            let locks = self.push_locks.clone();
            Box::pin(async move { Ok(locks) })
        }

        fn check_multipart_uploads(
            &self,
            _now: SystemTime,
            _grace: Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<MultipartMeta>>> + Send + '_>,
        > {
            let uploads = self.multipart_uploads.clone();
            Box::pin(async move { Ok(uploads) })
        }

        fn check_shard_list_divergence(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
        {
            let issues = self.shard_divergence_issues.clone();
            Box::pin(async move { Ok(issues) })
        }

        fn check_orphan_file_index(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
        {
            let issues = self.orphan_file_index_issues.clone();
            Box::pin(async move { Ok(issues) })
        }
    }

    // --- Mock repairer that tracks calls ---

    struct MockRepairer {
        repaired_file_indexes: std::sync::Mutex<Vec<String>>,
        repaired_locks: std::sync::Mutex<Vec<String>>,
        aborted_multiparts: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl MockRepairer {
        fn new() -> Self {
            Self {
                repaired_file_indexes: std::sync::Mutex::new(Vec::new()),
                repaired_locks: std::sync::Mutex::new(Vec::new()),
                aborted_multiparts: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl FsckRepairer for MockRepairer {
        fn repair_file_index_entry(
            &self,
            key: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>
        {
            if let Ok(mut v) = self.repaired_file_indexes.lock() {
                v.push(key.to_string());
            }
            Box::pin(async { Ok(true) })
        }

        fn repair_git_visibility_history(
            &self,
            _generation: u64,
            _digest: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>
        {
            Box::pin(async { Ok(true) })
        }

        fn repair_push_lock(
            &self,
            key: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>
        {
            if let Ok(mut v) = self.repaired_locks.lock() {
                v.push(key.to_string());
            }
            Box::pin(async { Ok(true) })
        }

        fn abort_multipart(
            &self,
            upload_id: &str,
            key: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>>
        {
            if let Ok(mut v) = self.aborted_multiparts.lock() {
                v.push((upload_id.to_string(), key.to_string()));
            }
            Box::pin(async { Ok(true) })
        }
    }

    // --- Tests ---

    #[tokio::test]
    async fn fsck_clean_repo_reports_no_issues() {
        let checker = MockChecker::default();
        let repairer = NullRepairer;
        let cancel = CancellationToken::new();
        let args = FsckArgs::default();

        let (issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        assert!(issues.is_empty());
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.info_count, 0);
    }

    #[tokio::test]
    async fn fsck_detects_all_issue_categories() {
        let now = SystemTime::now();
        let checker = MockChecker {
            git_issues: vec![
                FsckIssue::dangling_ref("refs/heads/main", "deadbeef"),
                FsckIssue::missing_tree("aaa111", "bbb222"),
                FsckIssue::missing_blob("ccc333"),
            ],
            data_chain_issues: vec![
                FsckIssue::missing_file_index("fff444"),
                FsckIssue::missing_xorb("xxx555"),
                FsckIssue::orphan_shard("shards/ab/orphan1"),
            ],
            pack_list_issues: vec![FsckIssue::pack_list_divergence("packs/cd/pack1")],
            push_locks: vec![PushLockMeta {
                key: "locks/push-abc".to_string(),
                created: now - Duration::from_secs(7200),
                ttl: Duration::from_secs(3600),
            }],
            multipart_uploads: vec![MultipartMeta {
                upload_id: "mp-123".to_string(),
                key: "xorbs/ab/partial".to_string(),
                initiated: now - Duration::from_secs(86400),
            }],
            shard_divergence_issues: vec![FsckIssue::shard_list_divergence("shards/ef/divergent")],
            orphan_file_index_issues: vec![FsckIssue::orphan_file_index("file-index/gh/orphan1")],
        };
        let repairer = NullRepairer;
        let cancel = CancellationToken::new();
        let args = FsckArgs::default();

        let (issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        // 3 git + 3 data chain + 1 pack-list + 1 push lock + 1 multipart
        // + 1 shard divergence = 10 errors, 1 orphan file-index = 1 info
        assert_eq!(issues.len(), 11);
        assert_eq!(outcome.errors, 10);
        assert_eq!(outcome.info_count, 1);
    }

    #[tokio::test]
    async fn fsck_orphan_file_index_is_informational() {
        let checker = MockChecker {
            orphan_file_index_issues: vec![FsckIssue::orphan_file_index("file-index/ab/orphan")],
            ..MockChecker::default()
        };
        let repairer = NullRepairer;
        let cancel = CancellationToken::new();
        let args = FsckArgs::default();

        let (issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, IssueSeverity::Info);
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.info_count, 1);
    }

    #[tokio::test]
    async fn fsck_repair_marks_expired_locks_released() {
        let now = SystemTime::now();
        let checker = MockChecker {
            push_locks: vec![PushLockMeta {
                key: "locks/push-expired".to_string(),
                created: now - Duration::from_secs(7200),
                ttl: Duration::from_secs(3600),
            }],
            ..MockChecker::default()
        };
        let repairer = MockRepairer::new();
        let cancel = CancellationToken::new();
        let args = FsckArgs {
            repair: true,
            ..FsckArgs::default()
        };

        let (_issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.repaired, 1);
        let repaired = repairer.repaired_locks.lock().expect("lock");
        assert_eq!(repaired.len(), 1);
        assert_eq!(repaired[0], "locks/push-expired");
    }

    #[tokio::test]
    async fn fsck_repair_aborts_abandoned_multiparts() {
        let now = SystemTime::now();
        let checker = MockChecker {
            multipart_uploads: vec![MultipartMeta {
                upload_id: "mp-old".to_string(),
                key: "xorbs/ab/stale".to_string(),
                initiated: now - Duration::from_secs(172800),
            }],
            ..MockChecker::default()
        };
        let repairer = MockRepairer::new();
        let cancel = CancellationToken::new();
        let args = FsckArgs {
            repair: true,
            ..FsckArgs::default()
        };

        let (_issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.repaired, 1);
        let aborted = repairer.aborted_multiparts.lock().expect("lock");
        assert_eq!(aborted.len(), 1);
        assert_eq!(
            aborted[0],
            ("mp-old".to_string(), "xorbs/ab/stale".to_string())
        );
    }

    #[tokio::test]
    async fn fsck_repair_checks_file_index_entry() {
        let checker = MockChecker {
            data_chain_issues: vec![FsckIssue::missing_file_index("file-hash")],
            ..MockChecker::default()
        };
        let repairer = MockRepairer::new();
        let cancel = CancellationToken::new();
        let args = FsckArgs {
            repair: true,
            ..FsckArgs::default()
        };

        let (_issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.repaired, 1);
        let entries = repairer.repaired_file_indexes.lock().expect("lock");
        assert_eq!(entries.as_slice(), ["file-hash"]);
    }

    #[tokio::test]
    async fn fsck_no_repair_without_flag() {
        let now = SystemTime::now();
        let checker = MockChecker {
            push_locks: vec![PushLockMeta {
                key: "locks/push-expired".to_string(),
                created: now - Duration::from_secs(7200),
                ttl: Duration::from_secs(3600),
            }],
            ..MockChecker::default()
        };
        let repairer = MockRepairer::new();
        let cancel = CancellationToken::new();
        let args = FsckArgs {
            repair: false,
            ..FsckArgs::default()
        };

        let (_issues, outcome) = run_fsck(
            &args,
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.repaired, 0);
        let repaired = repairer.repaired_locks.lock().expect("lock");
        assert!(repaired.is_empty());
    }

    #[tokio::test]
    async fn fsck_respects_cancellation() {
        let checker = MockChecker::default();
        let repairer = NullRepairer;
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_fsck(
            &FsckArgs::default(),
            &checker,
            &repairer,
            &cancel,
            Duration::from_secs(3600),
            None,
        )
        .await;

        assert!(matches!(result, Err(CrabError::Cancelled)));
    }

    // --- Display ---

    #[test]
    fn issue_display_formats_correctly() {
        let issue = FsckIssue::dangling_ref("refs/heads/main", "deadbeef");
        let s = issue.to_string();
        assert!(s.contains("ERROR"));
        assert!(s.contains("dangling ref"));
        assert!(s.contains("refs/heads/main"));

        let info = FsckIssue::orphan_file_index("file-index/ab/orphan");
        let s = info.to_string();
        assert!(s.contains("INFO"));
        assert!(s.contains("orphan file-index"));
    }

    #[test]
    fn issue_constructors_set_correct_severity() {
        assert_eq!(
            FsckIssue::dangling_ref("r", "t").severity,
            IssueSeverity::Error
        );
        assert_eq!(
            FsckIssue::missing_tree("o", "p").severity,
            IssueSeverity::Error
        );
        assert_eq!(FsckIssue::missing_blob("o").severity, IssueSeverity::Error);
        assert_eq!(
            FsckIssue::missing_file_index("h").severity,
            IssueSeverity::Error
        );
        assert_eq!(FsckIssue::missing_xorb("h").severity, IssueSeverity::Error);
        assert_eq!(FsckIssue::orphan_shard("k").severity, IssueSeverity::Error);
        assert_eq!(
            FsckIssue::pack_list_divergence("k").severity,
            IssueSeverity::Error
        );
        assert_eq!(
            FsckIssue::expired_push_lock("k", Duration::from_secs(1)).severity,
            IssueSeverity::Error
        );
        assert_eq!(
            FsckIssue::abandoned_multipart("u", "k", Duration::from_secs(1)).severity,
            IssueSeverity::Error
        );
        assert_eq!(
            FsckIssue::shard_list_divergence("k").severity,
            IssueSeverity::Error
        );
        assert_eq!(
            FsckIssue::orphan_file_index("k").severity,
            IssueSeverity::Info
        );
    }

    #[test]
    fn issue_constructors_set_correct_repairability() {
        assert!(!FsckIssue::dangling_ref("r", "t").repairable);
        assert!(!FsckIssue::missing_tree("o", "p").repairable);
        assert!(!FsckIssue::missing_blob("o").repairable);
        assert!(FsckIssue::missing_file_index("h").repairable);
        assert!(!FsckIssue::missing_xorb("h").repairable);
        assert!(!FsckIssue::orphan_shard("k").repairable);
        assert!(!FsckIssue::pack_list_divergence("k").repairable);
        assert!(FsckIssue::expired_push_lock("k", Duration::from_secs(1)).repairable);
        assert!(FsckIssue::abandoned_multipart("u", "k", Duration::from_secs(1)).repairable);
        assert!(!FsckIssue::shard_list_divergence("k").repairable);
        assert!(!FsckIssue::orphan_file_index("k").repairable);
    }
}
