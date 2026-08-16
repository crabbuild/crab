//! Chunk-level diff engine.
//!
//! Compares crab-tracked files between git refs using only metadata
//! (file-index + shards), producing per-file reports of which chunks
//! changed, bytes affected, and reuse ratio — with zero data transfer.

pub mod format_hint;
pub mod formatter;
pub mod term_resolver;
