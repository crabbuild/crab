//! Per-repo generation-pinned committed file-to-shard membership.
//!
//! Owns an `Arc<Db>` (the session-lazy-opened SlateDB for this repo's
//! `file_index_db`). The store is cheap-cloneable; callers that need to
//! keep a handle across a long-lived struct can `.clone()` without
//! re-opening anything.
//!
//! # Key and value encoding
//!
//! Committed rows are keyed by file hash and manifest generation. Legacy
//! unversioned rows are read only by tests and removed after a complete
//! manifest-scoped rebuild.

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;

use crate::core::error::{CrabError, Result};
use crate::metadata::metadb::db::Db;
use crate::metadata::metadb::transaction::{DbTarget, Transaction};
use crab_metadata::key_codec::{
    PREFIX_COMMITTED, PREFIX_CONTENT, decode_committed_file_key, decode_content_key,
    encode_committed_content_prefix, encode_committed_file_key, encode_content_key,
};
use crab_metadata::value_codec::{
    CommittedFileRecord, decode_committed_file_record, encode_committed_file_record,
};
use crab_xet::hash::MerkleHash;

/// Logical database label used in error payloads and structured logs.
pub(crate) const DB_LABEL: &str = "file_index_db";

/// Cheap-cloneable owning accessor over the per-repo `file_index_db`.
///
/// Constructed by [`MetaDb::file_index`] once the underlying
/// [`Db`] handle has been lazy-opened.
///
/// [`MetaDb::file_index`]: crate::metadata::metadb::MetaDb::file_index
#[derive(Clone)]
pub struct FileIndexStore {
    db: Arc<Db>,
}

impl FileIndexStore {
    /// Wrap a shared [`Db`] handle for point-only access.
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Look up one file hash.
    ///
    /// Returns `Ok(Some(shard_hash))` on a hit, `Ok(None)` on a miss,
    /// and `MetaDbError::Read` on SlateDB IO failure. A stored value
    /// whose length is not exactly 32 bytes is reported as
    /// `MetaDbError::CorruptValue`.
    #[cfg(test)]
    pub async fn get_legacy(&self, file_hash: &MerkleHash) -> Result<Option<MerkleHash>> {
        let key = encode_content_key(file_hash);
        let raw = self.db.get(&key).await?;
        decode_legacy_value(&key, raw.as_deref())
    }

    /// Look up several file hashes in bounded parallel fan-out.
    ///
    /// Returns a `Vec` aligned with the input. Under the hood this
    /// delegates to [`Db::get_batch`], which preserves input order
    /// and caps in-flight reads.
    #[cfg(test)]
    pub async fn get_legacy_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<MerkleHash>>> {
        if file_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<Bytes> = file_hashes
            .iter()
            .map(|hash| Bytes::copy_from_slice(&encode_content_key(hash)))
            .collect();
        let raw = self.db.get_batch(&keys).await?;

        let mut out = Vec::with_capacity(raw.len());
        for (idx, value) in raw.into_iter().enumerate() {
            let key_bytes: [u8; crab_metadata::key_codec::CONTENT_KEY_LEN] = keys[idx]
                .as_ref()
                .try_into()
                .expect("encode_content_key produced a 33-byte key");
            out.push(decode_legacy_value(&key_bytes, value.as_deref())?);
        }
        Ok(out)
    }

    /// Look up generation-pinned committed records; legacy values are corrupt.
    pub async fn get_committed_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<CommittedFileRecord>>> {
        self.get_committed_batch_at(file_hashes, u64::MAX).await
    }

    /// Return the newest committed record visible at `base_generation`.
    pub async fn get_committed_batch_at(
        &self,
        file_hashes: &[MerkleHash],
        base_generation: u64,
    ) -> Result<Vec<Option<CommittedFileRecord>>> {
        if file_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(file_hashes.len());
        for file_hash in file_hashes {
            let prefix = encode_committed_content_prefix(file_hash);
            let mut rows = self.db.scan_prefix(&prefix).await?;
            let mut selected: Option<CommittedFileRecord> = None;
            while let Some(row) = rows.next().await.map_err(|source| {
                CrabError::from(crate::core::error::MetaDbError::Read {
                    db: DB_LABEL.to_owned(),
                    prefix: String::from("<committed-file>"),
                    source,
                })
            })? {
                let (key_hash, key_generation) = decode_committed_file_key(&row.key)
                    .map_err(|error| super::map_value_codec_error(error, DB_LABEL, &row.key))?;
                if key_hash != *file_hash {
                    return Err(CrabError::Internal(
                        "committed file-index prefix scan returned a different hash".to_owned(),
                    ));
                }
                let record = decode_committed_file_record(&row.value)
                    .map_err(|error| super::map_value_codec_error(error, DB_LABEL, &row.key))?;
                if record.committed_generation != key_generation {
                    return Err(CrabError::Internal(format!(
                        "committed file-index key generation {key_generation} does not match value generation {}",
                        record.committed_generation
                    )));
                }
                if key_generation <= base_generation {
                    selected = Some(record);
                }
            }
            out.push(selected);
        }
        Ok(out)
    }

