//! Background refresh loop for branch tracking.
//!
//! Polls for remote ref changes and local `.git/HEAD` changes at a
//! configurable interval. When the tracked branch advances:
//!
//! 1. Build a new snapshot generation from the new HEAD tree.
//! 2. Diff old vs new tree to find changed paths.
//! 3. Detect conflicts (paths changed in both base and overlay).
//! 4. Reconcile the overlay (discard stale entries).
//! 5. Atomically swap the generation in the resolver.
//!
//! Inspired by artifact-fs's `watcher.Poller` for local HEAD polling
//! and extended with remote ref polling.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result};
use crate::overlay::OverlayStore;
use crate::pipeline::{PipelineConfig, PipelineOutput};
#[cfg(test)]
use crate::repository::{
    GitRemoteRefFetcher, NoopRemoteRefFetcher, commit_time_from_oid, redact_url,
};
use crate::repository::{RemoteRefFetcher, normalized_fetch_ref, reconcile_fetched_ref};
use crate::resolver::OverlayLookup;
use crate::snapshot::{BaseNode, SnapshotStore};

// ---------------------------------------------------------------------------
// Backoff state
// ---------------------------------------------------------------------------

/// Per-repo exponential backoff state for fetch failures.
///
/// Doubles the polling interval on each consecutive failure up to
/// `max_interval`, and resets to `base_interval` on success.
pub struct BackoffState {
    /// Current polling interval (may be elevated after failures).
    pub current_interval: Duration,
    /// Configured base interval to reset to on success.
    pub base_interval: Duration,
    /// Upper bound for the backoff interval.
    pub max_interval: Duration,
    /// Last fetch result description ("ok" or error message).
    pub last_fetch_result: Option<String>,
}

impl BackoffState {
    /// Create a new backoff state with the given base and max intervals.
    pub fn new(base_interval: Duration, max_interval: Duration) -> Self {
        Self {
            current_interval: base_interval,
            base_interval,
            max_interval,
            last_fetch_result: None,
        }
    }

    /// Record a fetch failure: double the interval (capped at max) and
    /// store the error description.
    pub fn on_failure(&mut self, error: &str) {
        self.current_interval = (self.current_interval * 2).min(self.max_interval);
        self.last_fetch_result = Some(error.to_owned());
    }

    /// Record a fetch success: reset to base interval.
    pub fn on_success(&mut self) {
        self.current_interval = self.base_interval;
        self.last_fetch_result = Some("ok".to_owned());
    }
}

// ---------------------------------------------------------------------------
// Credential redaction
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the refresh loop.
#[derive(Debug, Clone)]
pub struct RefreshConfig {
    /// Interval between remote ref polls (default 30s).
    pub remote_poll_interval: Duration,
    /// Interval between local `.git/HEAD` polls (default 500ms).
    pub local_poll_interval: Duration,
    /// Path to the `.git` directory of the repository.
    pub git_dir: PathBuf,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            remote_poll_interval: Duration::from_secs(30),
            local_poll_interval: Duration::from_millis(500),
            git_dir: PathBuf::from(".git"),
        }
    }
}

// ---------------------------------------------------------------------------
// Remote ref fetcher trait
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Local HEAD watcher
// ---------------------------------------------------------------------------

/// Tracks the selected local ref through the shared Git ref resolver.
struct LocalHeadWatcher {
    git_dir: PathBuf,
    tracked_ref: Option<String>,
    last_selection: Option<(String, String)>,
}

impl LocalHeadWatcher {
    fn new(git_dir: PathBuf, tracked_ref: Option<String>) -> Self {
        Self {
            git_dir,
            tracked_ref,
            last_selection: None,
        }
    }

    fn poll(&mut self) -> bool {
        let selection = self.read_selection();
        let selection = match selection {
            Ok(selection) => selection,
            Err(error) => {
                debug!(%error, "cannot resolve tracked local ref");
                return false;
            }
        };
        let changed = self
            .last_selection
            .as_ref()
            .is_some_and(|last| last != &selection);
        self.last_selection = Some(selection);
        changed
    }

    fn read_selection(&self) -> Result<(String, String)> {
        let ref_name = match self.tracked_ref.as_deref().filter(|name| *name != "HEAD") {
            Some(name) => normalized_fetch_ref(name)?,
            None => crab_git::ref_resolve::resolve_head_symref_at(&self.git_dir)?
                .unwrap_or_else(|| "HEAD".to_owned()),
        };
        let oid = crab_git::ref_resolve::resolve_ref_at(&self.git_dir, &ref_name)?;
        Ok((oid, ref_name))
    }

