//! Chunk, file, and ref stores.

pub mod manifest;
pub mod metadb;
pub mod refs;
pub mod segmented;
pub mod shard_sync;

// Re-export the metadb session surface so call sites outside
// `metadata::metadb::*` can reach the façade (`MetaDb`, `MetaDbGuard`,
// `MetaDbConfig`), the owning stores (`FileIndexStore`,
// `ChunkIndexStore`), the transaction primitives (`Transaction`,
// `DbTarget`), the thin SlateDB wrapper (`Db`), and the observable
// value types (`XorbRef`, `CacheDriftOutcome`, `PushWriteReceipt`)
// without poking into submodules.
pub use metadb::{
    CacheDriftOutcome, ChunkIndexStore, Db, DbTarget, FileIndexStore, MetaDb, MetaDbConfig,
    MetaDbEngineConfig, MetaDbGuard, PushWriteReceipt, SystemKeySnapshot, Transaction, XorbRef,
};
