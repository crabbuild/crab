//! File watcher for `crab run --watch`.
//!
//! Monitors declared dep paths for modifications using the `notify`
//! crate (FSEvents on macOS, inotify on Linux). Events are debounced
//! with a 200ms window — the timer resets on each new event. After
//! the window closes, the set of changed paths is returned so the
//! caller can recompute staleness and re-execute affected stages.
//!
//! Editor temp files (`.swp`, `~`, `#`, `.tmp`) are filtered out to
//! avoid spurious re-executions.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{Result, WorkflowError as CrabError};

/// Debounce window: events within this duration are coalesced.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(200);

/// Patterns that identify editor temp files. Any path whose file name
/// matches one of these heuristics is silently ignored.
pub fn is_editor_temp_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let extension = path.extension().and_then(|ext| ext.to_str());

    // Vim swap files: .filename.swp, .filename.swo, etc.
    if extension.is_some_and(|ext| {
        ext.eq_ignore_ascii_case("swp")
            || ext.eq_ignore_ascii_case("swo")
            || ext.eq_ignore_ascii_case("swn")
    }) {
        return true;
    }

    // Emacs backup/auto-save: filename~, #filename#
    if name.ends_with('~') {
        return true;
    }
    if name.starts_with('#') && name.ends_with('#') {
        return true;
    }

    // Generic temp files
    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("tmp")) {
        return true;
    }

    // Vim undo files
    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("un~")) {
        return true;
    }

    // Kate/KDE temp files
    if name.starts_with(".kate-swp") {
        return true;
    }

    // macOS metadata
    if name == ".DS_Store" {
        return true;
    }

    false
}

/// A running file watcher that monitors dep paths and yields batches
/// of changed paths after debouncing.
pub struct DepWatcher {
    /// Channel receiving raw file events from the notify watcher.
    event_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    /// The underlying watcher handle — kept alive for the duration.
    _watcher: RecommendedWatcher,
    /// The set of dep paths being watched (for filtering).
    watched_paths: BTreeSet<PathBuf>,
}

impl DepWatcher {
    /// Start watching the given dep paths.
    ///
    /// Each path is watched non-recursively (file-level). If a path
    /// is a directory, it's watched recursively so changes to files
    /// inside it are detected.
    pub fn start(dep_paths: &[PathBuf], repo_root: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();

        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| CrabError::Io(std::io::Error::other(e)))?;

        let mut watched_paths = BTreeSet::new();

        for dep_path in dep_paths {
            let abs_path = if dep_path.is_absolute() {
                dep_path.clone()
            } else {
                repo_root.join(dep_path)
            };

            // Canonicalize to handle symlinks (e.g., /tmp → /private/tmp on macOS).
            let canonical = abs_path.canonicalize().unwrap_or_else(|_| abs_path.clone());

            // Watch the parent directory for file deps (notify needs
            // the directory to exist). For directory deps, watch
            // recursively.
            if canonical.is_dir() {
                if let Err(e) = watcher.watch(&canonical, RecursiveMode::Recursive) {
                    debug!(path = %canonical.display(), error = %e, "watch: failed to watch directory");
                }
            } else {
                // Watch the parent directory non-recursively.
                if let Some(parent) = canonical.parent()
                    && parent.exists()
                    && let Err(e) = watcher.watch(parent, RecursiveMode::NonRecursive)
                {
                    debug!(path = %parent.display(), error = %e, "watch: failed to watch parent");
                }
            }

            watched_paths.insert(canonical);
        }

