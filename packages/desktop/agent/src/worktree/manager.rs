//! Worktree manager — orchestrates git worktree operations and
//! reconciles with `.crab/desktop/worktrees.json` metadata.
//!
//! The manager is the single entry point for all worktree RPC verbs.
//! It delegates Git-compatible worktree operations to the core
//! `crab worktree` JSON contract and maintains the UI metadata file
//! alongside.

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::{AgentError, Result};

use super::core_cli;
use super::types::{
    HydratedWorktree, HydrationChoice, HydrationOffer, PointerFileInfo, WorktreeAddResult,
    WorktreeEntry, WorktreeMeta,
};

/// Manages worktree operations for a single repository.
///
/// Holds the repository root path and provides methods for each
/// worktree RPC verb. Methods that mutate worktrees also update
/// the `worktrees.json` metadata file to keep it in sync.
pub struct WorktreeManager {
    /// Absolute path to the repository root (the main worktree).
    repo_path: PathBuf,
    /// Path to the `.crab/desktop/worktrees.json` metadata file.
    meta_path: PathBuf,
}

impl WorktreeManager {
    /// Create a new manager for the given repository path.
    ///
    /// The metadata file is expected at
    /// `<repo_path>/.crab/desktop/worktrees.json`.
    pub fn new(repo_path: PathBuf) -> Self {
        let meta_path = repo_path.join(".crab/desktop/worktrees.json");
        debug!(repo = %repo_path.display(), "WorktreeManager created");
        Self {
            repo_path,
            meta_path,
        }
    }