    /// Record a `file_hash → shard_hash` put into the transaction.
    /// The SlateDB write happens on [`MetaDb::commit`].
    ///
    /// [`MetaDb::commit`]: crate::metadata::metadb::MetaDb::commit
    #[cfg(test)]
    pub fn save_legacy(
        &self,
        txn: &mut Transaction,
        file_hash: &MerkleHash,
        shard_hash: &MerkleHash,
    ) {
        let key = Bytes::copy_from_slice(&encode_content_key(file_hash));
        let value_bytes = crab_metadata::value_codec::encode_file_index_value(shard_hash);
        let value = Bytes::copy_from_slice(&value_bytes);
        txn.put(DbTarget::FileIndex, key, value);
    }

    /// Record a batch of puts into the transaction in appearance order.
    #[cfg(test)]
    pub fn save_legacy_batch(&self, txn: &mut Transaction, entries: &[(MerkleHash, MerkleHash)]) {
        for (file_hash, shard_hash) in entries {
            self.save_legacy(txn, file_hash, shard_hash);
        }
    }

    /// Record generation-pinned file records in one transaction.
    pub fn save_committed_batch(
        &self,
        txn: &mut Transaction,
        entries: &[(MerkleHash, CommittedFileRecord)],
    ) {
        for (file_hash, record) in entries {
            txn.put(
                DbTarget::FileIndex,
                Bytes::copy_from_slice(&encode_committed_file_key(
                    file_hash,
                    record.committed_generation,
                )),
                Bytes::copy_from_slice(&encode_committed_file_record(record)),
            );
        }
    }

    /// Record a delete into the transaction.
    pub(crate) fn delete_legacy(&self, txn: &mut Transaction, file_hash: &MerkleHash) {
        let key = Bytes::copy_from_slice(&encode_content_key(file_hash));
        txn.delete(DbTarget::FileIndex, key);
    }

