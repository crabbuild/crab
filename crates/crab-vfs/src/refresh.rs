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
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result};
use crate::overlay::OverlayStore;
use crate::resolver::{FuseResolver, OverlayLookup};
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

/// Strip credentials from a URL before logging.
///
/// Replaces the username and password with `***`, preserving the host,
/// path, scheme, and query. Returns the original string unchanged if
/// parsing fails.
pub fn redact_url(url_str: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url_str) {
        if !parsed.username().is_empty() || parsed.password().is_some() {
            let _ = parsed.set_username("***");
            let _ = parsed.set_password(None);
        }
        parsed.to_string()
    } else {
        url_str.to_owned()
    }
}

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
    /// Specific ref to track (e.g. `refs/heads/feature`).
    /// If `None`, tracks whatever HEAD points to.
    pub tracked_ref: Option<String>,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            remote_poll_interval: Duration::from_secs(30),
            local_poll_interval: Duration::from_millis(500),
            git_dir: PathBuf::from(".git"),
            tracked_ref: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Remote ref fetcher trait
// ---------------------------------------------------------------------------

/// Trait for fetching remote refs.
///
/// The real implementation issues `ls-remote` or reads from the remote
/// store. Tests provide a mock.
pub trait RemoteRefFetcher: Send + Sync {
    /// Fetch the OID of the given ref from the remote.
    ///
    /// Returns `None` if the ref does not exist on the remote.
    fn fetch_ref_oid(&self, ref_name: &str) -> Result<Option<String>>;
}

/// Remote ref fetcher used when remote polling is not applicable.
pub struct NoopRemoteRefFetcher;

impl RemoteRefFetcher for NoopRemoteRefFetcher {
    fn fetch_ref_oid(&self, _ref_name: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Git-backed remote ref fetcher for blobless mount clones.
///
/// Fetches the tracked ref into the local git directory before returning its
/// OID so the refresh service can build a new snapshot from locally available
/// objects.
pub struct GitRemoteRefFetcher {
    git_dir: PathBuf,
    remote: String,
}

impl GitRemoteRefFetcher {
    pub fn new(git_dir: PathBuf) -> Self {
        Self {
            git_dir,
            remote: "origin".to_owned(),
        }
    }

    #[cfg(test)]
    fn with_remote(git_dir: PathBuf, remote: impl Into<String>) -> Self {
        Self {
            git_dir,
            remote: remote.into(),
        }
    }
}

impl RemoteRefFetcher for GitRemoteRefFetcher {
    fn fetch_ref_oid(&self, ref_name: &str) -> Result<Option<String>> {
        let ref_name = normalized_fetch_ref(ref_name)?;
        if ref_name == "HEAD" {
            self.fetch_head()
        } else {
            self.fetch_named_ref(&ref_name)
        }
    }
}

impl GitRemoteRefFetcher {
    fn fetch_named_ref(&self, ref_name: &str) -> Result<Option<String>> {
        let tracking_ref = remote_tracking_ref(&self.remote, ref_name);
        let refspec = format!("+{ref_name}:{tracking_ref}");
        let output = Command::new("git")
            .arg("fetch")
            .arg("--filter=blob:none")
            .arg(&self.remote)
            .arg(&refspec)
            .env("GIT_DIR", &self.git_dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .map_err(CrabError::Io)?;

        if !output.status.success() {
            return git_fetch_error(&output, ref_name);
        }

        resolve_git_ref(&self.git_dir, &tracking_ref)
    }

    fn fetch_head(&self) -> Result<Option<String>> {
        let output = Command::new("git")
            .arg("fetch")
            .arg("--filter=blob:none")
            .arg(&self.remote)
            .arg("HEAD")
            .env("GIT_DIR", &self.git_dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .map_err(CrabError::Io)?;

        if !output.status.success() {
            return git_fetch_error(&output, "HEAD");
        }

        resolve_git_ref(&self.git_dir, "FETCH_HEAD")
    }
}

fn remote_tracking_ref(remote: &str, ref_name: &str) -> String {
    if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
        return format!("refs/remotes/{remote}/{branch}");
    }
    let suffix = ref_name.strip_prefix("refs/").unwrap_or(ref_name);
    format!("refs/crab/remotes/{remote}/{suffix}")
}

pub fn normalized_fetch_ref(ref_name: &str) -> Result<String> {
    let trimmed = ref_name.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: "remote ref cannot be empty".into(),
            origin: "mount refresh".into(),
        });
    }
    if trimmed == "HEAD" || trimmed.starts_with("refs/") {
        return Ok(trimmed.to_owned());
    }
    Ok(format!("refs/heads/{trimmed}"))
}