        Ok(Self {
            event_rx: rx,
            _watcher: watcher,
            watched_paths,
        })
    }

    fn collect_event_paths(&self, event: notify::Result<Event>, changed: &mut BTreeSet<PathBuf>) {
        if let Ok(ev) = event
            && Self::is_relevant_event(&ev)
        {
            for path in &ev.paths {
                if self.matches_dep(path) && !is_editor_temp_file(path) {
                    changed.insert(path.clone());
                }
            }
        }
    }

    /// Wait for the next batch of changed dep paths.
    ///
    /// Blocks until at least one relevant file event arrives, then
    /// debounces for [`DEBOUNCE_DURATION`] (resetting on each new
    /// event). Returns the set of changed paths that match declared
    /// deps.
    ///
    /// Returns `None` if the watcher channel is closed (e.g., on
    /// shutdown).
    pub async fn next_batch(&mut self) -> Option<BTreeSet<PathBuf>> {
        // Wait for the first event.
        let mut changed = BTreeSet::new();

        loop {
            let event = self.event_rx.recv().await?;
            self.collect_event_paths(event, &mut changed);

            if !changed.is_empty() {
                break;
            }
        }

        // Debounce: keep collecting events for DEBOUNCE_DURATION,
        // resetting the timer on each new event.
        loop {
            match tokio::time::timeout(DEBOUNCE_DURATION, self.event_rx.recv()).await {
                Ok(Some(event)) => {
                    self.collect_event_paths(event, &mut changed);
                    // Timer resets implicitly by looping back to timeout.
                }
                Ok(None) => {
                    // Channel closed.
                    return if changed.is_empty() {
                        None
                    } else {
                        Some(changed)
                    };
                }
                Err(_) => {
                    // Timeout expired — debounce window closed.
                    break;
                }
            }
        }

        Some(changed)
    }

    /// Check if a notify event is relevant (create, modify, remove).
    fn is_relevant_event(event: &Event) -> bool {
        matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        )
    }

    /// Check if a changed path matches one of the declared dep paths.
    ///
    /// A path matches if it equals a watched path exactly, or if it's
    /// a descendant of a watched directory.
    fn matches_dep(&self, path: &Path) -> bool {
        // Canonicalize the event path to handle symlinks (e.g.,
        // /tmp → /private/tmp on macOS).
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        for watched in &self.watched_paths {
            if canonical == *watched {
                return true;
            }
            // If the watched path is a directory, any descendant matches.
            if canonical.starts_with(watched) {
                return true;
            }
        }
        false
    }
}

/// Collect all file-system dep paths from a set of stages.
///
/// Only `Dep::Path` variants are returned — remote deps (git refs,
/// URLs, OCI images) are not watchable.
pub fn collect_dep_paths(
    stages: &std::collections::BTreeMap<crate::stage::StageName, crate::stage::Stage>,
) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for stage in stages.values() {
        for dep in &stage.deps {
            if let crate::stage::Dep::Path(p) = dep {
                paths.insert(p.clone());
            }
        }
    }
    paths.into_iter().collect()
}

/// Collect dep paths for a specific stage and its transitive
/// producers (upstream stages whose outputs feed into this stage).
pub fn collect_transitive_dep_paths(
    target: &crate::stage::StageName,
    stages: &std::collections::BTreeMap<crate::stage::StageName, crate::stage::Stage>,
    graph: &crate::Graph,
) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(target.clone());

    while let Some(current) = queue.pop_front() {
        if !visited.insert(current.clone()) {
            continue;
        }
        if let Some(stage) = stages.get(&current) {
            for dep in &stage.deps {
                if let crate::stage::Dep::Path(p) = dep {
                    paths.insert(p.clone());
                }
            }
        }
        // Walk upstream producers.
        for producer in graph.producers_of(&current) {
            queue.push_back(producer);
        }
    }

    paths.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_temp_files_are_detected() {
        assert!(is_editor_temp_file(Path::new("src/.main.rs.swp")));
        assert!(is_editor_temp_file(Path::new("data/file.csv~")));
        assert!(is_editor_temp_file(Path::new("#autosave#")));
        assert!(is_editor_temp_file(Path::new("output.tmp")));
        assert!(is_editor_temp_file(Path::new(".DS_Store")));
    }

    #[test]
    fn normal_files_are_not_filtered() {
        assert!(!is_editor_temp_file(Path::new("data.csv")));
        assert!(!is_editor_temp_file(Path::new("model.pkl")));
        assert!(!is_editor_temp_file(Path::new("src/main.rs")));
        assert!(!is_editor_temp_file(Path::new("train.py")));
    }
}
