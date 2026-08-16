//! Publish stage for `crab import`.
//!
//! Once [`run_assemble`](crate::import::assemble::run_assemble) has landed
//! the commit history locally, publish pushes it to the target bucket.
//! The wrapper is deliberately thin: it wires a [`PushSpec`] for the
//! HEAD commit into [`run_push_batch`] using [`PushConfig::default`],
//! then translates the per-ref outcomes into a structured
//! [`PublishStats`].
//!
//! # What this stage does
//!
//! 1. Build one [`PushSpec`] targeting `refs/heads/<branch>` at
//!    `head_commit_oid`. The native-push graph walk picks up all
//!    ancestors automatically, so we never enumerate intermediate
//!    commits.
//! 2. Construct [`StoreLayout::new(target_store, repo_prefix)`] to
//!    route xorbs / shards / file-index to the shared `.crab/`
//!    prefix and refs / manifests to `<repo_prefix>/`.
//! 3. Drive [`run_push_batch`] — fresh imports have nothing to sync
//!    from the remote before the push, and the pre-push shard-sync
//!    step has been removed from the push pipeline.
//! 4. Snapshot [`Metrics`] before and after the push so
//!    [`PublishStats::bytes_uploaded`] reflects bytes produced by
//!    *this* publish, not a lifetime counter.
//! 5. Fold the per-ref outcomes into [`PublishStats`]. Any non-ok
//!    outcome surfaces as a [`CrabError::Internal`] with the
//!    combined error messages so callers can treat publish failure
//!    the same as any other stage failure.
//!
//! # What this stage does *not* do
//!
//! - Resolve credentials or build the target [`Store`]. That belongs
//!   to the import coordinator (task 14) — publish takes an already-
//!   resolved target.
//! - Re-run the commit walk or enumerate pointers. The push pipeline
//!   does that internally off [`PushPipeline::step_1_pointer_enumeration`].
//! - Touch the source bucket. Even on same-bucket imports the source
//!   objects sit untouched; the push pipeline only writes xorbs,
//!   shards, and refs to the `StoreLayout`-rooted prefix.
//!
//! `caching_store` and `progress` are intentionally not wired yet —
//! both are V1 nice-to-haves that the import command doesn't need
//! to ship first cut.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::metrics::Metrics;
use crate::git::push::{PushConfig, RefPushOutcome, run_push_batch};
use crate::git::remote_helper::PushSpec;
use crate::import::ingest::ResolvedStore;
use crate::storage::StoreLayout;
use crab_staging::StagingAreaReadOnly;

/// Inputs for [`run_publish`].
///
/// Grouped into a struct both to keep the entry-point signature
/// readable and because later plumbing (structured output, progress)
/// will add fields without rippling through every call site.
pub struct PublishInputs {
    /// Target-side [`ResolvedStore`] — where xorbs, shards, and refs
    /// land. Built by the coordinator from the user's `--to` URL.
    pub target: ResolvedStore,
    /// Per-repo object prefix (e.g. `"repos/v2"`). Passed verbatim to
    /// [`StoreLayout::new`] so refs and manifests route under
    /// `<repo_prefix>/…` while content-addressed objects stay global.
    pub repo_prefix: String,
    /// Open read-only view of the staging area populated by ingest.
    /// The push pipeline reads chunk bytes here; mutating it during
    /// publish is a bug.
    pub staging: Arc<StagingAreaReadOnly>,
    /// Branch name the import is landing on (e.g. `"main"`).
    pub branch: String,
    /// Commit OID of the final commit produced by assemble. The push
    /// pipeline walks from here through all ancestors on its own.
    pub head_commit_oid: String,
    /// Absolute path to the git directory we're publishing (`<into>/.git`).
    /// The push pipeline honors `GIT_DIR`, so the coordinator is
    /// responsible for threading this value through the env before
    /// calling [`run_publish`].
    pub git_dir: PathBuf,
    /// Optional [`Metrics`] handle. When present, publish captures a
    /// before/after snapshot so [`PublishStats::bytes_uploaded`]
    /// reflects this publish only.
    pub metrics: Option<Arc<Metrics>>,
    /// Cancellation token. Honored before the push kicks off and
    /// again after it returns.
    pub cancel: CancellationToken,
}

