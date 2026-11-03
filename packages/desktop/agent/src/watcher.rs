//! Filesystem watcher for incremental refresh.
//!
//! One `RepoWatcher` per open repository. Events from the working tree
//! are coalesced with a 200 ms debounce window before dispatching to
//! the indexer. The `.git/` directory is watched separately — ref
//! changes (HEAD, branch tips) are dispatched immediately because they
//! are infrequent and don't require a full reindex.
//!
//! Worktree paths can be dynamically added and removed via
//! [`WatchSet`], which is shared with the worktree manager so that
//! new worktrees are watched immediately and removed worktrees stop
//! generating events.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::indexer::IndexerCommand;

/// Duration of the debounce window for working-tree events.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(200);

// ─── Public types ───────────────────────────────────────────────────

/// Events emitted by the watcher before dispatch to the indexer.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A file in the working tree changed (created, modified, removed).
    FileChanged { path: PathBuf },
    /// A ref was updated (HEAD, branch tip, tag).
    RefUpdated,
    /// Something else inside `.git/` changed.
    GitDirChanged,
}

/// Handle for dynamically adding and removing watched paths.
///
/// Shared between the `RepoWatcher` and the worktree manager so that
/// worktree add/remove operations can update the watch set without
/// stopping and restarting the watcher.
#[derive(Clone)]
pub struct WatchSet {
    inner: Arc<Mutex<RecommendedWatcher>>,
}

impl WatchSet {
    /// Create a new `WatchSet` wrapping the given watcher.
    fn new(watcher: RecommendedWatcher) -> Self {
        Self {
            inner: Arc::new(Mutex::new(watcher)),
        }
    }

    /// Add a path to the watch set (recursive).
    ///
    /// Typically called after a worktree is created so that file
    /// changes in the new worktree trigger indexer refreshes.
    pub async fn watch_path(&self, path: &Path) -> crate::Result<()> {
        let mut watcher = self.inner.lock().await;
        watcher.watch(path, RecursiveMode::Recursive).map_err(|e| {
            crate::AgentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
        })?;
        debug!(path = %path.display(), "added path to watch set");
        Ok(())
    }

    /// Remove a path from the watch set.
    ///
    /// Typically called after a worktree is removed so that stale
    /// events are no longer generated for the deleted directory.
    pub async fn unwatch_path(&self, path: &Path) -> crate::Result<()> {
        let mut watcher = self.inner.lock().await;
        watcher.unwatch(path).map_err(|e| {
            crate::AgentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
        })?;
        debug!(path = %path.display(), "removed path from watch set");
        Ok(())
    }
}

/// Filesystem watcher for a single repository.
///
/// Watches the working tree (debounced) and `.git/` (immediate) and
/// forwards coalesced events to the indexer via its command channel.
pub struct RepoWatcher {
    /// Repo identifier passed through to indexer commands.
    repo_id: String,
    /// Root of the working tree.
    work_dir: PathBuf,
    /// Sender half of the indexer command channel.
    indexer_tx: mpsc::Sender<IndexerCommand>,
    /// Cancellation token — dropping or cancelling stops the watcher.
    cancel: CancellationToken,
    /// Handle to the background debounce task (working tree).
    work_tree_handle: Option<JoinHandle<()>>,
    /// Handle to the background git-dir watch task.
    git_dir_handle: Option<JoinHandle<()>>,
    /// Shared handle for dynamically adding/removing watched paths.
    watch_set: Option<WatchSet>,
}

impl RepoWatcher {
    /// Create a new watcher for the given repository.
    ///
    /// Does not start watching — call [`start`](Self::start) to begin.
    pub fn new(
        repo_id: String,
        work_dir: impl Into<PathBuf>,
        indexer_tx: mpsc::Sender<IndexerCommand>,
    ) -> Self {
        Self {
            repo_id,
            work_dir: work_dir.into(),
            indexer_tx,
            cancel: CancellationToken::new(),
            work_tree_handle: None,
            git_dir_handle: None,
            watch_set: None,
        }
    }