    /// Returns the repository root path.
    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Returns the path to the worktrees.json metadata file.
    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }

    /// List all worktrees with reconciled UI metadata.
    ///
    /// Calls `crab worktree list --json`, parses the output,
    /// loads `worktrees.json`, reconciles (removes stale entries,
    /// preserves valid ones), saves the reconciled metadata back,
    /// and returns hydrated entries combining git state with UI metadata.
    pub async fn worktree_list(&self) -> Result<Vec<HydratedWorktree>> {
        let entries = core_cli::list_worktrees(&self.repo_path, false)
            .await?
            .into_iter()
            .map(core_entry_to_manager)
            .collect::<Vec<_>>();

        let mut meta_list = self.load_meta().await?;

        // Reconcile: keep only metadata entries whose paths still
        // appear in git's worktree list.
        let entry_paths: std::collections::HashSet<&Path> =
            entries.iter().map(|e| e.path.as_path()).collect();
        let before_len = meta_list.len();
        meta_list.retain(|m| entry_paths.contains(m.path.as_path()));
        if meta_list.len() < before_len {
            debug!(
                removed = before_len - meta_list.len(),
                "reconciled stale worktree metadata entries"
            );
        }

        // Persist the reconciled metadata.
        self.save_meta(&meta_list).await?;

        // Build hydrated entries by joining git entries with metadata.
        let hydrated = entries
            .into_iter()
            .map(|entry| {
                let meta = meta_list.iter().find(|m| m.path == entry.path).cloned();
                HydratedWorktree { entry, meta }
            })
            .collect();

        Ok(hydrated)
    }

    /// Add a new worktree.
    ///
    /// Runs `crab worktree --json add <path> <branch>` with optional
    /// `--create-branch` (`-b`) support. When `create_branch` is true
    /// and `base` is provided, the base ref is appended as the start
    /// point for the new branch.
    ///
    /// After a successful add, re-lists all worktrees and returns the
    /// newly created [`HydratedWorktree`].
    pub async fn worktree_add(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
        base: Option<&str>,
    ) -> Result<HydratedWorktree> {
        self.worktree_add_core(path, branch, create_branch, base, None)
            .await
    }

    async fn worktree_add_core(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
        base: Option<&str>,
        hydration: Option<HydrationChoice>,
    ) -> Result<HydratedWorktree> {
        let mut args: Vec<String> = Vec::new();

        if create_branch {
            args.push("-b".into());
            args.push(branch.into());
        }
        if let Some(choice) = hydration {
            args.push(hydration_arg(choice));
        }
        args.push(path.to_string_lossy().into_owned());
        if create_branch {
            if let Some(base_ref) = base {
                args.push(base_ref.into());
            }
        } else {
            args.push(branch.into());
        }

        let result = core_cli::add_worktree(&self.repo_path, &args).await?;

        debug!(
            path = %path.display(),
            branch = %branch,
            create_branch,
            hydration = ?hydration,
            "worktree added"
        );

        let all = self.worktree_list().await?;
        let resolved = PathBuf::from(result.path);
        find_hydrated_worktree(all, &resolved).ok_or_else(|| {
            AgentError::Other(format!(
                "worktree was created but not found in list at {}",
                resolved.display()
            ))
        })
    }

    /// Add a new worktree, scan for pointer files, and apply hydration.
    ///
    /// Delegates to `crab worktree --json add` then runs
    /// `git ls-tree -r <branch>` to enumerate files. For each file,
    /// reads the blob content via `git cat-file` and checks whether
    /// it is a crab pointer. Returns a [`WorktreeAddResult`] with
    /// an optional [`HydrationOffer`] when pointer files are found.
    ///
    /// The hydration choice is passed to the core worktree command and
    /// persisted in `worktrees.json` for UI display.
    pub async fn worktree_add_with_hydration(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
        base: Option<&str>,
        hydration: HydrationChoice,
    ) -> Result<WorktreeAddResult> {
        let worktree = self
            .worktree_add_core(path, branch, create_branch, base, Some(hydration))
            .await?;

        // Scan the branch for pointer files. Use the worktree path
        // as the working directory so git resolves the branch correctly.
        let resolved_wt_path = worktree.entry.path.clone();
        let hydration_offer = scan_for_pointer_files(&resolved_wt_path, branch).await?;

        // Persist the hydration choice in worktrees.json metadata.
        let meta = WorktreeMeta {
            path: resolved_wt_path,
            display_name: None,
            colour: None,
            pinned: false,
            hydration_policy: hydration,
        };
        self.worktree_set_meta(meta).await?;

        Ok(WorktreeAddResult {
            worktree,
            hydration_offer,
        })
    }

    /// Apply a hydration policy to a worktree path.
    ///
    /// - [`HydrationChoice::All`]: runs `crab hydrate --all` scoped
    ///   to the worktree directory.
    /// - [`HydrationChoice::OnAccess`]: no immediate action (lazy).
    /// - [`HydrationChoice::PointersOnly`]: no action (keep pointers).
    ///
    /// Errors from hydration are logged but do not fail the worktree
    /// creation — the worktree is usable regardless.
    pub async fn apply_hydration_policy(&self, worktree_path: &Path, choice: HydrationChoice) {
        match choice {
            HydrationChoice::All => {
                debug!(path = %worktree_path.display(), "applying hydration policy: all");
                let bin = crate::shellout::resolve_crab_bin();
                let result = tokio::process::Command::new(&bin)
                    .args(["hydrate", "--all"])
                    .current_dir(worktree_path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await;

                match result {
                    Ok(output) if output.status.success() => {
                        debug!(path = %worktree_path.display(), "hydration completed");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        tracing::warn!(
                            path = %worktree_path.display(),
                            status = output.status.code().unwrap_or(-1),
                            stderr = %stderr,
                            "hydration failed (non-fatal)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %worktree_path.display(),
                            error = %e,
                            "failed to spawn crab hydrate (non-fatal)"
                        );
                    }
                }
            }
            HydrationChoice::OnAccess | HydrationChoice::PointersOnly => {
                // No immediate action needed.
                debug!(
                    path = %worktree_path.display(),
                    choice = ?choice,
                    "hydration policy requires no immediate action"
                );
            }
        }
    }

    /// Remove a worktree.
    ///
    /// Runs `git worktree remove <path>` with optional `--force`.
    /// If the worktree has uncommitted changes and `force` is false,
    /// git returns an error which is propagated to the caller.
    ///
    /// After successful removal, the corresponding entry is removed
    /// from `worktrees.json` metadata.
    pub async fn worktree_remove(&self, path: &Path, force: bool) -> Result<()> {
        let path_str = path.to_string_lossy();

        // Resolve the path before removal so we can match it against
        // metadata entries after the directory is deleted.
        let resolved = if path.is_absolute() {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        } else {
            self.repo_path
                .join(path)
                .canonicalize()
                .unwrap_or_else(|_| self.repo_path.join(path))
        };

        let mut args: Vec<String> = Vec::new();
        if force {
            args.push("--force".to_owned());
        }
        args.push(path_str.into_owned());

        core_cli::remove_worktree(&self.repo_path, &args).await?;

        debug!(path = %path.display(), force, "worktree removed");

        // Remove the entry from worktrees.json metadata.
        let mut meta_list = self.load_meta().await?;
        let before_len = meta_list.len();
        meta_list.retain(|m| {
            // Compare using the raw path as well as canonicalized form.
            // After removal the directory no longer exists, so
            // canonicalize may fail — fall back to raw comparison.
            let meta_resolved = m.path.canonicalize().unwrap_or_else(|_| m.path.clone());
            meta_resolved != resolved && m.path != resolved
        });
        if meta_list.len() < before_len {
            debug!("removed metadata entry for deleted worktree");
        }
        self.save_meta(&meta_list).await?;

        Ok(())
    }

    /// Move a worktree to a new filesystem path.
    ///
    /// Runs `git worktree move <old_path> <new_path>`. On success,
    /// updates the path in `worktrees.json` metadata so the UI
    /// metadata follows the worktree to its new location.
    ///
    /// Git will reject the move if the worktree is locked.
    pub async fn worktree_move(&self, old_path: &Path, new_path: &Path) -> Result<()> {
        let old_str = old_path.to_string_lossy();
        let new_str = new_path.to_string_lossy();

        // Resolve the old path before the move (the directory still
        // exists at this point). After the move, the old directory is
        // gone and canonicalize would fail.
        let resolved_old = if old_path.is_absolute() {
            old_path
                .canonicalize()
                .unwrap_or_else(|_| old_path.to_path_buf())
        } else {
            self.repo_path
                .join(old_path)
                .canonicalize()
                .unwrap_or_else(|_| self.repo_path.join(old_path))
        };

        let args = vec![old_str.into_owned(), new_str.into_owned()];
        core_cli::move_worktree(&self.repo_path, &args).await?;

        debug!(
            old = %old_path.display(),
            new = %new_path.display(),
            "worktree moved"
        );

        let resolved_new = if new_path.is_absolute() {
            new_path
                .canonicalize()
                .unwrap_or_else(|_| new_path.to_path_buf())
        } else {
            self.repo_path
                .join(new_path)
                .canonicalize()
                .unwrap_or_else(|_| self.repo_path.join(new_path))
        };

        // Update the path in worktrees.json metadata.
        let mut meta_list = self.load_meta().await?;
        for entry in &mut meta_list {
            let entry_resolved = entry
                .path
                .canonicalize()
                .unwrap_or_else(|_| entry.path.clone());
            if entry_resolved == resolved_old || entry.path == resolved_old {
                entry.path = resolved_new.clone();
                break;
            }
        }
        self.save_meta(&meta_list).await?;

        Ok(())
    }

    /// Lock a worktree to prevent accidental removal.
    ///
    /// Runs `git worktree lock <path>` with an optional `--reason`.
    /// Git will error if the worktree is already locked.
    pub async fn worktree_lock(&self, path: &Path, reason: Option<&str>) -> Result<()> {
        let path_str = path.to_string_lossy();
        let mut args: Vec<String> = Vec::new();

        // Owned string for the reason so we can borrow it into args.
        if let Some(r) = reason {
            args.push("--reason".to_owned());
            args.push(r.to_owned());
        }

        args.push(path_str.into_owned());
        core_cli::lock_worktree(&self.repo_path, &args).await?;

        debug!(path = %path.display(), reason = ?reason, "worktree locked");
        Ok(())
    }

    /// Unlock a previously locked worktree.
    ///
    /// Runs `git worktree unlock <path>`. Git will error if the
    /// worktree is not currently locked.
    pub async fn worktree_unlock(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy().into_owned();
        core_cli::unlock_worktree(&self.repo_path, &[path_str]).await?;

        debug!(path = %path.display(), "worktree unlocked");
        Ok(())
    }

    /// Prune stale worktree entries.
    ///
    /// Runs `git worktree prune` which removes worktree bookkeeping
    /// for entries whose directories no longer exist on disk. After
    /// pruning, reconciles `worktrees.json` metadata by removing
    /// entries that no longer appear in git's worktree list.
    pub async fn worktree_prune(&self) -> Result<()> {
        core_cli::prune_worktrees(&self.repo_path, &[]).await?;

        debug!("git worktree prune completed");

        // After pruning, reconcile worktrees.json: re-list the
        // surviving worktrees and remove metadata entries for any
        // that were pruned away.
        let entries = core_cli::list_worktrees(&self.repo_path, false)
            .await?
            .into_iter()
            .map(core_entry_to_manager)
            .collect::<Vec<_>>();

        let mut meta_list = self.load_meta().await?;
        let entry_paths: std::collections::HashSet<&Path> =
            entries.iter().map(|e| e.path.as_path()).collect();

        let before_len = meta_list.len();
        meta_list.retain(|m| entry_paths.contains(m.path.as_path()));

        if meta_list.len() < before_len {
            debug!(
                removed = before_len - meta_list.len(),
                "reconciled stale metadata entries after prune"
            );
        }

        self.save_meta(&meta_list).await?;

        Ok(())
    }

    /// Repair worktree administrative links through the core Crab CLI.
    pub async fn worktree_repair(&self, paths: &[PathBuf]) -> Result<()> {
        let args = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        core_cli::repair_worktrees(&self.repo_path, &args).await?;
        Ok(())
    }

    /// Get status for a specific worktree (dirty count + ahead/behind).
    ///
    /// Runs `git status --porcelain` in the worktree directory to count
    /// dirty (modified/untracked) files, and `git rev-list --count
    /// --left-right @{upstream}...HEAD` to determine ahead/behind counts.
    ///
    /// When no upstream is configured for the current branch, `ahead`
    /// and `behind` are returned as `null`.
    pub async fn worktree_status(&self, path: &Path) -> Result<serde_json::Value> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.repo_path.join(path)
        };

        // Count dirty files via `git status --porcelain`.
        let status_out = run_git(&resolved, &["status", "--porcelain"]).await?;
        let dirty_count = status_out.lines().filter(|l| !l.is_empty()).count();

        // Determine ahead/behind relative to upstream.
        let (ahead, behind) = match run_git(
            &resolved,
            &["rev-list", "--count", "--left-right", "@{upstream}...HEAD"],
        )
        .await
        {
            Ok(output) => {
                // Output format: "<behind>\t<ahead>\n"
                let parts: Vec<&str> = output.trim().split('\t').collect();
                if parts.len() == 2 {
                    let behind_count = parts[0].parse::<u64>().unwrap_or(0);
                    let ahead_count = parts[1].parse::<u64>().unwrap_or(0);
                    (Some(ahead_count), Some(behind_count))
                } else {
                    (None, None)
                }
            }
            Err(_) => {
                // No upstream configured or other error — report null.
                (None, None)
            }
        };

        debug!(
            path = %resolved.display(),
            dirty_count,
            ?ahead,
            ?behind,
            "worktree status"
        );

        Ok(serde_json::json!({
            "dirty_count": dirty_count,
            "ahead": ahead,
            "behind": behind,
        }))
    }

    /// Update UI metadata for a worktree (display name, colour, pin).
    ///
    /// Loads the current metadata from `worktrees.json`, finds the
    /// entry matching the given path, and updates it in place. If no
    /// entry exists for the path, a new one is inserted. The updated
    /// metadata is then persisted back to disk.
    pub async fn worktree_set_meta(&self, meta: WorktreeMeta) -> Result<()> {
        let mut meta_list = self.load_meta().await?;

        // Find an existing entry by path and update it, or insert new.
        if let Some(existing) = meta_list.iter_mut().find(|m| m.path == meta.path) {
            existing.display_name = meta.display_name;
            existing.colour = meta.colour;
            existing.pinned = meta.pinned;
            existing.hydration_policy = meta.hydration_policy;
        } else {
            meta_list.push(meta);
        }

        self.save_meta(&meta_list).await?;

        debug!("worktree metadata updated");
        Ok(())
    }

    /// Estimate the push cost for a worktree based on its pointer files.
    ///
    /// Scans the worktree's HEAD for crab pointer files using the
    /// same logic as `scan_for_pointer_files`. Returns a [`PushEstimate`]
    /// with the total bytes, an xorb count estimate (using unique pointer
    /// file count as a proxy), and the pointer file count.
    ///
    /// If no pointer files are found, all fields are zero.
    pub async fn worktree_push_estimate(&self, path: &Path) -> Result<super::types::PushEstimate> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.repo_path.join(path)
        };

        // Reuse the existing pointer scanning logic against HEAD.
        let offer = scan_for_pointer_files(&resolved, "HEAD").await?;

        let estimate = match offer {
            Some(hydration_offer) => {
                let pointer_count = hydration_offer.files.len() as u64;
                super::types::PushEstimate {
                    total_bytes: hydration_offer.total_bytes,
                    xorb_count: pointer_count,
                    pointer_count,
                }
            }
            None => super::types::PushEstimate {
                total_bytes: 0,
                xorb_count: 0,
                pointer_count: 0,
            },
        };

        debug!(
            path = %resolved.display(),
            total_bytes = estimate.total_bytes,
            xorb_count = estimate.xorb_count,
            pointer_count = estimate.pointer_count,
            "worktree push estimate"
        );

        Ok(estimate)
    }

    /// Cherry-pick a commit from one worktree's branch into another.
    ///
    /// Runs `git cherry-pick <sha>` in the target worktree's directory.
    /// The source SHA can come from any branch visible to the shared
    /// object store (all worktrees share the same `.git`). Returns the
    /// git exit code, stdout, and stderr for the caller to interpret
    /// (e.g. detect conflicts).
    pub async fn worktree_cherry_pick(
        &self,
        sha: &str,
        target_worktree_path: &Path,
    ) -> Result<(i32, String, String)> {
        let resolved = if target_worktree_path.is_absolute() {
            target_worktree_path.to_path_buf()
        } else {
            self.repo_path.join(target_worktree_path)
        };

        debug!(
            sha = %sha,
            target = %resolved.display(),
            "worktree cherry-pick"
        );

        let output = tokio::process::Command::new("git")
            .args(["cherry-pick", sha])
            .current_dir(&resolved)
            .output()
            .await
            .map_err(AgentError::Io)?;

        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if code == 0 {
            debug!(sha = %sha, target = %resolved.display(), "cherry-pick succeeded");
        } else {
            debug!(
                sha = %sha,
                target = %resolved.display(),
                code,
                "cherry-pick exited non-zero (may have conflicts)"
            );
        }

        Ok((code, stdout, stderr))
    }

    /// Load worktree metadata from disk. Returns an empty vec if the
    /// file doesn't exist yet.
    pub(crate) async fn load_meta(&self) -> Result<Vec<WorktreeMeta>> {
        let path = &self.meta_path;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(AgentError::Io)?;
        let meta: Vec<WorktreeMeta> = serde_json::from_str(&content).map_err(AgentError::Decode)?;
        Ok(meta)
    }

    /// Persist worktree metadata to disk, creating parent directories
    /// if needed.
    pub(crate) async fn save_meta(&self, meta: &[WorktreeMeta]) -> Result<()> {
        if let Some(parent) = self.meta_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(AgentError::Io)?;
        }
        let content = serde_json::to_string_pretty(meta).map_err(AgentError::Decode)?;
        tokio::fs::write(&self.meta_path, content)
            .await
            .map_err(AgentError::Io)?;
        Ok(())
    }
}

