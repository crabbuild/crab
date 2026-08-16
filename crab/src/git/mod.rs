//! Filter driver and remote helper glue.

pub mod clean;
pub mod connectivity;
pub mod delta_reconstruct;
pub mod discover;

#[cfg(feature = "gix-worktree")]
pub mod checkout;
pub mod fetch;
#[cfg(feature = "gix-transport")]
pub mod fetch_transport;
pub mod filter_process;
pub mod incremental_walk;
pub(crate) mod index;
pub mod pack;
#[cfg(feature = "gix-pack-native")]
pub mod pack_native;
pub mod prefetch;
pub mod progress;
pub(crate) mod protected_push;
pub mod push;
#[cfg(feature = "gix-ref-edits")]
pub mod push_edits;
pub mod push_native;
pub mod remote_helper;
pub mod shallow;
pub mod smudge;
pub mod store_client;
pub mod url;
pub mod worktree;
pub mod worktree_hydration;

#[cfg(feature = "gix-facade")]
pub use crab_git::facade;
pub use crab_git::{filter_attr_cache, odb_adapter, push_state, refname, reject_reason, walk};