    /// Begin watching the repository's working tree and `.git/` directory.
    ///
    /// Returns an error if the underlying `notify` watcher cannot be
    /// created or the paths cannot be watched.
    pub fn start(&mut self) -> crate::Result<()> {
        let git_dir = self.work_dir.join(".git");

        // --- Working-tree watcher (debounced) ---
        let (wt_tx, wt_rx) = mpsc::channel::<WatchEvent>(256);
        let wt_watcher = create_watcher(wt_tx.clone(), &self.work_dir, false)?;

        // Wrap the watcher in a WatchSet so worktree paths can be
        // dynamically added/removed without restarting.
        let watch_set = WatchSet::new(wt_watcher);
        self.watch_set = Some(watch_set.clone());

        let wt_cancel = self.cancel.clone();
        let wt_indexer_tx = self.indexer_tx.clone();
        let wt_repo_id = self.repo_id.clone();
        let wt_work_dir = self.work_dir.clone();

        self.work_tree_handle = Some(tokio::spawn(debounce_loop(
            wt_rx,
            wt_indexer_tx,
            wt_repo_id,
            wt_work_dir,
            wt_cancel,
            watch_set,
        )));

        // --- .git/ watcher (immediate dispatch) ---
        if git_dir.is_dir() {
            let (git_tx, git_rx) = mpsc::channel::<WatchEvent>(64);
            let git_watcher = create_watcher(git_tx, &git_dir, true)?;

            let git_cancel = self.cancel.clone();
            let git_indexer_tx = self.indexer_tx.clone();
            let git_repo_id = self.repo_id.clone();

            self.git_dir_handle = Some(tokio::spawn(git_dir_loop(
                git_rx,
                git_indexer_tx,
                git_repo_id,
                git_cancel,
                git_watcher,
            )));
        } else {
            warn!(
                work_dir = %self.work_dir.display(),
                "no .git/ directory found; skipping ref watcher"
            );
        }

        info!(
            repo_id = %self.repo_id,
            work_dir = %self.work_dir.display(),
            "watcher started"
        );
        Ok(())
    }

    /// Returns a clone of the [`WatchSet`] handle for dynamic path
    /// management. Available after [`start`](Self::start) has been
    /// called.
    pub fn watch_set(&self) -> Option<WatchSet> {
        self.watch_set.clone()
    }

    /// Stop watching and clean up background tasks.
    pub async fn stop(self) {
        self.cancel.cancel();

        if let Some(h) = self.work_tree_handle {
            let _ = h.await;
        }
        if let Some(h) = self.git_dir_handle {
            let _ = h.await;
        }

        info!(repo_id = %self.repo_id, "watcher stopped");
    }
}

// ─── Internal helpers ───────────────────────────────────────────────