    fn current_ref_name(&self) -> Option<String> {
        let (_, ref_name) = self.last_selection.as_ref()?;
        (ref_name != "HEAD").then(|| ref_name.clone())
    }

    fn current_ref_oid(&self) -> Option<String> {
        self.last_selection.as_ref().map(|(oid, _)| oid.clone())
    }
}

/// Detect paths that changed in both the base snapshot and the overlay.
///
/// For each changed path in the diff, checks if the overlay has a
/// non-delete entry for that path. Logs a warning for each conflict
/// but does not auto-resolve — the overlay entry is preserved.
fn detect_conflicts(changed_paths: &HashSet<String>, overlay: &Arc<OverlayStore>) -> Vec<String> {
    let mut conflicts = Vec::new();

    for path in changed_paths {
        if let Some(entry) = OverlayLookup::get(overlay.as_ref(), path)
            && !entry.is_deleted()
        {
            warn!(
                path = %path,
                overlay_kind = ?entry.kind,
                "conflict: file changed in both base and overlay, preserving overlay"
            );
            conflicts.push(path.clone());
        }
    }

    if !conflicts.is_empty() {
        warn!(
            count = conflicts.len(),
            "refresh detected conflicts — overlay entries preserved"
        );
    }

    conflicts
}

// ---------------------------------------------------------------------------
// Tree diff
// ---------------------------------------------------------------------------

/// Compute the set of paths that differ between two snapshot generations.
///
/// Collects all paths from both generations and returns those that exist
/// in only one generation or have different content (different OID, size,
/// or pointer).
fn diff_generations(
    snapshot: &SnapshotStore,
    old_gen: i64,
    new_gen: i64,
) -> Result<HashSet<String>> {
    let old_nodes = collect_all_nodes(snapshot, old_gen)?;
    let new_nodes = collect_all_nodes(snapshot, new_gen)?;

    let mut changed = HashSet::new();

    // Paths in old but not in new, or different content.
    for (path, old_node) in &old_nodes {
        match new_nodes.get(path) {
            Some(new_node) => {
                if nodes_differ(old_node, new_node) {
                    changed.insert(path.clone());
                }
            }
            None => {
                changed.insert(path.clone());
            }
        }
    }

    // Paths in new but not in old.
    for path in new_nodes.keys() {
        if !old_nodes.contains_key(path) {
            changed.insert(path.clone());
        }
    }

    debug!(
        changed_count = changed.len(),
        old_gen, new_gen, "tree diff complete"
    );
    Ok(changed)
}

/// Collect all nodes from a generation into a map keyed by path.
fn collect_all_nodes(
    snapshot: &SnapshotStore,
    generation: i64,
) -> Result<std::collections::HashMap<String, BaseNode>> {
    // Walk the tree recursively starting from root.
    let mut all_nodes = std::collections::HashMap::new();
    collect_children_recursive(snapshot, generation, "", &mut all_nodes)?;
    Ok(all_nodes)
}

/// Recursively collect all nodes under a parent path.
fn collect_children_recursive(
    snapshot: &SnapshotStore,
    generation: i64,
    parent: &str,
    out: &mut std::collections::HashMap<String, BaseNode>,
) -> Result<()> {
    let children = snapshot.list_children(generation, parent)?;
    for child in children {
        let path = child.path.clone();
        let is_dir = child.node_type == crate::snapshot::NodeType::Dir;
        out.insert(path.clone(), child);
        if is_dir {
            collect_children_recursive(snapshot, generation, &path, out)?;
        }
    }
    Ok(())
}

/// Check if two nodes differ in content-relevant fields.
fn nodes_differ(a: &BaseNode, b: &BaseNode) -> bool {
    a.object_oid != b.object_oid
        || a.pointer != b.pointer
        || a.size != b.size
        || a.mode != b.mode
        || a.node_type != b.node_type
}

use std::sync::Mutex as StdMutex;

// ---------------------------------------------------------------------------
// Refresh service
// ---------------------------------------------------------------------------

/// Background service that polls for branch changes and updates the
/// mounted filesystem's snapshot.
pub struct RefreshService {
    output: PipelineOutput,
    mountpoint: PathBuf,
    selection: PipelineConfig,
    overlay: Arc<OverlayStore>,
    remote_fetcher: Arc<dyn RemoteRefFetcher>,
    config: RefreshConfig,
    cancel: CancellationToken,
    /// Per-service backoff state for remote fetch failures.
    backoff: StdMutex<BackoffState>,
}