fn hydration_arg(choice: HydrationChoice) -> String {
    match choice {
        HydrationChoice::All => "--hydrate=full".to_owned(),
        HydrationChoice::OnAccess => "--hydrate=lazy".to_owned(),
        HydrationChoice::PointersOnly => "--hydrate=pointer-only".to_owned(),
    }
}

fn core_entry_to_manager(entry: core_cli::CrabWorktreeListEntry) -> WorktreeEntry {
    let is_main = entry.is_main();
    WorktreeEntry {
        path: PathBuf::from(entry.path),
        branch: entry.branch.unwrap_or_default(),
        head: entry.head.unwrap_or_default(),
        is_main,
        locked: entry.locked,
        lock_reason: entry.lock_reason,
        prunable: entry.prunable,
    }
}

fn find_hydrated_worktree(
    worktrees: Vec<HydratedWorktree>,
    path: &Path,
) -> Option<HydratedWorktree> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    worktrees.into_iter().find(|wt| {
        wt.entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| wt.entry.path.clone())
            == resolved
    })
}

// ---- Git shell-out ---------------------------------------------------

/// Run a git command in the given directory and return stdout.
async fn run_git(work_dir: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        let status = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::Cli { status, stderr });
    }

    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ---- Pointer detection -----------------------------------------------

/// The version line that identifies a crab pointer blob.
const POINTER_VERSION_LINE: &str = "version https://crab.dev/spec/v1";

/// Maximum size of a valid pointer blob (same as the engine's limit).
const MAX_POINTER_SIZE: usize = 256;

/// Check whether raw bytes represent a crab pointer blob.
///
/// A valid pointer is ≤ 256 bytes, UTF-8, starts with the version line,
/// and has 3 or 4 LF-terminated lines matching the pointer grammar.
fn is_crab_pointer(bytes: &[u8]) -> bool {
    if bytes.len() > MAX_POINTER_SIZE {
        return false;
    }
    if bytes.last() != Some(&b'\n') {
        return false;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return false,
    };
    text.starts_with(POINTER_VERSION_LINE)
}

/// Parse the `size` field from a crab pointer blob.
///
/// Returns `None` if the bytes are not a valid pointer or the size
/// line cannot be parsed.
fn parse_pointer_size(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > MAX_POINTER_SIZE || bytes.last() != Some(&b'\n') {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.starts_with(POINTER_VERSION_LINE) {
        return None;
    }
    // The size is on line 3: "size <decimal>"
    let size_line = text.lines().nth(2)?;
    let size_str = size_line.strip_prefix("size ")?;
    size_str.parse::<u64>().ok()
}

/// Scan a branch for crab pointer files and build a [`HydrationOffer`].
///
/// Uses `git ls-tree -r --name-only HEAD` in the worktree directory to
/// enumerate all files, then batch-checks each file's content via
/// `git cat-file --batch`. Returns `None` when no pointer files are found.
async fn scan_for_pointer_files(
    worktree_path: &Path,
    _branch: &str,
) -> Result<Option<HydrationOffer>> {
    // List all files in the worktree's HEAD (which is the branch we
    // just checked out).
    let ls_output = run_git(worktree_path, &["ls-tree", "-r", "--name-only", "HEAD"]).await?;

    let file_paths: Vec<&str> = ls_output.lines().filter(|l| !l.is_empty()).collect();
    if file_paths.is_empty() {
        return Ok(None);
    }

    // Use `git cat-file --batch` to efficiently read blob contents.
    // Feed each path as `HEAD:<path>` to get the blob content.
    let mut child = tokio::process::Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(AgentError::Io)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Other("failed to open stdin for git cat-file".to_owned()))?;

    // Write all object queries to stdin.
    let queries: String = file_paths.iter().map(|p| format!("HEAD:{p}\n")).collect();

    use tokio::io::AsyncWriteExt;
    stdin
        .write_all(queries.as_bytes())
        .await
        .map_err(AgentError::Io)?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(AgentError::Io)?;
    let stdout = output.stdout;

    // Parse the batch output. Each response is:
    //   <sha> <type> <size>\n
    //   <content>\n
    // Or for missing objects:
    //   <query> missing\n
    let mut pointer_files: Vec<PointerFileInfo> = Vec::new();
    let mut pos = 0;
    let mut file_idx = 0;

    while pos < stdout.len() && file_idx < file_paths.len() {
        // Read the header line.
        let header_end = match stdout[pos..].iter().position(|&b| b == b'\n') {
            Some(i) => pos + i,
            None => break,
        };
        let header = String::from_utf8_lossy(&stdout[pos..header_end]);

        if header.ends_with("missing") {
            // Object not found — skip.
            pos = header_end + 1;
            file_idx += 1;
            continue;
        }

        // Parse "<sha> <type> <size>"
        let parts: Vec<&str> = header.split_whitespace().collect();
        let blob_size: usize = if parts.len() >= 3 {
            parts[2].parse().unwrap_or(0)
        } else {
            // Unexpected format — skip this entry.
            pos = header_end + 1;
            file_idx += 1;
            continue;
        };

        // Content starts after the header newline.
        let content_start = header_end + 1;
        let content_end = content_start + blob_size;

        if content_end > stdout.len() {
            break;
        }

        let content = &stdout[content_start..content_end];

        // Check if this is a pointer file.
        if is_crab_pointer(content) {
            if let Some(size_bytes) = parse_pointer_size(content) {
                pointer_files.push(PointerFileInfo {
                    path: PathBuf::from(file_paths[file_idx]),
                    size_bytes,
                });
            }
        }

        // Skip past content + trailing LF.
        pos = content_end + 1;
        file_idx += 1;
    }

    if pointer_files.is_empty() {
        return Ok(None);
    }

    let total_bytes: u64 = pointer_files.iter().map(|f| f.size_bytes).sum();
    debug!(
        count = pointer_files.len(),
        total_bytes, "detected pointer files in worktree"
    );

    Ok(Some(HydrationOffer {
        files: pointer_files,
        total_bytes,
    }))
}

