#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::await_holding_lock
)]
#![warn(clippy::perf, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::struct_excessive_bools,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::result_large_err,
    clippy::match_same_arms,
    clippy::unused_self,
    clippy::items_after_statements,
    clippy::needless_return,
    clippy::assigning_clones,
    clippy::unnecessary_semicolon,
    clippy::new_without_default,
    clippy::unnecessary_wraps,
    clippy::question_mark,
    clippy::needless_continue,
    clippy::single_match_else,
    clippy::ignored_unit_patterns,
    clippy::used_underscore_binding,
    clippy::pub_underscore_fields
)]

//! Virtual filesystem mounts, snapshots, overlays, and hydration for Crab.

pub mod chunk_cache;
pub mod data_plane;
pub mod error;
#[cfg(feature = "fuse")]
mod executable;
pub mod fuse_prereq;
pub mod integration;

pub use chunk_cache::ChunkCache;
pub use error::{Result, VfsError};
pub use integration::{MountReadContext, MountReadResolver, NoopMountReadResolver};

/// Object-store routing used by mounted repositories.
pub type StoreLayout = crab_storage::StoreLayout<crab_storage::Store>;

pub mod core {
    pub mod error {
        pub use crate::error::{Result, VfsError as CrabError};
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod daemon;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod engine;
#[cfg(feature = "fuse")]
pub mod fuse;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod hydration;
#[cfg(feature = "fuse")]
pub mod mount;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod mount_control;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod mount_runtime;
#[cfg(feature = "nfs")]
pub mod nfs;
#[cfg(feature = "nfs")]
pub mod nfs_control;
#[cfg(feature = "nfs")]
pub mod nfs_mount;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod overlay;
#[cfg(feature = "nfs")]
pub mod read_lease_pool;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod refresh;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod resolver;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod snapshot;

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod clone_cache;
#[cfg(feature = "fuse")]
pub mod coordinator;
#[cfg(feature = "fuse")]
pub mod daemonize;
#[cfg(feature = "fuse")]
pub mod ipc_client;
#[cfg(feature = "fuse")]
pub mod ipc_server;
#[cfg(feature = "fuse")]
pub mod logging;
pub mod mounts_registry;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod pipeline;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod publish;
#[cfg(feature = "fuse")]
pub mod signal_handler;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod source;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub mod verified_set;

#[cfg(test)]
mod test_support;
