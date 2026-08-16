//! SlateDB adapter for metadata transaction operations.
//!
//! `crab_metadata::transaction` owns the ordered operation contract. This
//! Module only lowers that contract into the two SlateDB write batches used by
//! the current `MetaDb` runtime.

use std::time::Duration;

pub use crab_metadata::transaction::{DbTarget, Transaction, TransactionOp};

/// Split a metadata transaction into per-database SlateDB write batches.
///
/// Returned as `(file_index_batch, chunk_index_batch)`. Either batch may be
/// empty. Operations are applied in recorded order; SlateDB's batch semantics
/// resolve same-key put/delete conflicts with last-write-wins behavior.
pub(crate) fn into_per_db_batches(txn: Transaction) -> (slatedb::WriteBatch, slatedb::WriteBatch) {
    let mut fi = slatedb::WriteBatch::new();
    let mut ci = slatedb::WriteBatch::new();

    for op in txn.into_ops() {
        match op {
            TransactionOp::Put {
                target: DbTarget::FileIndex,
                key,
                value,
            } => fi.put(key.as_ref(), value.as_ref()),
            TransactionOp::Put {
                target: DbTarget::ChunkIndex,
                key,
                value,
            } => ci.put(key.as_ref(), value.as_ref()),
            TransactionOp::Delete {
                target: DbTarget::FileIndex,
                key,
            } => fi.delete(key.as_ref()),
            TransactionOp::Delete {
                target: DbTarget::ChunkIndex,
                key,
            } => ci.delete(key.as_ref()),
        }
    }

    (fi, ci)
}

/// Observable outcome of a [`MetaDb::commit`] call.
///
/// Collected so the push pipeline can log structured fields without re-opening
/// the stores. Epoch slots are reserved for the `sys:epoch` read-bump-write
/// cycle that lands with the observability layer; today they are always `0`.
///
/// [`MetaDb::commit`]: super::MetaDb::commit
#[derive(Debug, Default, Clone)]
pub struct PushWriteReceipt {
    /// Per-repo `file_index_db` epoch after the write. Currently `0`.
    pub file_index_epoch: u64,

    /// Global `chunk_index_db` epoch after the write. Currently `0`.
    pub chunk_index_epoch: u64,

    /// Number of ops (puts + deletes) flushed into `file_index_db`.
    pub file_ops_written: u64,

    /// Number of ops (puts + deletes) flushed into `chunk_index_db`.
    pub chunk_ops_written: u64,

    /// Total bytes across both databases.
    pub bytes_written: u64,

    /// Wall-clock elapsed around the parallel commit fan-out.
    pub elapsed: Duration,
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn empty_transaction_lowers_to_empty_batches_without_panic() {
        let txn = Transaction::new();
        let (_fi, _ci) = into_per_db_batches(txn);
    }

    #[test]
    fn ordered_operations_lower_without_panic() {
        let mut txn = Transaction::new();
        let key = Bytes::from_static(b"k");
        txn.put(DbTarget::FileIndex, key.clone(), Bytes::from_static(b"v"));
        txn.delete(DbTarget::FileIndex, key);

        let (_fi, _ci) = into_per_db_batches(txn);
    }
}