/// Counters the publish stage folds into the final `ImportSummary`.
///
/// `bytes_uploaded` is sourced from the [`Metrics`] snapshot delta
/// across the push. `xorbs_uploaded` and `shards_uploaded` are
/// reserved for future wiring — the push pipeline does not yet expose
/// dedicated counters for those totals, so V1 reports `0`. Callers
/// that render these fields should prefer `bytes_uploaded` for actual
/// throughput reporting today.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PublishStats {
    /// Count of `ok` ref outcomes returned by [`run_push_batch`].
    pub refs_pushed: u64,
    /// Count of `error` ref outcomes returned by [`run_push_batch`].
    /// Always zero on the happy path; non-zero values accompany a
    /// [`CrabError::Internal`] carrying the combined messages.
    pub refs_failed: u64,
    /// Bytes uploaded to the target bucket during this publish,
    /// measured as the delta between a `before` and `after`
    /// [`Metrics::snapshot`]. `0` when no metrics handle is provided.
    pub bytes_uploaded: u64,
    /// Xorbs uploaded this publish. Currently always `0` — see the
    /// type-level note on V1 scope.
    pub xorbs_uploaded: u64,
    /// Shards uploaded this publish. Currently always `0` — see the
    /// type-level note on V1 scope.
    pub shards_uploaded: u64,
    /// Commit OID of the HEAD this publish pushed; mirrors
    /// [`PublishInputs::head_commit_oid`].
    pub head_commit_oid: String,
    /// Branch the refs were written under.
    pub branch: String,
}

/// Publish the assembled commit history to the target bucket.
///
/// Wraps [`run_push_batch`] with a single [`PushSpec`] for
/// `refs/heads/<branch>` pointing at `head_commit_oid`. The push
/// pipeline handles the commit walk, pointer enumeration, xorb
/// packing, shard CAS, and ref CAS on its own.
///
/// # Errors
///
/// - [`CrabError::Cancelled`] if the cancellation token trips
///   before or after the push.
/// - [`CrabError::Internal`] carrying the combined per-ref error
///   messages when any ref outcome is not `Ok`. The stage treats
///   this as a hard failure — the import command should not proceed
///   past publish without every planned ref landing.
pub async fn run_publish(inputs: PublishInputs) -> Result<PublishStats> {
    let PublishInputs {
        target,
        repo_prefix,
        staging,
        branch,
        head_commit_oid,
        git_dir,
        metrics,
        cancel,
    } = inputs;

    check_cancelled(&cancel)?;

    info!(
        branch = %branch,
        head = %head_commit_oid,
        repo_prefix = %repo_prefix,
        git_dir = %git_dir.display(),
        "publish: starting"
    );

    let ref_name = format!("refs/heads/{branch}");
    let spec = PushSpec {
        force: false,
        // The push pipeline uses `src` for local rev resolution, so
        // pointing it at the destination ref matches what the normal
        // `crab push` path does when pushing a branch without an
        // explicit refspec.
        src: ref_name.clone(),
        dst: ref_name.clone(),
    };

    let router = StoreLayout::new(target.store.clone(), repo_prefix.clone());

    // Capture a metrics baseline so `bytes_uploaded` reflects this
    // publish, not a lifetime total. Default push config mirrors what
    // `crab push` uses on the non-native fallback path.
    let bytes_before = metrics.as_deref().map(|m| m.snapshot().bytes_uploaded);

    debug!(
        ref_name = %ref_name,
        "publish: invoking run_push_batch"
    );

    let push_config = PushConfig {
        git_dir: Some(git_dir.clone()),
        ..PushConfig::default()
    };
    let result = run_push_batch(
        &[spec],
        &push_config,
        Some(target.store.clone()),
        None, // caching_store: V1 — no cache wiring for fresh imports
        Some(Arc::clone(&staging)),
        router,
        metrics.clone(),
        cancel.clone(),
        None, // progress: V1 — no native progress hookup
    )
    .await;

    check_cancelled(&cancel)?;

    let (refs_pushed, refs_failed, failure_messages) = summarize_outcomes(&result);

    // Translate metrics deltas into `PublishStats` — see the type
    // docstring for why `xorbs_uploaded` / `shards_uploaded` are
    // zero in V1.
    let bytes_uploaded = match (bytes_before, metrics.as_deref()) {
        (Some(before), Some(m)) => m.snapshot().bytes_uploaded.saturating_sub(before),
        _ => 0,
    };

    let stats = PublishStats {
        refs_pushed,
        refs_failed,
        bytes_uploaded,
        xorbs_uploaded: 0,
        shards_uploaded: 0,
        head_commit_oid: head_commit_oid.clone(),
        branch: branch.clone(),
    };

    if refs_failed > 0 {
        return Err(CrabError::Internal(format!(
            "publish: {refs_failed} ref(s) failed to push: {failure_messages}"
        )));
    }

    info!(
        refs_pushed = stats.refs_pushed,
        bytes_uploaded = stats.bytes_uploaded,
        head = %stats.head_commit_oid,
        branch = %stats.branch,
        "publish: complete"
    );

    Ok(stats)
}

