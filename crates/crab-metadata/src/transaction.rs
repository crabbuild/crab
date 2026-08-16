//! Metadata write operation contracts.
//!
//! A [`Transaction`] describes ordered writes across Crab's two metadata
//! indexes. Runtime adapters decide how to apply the operations; this Module
//! owns only the target, ordering, and byte-accounting contract.

use bytes::Bytes;

/// Which metadata index a given operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbTarget {
    /// Per-repo `file_index_db`: `file_hash -> shard_hash`.
    FileIndex,
    /// Global `chunk_index_db`: `chunk_hash -> XorbRef`.
    ChunkIndex,
}

/// One put-or-delete operation tagged with its target metadata index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionOp {
    /// Put `value` at `key` in `target`.
    Put {
        /// Target metadata index.
        target: DbTarget,
        /// Encoded metadata key.
        key: Bytes,
        /// Encoded metadata value.
        value: Bytes,
    },
    /// Delete `key` from `target`.
    Delete {
        /// Target metadata index.
        target: DbTarget,
        /// Encoded metadata key.
        key: Bytes,
    },
}

/// Ordered write description spanning both metadata indexes.
#[derive(Debug, Default, Clone)]
pub struct Transaction {
    ops: Vec<TransactionOp>,
}

impl Transaction {
    /// Build an empty transaction.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the transaction holds any operations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Total number of operations across both targets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Record a put operation.
    pub fn put(&mut self, target: DbTarget, key: Bytes, value: Bytes) {
        self.ops.push(TransactionOp::Put { target, key, value });
    }

    /// Record a delete operation.
    pub fn delete(&mut self, target: DbTarget, key: Bytes) {
        self.ops.push(TransactionOp::Delete { target, key });
    }

    /// Count operations per target as `(file_index_ops, chunk_index_ops)`.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        let mut file = 0usize;
        let mut chunk = 0usize;
        for op in &self.ops {
            match op.target() {
                DbTarget::FileIndex => file += 1,
                DbTarget::ChunkIndex => chunk += 1,
            }
        }
        (file, chunk)
    }

    /// Byte volume per target as `(file_index_bytes, chunk_index_bytes)`.
    ///
    /// Put volume is key length plus value length. Delete volume is key length.
    #[must_use]
    pub fn byte_volume(&self) -> (u64, u64) {
        let mut file = 0u64;
        let mut chunk = 0u64;
        for op in &self.ops {
            let bytes = op.byte_volume();
            match op.target() {
                DbTarget::FileIndex => file += bytes,
                DbTarget::ChunkIndex => chunk += bytes,
            }
        }
        (file, chunk)
    }

    /// Consume the transaction into its ordered operation list.
    #[must_use]
    pub fn into_ops(self) -> Vec<TransactionOp> {
        self.ops
    }
}

impl TransactionOp {
    /// Return the operation target.
    #[must_use]
    pub fn target(&self) -> DbTarget {
        match self {
            Self::Put { target, .. } | Self::Delete { target, .. } => *target,
        }
    }

    fn byte_volume(&self) -> u64 {
        match self {
            Self::Put { key, value, .. } => (key.len() + value.len()) as u64,
            Self::Delete { key, .. } => key.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> Bytes {
        Bytes::from(vec![n])
    }

    fn val(n: u8) -> Bytes {
        Bytes::from(vec![n, n])
    }

    #[test]
    fn empty_transaction_is_empty_and_len_zero() {
        let txn = Transaction::new();

        assert!(txn.is_empty());
        assert_eq!(txn.len(), 0);
        assert_eq!(txn.counts(), (0, 0));
    }

    #[test]
    fn mixed_ops_counted_per_target() {
        let mut txn = Transaction::new();
        txn.put(DbTarget::FileIndex, key(1), val(1));
        txn.put(DbTarget::ChunkIndex, key(2), val(2));
        txn.put(DbTarget::ChunkIndex, key(3), val(3));
        txn.delete(DbTarget::FileIndex, key(4));
        txn.delete(DbTarget::ChunkIndex, key(5));

        assert_eq!(txn.len(), 5);
        assert_eq!(txn.counts(), (2, 3));
    }

    #[test]
    fn byte_volume_sums_per_target() {
        let mut txn = Transaction::new();
        txn.put(DbTarget::FileIndex, key(1), val(1));
        txn.put(DbTarget::ChunkIndex, key(2), val(2));
        txn.put(DbTarget::ChunkIndex, key(3), val(3));
        txn.delete(DbTarget::ChunkIndex, key(4));

        assert_eq!(txn.byte_volume(), (3, 7));
    }

    #[test]
    fn operations_preserve_recording_order() {
        let mut txn = Transaction::new();
        let k = key(7);
        txn.put(DbTarget::FileIndex, k.clone(), val(7));
        txn.delete(DbTarget::FileIndex, k);

        let ops = txn.into_ops();
        assert!(matches!(ops[0], TransactionOp::Put { .. }));
        assert!(matches!(ops[1], TransactionOp::Delete { .. }));
    }
}
