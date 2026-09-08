//! Write-side SlateDB helpers for Crab's remote metadata indexes.
//!
//! Server adapters use canonical file/chunk codecs through this module.
//! One-shot helpers close their own handles; long-lived writers transfer
//! explicit close ownership to their caller.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;

use crate::error::{MetadataError, Result};
use crate::key_codec::{
    decode_committed_chunk_key, encode_committed_chunk_head_key, encode_committed_chunk_key,
    encode_committed_content_prefix, encode_committed_file_key, encode_origin_proof_key,
    encode_source_anchor_key,
};
use crate::receipts::{
    CommittedChunkPlacement, CommittedChunkReceipt, OriginReceipt, SourceAnchor,
};
use crate::value_codec::{CommittedFileRecord, encode_committed_file_record};
use crab_xet::xorb::format::{MerkleHash, XorbRef};

const FILE_INDEX_DB_LABEL: &str = "file_index_db";
const CHUNK_INDEX_DB_LABEL: &str = "chunk_index_db";

/// Default L0 target for the global chunk index.
///
/// One high-entropy 5 GiB file produces roughly 32 MiB of compact placement
/// rows. Keeping that generation in one memtable avoids object-store and
/// compaction amplification while SlateDB's backpressure still bounds larger
/// publications.
pub const DEFAULT_CHUNK_INDEX_L0_SST_SIZE_BYTES: u64 = 64 * 1024 * 1024;

/// Remote SlateDB paths for Crab's file and chunk indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIndexConfig {
    /// Object-store path for the repo-scoped `file_index_db`.
    pub file_index_path: String,
    /// Object-store path for the global `chunk_index_db`.
    pub chunk_index_path: String,
}

impl RemoteIndexConfig {
    /// Build index paths for a repository using Crab's default global prefix.
    #[must_use]
    pub fn for_repo(repo_prefix: &str) -> Self {
        Self {
            file_index_path: file_index_path(repo_prefix),
            chunk_index_path: String::from(crate::CHUNK_INDEX_DB_PATH),
        }
    }

    /// Build index paths for a repository and an explicit global prefix.
    #[must_use]
    pub fn for_repo_with_global_prefix(repo_prefix: &str, global_prefix: &str) -> Self {
        Self {
            file_index_path: file_index_path(repo_prefix),
            chunk_index_path: chunk_index_path(global_prefix),
        }
    }
}

fn file_index_path(repo_prefix: &str) -> String {
    format!("{}/file_index_db/", repo_prefix.trim_end_matches('/'))
}

fn chunk_index_path(global_prefix: &str) -> String {
    format!("{}/chunk_index_db/", global_prefix.trim_end_matches('/'))
}

/// Write file-index and chunk-index entries to their remote SlateDB indexes.
///
/// Empty entry sets do not open their corresponding database. Any database
/// opened by this function is closed before the result is returned; when both
/// a write and close fail, the write error is returned. Await this future to
/// completion; dropping it cannot perform asynchronous cleanup.
pub async fn write_index_entries(
    store: Arc<dyn ObjectStore>,
    config: &RemoteIndexConfig,
    file_entries: &[(MerkleHash, CommittedFileRecord)],
    committed_chunk_entries: &[(MerkleHash, CommittedChunkReceipt)],
) -> Result<()> {
    if file_entries.is_empty() && committed_chunk_entries.is_empty() {
        return Ok(());
    }
    let writer = RemoteIndexWriter::open(
        store,
        config,
        !file_entries.is_empty(),
        !committed_chunk_entries.is_empty(),
    )
    .await?;
    let result = writer
        .write_entries(file_entries, committed_chunk_entries)
        .await;
    let close_result = writer.close().await;
    result?;
    close_result
}

/// Long-lived writer for caller-bounded remote index batches.
///
/// Writes buffer data without waiting for durability. Always await [`Self::close`]
/// before reporting publication success, including after a batch error. This
/// writer does not provide an atomic transaction across file and chunk indexes.
pub struct RemoteIndexWriter {
    file_db: Option<slatedb::Db>,
    chunk_db: Option<slatedb::Db>,
}