fn git_fetch_error(output: &std::process::Output, ref_name: &str) -> Result<Option<String>> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if remote_ref_missing(&stderr) {
        return Ok(None);
    }
    let redacted = redact_url(stderr.trim());
    Err(CrabError::Internal(format!(
        "git fetch failed for {ref_name}: {redacted}"
    )))
}

fn remote_ref_missing(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("couldn't find remote ref") || lower.contains("could not find remote ref")
}

fn resolve_git_ref(git_dir: &Path, ref_name: &str) -> Result<Option<String>> {
    let rev = format!("{ref_name}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify"])
        .arg(&rev)
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(CrabError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Needed a single revision") || stderr.contains("unknown revision") {
            return Ok(None);
        }
        let redacted = redact_url(stderr.trim());
        return Err(CrabError::Internal(format!(
            "failed to resolve fetched ref {ref_name}: {redacted}"
        )));
    }

    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if oid.is_empty() {
        return Err(CrabError::Internal(format!(
            "fetched ref {ref_name} resolved to an empty OID"
        )));
    }
    Ok(Some(oid))
}

pub fn commit_time_from_oid(git_dir: &Path, oid: &str) -> Option<i64> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct", oid])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<i64>()
        .ok()
}

// ---------------------------------------------------------------------------
// Local HEAD watcher
// ---------------------------------------------------------------------------

/// Polls `.git/HEAD` and the ref file it points to for changes.
///
/// Mirrors artifact-fs's `watcher.Poller`: reads `.git/HEAD` to find the
/// symbolic ref, then polls that ref file for mtime changes. Detects
/// both branch switches (`git checkout`) and branch advances (`git pull`
/// in another terminal).
struct LocalHeadWatcher {
    git_dir: PathBuf,
    /// Last known content of `.git/HEAD`.
    last_head_content: Option<String>,
    /// Last known content of the ref file HEAD points to.
    last_ref_content: Option<String>,
    /// Path to the ref file HEAD currently points to.
    current_ref_path: Option<PathBuf>,
}

impl LocalHeadWatcher {
    fn new(git_dir: PathBuf) -> Self {
        Self {
            git_dir,
            last_head_content: None,
            last_ref_content: None,
            current_ref_path: None,
        }
    }

    /// Check if HEAD or the current ref has changed since the last poll.
    ///
    /// Returns `true` if a change was detected (branch switch or advance).
    fn poll(&mut self) -> bool {
        let head_path = self.git_dir.join("HEAD");
        let head_content = match std::fs::read_to_string(&head_path) {
            Ok(c) => c.trim().to_owned(),
            Err(e) => {
                debug!(error = %e, "failed to read .git/HEAD");
                return false;
            }
        };

        // Detect HEAD content change (branch switch or detached HEAD change).
        let head_changed = self
            .last_head_content
            .as_ref()
            .is_some_and(|prev| *prev != head_content);

        self.last_head_content = Some(head_content.clone());

        if head_changed {
            // HEAD switched — prime the new ref path.
            self.prime_ref_from_head(&head_content);
            return true;
        }

        // If this is the first poll, prime without reporting a change.
        if self.current_ref_path.is_none() {
            self.prime_ref_from_head(&head_content);
            return false;
        }

        // Check if the ref file HEAD points to has changed (branch advance).
        self.ref_content_changed()
    }