    /// Return one bounded batch of legacy unversioned keys.
    pub(crate) async fn legacy_keys_batch(&self, limit: usize) -> Result<Vec<Bytes>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut rows = self.db.scan_prefix(&[PREFIX_CONTENT]).await?;
        let mut keys = Vec::with_capacity(limit);
        while keys.len() < limit {
            let Some(row) = rows.next().await.map_err(|source| {
                CrabError::from(crate::core::error::MetaDbError::Read {
                    db: DB_LABEL.to_owned(),
                    prefix: String::from("<legacy-content>"),
                    source,
                })
            })?
            else {
                break;
            };
            decode_content_key(&row.key)
                .map_err(|error| super::map_value_codec_error(error, DB_LABEL, &row.key))?;
            keys.push(row.key);
        }
        Ok(keys)
    }

    /// Tombstone a bounded legacy-key batch after a complete rebuild.
    pub(crate) fn delete_legacy_keys(&self, txn: &mut Transaction, keys: &[Bytes]) {
        for key in keys {
            txn.delete(DbTarget::FileIndex, key.clone());
        }
    }

    /// Remove generation-pinned rows whose file hash is outside the retained
    /// shard closure. The caller must hold the repository maintenance lease.
    pub(crate) async fn gc_unreferenced_committed(
        &self,
        referenced: &HashSet<MerkleHash>,
        dry_run: bool,
        batch_size: usize,
    ) -> Result<u64> {
        self.gc_unreferenced_committed_prefix(&[PREFIX_COMMITTED], referenced, dry_run, batch_size)
            .await
    }

    /// Remove unreferenced rows from one first-byte hash partition.
    pub(crate) async fn gc_unreferenced_committed_prefix(
        &self,
        prefix: &[u8],
        referenced: &HashSet<MerkleHash>,
        dry_run: bool,
        batch_size: usize,
    ) -> Result<u64> {
        let mut rows = self.db.scan_prefix(prefix).await?;
        let mut batch = slatedb::WriteBatch::new();
        let mut pending = 0usize;
        let mut removed = 0u64;

        while let Some(row) = rows.next().await.map_err(|source| {
            CrabError::from(crate::core::error::MetaDbError::Read {
                db: DB_LABEL.to_owned(),
                prefix: String::from("<committed-file-gc>"),
                source,
            })
        })? {
            let (file_hash, _) = decode_committed_file_key(&row.key)
                .map_err(|error| super::map_value_codec_error(error, DB_LABEL, &row.key))?;
            if referenced.contains(&file_hash) {
                continue;
            }

            removed += 1;
            if dry_run {
                continue;
            }
            batch.delete(row.key.as_ref());
            pending += 1;
            if pending >= batch_size.max(1) {
                self.db.write(batch).await?;
                batch = slatedb::WriteBatch::new();
                pending = 0;
            }
        }

        if pending > 0 {
            self.db.write(batch).await?;
        }
        Ok(removed)
    }

    /// Return the occupied first-four-byte hash partitions for committed rows.
    ///
    /// The bounded prefix set lets bucket GC avoid issuing one empty SlateDB
    /// scan for every possible partition while retaining a deterministic,
    /// exact sweep of every occupied hash range.
    pub(crate) async fn committed_hash_prefixes(&self) -> Result<HashSet<[u8; 4]>> {
        let mut rows = self.db.scan_prefix(&[PREFIX_COMMITTED]).await?;
        let mut prefixes = HashSet::new();
        while let Some(row) = rows.next().await.map_err(|source| {
            CrabError::from(crate::core::error::MetaDbError::Read {
                db: DB_LABEL.to_owned(),
                prefix: String::from("<committed-file-prefixes>"),
                source,
            })
        })? {
            let (file_hash, _) = decode_committed_file_key(&row.key)
                .map_err(|error| super::map_value_codec_error(error, DB_LABEL, &row.key))?;
            let bytes: [u8; 32] = file_hash.into();
            prefixes.insert([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Ok(prefixes)
    }
}

/// Decode a raw SlateDB value as a 32-byte shard hash.
///
/// `key` is consumed only for the corruption-error payload; the decode
/// itself is length-checked.
#[cfg(test)]
fn decode_legacy_value(
    key: &[u8; crab_metadata::key_codec::CONTENT_KEY_LEN],
    raw: Option<&[u8]>,
) -> Result<Option<MerkleHash>> {
    match raw {
        None => Ok(None),
        Some(bytes) => crab_metadata::value_codec::decode_file_index_value(bytes)
            .map(Some)
            .map_err(|e| super::map_value_codec_error(e, DB_LABEL, key)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;

    use super::*;
    use crate::core::error::{CrabError, MetaDbError};

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_mul(31),
            seed.wrapping_mul(97),
            seed.wrapping_mul(127),
        ])
    }

    async fn open_store() -> Arc<Db> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = Db::open(store, ObjectPath::from("file_index_db"), DB_LABEL)
            .await
            .expect("open db");
        Arc::new(db)
    }

    /// Seed a file-index entry by committing a small transaction
    /// through the public store surface, then constructing a fresh
    /// store over the same handle to read back.
    async fn seed_via_store(
        store: &FileIndexStore,
        db: &Arc<Db>,
        file_hash: &MerkleHash,
        shard_hash: &MerkleHash,
    ) {
        let mut txn = Transaction::new();
        store.save_legacy(&mut txn, file_hash, shard_hash);
        let (fi_batch, _ci_batch) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        db.write(fi_batch).await.expect("write seeded entry");
    }

    #[tokio::test]
    async fn file_index_get_returns_none_for_missing_key() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));
        let got = store
            .get_legacy(&hash_from_seed(1))
            .await
            .expect("get empty");
        assert!(got.is_none());
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_get_round_trips_a_written_pair() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));

        let file = hash_from_seed(42);
        let shard = hash_from_seed(4242);
        seed_via_store(&store, &db, &file, &shard).await;

        let got = store
            .get_legacy(&file)
            .await
            .expect("get succeeds")
            .expect("key present");
        assert_eq!(got, shard);

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_get_batch_preserves_order() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));

        // Seed 5 pairs.
        let seeded: Vec<(MerkleHash, MerkleHash)> = (0u64..5)
            .map(|i| (hash_from_seed(100 + i), hash_from_seed(900 + i)))
            .collect();
        for (f, s) in &seeded {
            seed_via_store(&store, &db, f, s).await;
        }

        // Interleave hits and misses to make sure `result[i]` is
        // aligned with input index, not keyspace order.
        let misses: Vec<MerkleHash> = (0u64..3).map(|i| hash_from_seed(9000 + i)).collect();
        let query: Vec<MerkleHash> = vec![
            seeded[3].0,
            misses[0],
            seeded[0].0,
            seeded[4].0,
            misses[1],
            seeded[1].0,
            misses[2],
            seeded[2].0,
        ];
        let results = store.get_legacy_batch(&query).await.expect("batch");
        let expected: Vec<Option<MerkleHash>> = vec![
            Some(seeded[3].1),
            None,
            Some(seeded[0].1),
            Some(seeded[4].1),
            None,
            Some(seeded[1].1),
            None,
            Some(seeded[2].1),
        ];
        assert_eq!(results, expected);

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_get_batch_empty_is_empty() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));
        let got = store.get_legacy_batch(&[]).await.expect("empty batch");
        assert!(got.is_empty());
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_save_batch_records_every_entry() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));

        let entries: Vec<(MerkleHash, MerkleHash)> = (0u64..3)
            .map(|i| (hash_from_seed(i), hash_from_seed(100 + i)))
            .collect();

        let mut txn = Transaction::new();
        store.save_legacy_batch(&mut txn, &entries);
        let (fi_batch, _ci_batch) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        db.write(fi_batch).await.expect("write");

        for (f, s) in &entries {
            assert_eq!(
                store.get_legacy(f).await.expect("get").expect("present"),
                *s,
                "entry for {f:?} must round-trip"
            );
        }

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_delete_removes_entry() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));

        let f = hash_from_seed(5);
        let s = hash_from_seed(50);
        seed_via_store(&store, &db, &f, &s).await;
        assert!(store.get_legacy(&f).await.expect("get").is_some());

        let mut txn = Transaction::new();
        store.delete_legacy(&mut txn, &f);
        let (fi_batch, _ci_batch) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        db.write(fi_batch).await.expect("write delete");

        assert!(
            store
                .get_legacy(&f)
                .await
                .expect("get after delete")
                .is_none(),
            "delete must remove the entry"
        );

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_gc_tombstones_only_unreferenced_committed_rows() {
        let db = open_store().await;
        let store = FileIndexStore::new(Arc::clone(&db));
        let retained = hash_from_seed(10);
        let stale = hash_from_seed(20);
        let entries = [
            (
                retained,
                CommittedFileRecord {
                    recipe_hash: [1; 32],
                    shard_hash: hash_from_seed(100),
                    committed_generation: 1,
                    shard_index_hash: hash_from_seed(200),
                },
            ),
            (
                stale,
                CommittedFileRecord {
                    recipe_hash: [2; 32],
                    shard_hash: hash_from_seed(101),
                    committed_generation: 1,
                    shard_index_hash: hash_from_seed(201),
                },
            ),
            (
                stale,
                CommittedFileRecord {
                    recipe_hash: [3; 32],
                    shard_hash: hash_from_seed(102),
                    committed_generation: 2,
                    shard_index_hash: hash_from_seed(202),
                },
            ),
        ];
        let mut txn = Transaction::new();
        store.save_committed_batch(&mut txn, &entries);
        let (batch, _) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        db.write(batch).await.expect("seed committed rows");

        let removed = store
            .gc_unreferenced_committed(&HashSet::from([retained]), false, 1)
            .await
            .expect("sweep stale rows");

        assert_eq!(removed, 2);
        assert!(store.get_committed_batch(&[retained]).await.unwrap()[0].is_some());
        assert!(store.get_committed_batch(&[stale]).await.unwrap()[0].is_none());
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn file_index_get_reports_corrupt_value_on_wrong_length() {
        let db = open_store().await;

        // Seed a malformed value through a raw WriteBatch so the
        // store's read path hits the length check.
        let f = hash_from_seed(7);
        let mut batch = slatedb::WriteBatch::new();
        batch.put(
            encode_content_key(&f).as_slice(),
            b"not-32-bytes".as_slice(),
        );
        db.write(batch).await.expect("seed corrupt value");

        let store = FileIndexStore::new(Arc::clone(&db));
        let err = store
            .get_legacy(&f)
            .await
            .expect_err("corrupt value must fail");
        match err {
            CrabError::MetaDb(MetaDbError::CorruptValue {
                db: label, reason, ..
            }) => {
                assert_eq!(label, DB_LABEL);
                assert!(
                    reason.contains("32-byte"),
                    "reason should name the invariant: {reason}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }

        db.close().await.expect("close");
    }
}