impl RemoteIndexWriter {
    /// Open only the indexes selected by the caller.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        config: &RemoteIndexConfig,
        write_files: bool,
        write_chunks: bool,
    ) -> Result<Self> {
        let file_db = if write_files {
            Some(
                open_writer(
                    Arc::clone(&store),
                    &config.file_index_path,
                    FILE_INDEX_DB_LABEL,
                )
                .await?,
            )
        } else {
            None
        };
        let chunk_db = if write_chunks {
            match open_writer(store, &config.chunk_index_path, CHUNK_INDEX_DB_LABEL).await {
                Ok(db) => Some(db),
                Err(error) => {
                    let _ = close_writer(file_db, FILE_INDEX_DB_LABEL).await;
                    return Err(error);
                }
            }
        } else {
            None
        };
        Ok(Self { file_db, chunk_db })
    }

    /// Buffer one caller-bounded batch in the opened databases.
    ///
    /// Nonempty entries require their index to have been selected in [`Self::open`].
    /// An unopened index returns [`MetadataError::Internal`] before either index
    /// is written. Other errors may occur after file entries were buffered;
    /// callers must still close the writer. Success is not a durability barrier.
    pub async fn write_entries(
        &self,
        file_entries: &[(MerkleHash, CommittedFileRecord)],
        committed_chunk_entries: &[(MerkleHash, CommittedChunkReceipt)],
    ) -> Result<()> {
        write_opened_entries(
            self.file_db.as_ref(),
            self.chunk_db.as_ref(),
            file_entries,
            committed_chunk_entries,
        )
        .await
    }

    /// Flush and close every database opened by this writer.
    ///
    /// Both closes are attempted. If both fail, return the file-index error.
    /// Await this future through completion; dropping it cannot finish cleanup.
    pub async fn close(self) -> Result<()> {
        close_opened_writers(self.file_db, self.chunk_db).await
    }
}

/// Read one chunk-index entry from a remote index.
///
/// This is intended for owner-crate tests and diagnostics; normal read paths
/// resolve file reconstruction through the shared read orchestration module.
/// Returns the head candidate when present. Otherwise, scans validated history
/// and selects the greatest (committed generation, placement ID), matching the
/// writer's ordering within a batch. A candidate is not proof of current source
/// visibility or object availability.
pub async fn read_chunk_index_entry(
    store: Arc<dyn ObjectStore>,
    config: &RemoteIndexConfig,
    chunk_hash: &MerkleHash,
) -> Result<Option<XorbRef>> {
    let reader = match open_reader(store, &config.chunk_index_path, CHUNK_INDEX_DB_LABEL).await? {
        Some(reader) => reader,
        None => return Ok(None),
    };
    let result = async {
        let head_key = encode_committed_chunk_head_key(chunk_hash);
        let head = reader
            .get(&head_key)
            .await
            .map_err(|source| MetadataError::SlateDbRead {
                db: CHUNK_INDEX_DB_LABEL.to_owned(),
                source,
            })?;
        if let Some(value) = head.as_deref() {
            let placement = decode_placement(chunk_hash, value, None)?;
            let receipt = resolve_receipt(&reader, placement).await?;
            return Ok(Some(XorbRef {
                xorb_hash: MerkleHash::from(receipt.xorb_hash),
                chunk_index: receipt.chunk_index,
                uncompressed_size: receipt.uncompressed_size,
            }));
        }

        let prefix = encode_committed_content_prefix(chunk_hash);
        let mut rows =
            reader
                .scan_prefix(&prefix, ..)
                .await
                .map_err(|source| MetadataError::SlateDbRead {
                    db: CHUNK_INDEX_DB_LABEL.to_owned(),
                    source,
                })?;
        let mut selected: Option<([u8; 32], CommittedChunkReceipt)> = None;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|source| MetadataError::SlateDbRead {
                db: CHUNK_INDEX_DB_LABEL.to_owned(),
                source,
            })?
        {
            let (key_hash, receipt_id) = decode_committed_chunk_key(&row.key)?;
            if key_hash != *chunk_hash {
                return Err(MetadataError::CorruptObject {
                    path: CHUNK_INDEX_DB_LABEL.to_owned(),
                    reason: "committed chunk prefix returned a different hash".to_owned(),
                });
            }
            let placement = decode_placement(chunk_hash, &row.value, Some(receipt_id))?;
            let receipt = resolve_receipt(&reader, placement).await?;
            // Use the writer's tie-break so removing a head does not change
            // which placement wins among the same batch's immutable rows.
            if selected.as_ref().is_none_or(|(prior_id, prior)| {
                (receipt.committed_generation, receipt_id) > (prior.committed_generation, *prior_id)
            }) {
                selected = Some((receipt_id, receipt));
            }
        }
        Ok(selected.map(|(_, receipt)| XorbRef {
            xorb_hash: MerkleHash::from(receipt.xorb_hash),
            chunk_index: receipt.chunk_index,
            uncompressed_size: receipt.uncompressed_size,
        }))
    }
    .await;
    let close_result = reader
        .close()
        .await
        .map_err(|source| MetadataError::SlateDbClose {
            db: CHUNK_INDEX_DB_LABEL.to_owned(),
            source,
        });

    let value = result?;
    close_result?;
    Ok(value)
}