    /// Parse HEAD content and set up tracking for the ref file.
    fn prime_ref_from_head(&mut self, head_content: &str) {
        if let Some(ref_name) = head_content.strip_prefix("ref: ") {
            let ref_name = ref_name.trim();
            if !ref_name.is_empty() {
                let ref_path = self.git_dir.join(ref_name);
                // Read current content as baseline.
                self.last_ref_content = std::fs::read_to_string(&ref_path)
                    .ok()
                    .map(|c| c.trim().to_owned());
                self.current_ref_path = Some(ref_path);
                return;
            }
        }
        // Detached HEAD — no ref file to watch.
        self.current_ref_path = None;
        self.last_ref_content = None;
    }

    /// Check if the ref file content has changed.
    fn ref_content_changed(&mut self) -> bool {
        let Some(ref_path) = &self.current_ref_path else {
            return false;
        };

        let content = match std::fs::read_to_string(ref_path) {
            Ok(c) => c.trim().to_owned(),
            Err(_) => return false,
        };

        let changed = self
            .last_ref_content
            .as_ref()
            .is_some_and(|prev| *prev != content);

        self.last_ref_content = Some(content);
        changed
    }

    /// Return the ref name HEAD currently points to (e.g. `refs/heads/main`).
    fn current_ref_name(&self) -> Option<String> {
        let head_content = self.last_head_content.as_ref()?;
        let ref_name = head_content.strip_prefix("ref: ")?;
        let ref_name = ref_name.trim();
        if ref_name.is_empty() {
            None
        } else {
            Some(ref_name.to_owned())
        }
    }

    /// Return the OID the current ref points to.
    fn current_ref_oid(&self) -> Option<String> {
        self.last_ref_content.clone()
    }
}

// ---------------------------------------------------------------------------
// Git read-tree HEAD
// ---------------------------------------------------------------------------