/// Fold a [`PushResult`](crate::git::push::PushResult) into
/// `(ok_count, err_count, joined_error_messages)`.
///
/// The joined messages are intentionally separated by `"; "` and
/// prefixed with the ref name so a multi-ref failure stays
/// intelligible in an error log without needing structured output.
fn summarize_outcomes(result: &crate::git::push::PushResult) -> (u64, u64, String) {
    let mut ok: u64 = 0;
    let mut failed: u64 = 0;
    let mut failures: Vec<String> = Vec::new();
    #[allow(
        deprecated,
        reason = "pattern-matches the deprecated Error variant for backward compat"
    )]
    for (ref_name, outcome) in &result.outcomes {
        match outcome {
            RefPushOutcome::Ok => ok += 1,
            RefPushOutcome::Error(msg) => {
                failed += 1;
                failures.push(format!("{ref_name}: {msg}"));
            }
            RefPushOutcome::Rejected(reason) => {
                failed += 1;
                failures.push(format!("{ref_name}: {reason}"));
            }
        }
    }
    // Sort for determinism — HashMap iteration order is otherwise
    // unstable across runs, which hurts both logs and tests.
    failures.sort();
    (ok, failed, failures.join("; "))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    deprecated,
    reason = "test assertions; exercises the deprecated RefPushOutcome::Error path"
)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, MutexGuard};
    use std::time::Duration;

    use bytes::Bytes;
    use futures_util::TryStreamExt;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::core::metrics::Metrics;
    use crate::git::push::PushResult;
    use crate::import::assemble::{AssembleInputs, AssembleProgressSink, run_assemble};
    use crate::import::ingest::{IngestInputs, IngestProgressSink, StageEvent, run_ingest};
    use crate::import::journal::{EntryState, ImportEntry, Journal};
    use crate::import::window::CommitWindow;
    use crate::metadata::manifest::Manifest;
    use crate::storage::store::{BucketIdentity, Store};
    use crate::test::git_repo::{CacheDirGuard, GIT_DIR_MUTEX};
    use crab_staging::{StagingArea, StagingAreaReadOnly};

    /// RAII guard that clears `GIT_DIR` and points `CRAB_CACHE_DIR`
    /// at a test-local path for the lifetime of the guard. Holds the
    /// process-wide env mutex so tests don't race on env vars.
    ///
    /// Unlike [`crate::test::git_repo::GitDirGuard`], this targets a
    /// caller-supplied `.git` directory instead of the shared test
    /// repo — publish tests need to push from a freshly-imported repo,
    /// not the static `file.txt` fixture.
    struct GitDirOverride {
        _cache_guard: CacheDirGuard,
        _cache_dir: TempDir,
        _lock: MutexGuard<'static, ()>,
        prev_git_dir: Option<String>,
        prev_git_work_tree: Option<String>,
        prev_git_common_dir: Option<String>,
    }

    impl GitDirOverride {
        /// Acquire the `GIT_DIR` mutex without setting the env var.
        /// Use this while running git operations that should honor
        /// the process's `current_dir` rather than an env override
        /// (e.g. `git init`, the assemble stage's commits).
        fn locked_without_env() -> Self {
            let cache_dir = TempDir::new().expect("tempdir for CRAB_CACHE_DIR");
            let cache_guard = CacheDirGuard::new(cache_dir.path());
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev_git_dir = std::env::var("GIT_DIR").ok();
            let prev_git_work_tree = std::env::var("GIT_WORK_TREE").ok();
            let prev_git_common_dir = std::env::var("GIT_COMMON_DIR").ok();
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            // Clear any stray Git env so `current_dir`-scoped git
            // commands don't get redirected to a sibling test's
            // repo.
            unsafe {
                std::env::remove_var("GIT_DIR");
                std::env::remove_var("GIT_WORK_TREE");
                std::env::remove_var("GIT_COMMON_DIR");
            }
            Self {
                _cache_guard: cache_guard,
                _cache_dir: cache_dir,
                _lock: lock,
                prev_git_dir,
                prev_git_work_tree,
                prev_git_common_dir,
            }
        }
    }

    impl Drop for GitDirOverride {
        fn drop(&mut self) {
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            unsafe {
                match &self.prev_git_dir {
                    Some(v) => std::env::set_var("GIT_DIR", v),
                    None => std::env::remove_var("GIT_DIR"),
                }
                match &self.prev_git_work_tree {
                    Some(v) => std::env::set_var("GIT_WORK_TREE", v),
                    None => std::env::remove_var("GIT_WORK_TREE"),
                }
                match &self.prev_git_common_dir {
                    Some(v) => std::env::set_var("GIT_COMMON_DIR", v),
                    None => std::env::remove_var("GIT_COMMON_DIR"),
                }
            }
        }
    }

    /// No-op ingest progress sink for the integration harness.
    #[derive(Default)]
    struct SilentIngest;

    impl IngestProgressSink for SilentIngest {
        fn stage_event(&mut self, _event: &StageEvent<'_>) {}
    }

    /// No-op assemble progress sink for the integration harness.
    #[derive(Default)]
    struct SilentAssemble;

    impl AssembleProgressSink for SilentAssemble {
        fn assemble_event(&mut self, _event: &crate::import::assemble::AssembleEvent) {}
    }

    /// Build a [`ResolvedStore`] from an existing [`ObjectStore`] and
    /// a prefix. Used for both source and target.
    fn resolved(inner: Arc<dyn ObjectStore>, prefix: &str) -> ResolvedStore {
        ResolvedStore {
            store: Store::new(inner),
            bucket: BucketIdentity::local_unset(),
            prefix: prefix.to_owned(),
        }
    }

    /// Write a raw object into the store at `<prefix>/<relative>`.
    async fn seed_object(store: &Arc<dyn ObjectStore>, prefix: &str, relative: &str, body: &[u8]) {
        let key = if prefix.is_empty() {
            relative.to_owned()
        } else {
            format!("{prefix}/{relative}")
        };
        store
            .put(
                &ObjectPath::from(key),
                PutPayload::from(Bytes::from(body.to_vec())),
            )
            .await
            .expect("seed object");
    }

    /// List every key currently in the store. Used for before/after
    /// comparisons in same-bucket and cross-bucket assertions.
    async fn list_all(store: &Arc<dyn ObjectStore>) -> Vec<String> {
        let metas = store.list(None).try_collect::<Vec<_>>().await.unwrap();
        let mut keys: Vec<String> = metas.into_iter().map(|m| m.location.to_string()).collect();
        keys.sort();
        keys
    }

    /// List keys under a specific prefix. Returns lexicographically
    /// sorted strings so tests can compare deterministically.
    async fn list_prefix(store: &Arc<dyn ObjectStore>, prefix: &str) -> Vec<String> {
        let path = ObjectPath::from(prefix.to_owned());
        let metas = store
            .list(Some(&path))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let mut keys: Vec<String> = metas.into_iter().map(|m| m.location.to_string()).collect();
        keys.sort();
        keys
    }

    /// Configure a repo-local git identity so commits succeed without
    /// inheriting the host identity.
    fn configure_test_identity(repo_root: &Path) {
        for (key, val) in [("user.name", "Crab Test"), ("user.email", "test@crab.dev")] {
            let status = Command::new("git")
                .args(["config", "--local", key, val])
                .current_dir(repo_root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git config --local must run");
            assert!(status.success(), "git config --local {key} failed");
        }
    }

    /// Build a [`Pending`] journal row for the given seeded object.
    fn pending_entry(path: &str, size: u64) -> ImportEntry {
        ImportEntry {
            relative_path: path.into(),
            version_id: String::new(),
            size,
            etag: None,
            last_modified: 0,
            is_delete_marker: false,
            state: EntryState::Pending,
        }
    }

    /// End-to-end harness output so each test can reuse the same
    /// ingest/assemble driver and only vary the store topology.
    struct EndToEnd {
        repo_root: PathBuf,
        staging_root: PathBuf,
        journal_root: PathBuf,
        head_oid: String,
        staged_entries: Vec<ImportEntry>,
    }

    /// Drive ingest and assemble against a seeded source store,
    /// producing a local git repo with one commit at `HEAD`.
    ///
    /// Returns the repo root, staging root, journal root (for the
    /// temp dir inspection), head commit OID, and the `Staged`
    /// entries that the assemble step fed the commit window.
    async fn run_ingest_and_assemble(
        source: ResolvedStore,
        objects: &[(&str, Vec<u8>)],
        tmp: &TempDir,
    ) -> EndToEnd {
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        // git init first so we can set a local identity before
        // `run_assemble` runs its identity check.
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&repo_root);

        // Journal + staging live *outside* the repo directory so
        // that `run_assemble`'s non-empty-target check still sees
        // an empty-ish git repo (only `.git/`). In production the
        // coordinator places them under `<into>/.crab/`, which
        // is created after assemble via the filter-driver install.
        // For this test harness, siblings are sufficient.
        let journal_root = tmp.path().join("journal");
        std::fs::create_dir_all(&journal_root).unwrap();
        let staging_root = tmp.path().join("staging");

        let journal = Journal::open(&journal_root).expect("journal open");
        let pending: Vec<ImportEntry> = objects
            .iter()
            .map(|(path, body)| pending_entry(path, body.len() as u64))
            .collect();
        journal.upsert_entry_batch(&pending).expect("upsert");
        let journal = Arc::new(Mutex::new(journal));

        let staging = Arc::new(
            StagingArea::open(staging_root.clone())
                .await
                .expect("open staging"),
        );

        let progress: Arc<Mutex<SilentIngest>> = Arc::new(Mutex::new(SilentIngest));
        let cancel = CancellationToken::new();

        let inputs = IngestInputs {
            source,
            journal: Arc::clone(&journal),
            staging: Arc::clone(&staging),
            repo_root: repo_root.clone(),
            lfs_store: None,
            jobs: 2,
            fail_fast: true,
            progress,
            metrics: None,
            cancel: cancel.clone(),
        };
        let stats = run_ingest(inputs).await.expect("ingest");
        let snapshot = stats.snapshot();
        assert_eq!(
            snapshot.failed, 0,
            "ingest must not fail any entry: {snapshot:?}"
        );
        assert_eq!(
            snapshot.staged as usize,
            objects.len(),
            "every object must stage: {snapshot:?}"
        );

        // Collect `Staged` entries off the journal so we can hand
        // them to the window planner.
        let staged_entries: Vec<ImportEntry> = {
            let guard = journal.lock().await;
            let mut out = Vec::new();
            guard
                .iter_entries_sorted_by_time(|e| {
                    if matches!(e.state, EntryState::Staged { .. }) {
                        out.push(e);
                    }
                    Ok(())
                })
                .expect("iterate journal");
            out
        };
        assert_eq!(staged_entries.len(), objects.len());

        // Release the journal before assemble runs (assemble never
        // touches the journal, but we want the Arc count to drop so
        // we can re-open later if needed).
        drop(journal);
        drop(staging);

        // One window containing all entries — the simplest flat
        // import shape.
        let window = CommitWindow {
            window_start: 0,
            window_end: 0,
            entries: staged_entries.clone(),
        };

        let assemble_progress = Arc::new(Mutex::new(SilentAssemble));
        let assemble_inputs = AssembleInputs {
            into: repo_root.clone(),
            branch: "main".into(),
            force: false,
            resume: false,
            target_url: "crab://test-bucket/repos/v2".into(),
            windows: vec![window],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: assemble_progress,
            metrics: None,
            cancel,
        };

        let stats = run_assemble(assemble_inputs).await.expect("assemble");
        let head_oid = stats.head_commit_oid.clone().expect("head oid");

        EndToEnd {
            repo_root,
            staging_root,
            journal_root,
            head_oid,
            staged_entries,
        }
    }

    /// Run publish against the given target and return both the
    /// publish stats and the full post-publish listing of the target
    /// store.
    ///
    /// `git_dir_guard` must already hold `GIT_DIR_MUTEX` with
    /// `GIT_DIR` cleared. `run_publish` is responsible for passing
    /// its explicit git directory into the push pipeline.
    async fn run_publish_against(
        target_inner: Arc<dyn ObjectStore>,
        target_prefix: &str,
        staging_root: PathBuf,
        repo_root: PathBuf,
        head_oid: String,
        git_dir_guard: GitDirOverride,
    ) -> (PublishStats, Vec<String>, GitDirOverride) {
        let git_dir = repo_root.join(".git");

        let staging_ro = Arc::new(
            StagingAreaReadOnly::open(staging_root)
                .await
                .expect("open staging RO"),
        );
        let metrics = Arc::new(Metrics::new());

        let target = resolved(Arc::clone(&target_inner), target_prefix);

        let inputs = PublishInputs {
            target,
            repo_prefix: target_prefix.to_owned(),
            staging: staging_ro,
            branch: "main".into(),
            head_commit_oid: head_oid,
            git_dir,
            metrics: Some(Arc::clone(&metrics)),
            cancel: CancellationToken::new(),
        };

        let stats = tokio::time::timeout(Duration::from_secs(60), run_publish(inputs))
            .await
            .expect("publish must finish within timeout")
            .expect("publish must succeed");

        let listing = list_all(&target_inner).await;
        (stats, listing, git_dir_guard)
    }

    async fn target_manifest(store: &Arc<dyn ObjectStore>, prefix: &str) -> Manifest {
        let path = ObjectPath::from(format!("{prefix}/manifest"));
        let body = store
            .get(&path)
            .await
            .expect("target manifest exists")
            .bytes()
            .await
            .expect("target manifest body");
        serde_json::from_slice(&body).expect("target manifest JSON")
    }

    // ── Unit: outcome summarization ──────────────────────────────

    #[test]
    fn summarize_outcomes_counts_ok_and_errors() {
        use std::collections::HashMap;
        let mut outcomes = HashMap::new();
        outcomes.insert("refs/heads/main".to_owned(), RefPushOutcome::Ok);
        outcomes.insert(
            "refs/heads/dev".to_owned(),
            RefPushOutcome::Error("conflict".to_owned()),
        );
        let result = PushResult::new(outcomes);
        let (ok, failed, msg) = summarize_outcomes(&result);
        assert_eq!(ok, 1);
        assert_eq!(failed, 1);
        assert!(msg.contains("refs/heads/dev"));
        assert!(msg.contains("conflict"));
    }

    #[test]
    fn summarize_outcomes_empty() {
        let result = PushResult::empty();
        let (ok, failed, msg) = summarize_outcomes(&result);
        assert_eq!(ok, 0);
        assert_eq!(failed, 0);
        assert!(msg.is_empty());
    }

    // ── Task 13.3: cross-bucket integration ──────────────────────

    /// Two separate in-memory stores. End-to-end
    /// enumerate-by-hand → ingest → assemble → publish. Source must
    /// see zero writes after publish; target must contain xorbs,
    /// shards, file-index, refs, and a manifest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cross_bucket_publish_writes_target_only() {
        // Hold `GIT_DIR_MUTEX` for the whole test, with env
        // cleared, so the ingest+assemble phase's `git` commands
        // can't get redirected by a sibling test's leftover
        // `GIT_DIR` — and so our own later override doesn't leak
        // into a sibling test.
        let git_dir_guard = GitDirOverride::locked_without_env();

        let tmp = TempDir::new().unwrap();

        // Seed the source bucket. Use payloads large enough to
        // exercise CDC + xorb packing but small enough to stay
        // below STREAM_THRESHOLD.
        let source_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let objects: Vec<(&str, Vec<u8>)> = vec![
            ("data/a.bin", vec![0x11u8; 8 * 1024]),
            ("data/b.bin", vec![0x22u8; 16 * 1024]),
            ("models/m.safetensors", vec![0x33u8; 32 * 1024]),
        ];
        for (path, body) in &objects {
            seed_object(&source_inner, "", path, body).await;
        }

        // Snapshot the source keys before anything runs so we can
        // prove they stay byte-for-byte identical after publish.
        let source_before = list_all(&source_inner).await;
        assert_eq!(
            source_before.len(),
            objects.len(),
            "source seed must match expected count"
        );

        let source = resolved(Arc::clone(&source_inner), "");
        let e2e = run_ingest_and_assemble(source, &objects, &tmp).await;

        // Separate target store with a "repos/v2" prefix.
        let target_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let target_prefix = "repos/v2";
        let (stats, target_keys, _git_dir_guard) = run_publish_against(
            Arc::clone(&target_inner),
            target_prefix,
            e2e.staging_root.clone(),
            e2e.repo_root.clone(),
            e2e.head_oid.clone(),
            git_dir_guard,
        )
        .await;

        // Publish stats sanity.
        assert_eq!(stats.refs_pushed, 1, "exactly one ref was planned");
        assert_eq!(stats.refs_failed, 0);
        assert_eq!(stats.branch, "main");
        assert_eq!(stats.head_commit_oid, e2e.head_oid);
        let manifest = target_manifest(&target_inner, target_prefix).await;
        assert_eq!(
            manifest.refs.get("refs/heads/main"),
            Some(&e2e.head_oid),
            "publish must push the assembled import repo, not the process cwd"
        );

        // Sanity: the target listing must be non-empty. If the
        // push pipeline stubbed everything out, `target_keys`
        // would be empty and the `have_prefix` checks below would
        // silently confirm that (since `.starts_with` on nothing
        // is never true). A non-empty listing is the first
        // signal the pipeline actually uploaded.
        assert!(
            !target_keys.is_empty(),
            "publish must produce at least one object in the target store"
        );

        // Source must have zero new writes (and no deletes either).
        let source_after = list_all(&source_inner).await;
        assert_eq!(
            source_before, source_after,
            "cross-bucket publish must not touch the source store"
        );

        // Target must contain the full Crab layout.
        let target_keyset: HashSet<&str> = target_keys.iter().map(String::as_str).collect();
        let have_prefix = |p: &str| target_keyset.iter().any(|k| k.starts_with(p));

        assert!(
            have_prefix(".crab/xorbs/"),
            "target missing xorbs: {target_keys:?}"
        );
        assert!(
            have_prefix(".crab/shards/"),
            "target missing shards: {target_keys:?}"
        );
        // file-index is now written through the per-repo SlateDB at
        // `{repo_prefix}/file_index_db/` instead of per-file
        // `.crab/file-index/{hash}` objects (spec: slatedb
        // metadata hard cutover).
        assert!(
            have_prefix(&format!("{target_prefix}/file_index_db/")),
            "target missing file_index_db: {target_keys:?}"
        );
        assert!(
            target_keyset.contains(&format!("{target_prefix}/manifest").as_str()),
            "target missing manifest pointer: {target_keys:?}"
        );
        assert!(
            have_prefix(&format!("{target_prefix}/metadata/pack/segments/"))
                && have_prefix(&format!("{target_prefix}/metadata/pack/indexes/"))
                && have_prefix(&format!("{target_prefix}/metadata/shard/segments/"))
                && have_prefix(&format!("{target_prefix}/metadata/shard/indexes/")),
            "target missing segmented metadata under {target_prefix}/metadata/: {target_keys:?}"
        );

        // Ingest stats / assemble stats are already validated in
        // run_ingest_and_assemble; keep this test tight on the
        // publish contract.
        assert!(
            e2e.staged_entries.iter().all(|e| !e.is_delete_marker),
            "flat seed must not produce delete-markers"
        );

        // Sanity-check the journal dir exists (publish does not
        // clean it up — that's the coordinator's job).
        assert!(e2e.journal_root.join(".crab").exists());
    }

    // ── Task 13.4: same-bucket integration ───────────────────────

    /// One in-memory store plays both source and target, with
    /// distinct prefixes. Source keys must remain byte-for-byte
    /// identical; target prefix receives the full Crab layout.
    ///
    /// Note: xorbs / shards / file-index route to the shared global
    /// prefix `.crab/` regardless of which `repo_prefix` the target
    /// uses. This is the expected layout — the prompt's "xorbs under
    /// the shared `.crab/xorbs/` prefix" matches this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_bucket_publish_leaves_source_untouched() {
        // See the cross-bucket test for why we hold `GIT_DIR_MUTEX`
        // across the entire ingest+assemble+publish pipeline.
        let git_dir_guard = GitDirOverride::locked_without_env();

        let tmp = TempDir::new().unwrap();
        let shared: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // Source lives under `data/`; target lives under `repos/v2/`.
        // Both in the same physical store.
        let source_prefix = "data";
        let target_prefix = "repos/v2";

        let objects: Vec<(&str, Vec<u8>)> = vec![
            ("a.bin", vec![0x44u8; 8 * 1024]),
            ("sub/b.bin", vec![0x55u8; 16 * 1024]),
        ];
        for (rel, body) in &objects {
            seed_object(&shared, source_prefix, rel, body).await;
        }

        // Capture source-prefix state before publish, keyed on
        // `(path, body_bytes_hash)` so we can detect both
        // additions and in-place mutations.
        let source_before_keys = list_prefix(&shared, source_prefix).await;
        let mut source_before_bodies = Vec::new();
        for key in &source_before_keys {
            let bytes = shared
                .get(&ObjectPath::from(key.clone()))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            source_before_bodies.push((key.clone(), bytes.to_vec()));
        }
        assert_eq!(source_before_keys.len(), objects.len());

        let source = resolved(Arc::clone(&shared), source_prefix);
        let e2e = run_ingest_and_assemble(source, &objects, &tmp).await;

        let (stats, all_keys, _git_dir_guard) = run_publish_against(
            Arc::clone(&shared),
            target_prefix,
            e2e.staging_root.clone(),
            e2e.repo_root.clone(),
            e2e.head_oid.clone(),
            git_dir_guard,
        )
        .await;

        assert_eq!(stats.refs_pushed, 1);
        assert_eq!(stats.refs_failed, 0);

        // Sanity: publish must produce objects in the shared store.
        assert!(
            !all_keys.is_empty(),
            "publish must produce at least one object"
        );

        // Source keys must be byte-for-byte identical. We re-list
        // and re-GET to cover both "did we add anything under
        // `data/`?" and "did we overwrite existing bytes?".
        let source_after_keys = list_prefix(&shared, source_prefix).await;
        assert_eq!(
            source_before_keys, source_after_keys,
            "same-bucket publish must not add or remove keys under the source prefix"
        );
        for (key, expected) in &source_before_bodies {
            let actual = shared
                .get(&ObjectPath::from(key.clone()))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert_eq!(
                actual.as_ref(),
                expected.as_slice(),
                "same-bucket publish must not mutate source object {key}"
            );
        }

        // Target prefix got the Crab layout:
        //   - `.crab/xorbs/*`, `.crab/shards/*` (global)
        //   - `{target_prefix}/file_index_db/*` (per-repo SlateDB)
        //   - `{target_prefix}/manifest`, `{target_prefix}/metadata/*` (per-repo)
        // Refs are embedded in the manifest pointer (unified manifest).
        let key_set: HashSet<&str> = all_keys.iter().map(String::as_str).collect();
        let have_prefix = |p: &str| key_set.iter().any(|k| k.starts_with(p));

        assert!(have_prefix(".crab/xorbs/"), "xorbs: {all_keys:?}");
        assert!(have_prefix(".crab/shards/"), "shards: {all_keys:?}");
        // file-index is now written through the per-repo SlateDB at
        // `{repo_prefix}/file_index_db/`. See the note in
        // `cross_bucket_publish_writes_target_only` above.
        assert!(
            have_prefix(&format!("{target_prefix}/file_index_db/")),
            "file_index_db: {all_keys:?}"
        );
        assert!(
            key_set.contains(format!("{target_prefix}/manifest").as_str()),
            "target manifest pointer: {all_keys:?}"
        );
        assert!(
            have_prefix(&format!("{target_prefix}/metadata/pack/segments/"))
                && have_prefix(&format!("{target_prefix}/metadata/pack/indexes/"))
                && have_prefix(&format!("{target_prefix}/metadata/shard/segments/"))
                && have_prefix(&format!("{target_prefix}/metadata/shard/indexes/")),
            "target segmented metadata: {all_keys:?}"
        );

        // And the source prefix is still exactly what we seeded —
        // no xorb or shard accidentally landed under `data/`.
        for key in list_prefix(&shared, source_prefix).await {
            assert!(
                !key.starts_with(".crab/"),
                "xorb/shard must never land under source prefix: {key}"
            );
        }
    }
}