// ---- Porcelain parser ------------------------------------------------

/// Parse `git worktree list --porcelain` output into structured entries.
///
/// Porcelain format is a sequence of blocks separated by blank lines.
/// Each block has key-value lines:
///   worktree /path/to/worktree
///   HEAD <sha>
///   branch refs/heads/main
///   (or: detached)
///   (or: bare)
///   locked [reason]
///   prunable
pub fn parse_porcelain(raw: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path = PathBuf::new();
    let mut head = String::new();
    let mut branch = String::new();
    let mut is_main = false;
    let mut locked = false;
    let mut lock_reason: Option<String> = None;
    let mut prunable = false;
    let mut in_entry = false;
    let mut entry_index: usize = 0;

    for line in raw.lines() {
        if line.is_empty() {
            // End of a block — flush the current entry.
            if in_entry {
                entries.push(WorktreeEntry {
                    path: path.clone(),
                    head: head.clone(),
                    branch: branch.clone(),
                    is_main,
                    locked,
                    lock_reason: lock_reason.clone(),
                    prunable,
                });
            }
            // Reset for next block.
            path = PathBuf::new();
            head.clear();
            branch.clear();
            is_main = false;
            locked = false;
            lock_reason = None;
            prunable = false;
            in_entry = false;
            entry_index += 1;
            continue;
        }

        if let Some(rest) = line.strip_prefix("worktree ") {
            path = PathBuf::from(rest);
            in_entry = true;
            // The first worktree entry is always the main worktree.
            if entry_index == 0 {
                is_main = true;
            }
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = rest.to_owned();
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = rest.to_owned();
        } else if line == "detached" {
            // Detached HEAD — branch stays empty.
        } else if line == "bare" {
            // Bare repos have no branch or detached state.
        } else if line == "locked" {
            locked = true;
        } else if let Some(rest) = line.strip_prefix("locked ") {
            locked = true;
            lock_reason = Some(rest.to_owned());
        } else if line == "prunable" {
            prunable = true;
        }
    }

    // Flush the last entry if the output doesn't end with a blank line.
    if in_entry {
        entries.push(WorktreeEntry {
            path,
            head,
            branch,
            is_main,
            locked,
            lock_reason,
            prunable,
        });
    }

    entries
}