/// Runs `git read-tree HEAD` to update the git index after a snapshot change.
///
/// Without this, `git status` inside the mount shows phantom diffs because
/// the index is stale relative to the new HEAD tree. Failures are logged
/// but not propagated — the mount remains functional even if read-tree fails.
pub fn run_read_tree_head(git_dir: &Path) {
    // SHELLOUT: `git read-tree HEAD` refreshes the index from
    // HEAD's tree after a VFS snapshot swap. The gitoxide
    // equivalent would need checkout-writer integration through the
    // ODB adapter. Keep the shellout here until that path owns index
    // refreshes without requiring a worktree checkout.
    let output = match std::process::Command::new("git")
        .args(["read-tree", "HEAD"])
        .env("GIT_DIR", git_dir)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            warn!(
                git_dir = %git_dir.display(),
                error = %e,
                "failed to spawn git read-tree HEAD"
            );
            return;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            git_dir = %git_dir.display(),
            stderr = %stderr.trim(),
            "git read-tree HEAD failed"
        );
    }
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

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
    resolver: Arc<FuseResolver>,
    snapshot: Arc<SnapshotStore>,
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
        resolver: Arc<FuseResolver>,
        snapshot: Arc<SnapshotStore>,
        overlay: Arc<OverlayStore>,
        remote_fetcher: Arc<dyn RemoteRefFetcher>,
        config: RefreshConfig,
        cancel: CancellationToken,
    ) -> Self {
        let backoff = BackoffState::new(config.remote_poll_interval, Duration::from_mins(10));
        Self {
            resolver,
            snapshot,
            overlay,
            remote_fetcher,
            config,
            cancel,
            backoff: StdMutex::new(backoff),
        }
    }

    /// Run the refresh loop until cancellation.
    ///
    /// Spawns two polling loops:
    /// - Remote ref polling at `remote_poll_interval` (with backoff on failure)
    /// - Local `.git/HEAD` polling at `local_poll_interval`
    ///
    /// Either trigger rebuilds the snapshot and swaps the generation.
    pub async fn run(&self) {
        let mut local_watcher = LocalHeadWatcher::new(self.config.git_dir.clone());
        let mut remote_ticker = tokio::time::interval(self.config.remote_poll_interval);
        let mut local_ticker = tokio::time::interval(self.config.local_poll_interval);

        // Skip the first immediate tick.
        remote_ticker.tick().await;
        local_ticker.tick().await;

        info!(
            remote_interval = ?self.config.remote_poll_interval,
            local_interval = ?self.config.local_poll_interval,
            tracked_ref = ?self.config.tracked_ref,
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
                    if local_watcher.poll() {
                        info!("local HEAD change detected");
                        self.handle_local_change(&local_watcher);
                    }
                }
            }
        }
    }

    /// Poll the remote for ref changes and refresh if needed.
    async fn poll_remote(&self, local_watcher: &LocalHeadWatcher) {
        let ref_name = self.effective_ref_name(local_watcher);
        let Some(ref_name) = ref_name else {
            debug!("no ref to track for remote polling");
            return;
        };

        let fetcher = Arc::clone(&self.remote_fetcher);
        let ref_name_for_fetch = ref_name.clone();
        let fetch_result =
            tokio::task::spawn_blocking(move || fetcher.fetch_ref_oid(&ref_name_for_fetch)).await;

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
        let current_oid = self.snapshot.head_oid().ok().flatten();
        if current_oid.as_deref() == Some(selected_oid.as_str()) {
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

        match self.refresh_to_oid(&selected_oid, &ref_name) {
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
    fn handle_local_change(&self, local_watcher: &LocalHeadWatcher) {
        let ref_name = self.effective_ref_name(local_watcher);
        let new_oid = local_watcher.current_ref_oid();

        let Some(new_oid) = new_oid else {
            debug!("no OID available after local HEAD change");
            return;
        };

        let current_oid = self.snapshot.head_oid().ok().flatten();
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

        if let Err(e) = self.refresh_to_oid(&new_oid, &ref_name) {
            warn!(error = %e, "refresh to local OID failed");
        }
    }

    /// Determine the effective ref name to track.
    ///
    /// If `--ref=<branch>` was specified, use that. Otherwise, use
    /// whatever HEAD currently points to.
    fn effective_ref_name(&self, local_watcher: &LocalHeadWatcher) -> Option<String> {
        if let Some(ref tracked) = self.config.tracked_ref {
            return Some(tracked.clone());
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
    fn refresh_to_oid(&self, new_oid: &str, ref_name: &str) -> Result<()> {
        let old_gen = self.resolver.generation();

        // Build new snapshot generation.
        self.snapshot
            .publish_generation_from_git(&self.config.git_dir, new_oid, ref_name)?;

        let new_gen = self
            .snapshot
            .current_generation()?
            .ok_or_else(|| CrabError::Internal("no generation after publish".into()))?;
        let commit_time = commit_time_from_oid(&self.config.git_dir, new_oid).unwrap_or(0);

        // Diff old vs new tree to find changed paths.
        let changed_paths = diff_generations(&self.snapshot, old_gen, new_gen)?;

        if changed_paths.is_empty() {
            debug!("no tree changes between generations");
            self.resolver.set_commit_time(commit_time);
            self.resolver.set_generation(new_gen);
            return Ok(());
        }

        // Detect conflicts (paths changed in both base and overlay).
        let _conflicts = detect_conflicts(&changed_paths, &self.overlay);

        // Reconcile overlay: discard stale entries that match the new base.
        let snapshot_ref = &self.snapshot;
        let generation = new_gen;
        self.overlay.reconcile(|path| {
            let node = snapshot_ref.get_node(generation, path).ok().flatten()?;
            Some(crate::overlay::ReconcileBaseInfo {
                is_dir: node.node_type == crate::snapshot::NodeType::Dir,
                object_oid: node.object_oid.clone(),
            })
        })?;

        // Update the git index so `git status` reflects the new HEAD.
        run_read_tree_head(&self.config.git_dir);

        // Atomically swap generation in resolver.
        self.resolver.set_commit_time(commit_time);
        self.resolver.set_generation(new_gen);

        info!(
            old_gen,
            new_gen,
            changed = changed_paths.len(),
            "snapshot refreshed"
        );

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitRelationship {
    Equal,
    LocalAncestor,
    RemoteAncestor,
    Diverged,
}

pub(crate) fn reconcile_fetched_ref(
    git_dir: &Path,
    ref_name: &str,
    remote_oid: &str,
) -> Result<String> {
    if !ref_name.starts_with("refs/heads/") {
        return Ok(remote_oid.to_owned());
    }
    let Some(local_oid) = resolve_git_ref(git_dir, ref_name)? else {
        return Ok(remote_oid.to_owned());
    };
    match commit_relationship(git_dir, &local_oid, remote_oid)? {
        CommitRelationship::Equal => Ok(remote_oid.to_owned()),
        CommitRelationship::LocalAncestor => {
            update_local_ref(git_dir, ref_name, remote_oid, &local_oid)?;
            Ok(remote_oid.to_owned())
        }
        CommitRelationship::RemoteAncestor => {
            warn!(
                ref_name,
                local_oid, remote_oid, "preserving unpushed local daemon commit"
            );
            Ok(local_oid)
        }
        CommitRelationship::Diverged => {
            warn!(
                ref_name,
                local_oid,
                remote_oid,
                "local daemon commit diverged from remote; preserving local snapshot"
            );
            Ok(local_oid)
        }
    }
}

fn commit_relationship(
    git_dir: &Path,
    local_oid: &str,
    remote_oid: &str,
) -> Result<CommitRelationship> {
    if local_oid == remote_oid {
        return Ok(CommitRelationship::Equal);
    }
    if git_is_ancestor(git_dir, local_oid, remote_oid)? {
        return Ok(CommitRelationship::LocalAncestor);
    }
    if git_is_ancestor(git_dir, remote_oid, local_oid)? {
        return Ok(CommitRelationship::RemoteAncestor);
    }
    Ok(CommitRelationship::Diverged)
}

fn git_is_ancestor(git_dir: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(ancestor)
        .arg(descendant)
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .status()
        .map_err(CrabError::Io)?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CrabError::Internal(format!(
            "git merge-base failed with {status}"
        ))),
    }
}

fn update_local_ref(git_dir: &Path, ref_name: &str, new_oid: &str, old_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["update-ref", ref_name, new_oid, old_oid])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "git update-ref failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::snapshot::NodeType;

    // --- LocalHeadWatcher tests ---

    #[test]
    fn watcher_detects_head_change() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();

        // Initial HEAD pointing to main.
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git_dir.join("refs/heads/main"), "aaa111\n").unwrap();

        let mut watcher = LocalHeadWatcher::new(git_dir.clone());

        // First poll primes state, no change reported.
        assert!(!watcher.poll());
        assert_eq!(
            watcher.current_ref_name().as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(watcher.current_ref_oid().as_deref(), Some("aaa111"));

        // No change on second poll.
        assert!(!watcher.poll());

        // Switch to a different branch.
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(git_dir.join("refs/heads/feature"), "bbb222\n").unwrap();

        assert!(watcher.poll());
        assert_eq!(
            watcher.current_ref_name().as_deref(),
            Some("refs/heads/feature")
        );
        assert_eq!(watcher.current_ref_oid().as_deref(), Some("bbb222"));
    }

    #[test]
    fn watcher_detects_branch_advance() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();

        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git_dir.join("refs/heads/main"), "aaa111\n").unwrap();

        let mut watcher = LocalHeadWatcher::new(git_dir.clone());
        assert!(!watcher.poll()); // prime

        // Advance the branch.
        std::fs::write(git_dir.join("refs/heads/main"), "ccc333\n").unwrap();
        assert!(watcher.poll());
        assert_eq!(watcher.current_ref_oid().as_deref(), Some("ccc333"));
    }

    #[test]
    fn watcher_handles_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        // Detached HEAD (raw OID, no "ref:" prefix).
        std::fs::write(git_dir.join("HEAD"), "deadbeef1234\n").unwrap();

        let mut watcher = LocalHeadWatcher::new(git_dir);
        assert!(!watcher.poll());
        assert!(watcher.current_ref_name().is_none());
    }

    // --- remote fetcher tests ---

    #[test]
    fn noop_remote_ref_fetcher_returns_no_ref() {
        let fetcher = NoopRemoteRefFetcher;

        let oid = fetcher.fetch_ref_oid("refs/heads/main").unwrap();

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
        let fetched = fetcher.fetch_ref_oid("main").unwrap().unwrap();

        assert_eq!(fetched, new_head);
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
        let missing = fetcher.fetch_ref_oid("missing").unwrap();

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

        let old_head = git_stdout(&clone, ["rev-parse", "refs/heads/main"]);
        let snapshot = Arc::new(SnapshotStore::open_or_create(&snapshot_path).unwrap());
        let old_nodes = crate::snapshot::build_snapshot(&clone, &old_head).unwrap();
        snapshot
            .publish_generation(&old_head, "refs/heads/main", &old_nodes)
            .unwrap();
        let old_gen = snapshot.current_generation().unwrap().unwrap();
        let overlay = Arc::new(OverlayStore::open(&overlay_db, &overlay_upper).unwrap());
        let resolver = Arc::new(crate::resolver::FuseResolver::new(
            Arc::clone(&snapshot),
            Some(Arc::clone(&overlay) as Arc<dyn OverlayLookup>),
            old_gen,
            commit_time_from_oid(&clone, &old_head).unwrap_or(0),
        ));

        std::fs::write(source.join("file.txt"), "updated").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "update"]);
        git(&source, ["push", remote.to_str().unwrap(), "main"]);
        let new_head = git_stdout(&source, ["rev-parse", "HEAD"]);
        assert_ne!(old_head, new_head);

        let service = RefreshService::new(
            Arc::clone(&resolver),
            Arc::clone(&snapshot),
            Arc::clone(&overlay),
            Arc::new(GitRemoteRefFetcher::new(clone.clone())),
            RefreshConfig {
                git_dir: clone.clone(),
                tracked_ref: Some("refs/heads/main".to_owned()),
                ..RefreshConfig::default()
            },
            CancellationToken::new(),
        );
        let watcher = LocalHeadWatcher::new(clone.clone());

        service.poll_remote(&watcher).await;

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
            .refresh_to_oid(&local_commit, "refs/heads/main")
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
            mode: 0o100644,
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
            .unwrap_or_else(|e| e.into_inner());
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
            .unwrap_or_else(|e| e.into_inner());
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
            .unwrap_or_else(|e| e.into_inner());
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
        overlay.create_file("a.txt", 0o100644).unwrap();

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
        overlay.create_file("a.txt", 0o100644).unwrap();
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
        let max = Duration::from_secs(600);
        let mut backoff = BackoffState::new(base, max);

        assert_eq!(backoff.current_interval, base);

        backoff.on_failure("network error");
        assert_eq!(backoff.current_interval, Duration::from_secs(60));
        assert_eq!(backoff.last_fetch_result.as_deref(), Some("network error"));

        backoff.on_failure("timeout");
        assert_eq!(backoff.current_interval, Duration::from_secs(120));
    }

    #[test]
    fn backoff_caps_at_max() {
        let base = Duration::from_secs(30);
        let max = Duration::from_secs(600);
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
        let max = Duration::from_secs(600);
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
        let max = Duration::from_secs(600);
        let mut a = BackoffState::new(base, max);
        let mut b = BackoffState::new(base, max);

        a.on_failure("err");
        assert_eq!(a.current_interval, Duration::from_secs(60));
        assert_eq!(b.current_interval, base);

        b.on_success();
        assert_eq!(b.current_interval, base);
        assert_eq!(a.current_interval, Duration::from_secs(60));
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