async fn write_opened_entries(
    file_db: Option<&slatedb::Db>,
    chunk_db: Option<&slatedb::Db>,
    file_entries: &[(MerkleHash, CommittedFileRecord)],
    committed_chunk_entries: &[(MerkleHash, CommittedChunkReceipt)],
) -> Result<()> {
    // Validate both selections before buffering anything: accepting half a batch
    // would hide caller mistakes behind a successful batch result.
    for (opened, has_entries, label) in [
        (
            file_db.is_some(),
            !file_entries.is_empty(),
            FILE_INDEX_DB_LABEL,
        ),
        (
            chunk_db.is_some(),
            !committed_chunk_entries.is_empty(),
            CHUNK_INDEX_DB_LABEL,
        ),
    ] {
        if has_entries && !opened {
            return Err(MetadataError::Internal(format!(
                "{label} was not opened for writing"
            )));
        }
    }

    if let Some(db) = file_db
        && !file_entries.is_empty()
    {
        let mut batch = slatedb::WriteBatch::new();
        for (file_hash, record) in file_entries {
            batch.put(
                encode_committed_file_key(file_hash, record.committed_generation).as_slice(),
                encode_committed_file_record(record).as_slice(),
            );
        }
        db.write_with_options(
            batch,
            &slatedb::config::WriteOptions {
                await_durable: false,
                ..slatedb::config::WriteOptions::default()
            },
        )
        .await
        .map(|_| ())
        .map_err(|source| MetadataError::SlateDbWrite {
            db: FILE_INDEX_DB_LABEL.to_owned(),
            source,
        })?;
    }

    if let Some(db) = chunk_db
        && !committed_chunk_entries.is_empty()
    {
        let mut batch = slatedb::WriteBatch::new();
        let mut heads: HashMap<MerkleHash, (u64, CommittedChunkPlacement)> = HashMap::new();
        let mut persisted_proofs = HashSet::new();
        let mut persisted_anchors = HashSet::new();
        for (chunk_hash, receipt) in committed_chunk_entries {
            if MerkleHash::from(receipt.chunk_hash) != *chunk_hash {
                return Err(MetadataError::Internal(
                    "committed chunk receipt key mismatch".to_owned(),
                ));
            }
            receipt.validate(receipt.committed_generation, receipt.shard_index_hash)?;
            let source = receipt.source_anchor();
            let placement = receipt.compact_placement();
            let proof_id = placement.origin_proof_id;
            let anchor_id = placement.source_anchor_id;
            if persisted_proofs.insert(proof_id) {
                let value = serde_json::to_vec(&receipt.origin).map_err(|error| {
                    MetadataError::Internal(format!("origin proof serialize failed: {error}"))
                })?;
                batch.put(
                    encode_origin_proof_key(&proof_id).as_slice(),
                    value.as_slice(),
                );
            }
            if persisted_anchors.insert(anchor_id) {
                let value = serde_json::to_vec(&source).map_err(|error| {
                    MetadataError::Internal(format!("source anchor serialize failed: {error}"))
                })?;
                batch.put(
                    encode_source_anchor_key(&anchor_id).as_slice(),
                    value.as_slice(),
                );
            }
            let value = placement.encode()?;
            batch.put(
                encode_committed_chunk_key(chunk_hash, &placement.placement_id()).as_slice(),
                value.as_slice(),
            );
            let replace = heads.get(chunk_hash).is_none_or(|(generation, prior)| {
                receipt
                    .committed_generation
                    .cmp(generation)
                    .then_with(|| placement.placement_id().cmp(&prior.placement_id()))
                    .is_gt()
            });
            if replace {
                heads.insert(*chunk_hash, (receipt.committed_generation, placement));
            }
        }
        for (chunk_hash, (_, placement)) in heads {
            let value = placement.encode()?;
            batch.put(
                encode_committed_chunk_head_key(&chunk_hash).as_slice(),
                value.as_slice(),
            );
        }
        db.write_with_options(
            batch,
            &slatedb::config::WriteOptions {
                await_durable: false,
                ..slatedb::config::WriteOptions::default()
            },
        )
        .await
        .map(|_| ())
        .map_err(|source| MetadataError::SlateDbWrite {
            db: CHUNK_INDEX_DB_LABEL.to_owned(),
            source,
        })?;
    }

    Ok(())
}