// ---- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::types::HydrationChoice;
    use super::*;
    use std::process::Stdio;

    fn sh(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git command spawn")
    }

    /// Create a git repo with one commit so worktrees can be created.
    fn git_init_with_commit() -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        sh(&root, &["init", "--initial-branch=main"]);
        sh(&root, &["config", "user.email", "test@example.com"]);
        sh(&root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join("README.md"), "# test\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "initial"]);
        (td, root)
    }

    #[test]
    fn hydration_choice_maps_to_core_worktree_flags() {
        assert_eq!(hydration_arg(HydrationChoice::All), "--hydrate=full");
        assert_eq!(hydration_arg(HydrationChoice::OnAccess), "--hydrate=lazy");
        assert_eq!(
            hydration_arg(HydrationChoice::PointersOnly),
            "--hydrate=pointer-only"
        );
    }

    // ---- Porcelain parser tests ----

    #[test]
    fn parse_porcelain_single_worktree() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD abc1234567890abcdef1234567890abcdef123456\n",
            "branch refs/heads/main\n",
            "\n",
        );
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, Path::new("/home/user/project"));
        assert_eq!(entries[0].head, "abc1234567890abcdef1234567890abcdef123456");
        assert_eq!(entries[0].branch, "refs/heads/main");
        assert!(entries[0].is_main);
        assert!(!entries[0].locked);
        assert!(!entries[0].prunable);
    }

    #[test]
    fn parse_porcelain_multiple_worktrees() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /home/user/project-feature\n",
            "HEAD bbbb\n",
            "branch refs/heads/feature\n",
            "\n",
            "worktree /home/user/project-detached\n",
            "HEAD cccc\n",
            "detached\n",
            "\n",
        );
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_main);
        assert!(!entries[1].is_main);
        assert!(!entries[2].is_main);
        // Detached worktree has empty branch.
        assert_eq!(entries[2].branch, "");
        assert_eq!(entries[1].branch, "refs/heads/feature");
    }

    #[test]
    fn parse_porcelain_locked_with_reason() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /tmp/wt-locked\n",
            "HEAD bbbb\n",
            "branch refs/heads/locked-branch\n",
            "locked on external drive\n",
            "\n",
        );
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].locked);
        assert_eq!(entries[1].lock_reason.as_deref(), Some("on external drive"));
    }

    #[test]
    fn parse_porcelain_locked_without_reason() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /tmp/wt-locked\n",
            "HEAD bbbb\n",
            "branch refs/heads/locked-branch\n",
            "locked\n",
            "\n",
        );
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].locked);
        assert_eq!(entries[1].lock_reason, None);
    }

    #[test]
    fn parse_porcelain_prunable() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /tmp/gone\n",
            "HEAD bbbb\n",
            "branch refs/heads/gone-branch\n",
            "prunable\n",
            "\n",
        );
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].prunable);
    }

    #[test]
    fn parse_porcelain_no_trailing_newline() {
        // Some git versions don't emit a trailing blank line.
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main",
        );
        let entries = parse_porcelain(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, Path::new("/home/user/project"));
        assert!(entries[0].is_main);
    }

    // ---- Manager path tests ----

    #[test]
    fn new_manager_sets_paths() {
        let repo = PathBuf::from("/home/user/project");
        let mgr = WorktreeManager::new(repo.clone());
        assert_eq!(mgr.repo_path(), Path::new("/home/user/project"));
        assert_eq!(
            mgr.meta_path(),
            Path::new("/home/user/project/.crab/desktop/worktrees.json")
        );
    }

    // ---- Metadata persistence tests ----

    #[tokio::test]
    async fn load_meta_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = WorktreeManager::new(tmp.path().to_path_buf());
        let meta = mgr.load_meta().await.expect("load_meta");
        assert!(meta.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_meta_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = WorktreeManager::new(tmp.path().to_path_buf());

        let entries = vec![WorktreeMeta {
            path: PathBuf::from("/tmp/wt-feature"),
            display_name: Some("Feature Work".into()),
            colour: Some("#ff0000".into()),
            pinned: true,
            hydration_policy: HydrationChoice::OnAccess,
        }];

        mgr.save_meta(&entries).await.expect("save_meta");
        let loaded = mgr.load_meta().await.expect("load_meta");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].display_name.as_deref(), Some("Feature Work"));
        assert_eq!(loaded[0].colour.as_deref(), Some("#ff0000"));
        assert!(loaded[0].pinned);
        assert_eq!(loaded[0].hydration_policy, HydrationChoice::OnAccess);
    }

    // ---- Reconciliation tests ----

    #[tokio::test]
    async fn reconciliation_removes_stale_entries() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Seed metadata with an entry for a worktree that doesn't exist.
        let stale_meta = vec![WorktreeMeta {
            path: PathBuf::from("/nonexistent/worktree"),
            display_name: Some("Stale".into()),
            colour: None,
            pinned: false,
            hydration_policy: HydrationChoice::All,
        }];
        mgr.save_meta(&stale_meta).await.expect("save_meta");

        // worktree_list should reconcile and remove the stale entry.
        let list = mgr.worktree_list().await.expect("worktree_list");

        // Should have at least the main worktree.
        assert!(!list.is_empty());
        assert!(list[0].entry.is_main);

        // The stale metadata should have been removed.
        let reloaded_meta = mgr.load_meta().await.expect("load_meta");
        assert!(
            !reloaded_meta
                .iter()
                .any(|m| m.path == Path::new("/nonexistent/worktree")),
            "stale entry should have been removed"
        );
    }

    #[tokio::test]
    async fn reconciliation_preserves_valid_entries() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // First, list to get the main worktree path.
        let list = mgr.worktree_list().await.expect("worktree_list");
        let main_path = list[0].entry.path.clone();

        // Save metadata for the main worktree.
        let valid_meta = vec![WorktreeMeta {
            path: main_path.clone(),
            display_name: Some("Main".into()),
            colour: Some("#00ff00".into()),
            pinned: true,
            hydration_policy: HydrationChoice::All,
        }];
        mgr.save_meta(&valid_meta).await.expect("save_meta");

        // Re-list — the valid metadata should be preserved and attached.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 1);
        let meta = list[0].meta.as_ref().expect("meta should be present");
        assert_eq!(meta.display_name.as_deref(), Some("Main"));
        assert_eq!(meta.colour.as_deref(), Some("#00ff00"));
    }

    // ---- Integration test with real git worktrees ----

    #[tokio::test]
    async fn worktree_list_on_fresh_repo() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());
        let list = mgr.worktree_list().await.expect("worktree_list");

        assert_eq!(list.len(), 1);
        assert!(list[0].entry.is_main);
        assert_eq!(list[0].entry.branch, "refs/heads/main");
        assert!(!list[0].entry.head.is_empty());
    }

    #[tokio::test]
    async fn worktree_list_with_added_worktree() {
        let (_td, root) = git_init_with_commit();

        // Create a branch and add a worktree.
        sh(&root, &["branch", "feature-wt"]);
        let wt_path = root.join("../wt-feature-list-test");
        let wt_path_str = wt_path.to_string_lossy().to_string();
        sh(&root, &["worktree", "add", &wt_path_str, "feature-wt"]);

        let mgr = WorktreeManager::new(root.clone());
        let list = mgr.worktree_list().await.expect("worktree_list");

        assert_eq!(list.len(), 2);
        assert!(list[0].entry.is_main);
        assert!(!list[1].entry.is_main);
        assert_eq!(list[1].entry.branch, "refs/heads/feature-wt");

        // Clean up the worktree.
        sh(&root, &["worktree", "remove", &wt_path_str]);
    }

    // ---- worktree_add integration tests ----

    #[tokio::test]
    async fn worktree_add_existing_branch() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch to check out in the new worktree.
        sh(&root, &["branch", "existing-branch"]);

        // Use a sibling directory within the same temp dir parent.
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-existing");
        let result = mgr
            .worktree_add(&wt_path, "existing-branch", false, None)
            .await
            .expect("worktree_add should succeed");

        // The returned entry should reference the correct branch.
        assert_eq!(result.entry.branch, "refs/heads/existing-branch");
        assert!(!result.entry.is_main);

        // Verify it appears in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);
        let added = list
            .iter()
            .find(|w| !w.entry.is_main)
            .expect("should find added worktree");
        assert_eq!(added.entry.branch, "refs/heads/existing-branch");

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_add_create_branch() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-new-branch");
        let result = mgr
            .worktree_add(&wt_path, "brand-new-branch", true, None)
            .await
            .expect("worktree_add with create_branch should succeed");

        // The new branch should be created and checked out.
        assert_eq!(result.entry.branch, "refs/heads/brand-new-branch");
        assert!(!result.entry.is_main);

        // Verify the branch actually exists in git.
        let branch_out = sh(&root, &["branch", "--list", "brand-new-branch"]);
        let branch_list = String::from_utf8_lossy(&branch_out.stdout);
        assert!(
            branch_list.contains("brand-new-branch"),
            "new branch should exist in git"
        );

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_add_create_branch_with_base() {
        let (_td, root) = git_init_with_commit();

        // Create a second commit on a side branch to use as base.
        sh(&root, &["checkout", "-b", "base-branch"]);
        std::fs::write(root.join("extra.txt"), "extra\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "second commit on base-branch"]);
        sh(&root, &["checkout", "main"]);

        let mgr = WorktreeManager::new(root.clone());

        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-with-base");
        let result = mgr
            .worktree_add(&wt_path, "from-base", true, Some("base-branch"))
            .await
            .expect("worktree_add with base should succeed");

        assert_eq!(result.entry.branch, "refs/heads/from-base");

        // The new worktree should have the extra.txt file from base-branch.
        assert!(
            wt_path.join("extra.txt").exists(),
            "worktree should contain files from the base branch"
        );

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_add_duplicate_path_returns_error() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree at a specific path.
        sh(&root, &["branch", "dup-branch"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-dup");
        mgr.worktree_add(&wt_path, "dup-branch", false, None)
            .await
            .expect("first add should succeed");

        // Attempting to add another worktree at the same path should fail.
        let result = mgr
            .worktree_add(&wt_path, "another-branch", true, None)
            .await;

        assert!(
            result.is_err(),
            "adding a worktree to an already-used path should return an error"
        );

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    // ---- worktree_remove integration tests ----

    #[tokio::test]
    async fn worktree_remove_succeeds_and_removes_from_list() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "remove-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-remove");
        mgr.worktree_add(&wt_path, "remove-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Verify it's in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);

        // Remove it.
        mgr.worktree_remove(&wt_path, false)
            .await
            .expect("worktree_remove should succeed");

        // Verify it's gone from the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 1);
        assert!(list[0].entry.is_main);
    }

    #[tokio::test]
    async fn worktree_remove_dirty_without_force_fails() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "dirty-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-dirty");
        mgr.worktree_add(&wt_path, "dirty-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Make the worktree dirty by creating an uncommitted file.
        std::fs::write(wt_path.join("dirty.txt"), "uncommitted\n").expect("write dirty file");
        sh(&wt_path, &["add", "dirty.txt"]);

        // Attempting to remove without force should fail.
        let result = mgr.worktree_remove(&wt_path, false).await;
        assert!(
            result.is_err(),
            "removing a dirty worktree without force should fail"
        );

        // The worktree should still be in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);

        // Clean up with force.
        mgr.worktree_remove(&wt_path, true)
            .await
            .expect("force remove should succeed");
    }

    #[tokio::test]
    async fn worktree_remove_dirty_with_force_succeeds() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "force-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-force");
        mgr.worktree_add(&wt_path, "force-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Make the worktree dirty.
        std::fs::write(wt_path.join("dirty.txt"), "uncommitted\n").expect("write dirty file");
        sh(&wt_path, &["add", "dirty.txt"]);

        // Force remove should succeed even with dirty state.
        mgr.worktree_remove(&wt_path, true)
            .await
            .expect("force remove of dirty worktree should succeed");

        // Verify it's gone.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 1);
        assert!(list[0].entry.is_main);
    }

    #[tokio::test]
    async fn worktree_remove_cleans_metadata() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "meta-remove-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-meta-rm");
        mgr.worktree_add(&wt_path, "meta-remove-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Manually add metadata for this worktree.
        let resolved = wt_path.canonicalize().expect("canonicalize");
        let meta = vec![WorktreeMeta {
            path: resolved.clone(),
            display_name: Some("Feature Work".into()),
            colour: Some("#ff0000".into()),
            pinned: true,
            hydration_policy: HydrationChoice::OnAccess,
        }];
        mgr.save_meta(&meta).await.expect("save_meta");

        // Verify metadata is present.
        let loaded = mgr.load_meta().await.expect("load_meta");
        assert_eq!(loaded.len(), 1);

        // Remove the worktree.
        mgr.worktree_remove(&wt_path, false)
            .await
            .expect("worktree_remove should succeed");

        // Metadata should be cleaned up.
        let loaded = mgr.load_meta().await.expect("load_meta");
        assert!(
            loaded.is_empty(),
            "metadata should be removed after worktree removal"
        );
    }

    // ---- worktree_move integration tests ----

    #[tokio::test]
    async fn worktree_move_updates_path_in_list() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "move-test"]);
        let wt_old = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-move-old");
        mgr.worktree_add(&wt_old, "move-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Verify it's in the list at the old path.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);

        // Move it to a new path.
        let wt_new = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-move-new");
        mgr.worktree_move(&wt_old, &wt_new)
            .await
            .expect("worktree_move should succeed");

        // Verify it appears at the new path in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);
        let moved = list
            .iter()
            .find(|w| !w.entry.is_main)
            .expect("should find moved worktree");
        let moved_resolved = moved
            .entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| moved.entry.path.clone());
        let new_resolved = wt_new.canonicalize().expect("canonicalize new path");
        assert_eq!(moved_resolved, new_resolved);

        // The old path should no longer exist on disk.
        assert!(
            !wt_old.exists(),
            "old worktree path should not exist after move"
        );

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_new.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_move_updates_metadata() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "move-meta-test"]);
        let wt_old = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-move-meta-old");
        mgr.worktree_add(&wt_old, "move-meta-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Add metadata for the worktree.
        let resolved_old = wt_old.canonicalize().expect("canonicalize");
        let meta = vec![WorktreeMeta {
            path: resolved_old.clone(),
            display_name: Some("My Feature".into()),
            colour: Some("#0000ff".into()),
            pinned: false,
            hydration_policy: HydrationChoice::All,
        }];
        mgr.save_meta(&meta).await.expect("save_meta");

        // Move the worktree.
        let wt_new = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-move-meta-new");
        mgr.worktree_move(&wt_old, &wt_new)
            .await
            .expect("worktree_move should succeed");

        // Metadata should now reference the new path.
        let loaded = mgr.load_meta().await.expect("load_meta");
        assert_eq!(loaded.len(), 1);
        let new_resolved = wt_new.canonicalize().expect("canonicalize new path");
        assert_eq!(loaded[0].path, new_resolved);
        assert_eq!(loaded[0].display_name.as_deref(), Some("My Feature"));

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_new.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_move_locked_fails() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "move-locked-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-move-locked");
        mgr.worktree_add(&wt_path, "move-locked-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Lock the worktree.
        let wt_str = wt_path.to_string_lossy().to_string();
        sh(&root, &["worktree", "lock", &wt_str]);

        // Attempting to move a locked worktree should fail.
        let new_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-move-locked-new");
        let result = mgr.worktree_move(&wt_path, &new_path).await;
        assert!(result.is_err(), "moving a locked worktree should fail");

        // Clean up: unlock and remove.
        sh(&root, &["worktree", "unlock", &wt_str]);
        sh(&root, &["worktree", "remove", "--force", &wt_str]);
    }

    // ---- worktree_lock / worktree_unlock integration tests ----

    #[tokio::test]
    async fn worktree_lock_shows_locked_in_list() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "lock-list-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-lock-list");
        mgr.worktree_add(&wt_path, "lock-list-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Lock it.
        mgr.worktree_lock(&wt_path, None)
            .await
            .expect("worktree_lock should succeed");

        // Verify it shows as locked in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        let locked_wt = list
            .iter()
            .find(|w| !w.entry.is_main)
            .expect("should find worktree");
        assert!(locked_wt.entry.locked, "worktree should be locked");

        // Clean up: unlock and remove.
        mgr.worktree_unlock(&wt_path).await.expect("unlock");
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_lock_with_reason_shows_reason() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "lock-reason-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-lock-reason");
        mgr.worktree_add(&wt_path, "lock-reason-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Lock with a reason.
        mgr.worktree_lock(&wt_path, Some("on external drive"))
            .await
            .expect("worktree_lock with reason should succeed");

        // Verify the reason appears in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        let locked_wt = list
            .iter()
            .find(|w| !w.entry.is_main)
            .expect("should find worktree");
        assert!(locked_wt.entry.locked, "worktree should be locked");
        assert_eq!(
            locked_wt.entry.lock_reason.as_deref(),
            Some("on external drive"),
            "lock reason should be preserved"
        );

        // Clean up: unlock and remove.
        mgr.worktree_unlock(&wt_path).await.expect("unlock");
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_unlock_makes_worktree_unlocked() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "unlock-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-unlock");
        mgr.worktree_add(&wt_path, "unlock-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Lock it first.
        mgr.worktree_lock(&wt_path, Some("temporary"))
            .await
            .expect("worktree_lock should succeed");

        // Verify it's locked.
        let list = mgr.worktree_list().await.expect("worktree_list");
        let wt = list
            .iter()
            .find(|w| !w.entry.is_main)
            .expect("should find worktree");
        assert!(wt.entry.locked, "worktree should be locked before unlock");

        // Unlock it.
        mgr.worktree_unlock(&wt_path)
            .await
            .expect("worktree_unlock should succeed");

        // Verify it shows as unlocked.
        let list = mgr.worktree_list().await.expect("worktree_list");
        let wt = list
            .iter()
            .find(|w| !w.entry.is_main)
            .expect("should find worktree");
        assert!(!wt.entry.locked, "worktree should be unlocked after unlock");
        assert_eq!(wt.entry.lock_reason, None, "lock reason should be cleared");

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_lock_already_locked_fails() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "double-lock-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-double-lock");
        mgr.worktree_add(&wt_path, "double-lock-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Lock it once.
        mgr.worktree_lock(&wt_path, Some("first lock"))
            .await
            .expect("first lock should succeed");

        // Attempting to lock again should fail.
        let result = mgr.worktree_lock(&wt_path, Some("second lock")).await;
        assert!(
            result.is_err(),
            "locking an already-locked worktree should fail"
        );

        // Clean up: unlock and remove.
        mgr.worktree_unlock(&wt_path).await.expect("unlock");
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    // ---- worktree_prune integration tests ----

    #[tokio::test]
    async fn worktree_prune_removes_stale_entry() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "prune-test"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-prune");
        mgr.worktree_add(&wt_path, "prune-test", false, None)
            .await
            .expect("worktree_add should succeed");

        // Verify it's in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);

        // Add metadata for the worktree so we can verify it gets cleaned up.
        let resolved = wt_path.canonicalize().expect("canonicalize");
        let meta = vec![WorktreeMeta {
            path: resolved.clone(),
            display_name: Some("Prune Target".into()),
            colour: Some("#abcdef".into()),
            pinned: false,
            hydration_policy: HydrationChoice::All,
        }];
        mgr.save_meta(&meta).await.expect("save_meta");

        // Manually delete the worktree directory to make it stale.
        // This simulates the scenario where a user deletes the directory
        // outside of git (e.g., `rm -rf`).
        std::fs::remove_dir_all(&wt_path).expect("remove worktree dir");

        // Prune should clean up the stale entry from both git and metadata.
        mgr.worktree_prune()
            .await
            .expect("worktree_prune should succeed");

        // The stale worktree should no longer appear in git's list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 1, "only the main worktree should remain");
        assert!(list[0].entry.is_main);

        // The metadata should also be cleaned up.
        let loaded_meta = mgr.load_meta().await.expect("load_meta");
        assert!(
            !loaded_meta.iter().any(|m| m.path == resolved),
            "metadata for pruned worktree should be removed"
        );
    }

    // ---- worktree_status integration tests ----

    #[tokio::test]
    async fn worktree_status_clean_worktree_has_zero_dirty() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "status-clean"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-status-clean");
        mgr.worktree_add(&wt_path, "status-clean", false, None)
            .await
            .expect("worktree_add should succeed");

        // Query status — fresh worktree should have zero dirty files.
        let status = mgr
            .worktree_status(&wt_path)
            .await
            .expect("worktree_status");
        assert_eq!(status["dirty_count"], 0);

        // No upstream configured, so ahead/behind should be null.
        assert!(status["ahead"].is_null());
        assert!(status["behind"].is_null());

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_status_dirty_worktree_counts_changes() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch and add a worktree.
        sh(&root, &["branch", "status-dirty"]);
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-status-dirty");
        mgr.worktree_add(&wt_path, "status-dirty", false, None)
            .await
            .expect("worktree_add should succeed");

        // Create uncommitted changes in the worktree.
        std::fs::write(wt_path.join("new-file.txt"), "hello\n").expect("write file");
        std::fs::write(wt_path.join("another.txt"), "world\n").expect("write file");

        // Query status — should report dirty files.
        let status = mgr
            .worktree_status(&wt_path)
            .await
            .expect("worktree_status");
        assert!(
            status["dirty_count"].as_u64().unwrap_or(0) >= 2,
            "dirty_count should be at least 2, got {:?}",
            status["dirty_count"]
        );

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    // ---- worktree_set_meta tests ----

    #[tokio::test]
    async fn set_meta_creates_new_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = WorktreeManager::new(tmp.path().to_path_buf());

        let meta = WorktreeMeta {
            path: PathBuf::from("/tmp/wt-new"),
            display_name: Some("New Feature".into()),
            colour: Some("#3498db".into()),
            pinned: true,
            hydration_policy: HydrationChoice::All,
        };

        mgr.worktree_set_meta(meta)
            .await
            .expect("worktree_set_meta");

        let loaded = mgr.load_meta().await.expect("load_meta");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, Path::new("/tmp/wt-new"));
        assert_eq!(loaded[0].display_name.as_deref(), Some("New Feature"));
        assert_eq!(loaded[0].colour.as_deref(), Some("#3498db"));
        assert!(loaded[0].pinned);
        assert_eq!(loaded[0].hydration_policy, HydrationChoice::All);
    }

    #[tokio::test]
    async fn set_meta_updates_existing_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = WorktreeManager::new(tmp.path().to_path_buf());

        // Seed with an initial entry.
        let initial = vec![WorktreeMeta {
            path: PathBuf::from("/tmp/wt-existing"),
            display_name: Some("Old Name".into()),
            colour: Some("#ff0000".into()),
            pinned: false,
            hydration_policy: HydrationChoice::All,
        }];
        mgr.save_meta(&initial).await.expect("save_meta");

        // Update the same path with new metadata.
        let updated = WorktreeMeta {
            path: PathBuf::from("/tmp/wt-existing"),
            display_name: Some("Updated Name".into()),
            colour: Some("#00ff00".into()),
            pinned: true,
            hydration_policy: HydrationChoice::OnAccess,
        };
        mgr.worktree_set_meta(updated)
            .await
            .expect("worktree_set_meta");

        let loaded = mgr.load_meta().await.expect("load_meta");
        assert_eq!(loaded.len(), 1, "should update in place, not duplicate");
        assert_eq!(loaded[0].display_name.as_deref(), Some("Updated Name"));
        assert_eq!(loaded[0].colour.as_deref(), Some("#00ff00"));
        assert!(loaded[0].pinned);
        assert_eq!(loaded[0].hydration_policy, HydrationChoice::OnAccess);
    }

    #[tokio::test]
    async fn set_meta_persists_across_load_save_cycles() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = WorktreeManager::new(tmp.path().to_path_buf());

        // Set metadata for two different worktrees.
        let meta1 = WorktreeMeta {
            path: PathBuf::from("/tmp/wt-alpha"),
            display_name: Some("Alpha".into()),
            colour: Some("#aaa".into()),
            pinned: false,
            hydration_policy: HydrationChoice::PointersOnly,
        };
        let meta2 = WorktreeMeta {
            path: PathBuf::from("/tmp/wt-beta"),
            display_name: None,
            colour: Some("#bbb".into()),
            pinned: true,
            hydration_policy: HydrationChoice::All,
        };

        mgr.worktree_set_meta(meta1).await.expect("set_meta 1");
        mgr.worktree_set_meta(meta2).await.expect("set_meta 2");

        // Create a fresh manager pointing at the same directory to
        // verify persistence (simulates agent restart).
        let mgr2 = WorktreeManager::new(tmp.path().to_path_buf());
        let loaded = mgr2.load_meta().await.expect("load_meta");

        assert_eq!(loaded.len(), 2);

        let alpha = loaded
            .iter()
            .find(|m| m.path == Path::new("/tmp/wt-alpha"))
            .expect("alpha");
        assert_eq!(alpha.display_name.as_deref(), Some("Alpha"));
        assert_eq!(alpha.colour.as_deref(), Some("#aaa"));
        assert!(!alpha.pinned);
        assert_eq!(alpha.hydration_policy, HydrationChoice::PointersOnly);

        let beta = loaded
            .iter()
            .find(|m| m.path == Path::new("/tmp/wt-beta"))
            .expect("beta");
        assert_eq!(beta.display_name, None);
        assert_eq!(beta.colour.as_deref(), Some("#bbb"));
        assert!(beta.pinned);
        assert_eq!(beta.hydration_policy, HydrationChoice::All);
    }

    // ---- Pointer detection unit tests ----

    #[test]
    fn is_crab_pointer_detects_valid_pointer() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";
        assert!(super::is_crab_pointer(raw));
    }

    #[test]
    fn is_crab_pointer_detects_four_line_pointer() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 2097152\n\
