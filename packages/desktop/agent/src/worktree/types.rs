//! Data types for the worktree manager.
//!
//! [`WorktreeEntry`] represents a worktree as reported by git itself.
//! [`WorktreeMeta`] holds UI-only metadata persisted in
//! `.crab/desktop/worktrees.json`. The manager reconciles the two
//! on every list call.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A single worktree as reported by `git worktree list --porcelain`.
///
/// This is the source of truth for worktree existence and branch
/// mapping. The agent parses porcelain output into these entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeEntry {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Branch checked out in this worktree (e.g. "refs/heads/main").
    pub branch: String,
    /// HEAD commit SHA.
    pub head: String,
    /// Whether this is the main worktree (the original `.git` dir).
    pub is_main: bool,
    /// Whether the worktree is locked (prevents pruning/removal).
    pub locked: bool,
    /// Optional lock reason.
    pub lock_reason: Option<String>,
    /// Whether the worktree directory is missing from disk (prunable/stale).
    pub prunable: bool,
}

/// UI-only metadata stored in `.crab/desktop/worktrees.json`.
///
/// This file is managed by the agent and stores display preferences
/// that have no git-level meaning. On every `worktree_list` call the
/// manager reconciles: removes stale entries whose paths no longer
/// appear in git's worktree list, preserves metadata for existing ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeMeta {
    /// Canonical path on disk; hydrated entries use [`WorktreeEntry::path`].
    pub path: PathBuf,
    /// User-chosen display name shown in the worktree card.
    pub display_name: Option<String>,
    /// Colour label for the worktree card (CSS colour string).
    pub colour: Option<String>,
    /// Whether the worktree is pinned to the top of the list.
    pub pinned: bool,
    /// Hydration policy chosen when the worktree was created.
    pub hydration_policy: HydrationChoice,
}

/// Hydration policy for pointer files in a worktree.
///
/// Chosen by the user when creating a worktree that contains pointer
/// files. Persisted in `worktrees.json` and applied via scoped
/// `crab hydrate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HydrationChoice {
    /// Hydrate all pointer files immediately after worktree creation.
    #[default]
    All,
    /// Hydrate files on first access (lazy).
    OnAccess,
    /// Keep pointer blobs as-is; user hydrates manually.
    PointersOnly,
}

/// A hydrated worktree entry combining git state with UI metadata.
///
/// Returned by `worktree_list` after reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HydratedWorktree {
    /// Git-level worktree information.
    #[serde(flatten)]
    pub entry: WorktreeEntry,
    /// UI metadata (if any exists in worktrees.json for this path).
    pub meta: Option<WorktreeMeta>,
}

/// Information about a single pointer file detected in a worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointerFileInfo {
    /// Relative path of the pointer file within the worktree.
    pub path: PathBuf,
    /// Original file size in bytes (as declared in the pointer blob).
    pub size_bytes: u64,
}

/// An offer to hydrate pointer files found in a newly created worktree.
///
/// Returned alongside the worktree entry when `worktree_add` detects
/// pointer files in the target branch. The renderer uses this to show
/// a hydration choice modal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HydrationOffer {
    /// List of pointer files with their declared sizes.
    pub files: Vec<PointerFileInfo>,
    /// Sum of all `size_bytes` across pointer files.
    pub total_bytes: u64,
}

/// Result of a `worktree_add` operation combining the created worktree
/// with an optional hydration offer when pointer files are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeAddResult {
    /// The newly created worktree entry.
    pub worktree: HydratedWorktree,
    /// Hydration offer if the branch contains pointer files.
    /// `None` when no pointer files are detected.
    pub hydration_offer: Option<HydrationOffer>,
}

/// Estimated push cost for a worktree based on its pointer files.
///
/// Returned by `worktree_push_estimate` to give the renderer a
/// size/cost preview for the worktree card's disk-usage chip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushEstimate {
    /// Total bytes across all pointer files (sum of declared sizes).
    pub total_bytes: u64,
    /// Estimated xorb count (uses unique pointer file count as proxy).
    pub xorb_count: u64,
    /// Number of pointer files detected in the worktree.
    pub pointer_count: u64,
}