fn decode_placement(
    chunk_hash: &MerkleHash,
    value: &[u8],
    expected_receipt_id: Option<[u8; 32]>,
) -> Result<CommittedChunkPlacement> {
    let placement =
        CommittedChunkPlacement::decode(value).map_err(|error| MetadataError::CorruptObject {
            path: CHUNK_INDEX_DB_LABEL.to_owned(),
            reason: format!("committed chunk placement decode failed: {error}"),
        })?;
    if MerkleHash::from(placement.chunk_hash) != *chunk_hash
        || expected_receipt_id.is_some_and(|expected| placement.placement_id() != expected)
    {
        return Err(MetadataError::CorruptObject {
            path: CHUNK_INDEX_DB_LABEL.to_owned(),
            reason: "committed chunk key does not match its placement".to_owned(),
        });
    }
    Ok(placement)
}

async fn resolve_receipt(
    reader: &slatedb::DbReader,
    placement: CommittedChunkPlacement,
) -> Result<CommittedChunkReceipt> {
    let proof_key = encode_origin_proof_key(&placement.origin_proof_id);
    let proof = reader
        .get(&proof_key)
        .await
        .map_err(|source| MetadataError::SlateDbRead {
            db: CHUNK_INDEX_DB_LABEL.to_owned(),
            source,
        })?
        .ok_or_else(|| missing_receipt_record("origin proof", placement.origin_proof_id))?;
    let origin: OriginReceipt =
        serde_json::from_slice(&proof).map_err(|error| MetadataError::CorruptObject {
            path: CHUNK_INDEX_DB_LABEL.to_owned(),
            reason: format!("origin proof decode failed: {error}"),
        })?;
    if origin.proof_id() != placement.origin_proof_id {
        return Err(MetadataError::CorruptObject {
            path: CHUNK_INDEX_DB_LABEL.to_owned(),
            reason: "origin proof key does not match its value".to_owned(),
        });
    }

    let anchor_key = encode_source_anchor_key(&placement.source_anchor_id);
    let anchor = reader
        .get(&anchor_key)
        .await
        .map_err(|source| MetadataError::SlateDbRead {
            db: CHUNK_INDEX_DB_LABEL.to_owned(),
            source,
        })?
        .ok_or_else(|| missing_receipt_record("source anchor", placement.source_anchor_id))?;
    let source: SourceAnchor =
        serde_json::from_slice(&anchor).map_err(|error| MetadataError::CorruptObject {
            path: CHUNK_INDEX_DB_LABEL.to_owned(),
            reason: format!("source anchor decode failed: {error}"),
        })?;
    if source.anchor_id() != placement.source_anchor_id {
        return Err(MetadataError::CorruptObject {
            path: CHUNK_INDEX_DB_LABEL.to_owned(),
            reason: "source anchor key does not match its value".to_owned(),
        });
    }
    CommittedChunkReceipt::from_compact(placement, origin, source)
}

fn missing_receipt_record(kind: &str, _id: [u8; 32]) -> MetadataError {
    MetadataError::CorruptObject {
        path: CHUNK_INDEX_DB_LABEL.to_owned(),
        reason: format!("compact committed chunk references missing {kind}"),
    }
}

async fn open_writer(
    store: Arc<dyn ObjectStore>,
    path: &str,
    db: &'static str,
) -> Result<slatedb::Db> {
    let l0_sst_size_bytes = usize::try_from(DEFAULT_CHUNK_INDEX_L0_SST_SIZE_BYTES)
        .map_err(|error| MetadataError::Internal(format!("L0 SST size unsupported: {error}")))?;
    let settings = slatedb::config::Settings {
        // Publication owns the final close barrier. Timer flushes turn bounded
        // replay batches into one remote WAL object each without making the
        // pre-visibility result any safer.
        flush_interval: None,
        l0_sst_size_bytes,
        ..slatedb::config::Settings::default()
    };
    slatedb::Db::builder(ObjectPath::from(path), store)
        .with_settings(settings)
        .build()
        .await
        .map_err(|source| MetadataError::SlateDbOpen {
            db: db.to_owned(),
            path: path.to_owned(),
            source,
        })
}