impl RefreshService {
    /// Create a new refresh service.
    pub fn new(
        output: &PipelineOutput,
        mountpoint: &Path,
        overlay: Arc<OverlayStore>,
        remote_fetcher: Arc<dyn RemoteRefFetcher>,
        config: RefreshConfig,
        selection: PipelineConfig,
        cancel: CancellationToken,
    ) -> Self {
        let backoff = BackoffState::new(config.remote_poll_interval, Duration::from_mins(10));
        Self {
            output: output.clone(),
            mountpoint: mountpoint.to_owned(),
            selection,
            overlay,
            remote_fetcher,
            config,
            cancel,
            backoff: StdMutex::new(backoff),
        }
    }

    /// Run the refresh loop until cancellation.
    ///
    /// Polls two timers in one task:
    /// - Remote ref polling at `remote_poll_interval` (with backoff on failure)
    /// - Local `.git/HEAD` polling at `local_poll_interval`
    ///
    /// Either trigger rebuilds the snapshot and swaps the generation.
    pub async fn run(&self) {
        let mut local_watcher = LocalHeadWatcher::new(
            self.config.git_dir.clone(),
            self.output.resolver.capture_view().tracked_ref.clone(),
        );
        let mut remote_ticker = tokio::time::interval(self.config.remote_poll_interval);
        let mut local_ticker = tokio::time::interval(self.config.local_poll_interval);

        // Capture before reading Git; queued work cannot overwrite a later
        // switch or adoption even when both transitions select the same ref.
        let expected = self.output.resolver.capture_view();
        local_watcher.poll();
        self.handle_local_change(&local_watcher, expected).await;

        // Skip the first immediate tick.
        remote_ticker.tick().await;
        local_ticker.tick().await;

        info!(
            remote_interval = ?self.config.remote_poll_interval,
            local_interval = ?self.config.local_poll_interval,
            tracked_ref = ?local_watcher.tracked_ref,
            "refresh loop started"
        );

        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    info!("refresh loop cancelled");
                    break;
                }
                _ = remote_ticker.tick() => {
                    self.poll_remote(&local_watcher).await;
                    // Adjust the remote ticker period based on backoff state.
                    let interval = self.backoff.lock()
                        .map_or(self.config.remote_poll_interval, |b| b.current_interval);
                    remote_ticker = tokio::time::interval(interval);
                    remote_ticker.tick().await; // consume the immediate first tick
                }
                _ = local_ticker.tick() => {
                    let expected = self.output.resolver.capture_view();
                    local_watcher.tracked_ref = expected.tracked_ref.clone();
                    if local_watcher.poll() {
                        info!("local HEAD change detected");
                        self.handle_local_change(&local_watcher, expected).await;
                    }
                }
            }
        }
    }

    /// Poll the remote for ref changes and refresh if needed.
    async fn poll_remote(&self, local_watcher: &LocalHeadWatcher) {
        let expected = self.output.resolver.capture_view();
        let ref_name = self.effective_ref_name(local_watcher);
        let Some(ref_name) = ref_name else {
            debug!("no ref to track for remote polling");
            return;
        };

        let fetcher = Arc::clone(&self.remote_fetcher);
        let ref_name_for_fetch = ref_name.clone();
        let fetch_result =
            tokio::task::spawn_blocking(move || fetcher.fetch_ref(&ref_name_for_fetch)).await;

        let remote_oid = match fetch_result {
            Ok(Ok(Some(oid))) => oid,
            Ok(Ok(None)) => {
                debug!(ref_name = %ref_name, "remote ref not found");
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.on_success();
                }
                return;
            }
            Ok(Err(e)) => {
                warn!(ref_name = %ref_name, error = %e, "remote ref fetch failed");
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.on_failure(&e.to_string());
                }
                return;
            }
            Err(e) => {
                warn!(ref_name = %ref_name, error = %e, "remote ref fetch task failed");
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.on_failure(&e.to_string());
                }
                return;
            }
        };

        let selected_oid = match reconcile_fetched_ref(&self.config.git_dir, &ref_name, &remote_oid)
        {
            Ok(oid) => oid,
            Err(error) => {
                warn!(ref_name = %ref_name, error = %error, "failed to reconcile local and remote commits");
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.on_failure(&error.to_string());
                }
                return;
            }
        };

        // Compare with current snapshot HEAD after preserving any local commit.
        let current_oid = self.output.resolver.head().map(|head| head.0);
        if current_oid.as_deref() == Some(selected_oid.as_str())
            && self.output.snapshot.current_generation().ok().flatten()
                == Some(self.output.resolver.generation())
        {
            debug!(ref_name = %ref_name, "remote ref unchanged");
            if let Ok(mut backoff) = self.backoff.lock() {
                backoff.on_success();
            }
            return;
        }

        info!(
            ref_name = %ref_name,
            old_oid = ?current_oid,
            new_oid = %selected_oid,
            "tracked branch advanced"
        );

        match self
            .refresh_to_oid(&selected_oid, &ref_name, expected)
            .await
        {
            Ok(()) => {
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.on_success();
                }
            }
            Err(e) => {
                warn!(error = %e, "refresh to remote OID failed");
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.on_failure(&e.to_string());
                }
            }
        }
    }

    /// Handle a local HEAD change (branch switch or advance).
    async fn handle_local_change(
        &self,
        local_watcher: &LocalHeadWatcher,
        expected: Arc<crate::resolver::SnapshotView>,
    ) {
        let ref_name = self.effective_ref_name(local_watcher);
        let new_oid = local_watcher.current_ref_oid();

        let Some(new_oid) = new_oid else {
            debug!("no OID available after local HEAD change");
            return;
        };

        let current_oid = self.output.resolver.head().map(|head| head.0);
        if current_oid.as_deref() == Some(new_oid.as_str()) {
            debug!("local HEAD change but OID unchanged");
            return;
        }

        let ref_name = ref_name.unwrap_or_else(|| "HEAD".to_owned());
        info!(
            ref_name = %ref_name,
            old_oid = ?current_oid,
            new_oid = %new_oid,
            "local branch change"
        );

        if let Err(e) = self.refresh_to_oid(&new_oid, &ref_name, expected).await {
            warn!(error = %e, "refresh to local OID failed");
        }
    }

    /// Determine the effective ref name to track.
    ///
    /// If `--ref=<branch>` was specified, use that. Otherwise, use
    /// whatever HEAD currently points to.
    fn effective_ref_name(&self, local_watcher: &LocalHeadWatcher) -> Option<String> {
        if let Some(tracked) = self.output.resolver.capture_view().tracked_ref.clone() {
            return Some(tracked);
        }
        local_watcher.current_ref_name()
    }

    /// Refresh the snapshot to a new OID.
    ///
    /// 1. Build new snapshot from the new HEAD tree.
    /// 2. Diff old vs new generation.
    /// 3. Detect conflicts with overlay.
    /// 4. Reconcile overlay.
    /// 5. Atomically swap generation in resolver.
    async fn refresh_to_oid(
        &self,
        new_oid: &str,
        ref_name: &str,
        expected: Arc<crate::resolver::SnapshotView>,
    ) -> Result<()> {
        let engine = Arc::clone(&self.output.engine);
        let owner = Arc::clone(&engine);
        let mut output = self.output.clone();
        let mut selection = self.selection.clone();
        let oid = new_oid.to_owned();
        let selected_ref = ref_name.to_owned();
        let overlay = Arc::clone(&self.overlay);
        let mountpoint = self.mountpoint.clone();
        engine
            .run_control(async move {
                let _reset = crate::mount_runtime::begin_mount_update(&owner, &mountpoint).await?;
                let old_view = output.resolver.capture_view();
                if !Arc::ptr_eq(&old_view, &expected) {
                    return Ok(());
                }
                selection.ref_name = old_view.tracked_ref.clone();
                let update = crate::mount_runtime::publish_runtime_snapshot(
                    &mut output,
                    &selection,
                    &oid,
                    &selected_ref,
                )
                .await?;
                let snapshot = Arc::clone(&output.snapshot);
                tokio::task::spawn_blocking(move || {
                    let changed =
                        diff_generations(&snapshot, old_view.generation, update.generation)?;
                    detect_conflicts(&changed, &overlay);
                    Ok::<_, CrabError>(())
                })
                .await
                .map_err(CrabError::ReadTask)??;
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::snapshot::NodeType;
    use std::path::Path;
    use std::process::Command;

    // --- LocalHeadWatcher tests ---

    #[test]
    fn watcher_detects_head_change() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();

        // Initial HEAD pointing to main.
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git_dir.join("refs/heads/main"),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();

        let mut watcher = LocalHeadWatcher::new(git_dir.clone(), None);

        // First poll primes state, no change reported.
        assert!(!watcher.poll());
        assert_eq!(
            watcher.current_ref_name().as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(
            watcher.current_ref_oid().as_deref(),
            Some("1111111111111111111111111111111111111111")
        );

        // No change on second poll.
        assert!(!watcher.poll());

        // Switch to a different branch.
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(
            git_dir.join("refs/heads/feature"),
            "2222222222222222222222222222222222222222\n",
        )
        .unwrap();

        assert!(watcher.poll());
        assert_eq!(
            watcher.current_ref_name().as_deref(),
            Some("refs/heads/feature")
        );
        assert_eq!(
            watcher.current_ref_oid().as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn watcher_detects_branch_advance() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();

        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git_dir.join("refs/heads/main"),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();

        let mut watcher = LocalHeadWatcher::new(git_dir.clone(), None);
        assert!(!watcher.poll()); // prime

        // Advance the branch.
        std::fs::write(
            git_dir.join("refs/heads/main"),
            "3333333333333333333333333333333333333333\n",
        )
        .unwrap();
        assert!(watcher.poll());
        assert_eq!(
            watcher.current_ref_oid().as_deref(),
            Some("3333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn watcher_handles_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        // Detached HEAD (raw OID, no "ref:" prefix).
        std::fs::write(
            git_dir.join("HEAD"),
            "4444444444444444444444444444444444444444\n",
        )
        .unwrap();

        let mut watcher = LocalHeadWatcher::new(git_dir, None);
        assert!(!watcher.poll());
        assert!(watcher.current_ref_name().is_none());
    }

    #[test]
    fn watcher_tracks_selected_branch_through_packing() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path();
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git_dir.join("refs/heads/main"), "1".repeat(40)).unwrap();
        std::fs::write(git_dir.join("refs/heads/other"), "2".repeat(40)).unwrap();
        let mut watcher = LocalHeadWatcher::new(git_dir.to_owned(), Some("other".into()));
        assert!(!watcher.poll());
        std::fs::write(git_dir.join("refs/heads/main"), "3".repeat(40)).unwrap();
        assert!(
            !watcher.poll(),
            "unselected source HEAD must not change the mounted branch"
        );
        std::fs::write(
            git_dir.join("packed-refs"),
            format!("{} refs/heads/other\n", "4".repeat(40)),
        )
        .unwrap();
        std::fs::remove_file(git_dir.join("refs/heads/other")).unwrap();
        assert!(watcher.poll());
        assert_eq!(watcher.current_ref_oid(), Some("4".repeat(40)));
    }

    #[test]
    fn watcher_tracks_linked_worktree_and_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let git_dir = common.join("worktrees/linked");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
        std::fs::write(common.join("refs/heads/linked"), "1".repeat(40)).unwrap();
        let mut watcher = LocalHeadWatcher::new(git_dir.clone(), None);
        assert!(!watcher.poll());
        std::fs::write(common.join("refs/heads/linked"), "2".repeat(40)).unwrap();
        assert!(watcher.poll());
        std::fs::write(git_dir.join("HEAD"), "3".repeat(40)).unwrap();
        assert!(watcher.poll());
        assert_eq!(watcher.current_ref_oid(), Some("3".repeat(40)));
    }

    // --- remote fetcher tests ---

    #[test]
    fn noop_remote_ref_fetcher_returns_no_ref() {
        let fetcher = NoopRemoteRefFetcher;

        let oid = fetcher.fetch_ref("refs/heads/main").unwrap();

        assert!(oid.is_none());
    }

    #[test]
    fn git_remote_ref_fetcher_preserves_unpushed_local_branch() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let remote = dir.path().join("remote.git");
        let clone = dir.path().join("clone.git");

        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(&source, ["config", "user.email", "refresh-test@crab.local"]);
        git(&source, ["config", "user.name", "refresh test"]);
        std::fs::write(source.join("file.txt"), "base").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "base"]);
        git_in(
            dir.path(),
            ["clone", "--bare", source.to_str().unwrap(), "remote.git"],
        );
        git_in(
            dir.path(),
            [
                "clone",
                "--bare",
                "--filter=blob:none",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(&clone, ["config", "user.email", "refresh-test@crab.local"]);
        git(&clone, ["config", "user.name", "refresh test"]);

        let old_head = git_stdout(&clone, ["rev-parse", "refs/heads/main"]);
        let local_commit = git_stdout(
            &clone,
            [
                "commit-tree",
                "refs/heads/main^{tree}",
                "-p",
                &old_head,
                "-m",
                "local",
            ],
        );
        git(
            &clone,
            ["update-ref", "refs/heads/main", &local_commit, &old_head],
        );
        std::fs::write(source.join("file.txt"), "updated").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "update"]);
        git(&source, ["push", remote.to_str().unwrap(), "main"]);
        let new_head = git_stdout(&source, ["rev-parse", "HEAD"]);
        assert_ne!(old_head, new_head);

        let fetcher = GitRemoteRefFetcher::with_remote(clone.clone(), "origin");
        let fetched = fetcher.fetch_ref("main").unwrap().unwrap();

        assert_eq!(
            (&fetched.object_oid, &fetched.commit_oid),
            (&new_head, &new_head)
        );
        assert_eq!(
            git_stdout(&clone, ["rev-parse", "refs/heads/main"]),
            local_commit
        );
        assert_eq!(
            git_stdout(&clone, ["rev-parse", "refs/remotes/origin/main"]),
            new_head
        );
    }

    #[test]
    fn git_remote_ref_fetcher_returns_none_for_missing_ref() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let remote = dir.path().join("remote.git");
        let clone = dir.path().join("clone.git");

        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(&source, ["config", "user.email", "refresh-test@crab.local"]);
        git(&source, ["config", "user.name", "refresh test"]);
        std::fs::write(source.join("file.txt"), "base").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "base"]);
        git_in(
            dir.path(),
            ["clone", "--bare", source.to_str().unwrap(), "remote.git"],
        );
        git_in(
            dir.path(),
            [
                "clone",
                "--bare",
                "--filter=blob:none",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );

        let fetcher = GitRemoteRefFetcher::with_remote(clone, "origin");
        let missing = fetcher.fetch_ref("missing").unwrap();

        assert!(missing.is_none());
    }

    #[test]
    fn commit_time_from_oid_reads_commit_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");

        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(&source, ["config", "user.email", "refresh-test@crab.local"]);
        git(&source, ["config", "user.name", "refresh test"]);
        std::fs::write(source.join("file.txt"), "base").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "base"]);
        let head = git_stdout(&source, ["rev-parse", "HEAD"]);
        let expected = git_stdout(&source, ["log", "-1", "--format=%ct", "HEAD"])
            .parse::<i64>()
            .unwrap();

        assert_eq!(
            commit_time_from_oid(&source.join(".git"), &head),
            Some(expected)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_service_remote_poll_fetches_and_swaps_generation() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let remote = dir.path().join("remote.git");
        let clone = dir.path().join("clone.git");
        let snapshot_path = dir.path().join("snapshot.sqlite");
        let overlay_db = dir.path().join("overlay.sqlite");
        let overlay_upper = dir.path().join("upper");

        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(&source, ["config", "user.email", "refresh-test@crab.local"]);
        git(&source, ["config", "user.name", "refresh test"]);
        std::fs::write(source.join("file.txt"), "base").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "base"]);
        git_in(
            dir.path(),
            ["clone", "--bare", source.to_str().unwrap(), "remote.git"],
        );
        git_in(
            dir.path(),
            [
                "clone",
                "--bare",
                "--filter=blob:none",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(&clone, ["config", "user.email", "refresh-test@crab.local"]);
        git(&clone, ["config", "user.name", "refresh test"]);

        let old_head = git_stdout(&clone, ["rev-parse", "refs/heads/main"]);
        let snapshot = Arc::new(SnapshotStore::open_or_create(&snapshot_path).unwrap());
        let old_nodes = crate::snapshot::build_snapshot(&clone, &old_head).unwrap();
        snapshot
            .publish_generation(&old_head, "refs/heads/main", &old_nodes)
            .unwrap();
        let old_gen = snapshot.current_generation().unwrap().unwrap();
        let overlay = Arc::new(OverlayStore::open(&overlay_db, &overlay_upper).unwrap());
        let resolver = Arc::new(
            crate::resolver::FuseResolver::new(
                Arc::clone(&snapshot),
                Some(Arc::clone(&overlay) as Arc<dyn OverlayLookup>),
                old_gen,
                commit_time_from_oid(&clone, &old_head).unwrap_or(0),
                None,
                Some("refs/heads/main".into()),
            )
            .unwrap(),
        );

        std::fs::write(source.join("file.txt"), "updated").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "update"]);
        git(&source, ["push", remote.to_str().unwrap(), "main"]);
        let new_head = git_stdout(&source, ["rev-parse", "HEAD"]);
        assert_ne!(old_head, new_head);

        let hydration = crate::pipeline::create_hydration(
            Arc::new(crate::ChunkCache::open(dir.path().join("chunks"), None).unwrap()),
            Arc::new(crate::verified_set::VerifiedSet::default()),
            CancellationToken::new(),
            None,
            None,
            None,
        )
        .unwrap();
        let engine = Arc::new(crate::engine::VfsEngine::new(
            Arc::clone(&resolver),
            None,
            Arc::clone(&hydration),
            None,
            Some(Arc::clone(&snapshot)),
        ));
        let output = PipelineOutput {
            resolver: Arc::clone(&resolver),
            engine,
            hydration,
            snapshot: Arc::clone(&snapshot),
            overlay: Some(Arc::clone(&overlay)),
            head_oid: old_head.clone(),
            head_ref: "refs/heads/main".into(),
            generation: old_gen,
            read_context: None,
        };
        let selection = PipelineConfig {
            source: remote.display().to_string(),
            git_dir: clone.clone(),
            git_metadata: crate::pipeline::GitMetadataOwnership::Mount,
            ref_name: Some("refs/heads/main".into()),
            read_only: false,
            cache_dir: dir.path().join("cache"),
            cancel_token: CancellationToken::new(),
        };
        let service = RefreshService::new(
            &output,
            &dir.path().join("mount"),
            Arc::clone(&overlay),
            Arc::new(GitRemoteRefFetcher::new(clone.clone())),
            RefreshConfig {
                git_dir: clone.clone(),
                ..RefreshConfig::default()
            },
            selection,
            CancellationToken::new(),
        );
        let watcher = LocalHeadWatcher::new(clone.clone(), None);

        let obsolete = resolver.capture_view();
        service.poll_remote(&watcher).await;
        service
            .refresh_to_oid(&old_head, "refs/heads/main", obsolete)
            .await
            .unwrap();

        assert_eq!(
            snapshot.head_oid().unwrap().as_deref(),
            Some(new_head.as_str())
        );
        assert_eq!(resolver.generation(), old_gen + 1);
        assert_eq!(
            resolver.commit_time(),
            commit_time_from_oid(&clone, &new_head).unwrap()
        );
        assert_eq!(
            git_stdout(&clone, ["rev-parse", "refs/heads/main"]),
            new_head
        );

        let local_commit = git_stdout(
            &clone,
            [
                "commit-tree",
                "refs/heads/main^{tree}",
                "-p",
                &new_head,
                "-m",
                "local daemon commit",
            ],
        );
        git(
            &clone,
            ["update-ref", "refs/heads/main", &local_commit, &new_head],
        );
        service
            .refresh_to_oid(&local_commit, "refs/heads/main", resolver.capture_view())
            .await
            .unwrap();

        service.poll_remote(&watcher).await;

        assert_eq!(
            snapshot.head_oid().unwrap().as_deref(),
            Some(local_commit.as_str())
        );
        assert_eq!(
            git_stdout(&clone, ["rev-parse", "refs/heads/main"]),
            local_commit
        );
    }

    // --- diff_generations tests ---

    fn make_file_node(path: &str, size: u64) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::File,
            mode: 0o100_644,
            object_oid: Some("abcd1234".to_owned()),
            pointer: None,
            size,
        }
    }

    fn temp_snapshot() -> (tempfile::TempDir, SnapshotStore) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("snapshot.sqlite");
        let store = SnapshotStore::open_or_create(&db_path).unwrap();
        (dir, store)
    }

    fn git<I, S>(repo: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_in<I, S>(cwd: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout<I, S>(repo: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn diff_detects_added_file() {
        let (_dir, store) = temp_snapshot();

        let gen1_nodes = vec![make_file_node("a.txt", 100)];
        store
            .publish_generation("oid1", "refs/heads/main", &gen1_nodes)
            .unwrap();

        let gen2_nodes = vec![make_file_node("a.txt", 100), make_file_node("b.txt", 200)];
        store
            .publish_generation("oid2", "refs/heads/main", &gen2_nodes)
            .unwrap();

        let changed = diff_generations(&store, 1, 2).unwrap();
        assert!(changed.contains("b.txt"));
        assert!(!changed.contains("a.txt"));
    }

    #[test]
    fn diff_detects_removed_file() {
        let (_dir, store) = temp_snapshot();

        let gen1_nodes = vec![make_file_node("a.txt", 100), make_file_node("b.txt", 200)];
        store
            .publish_generation("oid1", "refs/heads/main", &gen1_nodes)
            .unwrap();

        let gen2_nodes = vec![make_file_node("a.txt", 100)];
        store
            .publish_generation("oid2", "refs/heads/main", &gen2_nodes)
            .unwrap();

        let changed = diff_generations(&store, 1, 2).unwrap();
        assert!(changed.contains("b.txt"));
        assert!(!changed.contains("a.txt"));
    }

    #[test]
    fn diff_detects_modified_file() {
        let (_dir, store) = temp_snapshot();

        let gen1_nodes = vec![make_file_node("a.txt", 100)];
        store
            .publish_generation("oid1", "refs/heads/main", &gen1_nodes)
            .unwrap();

        let mut modified = make_file_node("a.txt", 200);
        modified.object_oid = Some("different_oid".to_owned());
        let gen2_nodes = vec![modified];
        store
            .publish_generation("oid2", "refs/heads/main", &gen2_nodes)
            .unwrap();

        let changed = diff_generations(&store, 1, 2).unwrap();
        assert!(changed.contains("a.txt"));
    }

    #[test]
    fn diff_empty_when_identical() {
        let (_dir, store) = temp_snapshot();

        let nodes = vec![make_file_node("a.txt", 100)];
        store
            .publish_generation("oid1", "refs/heads/main", &nodes)
            .unwrap();
        store
            .publish_generation("oid2", "refs/heads/main", &nodes)
            .unwrap();

        let changed = diff_generations(&store, 1, 2).unwrap();
        assert!(changed.is_empty());
    }

    // --- conflict detection tests ---

    #[test]
    fn conflict_detection_finds_overlapping_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");
        let overlay = Arc::new(OverlayStore::open(&db_path, &upper_dir).unwrap());

        // Create an overlay entry for "a.txt".
        use crate::engine::OverlayWriter;
        overlay.create_file("a.txt", 0o100_644).unwrap();

        let mut changed = HashSet::new();
        changed.insert("a.txt".to_owned());
        changed.insert("b.txt".to_owned());

        let conflicts = detect_conflicts(&changed, &overlay);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts.contains(&"a.txt".to_owned()));
    }

    #[test]
    fn conflict_detection_ignores_deleted_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlay.db");
        let upper_dir = dir.path().join("upper");
        let overlay = Arc::new(OverlayStore::open(&db_path, &upper_dir).unwrap());

        use crate::engine::OverlayWriter;
        overlay.create_file("a.txt", 0o100_644).unwrap();
        overlay.remove("a.txt").unwrap();

        let mut changed = HashSet::new();
        changed.insert("a.txt".to_owned());

        let conflicts = detect_conflicts(&changed, &overlay);
        assert!(conflicts.is_empty());
    }

    // --- BackoffState tests ---

    #[test]
    fn backoff_doubles_on_failure() {
        let base = Duration::from_secs(30);
        let max = Duration::from_mins(10);
        let mut backoff = BackoffState::new(base, max);

        assert_eq!(backoff.current_interval, base);

        backoff.on_failure("network error");
        assert_eq!(backoff.current_interval, Duration::from_mins(1));
        assert_eq!(backoff.last_fetch_result.as_deref(), Some("network error"));

        backoff.on_failure("timeout");
        assert_eq!(backoff.current_interval, Duration::from_mins(2));
    }

    #[test]
    fn backoff_caps_at_max() {
        let base = Duration::from_secs(30);
        let max = Duration::from_mins(10);
        let mut backoff = BackoffState::new(base, max);

        // Drive past the cap.
        for _ in 0..20 {
            backoff.on_failure("err");
        }
        assert_eq!(backoff.current_interval, max);
    }

    #[test]
    fn backoff_resets_on_success() {
        let base = Duration::from_secs(30);
        let max = Duration::from_mins(10);
        let mut backoff = BackoffState::new(base, max);

        backoff.on_failure("err");
        backoff.on_failure("err");
        assert!(backoff.current_interval > base);

        backoff.on_success();
        assert_eq!(backoff.current_interval, base);
        assert_eq!(backoff.last_fetch_result.as_deref(), Some("ok"));
    }

    #[test]
    fn backoff_independent_per_instance() {
        let base = Duration::from_secs(30);
        let max = Duration::from_mins(10);
        let mut a = BackoffState::new(base, max);
        let mut b = BackoffState::new(base, max);

        a.on_failure("err");
        assert_eq!(a.current_interval, Duration::from_mins(1));
        assert_eq!(b.current_interval, base);

        b.on_success();
        assert_eq!(b.current_interval, base);
        assert_eq!(a.current_interval, Duration::from_mins(1));
    }

    // --- redact_url tests ---

    #[test]
    fn redact_url_strips_user_pass() {
        let url = "https://user:pass@github.com/org/repo.git";
        let redacted = redact_url(url);
        assert!(redacted.contains("***"));
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("pass"));
        assert!(redacted.contains("github.com"));
        assert!(redacted.contains("/org/repo.git"));
        assert!(redacted.starts_with("https://"));
    }

    #[test]
    fn redact_url_strips_token() {
        let url = "https://ghp_secret_token@github.com/org/repo.git";
        let redacted = redact_url(url);
        assert!(!redacted.contains("ghp_secret_token"));
        assert!(redacted.contains("***"));
        assert!(redacted.contains("github.com"));
    }

    #[test]
    fn redact_url_preserves_no_creds() {
        let url = "https://github.com/org/repo.git";
        let redacted = redact_url(url);
        assert_eq!(redacted, "https://github.com/org/repo.git");
    }

    #[test]
    fn redact_url_handles_malformed() {
        let url = "not-a-url";
        let redacted = redact_url(url);
        assert_eq!(redacted, "not-a-url");
    }

    #[test]
    fn redact_url_preserves_path_and_scheme() {
        let url = "https://token@host.example.com:8443/deep/path/repo.git";
        let redacted = redact_url(url);
        assert!(redacted.contains("host.example.com"));
        assert!(redacted.contains("/deep/path/repo.git"));
        assert!(redacted.starts_with("https://"));
    }
}
