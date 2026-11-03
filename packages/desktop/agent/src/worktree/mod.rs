//! Worktree manager module.
//!
//! Provides [`WorktreeManager`] which orchestrates git worktree
//! operations and reconciles with `.crab/desktop/worktrees.json`
//! UI metadata. The manager is the single entry point for all
//! worktree-related RPC verbs (Pillar B).
//!
//! # Architecture
//!
//! Git itself (`git worktree list --porcelain`) is the source of
//! truth for worktree existence and branch mapping. The metadata
//! file stores only UI preferences (display names, colours,
//! hydration policies). On every `worktree_list` call the manager
//! reconciles the two sources.

pub(crate) mod core_cli;
pub mod manager;
pub mod types;

pub use manager::WorktreeManager;
pub use types::{
    HydratedWorktree, HydrationChoice, HydrationOffer, PointerFileInfo, PushEstimate,
    WorktreeAddResult, WorktreeEntry, WorktreeMeta,
};