async fn open_reader(
    store: Arc<dyn ObjectStore>,
    path: &str,
    db: &'static str,
) -> Result<Option<slatedb::DbReader>> {
    match slatedb::DbReader::builder(ObjectPath::from(path), store)
        .build()
        .await
    {
        Ok(reader) => Ok(Some(reader)),
        Err(source) if is_manifest_missing(&source) => Ok(None),
        Err(source) => Err(MetadataError::SlateDbOpen {
            db: db.to_owned(),
            path: path.to_owned(),
            source,
        }),
    }
}

async fn close_opened_writers(
    file_db: Option<slatedb::Db>,
    chunk_db: Option<slatedb::Db>,
) -> Result<()> {
    let file_result = close_writer(file_db, FILE_INDEX_DB_LABEL).await;
    let chunk_result = close_writer(chunk_db, CHUNK_INDEX_DB_LABEL).await;

    file_result?;
    chunk_result
}

async fn close_writer(db: Option<slatedb::Db>, label: &'static str) -> Result<()> {
    let Some(db) = db else {
        return Ok(());
    };
    db.close()
        .await
        .map_err(|source| MetadataError::SlateDbClose {
            db: label.to_owned(),
            source,
        })
}

fn is_manifest_missing(err: &slatedb::Error) -> bool {
    if !matches!(err.kind(), slatedb::ErrorKind::Data) {
        return false;
    }
    err.to_string()
        .contains("failed to find latest transactional object")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use object_store::memory::InMemory;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed.wrapping_mul(31), seed.wrapping_mul(97), seed])
    }

    #[tokio::test]
    async fn unopened_index_rejects_entries_before_writing_either_index() {
        let hash = hash_from_seed(1);
        let files = [(
            hash,
            CommittedFileRecord {
                recipe_hash: [1; 32],
                shard_hash: hash,
                committed_generation: 1,
                shard_index_hash: hash,
            },
        )];
        let chunks = [(
            hash,
            CommittedChunkReceipt {
                schema_version: crate::receipts::RECEIPT_SCHEMA_VERSION,
                chunk_hash: hash.into(),
                xorb_hash: hash.into(),
                chunk_index: 0,
                uncompressed_size: 100,
                origin: OriginReceipt::new(
                    "origin".into(),
                    "xorbs/object".into(),
                    hash.into(),
                    [9; 32],
                    100,
                    None,
                    None,
                ),
                source_repo_prefix: "org/repo".into(),
                source_shard_hash: hash.into(),
                committed_generation: 1,
                shard_index_hash: hash.into(),
                gc_registry_generation: 1,
            },
        )];
        for (write_files, write_chunks, missing) in [
            (false, true, FILE_INDEX_DB_LABEL),
            (true, false, CHUNK_INDEX_DB_LABEL),
        ] {
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let config = RemoteIndexConfig::for_repo("org/repo");
            let writer = RemoteIndexWriter::open(store, &config, write_files, write_chunks)
                .await
                .unwrap();
            let result = writer.write_entries(&files, &chunks).await;
            let file_value = match &writer.file_db {
                Some(db) => db.get(encode_committed_file_key(&hash, 1)).await.unwrap(),
                None => None,
            };
            let chunk_value = match &writer.chunk_db {
                Some(db) => db
                    .get(encode_committed_chunk_head_key(&hash))
                    .await
                    .unwrap(),
                None => None,
            };
            writer.close().await.unwrap();
            assert!(
                matches!(result, Err(MetadataError::Internal(message)) if message.contains(missing))
            );
            assert!(
                file_value.is_none() && chunk_value.is_none(),
                "rejected batch must not write either index"
            );
        }
    }

    #[tokio::test]
    async fn bounded_writer_defers_remote_flush_until_close() {
        let memory = Arc::new(InMemory::new());
        let store: Arc<dyn ObjectStore> = memory.clone();
        let config = RemoteIndexConfig::for_repo("org/buffered");
        let writer = RemoteIndexWriter::open(store, &config, true, false)
            .await
            .expect("open writer");
        let objects_after_open = memory.list(None).count().await;

        for seed in 1..=8 {
            writer
                .write_entries(
                    &[(
                        hash_from_seed(seed),
                        CommittedFileRecord {
                            recipe_hash: [seed as u8; 32],
                            shard_hash: hash_from_seed(seed + 100),
                            committed_generation: 1,
                            shard_index_hash: hash_from_seed(500),
                        },
                    )],
                    &[],
                )
                .await
                .expect("buffer entry");
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        assert_eq!(memory.list(None).count().await, objects_after_open);
        writer.close().await.expect("close writer");
        assert!(memory.list(None).count().await > objects_after_open);
    }

    fn committed_receipt(
        chunk_hash: MerkleHash,
        xorb_ref: XorbRef,
        generation: u64,
    ) -> CommittedChunkReceipt {
        CommittedChunkReceipt {
            schema_version: crate::receipts::RECEIPT_SCHEMA_VERSION,
            chunk_hash: chunk_hash.into(),
            xorb_hash: xorb_ref.xorb_hash.into(),
            chunk_index: xorb_ref.chunk_index,
            uncompressed_size: xorb_ref.uncompressed_size,
            origin: crate::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                crab_storage::canonical_global_content_path("xorbs", &xorb_ref.xorb_hash.hex())
                    .to_string(),
                xorb_ref.xorb_hash.into(),
                [9; 32],
                32,
                None,
                None,
            ),
            source_repo_prefix: "org/repo".to_owned(),
            source_shard_hash: hash_from_seed(3).into(),
            committed_generation: generation,
            shard_index_hash: hash_from_seed(4).into(),
            gc_registry_generation: 1,
        }
    }

    #[tokio::test]
    async fn headless_scan_matches_batch_head_selection() {
        let chunk_hash = hash_from_seed(1);
        for generations in [[1, 1], [1, 2]] {
            for reverse in [false, true] {
                let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
                let config = RemoteIndexConfig::for_repo("org/repo");
                let mut entries = generations.map(|generation| {
                    (
                        chunk_hash,
                        committed_receipt(
                            chunk_hash,
                            XorbRef {
                                xorb_hash: hash_from_seed(2),
                                chunk_index: 0,
                                uncompressed_size: 4096,
                            },
                            generation,
                        ),
                    )
                });
                entries[1].1.chunk_index = 1;
                if reverse {
                    entries.reverse();
                }
                write_index_entries(Arc::clone(&store), &config, &[], &entries)
                    .await
                    .expect("write competing placements");
                let with_head = read_chunk_index_entry(Arc::clone(&store), &config, &chunk_hash)
                    .await
                    .expect("read head")
                    .expect("head placement");

                let db = open_writer(
                    Arc::clone(&store),
                    &config.chunk_index_path,
                    CHUNK_INDEX_DB_LABEL,
                )
                .await
                .expect("open head deletion writer");
                db.delete_with_options(
                    encode_committed_chunk_head_key(&chunk_hash),
                    &slatedb::config::WriteOptions {
                        await_durable: false,
                        ..slatedb::config::WriteOptions::default()
                    },
                )
                .await
                .expect("buffer head deletion");
                db.close().await.expect("persist head deletion");
                let without_head = read_chunk_index_entry(store, &config, &chunk_hash)
                    .await
                    .expect("scan immutable placements");
                assert_eq!(
                    without_head,
                    Some(with_head),
                    "generations={generations:?}, reverse={reverse}"
                );
            }
        }
    }

    #[tokio::test]
    async fn write_index_entries_reads_committed_entry() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let config = RemoteIndexConfig::for_repo_with_global_prefix("org/repo", ".crab");
        let chunk_hash = hash_from_seed(1);
        let xorb_ref = XorbRef {
            xorb_hash: hash_from_seed(2),
            chunk_index: 7,
            uncompressed_size: 4096,
        };

        let receipt = committed_receipt(chunk_hash, xorb_ref, 1);
        write_index_entries(Arc::clone(&store), &config, &[], &[(chunk_hash, receipt)])
            .await
            .expect("write committed entry");

        let got = read_chunk_index_entry(store, &config, &chunk_hash)
            .await
            .expect("read chunk index")
            .expect("committed chunk entry");
        assert_eq!(got, xorb_ref);
    }

    #[tokio::test]
    async fn read_chunk_index_entry_returns_none_for_fresh_database() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let config = RemoteIndexConfig::for_repo("org/repo");

        let got = read_chunk_index_entry(store, &config, &hash_from_seed(1))
            .await
            .expect("read fresh index");
        assert!(got.is_none());
    }

    #[test]
    fn config_paths_trim_prefix_slashes() {
        let config = RemoteIndexConfig::for_repo_with_global_prefix("org/repo/", ".crab/");

        assert_eq!(config.file_index_path, "org/repo/file_index_db/");
        assert_eq!(config.chunk_index_path, ".crab/chunk_index_db/");
    }
}