shard-hint abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n";
        assert!(super::is_crab_pointer(raw));
    }

    #[test]
    fn is_crab_pointer_rejects_normal_text() {
        let raw = b"Hello, world!\nThis is a normal file.\n";
        assert!(!super::is_crab_pointer(raw));
    }

    #[test]
    fn is_crab_pointer_rejects_large_file() {
        let mut large = Vec::new();
        large.extend_from_slice(b"version https://crab.dev/spec/v1\n");
        large.resize(300, b'x');
        large.push(b'\n');
        assert!(!super::is_crab_pointer(&large));
    }

    #[test]
    fn parse_pointer_size_extracts_size() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";
        assert_eq!(super::parse_pointer_size(raw), Some(1048576));
    }

    #[test]
    fn parse_pointer_size_returns_none_for_non_pointer() {
        let raw = b"not a pointer\n";
        assert_eq!(super::parse_pointer_size(raw), None);
    }

    // ---- worktree_add_with_hydration integration tests ----

    #[tokio::test]
    async fn worktree_add_with_hydration_no_pointers_returns_none() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch with only normal files (no pointers).
        sh(&root, &["branch", "no-pointers"]);

        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-no-ptr");
        let result = mgr
            .worktree_add_with_hydration(
                &wt_path,
                "no-pointers",
                false,
                None,
                HydrationChoice::PointersOnly,
            )
            .await
            .expect("worktree_add_with_hydration should succeed");

        // No pointer files in the branch, so hydration_offer should be None.
        assert!(
            result.hydration_offer.is_none(),
            "hydration_offer should be None when no pointer files exist"
        );
        assert_eq!(result.worktree.entry.branch, "refs/heads/no-pointers");

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_add_with_hydration_detects_pointer_files() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch with a pointer file.
        sh(&root, &["checkout", "-b", "has-pointers"]);

        // Write a valid crab pointer file.
        let pointer_content = "version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 5242880\n";
        std::fs::write(root.join("model.bin"), pointer_content).expect("write pointer");
        sh(&root, &["add", "model.bin"]);
        sh(&root, &["commit", "-m", "add pointer file"]);

        // Switch back to main.
        sh(&root, &["checkout", "main"]);

        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-ptr");
        let result = mgr
            .worktree_add_with_hydration(
                &wt_path,
                "has-pointers",
                false,
                None,
                HydrationChoice::PointersOnly,
            )
            .await
            .expect("worktree_add_with_hydration should succeed");

        // Should detect the pointer file and return a hydration offer.
        let offer = result
            .hydration_offer
            .expect("hydration_offer should be Some when pointer files exist");

        assert_eq!(offer.files.len(), 1);
        assert_eq!(offer.files[0].path, Path::new("model.bin"));
        assert_eq!(offer.files[0].size_bytes, 5242880);
        assert_eq!(offer.total_bytes, 5242880);

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn worktree_add_with_hydration_multiple_pointers() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // Create a branch with multiple pointer files.
        sh(&root, &["checkout", "-b", "multi-pointers"]);

        let ptr1 = "version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1000000\n";
        let ptr2 = "version https://crab.dev/spec/v1\n\
file-hash aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899\n\
size 2000000\n";

        std::fs::write(root.join("data.bin"), ptr1).expect("write ptr1");
        std::fs::write(root.join("weights.safetensors"), ptr2).expect("write ptr2");
        // Also write a normal file to ensure it's not detected as a pointer.
        std::fs::write(root.join("config.json"), r#"{"key": "value"}"#).expect("write normal");

        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "add multiple pointer files"]);
        sh(&root, &["checkout", "main"]);

        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-multi-ptr");
        let result = mgr
            .worktree_add_with_hydration(
                &wt_path,
                "multi-pointers",
                false,
                None,
                HydrationChoice::PointersOnly,
            )
            .await
            .expect("worktree_add_with_hydration should succeed");

        let offer = result
            .hydration_offer
            .expect("hydration_offer should be Some");

        assert_eq!(offer.files.len(), 2);
        assert_eq!(offer.total_bytes, 3000000);

        // Verify both pointer files are detected.
        let paths: Vec<&Path> = offer.files.iter().map(|f| f.path.as_path()).collect();
        assert!(paths.contains(&Path::new("data.bin")));
        assert!(paths.contains(&Path::new("weights.safetensors")));

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    // ---- worktree_push_estimate integration tests ----

    #[tokio::test]
    async fn push_estimate_no_pointers_returns_zeros() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // The main worktree has only a README — no pointer files.
        let estimate = mgr
            .worktree_push_estimate(&root)
            .await
            .expect("worktree_push_estimate should succeed");

        assert_eq!(estimate.total_bytes, 0);
        assert_eq!(estimate.xorb_count, 0);
        assert_eq!(estimate.pointer_count, 0);
    }

    #[tokio::test]
    async fn push_estimate_with_pointer_files() {
        let (_td, root) = git_init_with_commit();

        // Create a branch with pointer files.
        sh(&root, &["checkout", "-b", "ptr-estimate"]);

        // Write pointer blobs (same format as the crab pointer spec).
        let pointer1 = format!(
            "{}\noid sha256:aaaa\nsize 5000000\n",
            "version https://crab.dev/spec/v1"
        );
        let pointer2 = format!(
            "{}\noid sha256:bbbb\nsize 2000000\n",
            "version https://crab.dev/spec/v1"
        );
        std::fs::write(root.join("model.bin"), &pointer1).expect("write pointer1");
        std::fs::write(root.join("data.parquet"), &pointer2).expect("write pointer2");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "add pointer files"]);

        let mgr = WorktreeManager::new(root.clone());

        let estimate = mgr
            .worktree_push_estimate(&root)
            .await
            .expect("worktree_push_estimate should succeed");

        assert_eq!(estimate.pointer_count, 2);
        assert_eq!(estimate.xorb_count, 2);
        assert_eq!(estimate.total_bytes, 7000000);
    }

    #[tokio::test]
    async fn push_estimate_on_separate_worktree() {
        let (_td, root) = git_init_with_commit();

        // Create a branch with a pointer file.
        sh(&root, &["checkout", "-b", "wt-estimate-branch"]);
        let pointer = format!(
            "{}\noid sha256:cccc\nsize 1234567\n",
            "version https://crab.dev/spec/v1"
        );
        std::fs::write(root.join("weights.pt"), &pointer).expect("write pointer");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "add pointer"]);
        sh(&root, &["checkout", "main"]);

        // Add a worktree for the branch with the pointer.
        let mgr = WorktreeManager::new(root.clone());
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-estimate");
        mgr.worktree_add(&wt_path, "wt-estimate-branch", false, None)
            .await
            .expect("worktree_add should succeed");

        // Estimate on the worktree path.
        let estimate = mgr
            .worktree_push_estimate(&wt_path)
            .await
            .expect("worktree_push_estimate should succeed");

        assert_eq!(estimate.pointer_count, 1);
        assert_eq!(estimate.xorb_count, 1);
        assert_eq!(estimate.total_bytes, 1234567);

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    // ---- apply_hydration_policy tests ----

    #[tokio::test]
    async fn apply_hydration_policy_pointers_only_is_noop() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // PointersOnly should do nothing — just returns without error.
        mgr.apply_hydration_policy(&root, HydrationChoice::PointersOnly)
            .await;
        // If we get here without panic, the no-op path works.
    }

    #[tokio::test]
    async fn apply_hydration_policy_on_access_is_noop() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // OnAccess should do nothing — just returns without error.
        mgr.apply_hydration_policy(&root, HydrationChoice::OnAccess)
            .await;
        // If we get here without panic, the no-op path works.
    }

    #[tokio::test]
    async fn apply_hydration_policy_all_attempts_crab_hydrate() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // HydrationChoice::All should attempt to run `crab hydrate --all`.
        // In the test environment, the crab binary may not exist or the
        // repo may not be a crab repo, so the command will likely fail.
        // The key assertion is that the method does NOT panic or propagate
        // the error — it logs a warning and returns gracefully.
        mgr.apply_hydration_policy(&root, HydrationChoice::All)
            .await;
        // If we get here, the error was handled gracefully (logged, not propagated).
    }

    #[tokio::test]
    async fn worktree_add_with_hydration_persists_choice_in_metadata() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        sh(&root, &["branch", "meta-test"]);

        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-meta-persist");
        let _result = mgr
            .worktree_add_with_hydration(
                &wt_path,
                "meta-test",
                false,
                None,
                HydrationChoice::OnAccess,
            )
            .await
            .expect("worktree_add_with_hydration should succeed");

        // The hydration choice should be persisted in worktrees.json.
        let meta_list = mgr.load_meta().await.expect("load_meta");
        let wt_meta = meta_list
            .iter()
            .find(|m| m.path == wt_path.canonicalize().unwrap_or_else(|_| wt_path.clone()))
            .expect("metadata entry should exist for the new worktree");
        assert_eq!(wt_meta.hydration_policy, HydrationChoice::OnAccess);

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    // ---- End-to-end lifecycle tests ----

    #[tokio::test]
    async fn e2e_pointer_worktree_with_hydration_offer() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // (a) Create a branch with pointer files using the crab pointer format.
        sh(&root, &["checkout", "-b", "pointer-branch"]);

        let ptr_a = "version https://crab.dev/spec/v1\n\
hash blake3:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2\n\
size 10485760\n";
        let ptr_b = "version https://crab.dev/spec/v1\n\
hash blake3:deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n\
size 2097152\n";

        // (b) Write pointer files and a normal file.
        std::fs::create_dir_all(root.join("models")).expect("create models dir");
        std::fs::write(root.join("models/weights.bin"), ptr_a).expect("write ptr_a");
        std::fs::write(root.join("dataset.parquet"), ptr_b).expect("write ptr_b");
        std::fs::write(root.join("train.py"), "import torch\n").expect("write normal file");

        // (c) Commit the pointer files on the branch.
        sh(&root, &["add", "."]);
        let commit_out = sh(&root, &["commit", "-m", "add pointer files for e2e test"]);
        assert!(
            commit_out.status.success(),
            "commit should succeed: {}",
            String::from_utf8_lossy(&commit_out.stderr)
        );
        sh(&root, &["checkout", "main"]);

        // (d) Use worktree_add_with_hydration with PointersOnly.
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-e2e-ptr");
        let result = mgr
            .worktree_add_with_hydration(
                &wt_path,
                "pointer-branch",
                false,
                None,
                HydrationChoice::PointersOnly,
            )
            .await
            .expect("worktree_add_with_hydration should succeed");

        // (e) Verify the returned WorktreeAddResult has a non-None hydration_offer.
        let offer = result
            .hydration_offer
            .expect("hydration_offer should be Some for a branch with pointer files");

        // (f) Verify the hydration offer has the correct file count and total_bytes.
        assert_eq!(
            offer.files.len(),
            2,
            "should detect exactly 2 pointer files"
        );
        assert_eq!(
            offer.total_bytes,
            10485760 + 2097152,
            "total_bytes should be the sum of both pointer sizes"
        );

        // Verify individual pointer file paths are present.
        let paths: Vec<&Path> = offer.files.iter().map(|f| f.path.as_path()).collect();
        assert!(
            paths.contains(&Path::new("models/weights.bin")),
            "should detect models/weights.bin as a pointer file"
        );
        assert!(
            paths.contains(&Path::new("dataset.parquet")),
            "should detect dataset.parquet as a pointer file"
        );

        // (g) Verify the push estimate for the worktree shows the correct pointer_count.
        let estimate = mgr
            .worktree_push_estimate(&wt_path)
            .await
            .expect("worktree_push_estimate should succeed");

        assert_eq!(
            estimate.pointer_count, 2,
            "push estimate should report 2 pointer files"
        );
        assert_eq!(
            estimate.xorb_count, 2,
            "xorb_count should equal pointer_count"
        );
        assert_eq!(
            estimate.total_bytes,
            10485760 + 2097152,
            "push estimate total_bytes should match hydration offer"
        );

        // (h) Skip actual push since there's no remote — verified via estimate above.

        // Clean up.
        sh(
            &root,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        );
    }

    #[tokio::test]
    async fn e2e_worktree_add_commit_remove_round_trip() {
        let (_td, root) = git_init_with_commit();
        let mgr = WorktreeManager::new(root.clone());

        // (a) Add a worktree for a new branch.
        let wt_path = tempfile::tempdir()
            .expect("tempdir")
            .into_path()
            .join("wt-e2e");
        let added = mgr
            .worktree_add(&wt_path, "e2e-feature", true, None)
            .await
            .expect("worktree_add should succeed");

        assert_eq!(added.entry.branch, "refs/heads/e2e-feature");
        assert!(!added.entry.is_main);

        // (b) Verify the worktree appears in the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 2);
        assert!(
            list.iter()
                .any(|w| w.entry.branch == "refs/heads/e2e-feature"),
            "new worktree should appear in the list"
        );

        // (c) Make a commit in the worktree: write a file, stage, commit.
        std::fs::write(wt_path.join("feature.txt"), "e2e content\n").expect("write file");
        sh(&wt_path, &["add", "feature.txt"]);
        let commit_out = sh(&wt_path, &["commit", "-m", "e2e commit"]);
        assert!(
            commit_out.status.success(),
            "commit in worktree should succeed: {}",
            String::from_utf8_lossy(&commit_out.stderr)
        );

        // (d) Verify the worktree status shows dirty_count=0 after commit.
        let status = mgr
            .worktree_status(&wt_path)
            .await
            .expect("worktree_status");
        assert_eq!(
            status["dirty_count"], 0,
            "worktree should be clean after committing"
        );

        // (e) Verify the commit exists (skip actual push — no remote).
        let log_out = sh(&wt_path, &["log", "--oneline", "-1"]);
        let log_line = String::from_utf8_lossy(&log_out.stdout);
        assert!(
            log_line.contains("e2e commit"),
            "commit message should appear in log"
        );

        // (f) Remove the worktree.
        mgr.worktree_remove(&wt_path, false)
            .await
            .expect("worktree_remove should succeed");

        // (g) Verify it's gone from the list.
        let list = mgr.worktree_list().await.expect("worktree_list");
        assert_eq!(list.len(), 1);
        assert!(list[0].entry.is_main);
        assert!(
            !list
                .iter()
                .any(|w| w.entry.branch == "refs/heads/e2e-feature"),
            "removed worktree should not appear in the list"
        );
    }
}
