//! Metadata schema and graph contracts for Crab repositories.
//!
//! This crate owns metadata payload Interfaces by default. Feature-gated
//! helpers add storage, read-only remote indexes, and the local SQLite-backed
//! dedup index without making payload-only consumers inherit those runtimes.

/// Canonical bucket-global SlateDB for chunk receipts.
pub const CHUNK_INDEX_DB_PATH: &str = ".crab/chunk_index_db/";

#[cfg(feature = "storage")]
pub mod bloom_prefilter;
pub mod chunk_index;
pub mod commit_graph;
pub mod error;
#[cfg(feature = "file-index-reader")]
pub mod file_index_lookup;
#[cfg(feature = "remote-index")]
pub mod git_object_locator;
pub mod git_visibility;
pub mod key_codec;
pub mod layout_descriptor;
#[cfg(feature = "storage")]
pub mod manifest_store;
pub mod manifests;
pub mod pack_metadata;
#[cfg(feature = "storage")]
pub mod pack_origin;
#[cfg(feature = "local-index")]
pub mod persistent_chunk_index;
#[cfg(feature = "storage")]
pub mod plan_receipt;
pub mod receipts;
#[cfg(feature = "storage")]
pub mod ref_journal;
pub mod ref_registry;
#[cfg(feature = "remote-index")]
pub mod remote_index;
pub mod segmented;
#[cfg(feature = "storage")]
pub mod segmented_store;
pub mod shallow_closure;
pub mod split_commit_graph;
pub mod transaction;
mod validation;
pub mod value_codec;