/// Create a `notify` watcher that forwards events into the given
/// channel. If `is_git_dir` is true, events are classified as ref or
/// git-dir changes; otherwise they are `FileChanged`.
fn create_watcher(
    tx: mpsc::Sender<WatchEvent>,
    path: &Path,
    is_git_dir: bool,
) -> crate::Result<RecommendedWatcher> {
    let watch_path = path.to_path_buf();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                let watch_event = classify_event(&event, is_git_dir);
                if let Some(ev) = watch_event {
                    // Best-effort send; if the channel is full we drop
                    // the event — the debounce window will catch the
                    // next one.
                    let _ = tx.try_send(ev);
                }
            }
            Err(e) => {
                warn!(error = %e, "notify watcher error");
            }
        }
    })
    .map_err(|e| crate::AgentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

    watcher
        .watch(path, RecursiveMode::Recursive)
        .map_err(|e| crate::AgentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

    debug!(path = %watch_path.display(), is_git_dir, "notify watcher registered");
    Ok(watcher)
}

/// Classify a raw `notify` event into our domain event type.
fn classify_event(event: &Event, is_git_dir: bool) -> Option<WatchEvent> {
    // Ignore access events (reads) — they don't indicate changes.
    if matches!(event.kind, EventKind::Access(_)) {
        return None;
    }

    if is_git_dir {
        // Check if this is a ref update (HEAD, refs/heads/*, refs/tags/*).
        let is_ref = event.paths.iter().any(|p| {
            let s = p.to_string_lossy();
            s.contains("HEAD")
                || s.contains("refs/heads")
                || s.contains("refs/tags")
                || s.contains("refs/remotes")
        });
        if is_ref {
            Some(WatchEvent::RefUpdated)
        } else {
            Some(WatchEvent::GitDirChanged)
        }
    } else {
        // Working-tree event — pick the first path.
        let path = event.paths.first()?.clone();
        Some(WatchEvent::FileChanged { path })
    }
}

/// Background task that collects working-tree events and dispatches
/// them to the indexer after a 200 ms debounce window.
///
/// The `_watch_set` parameter keeps the `WatchSet` (and its inner
/// `RecommendedWatcher`) alive for the lifetime of this task —
/// dropping it unregisters the OS watch.
async fn debounce_loop(
    mut rx: mpsc::Receiver<WatchEvent>,
    indexer_tx: mpsc::Sender<IndexerCommand>,
    repo_id: String,
    work_dir: PathBuf,
    cancel: CancellationToken,
    _watch_set: WatchSet,
) {
    loop {
        // Wait for the first event or cancellation.
        let first = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            ev = rx.recv() => match ev {
                Some(e) => e,
                None => break,
            },
        };

        debug!(repo_id = %repo_id, event = ?first, "debounce window opened");

        // Drain any additional events that arrive within the debounce
        // window. We don't need to track individual paths — the indexer
        // will do a full incremental scan.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(DEBOUNCE_DURATION) => {},
        }

        // Drain anything that arrived during the sleep.
        while rx.try_recv().is_ok() {}

        // Dispatch a single invalidation to the indexer.
        let cmd = IndexerCommand::WarmRepo {
            repo_id: repo_id.clone(),
            work_dir: work_dir.clone(),
        };
        if let Err(e) = indexer_tx.send(cmd).await {
            error!(error = %e, "watcher: failed to send to indexer");
            break;
        }
        debug!(repo_id = %repo_id, "debounced event dispatched to indexer");
    }

    info!(repo_id = %repo_id, "work-tree debounce loop exiting");
}

/// Background task that watches `.git/` and dispatches ref updates
/// immediately (no debounce — ref changes are infrequent).
///
/// Non-ref `.git/` changes (e.g. index, MERGE_HEAD) are currently
/// logged but not dispatched; they can be wired in later if needed.
async fn git_dir_loop(
    mut rx: mpsc::Receiver<WatchEvent>,
    indexer_tx: mpsc::Sender<IndexerCommand>,
    repo_id: String,
    cancel: CancellationToken,
    _watcher: RecommendedWatcher,
) {
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            ev = rx.recv() => match ev {
                Some(e) => e,
                None => break,
            },
        };

        match &event {
            WatchEvent::RefUpdated => {
                debug!(repo_id = %repo_id, "ref updated — dispatching immediately");
                let cmd = IndexerCommand::Invalidate {
                    repo_id: repo_id.clone(),
                    path: "HEAD".to_string(),
                };
                if let Err(e) = indexer_tx.send(cmd).await {
                    error!(error = %e, "watcher: failed to send ref update to indexer");
                    break;
                }
            }
            WatchEvent::GitDirChanged => {
                debug!(repo_id = %repo_id, "git dir changed (non-ref)");
                // Non-ref git-dir changes are informational for now.
            }
            WatchEvent::FileChanged { .. } => {
                // Should not happen in the git-dir watcher, but handle
                // gracefully.
                debug!(repo_id = %repo_id, "unexpected FileChanged in git-dir watcher");
            }
        }
    }

    info!(repo_id = %repo_id, ".git/ watcher loop exiting");
}
