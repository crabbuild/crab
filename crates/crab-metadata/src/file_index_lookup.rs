//! Read-only file-index lookup over a repository's SlateDB metadata.
//!
//! This Module owns the read Interface for `file_hash -> shard_hash` point
//! lookups. It does not own metadata writes, CLI error presentation, metrics,
//! or `MetaDb` session lifecycle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::stream;
use futures_util::{StreamExt, TryStreamExt};
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;

use crate::error::{MetadataError, Result};
use crate::key_codec::{decode_committed_file_key, encode_committed_content_prefix};
use crate::value_codec::{CommittedFileRecord, decode_committed_file_record};
use crab_xet::xorb::format::MerkleHash;

const DB_LABEL: &str = "file_index_db";
const GET_BATCH_CONCURRENCY: usize = 256;
const SHARD_SEARCH_CONCURRENCY: usize = 16;

fn file_index_path(repo_prefix: &str) -> String {
    format!("{}/file_index_db/", repo_prefix.trim_end_matches('/'))
}

fn is_manifest_missing(err: &slatedb::Error) -> bool {
    if !matches!(err.kind(), slatedb::ErrorKind::Data) {
        return false;
    }
    err.to_string()
        .contains("failed to find latest transactional object")
}

fn map_decode_error(key: &[u8], error: MetadataError) -> MetadataError {
    match error {
        MetadataError::CorruptObject { reason, .. } => MetadataError::CorruptObject {
            path: format!("{}:{}", DB_LABEL, hex_key(key)),
            reason,
        },
        other => other,
    }
}

fn hex_key(key: &[u8]) -> String {
    let mut out = String::with_capacity(key.len() * 2);
    for byte in key {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[derive(Debug, Clone)]
struct CommittedShardAnchor {
    generation: u64,
    shard_index_hash: MerkleHash,
    shards: HashSet<MerkleHash>,
}

fn validate_record(
    record: CommittedFileRecord,
    anchor: Option<&CommittedShardAnchor>,
) -> Option<CommittedFileRecord> {
    let anchor = anchor?;
    if record.committed_generation == 0
        || record.committed_generation > anchor.generation
        || !anchor.shards.contains(&record.shard_hash)
    {
        return None;
    }
    if record.committed_generation == anchor.generation
        && record.shard_index_hash != anchor.shard_index_hash
    {
        return None;
    }
    Some(record)
}

async fn lookup_committed_record(
    reader: &slatedb::DbReader,
    file_hash: MerkleHash,
    anchor: Option<&CommittedShardAnchor>,
) -> Result<Option<CommittedFileRecord>> {
    let prefix = encode_committed_content_prefix(&file_hash);
    let mut rows =
        reader
            .scan_prefix(&prefix, ..)
            .await
            .map_err(|source| MetadataError::SlateDbRead {
                db: DB_LABEL.to_owned(),
                source,
            })?;
    let mut selected = None;
    while let Some(row) = rows
        .next()
        .await
        .map_err(|source| MetadataError::SlateDbRead {
            db: DB_LABEL.to_owned(),
            source,
        })?
    {
        let (key_hash, key_generation) = decode_committed_file_key(&row.key)
            .map_err(|error| map_decode_error(&row.key, error))?;
        if key_hash != file_hash {
            return Err(MetadataError::CorruptObject {
                path: format!("{DB_LABEL}:{}", hex_key(&row.key)),
                reason: "committed file prefix scan returned a different file hash".to_owned(),
            });
        }
        let record = decode_committed_file_record(&row.value)
            .map_err(|error| map_decode_error(&row.key, error))?;
        if record.committed_generation != key_generation {
            return Err(MetadataError::CorruptObject {
                path: format!("{DB_LABEL}:{}", hex_key(&row.key)),
                reason: format!(
                    "key generation {key_generation} does not match value generation {}",
                    record.committed_generation
                ),
            });
        }
        if validate_record(record, anchor).is_some() {
            selected = Some(record);
        }
    }
    Ok(selected)
}

/// Read-only file-index lookup session for repo-scoped batches.
///
/// Opens the repo's `file_index_db` once, serves one or many point lookups, and
/// closes the underlying SlateDB reader when consumed.
pub struct FileIndexLookupSession {
    reader: Option<Arc<slatedb::DbReader>>,
    anchor: Option<CommittedShardAnchor>,
    storage: crab_storage::Store,
    router: crab_storage::StoreLayout<crab_storage::Store>,
    manifest_fallback: tokio::sync::OnceCell<HashMap<MerkleHash, MerkleHash>>,
}

impl FileIndexLookupSession {
    /// Open a read-only lookup session for `repo_prefix`.
    ///
    /// A never-written `file_index_db` opens as an empty session: every lookup
    /// returns `None`.
    pub async fn open(store: Arc<dyn ObjectStore>, repo_prefix: &str) -> Result<Self> {
        Self::open_with_mode(crab_storage::Store::new(store), repo_prefix, true).await
    }

    /// Open a lookup session using the storage scope's safe read mode.
    ///
    /// SlateDB readers maintain checkpoints by writing manifests. Scoped
    /// stores can be read-only, so they use the manifest-pinned shard fallback
    /// instead of attempting those maintenance writes.
    pub async fn open_for_storage(store: &crab_storage::Store, repo_prefix: &str) -> Result<Self> {
        Self::open_with_mode(store.clone(), repo_prefix, store.storage_scope().is_none()).await
    }

    async fn open_with_mode(
        storage: crab_storage::Store,
        repo_prefix: &str,
        use_acceleration: bool,
    ) -> Result<Self> {
        let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
        let anchor = match crate::manifest_store::read_repository_snapshot(&storage, &router).await
        {
            Ok(snapshot) if !snapshot.journal.shards.is_empty() => {
                let shard_index_hash = if snapshot.manifest.shard_index_hash.is_empty() {
                    MerkleHash::default()
                } else {
                    MerkleHash::from_hex(&snapshot.manifest.shard_index_hash).map_err(|error| {
                        MetadataError::CorruptObject {
                            path: router.manifest_path().to_string(),
                            reason: format!("invalid shard-index hash: {error}"),
                        }
                    })?
                };
                let shards = snapshot
                    .journal
                    .shards
                    .into_iter()
                    .map(|hash| {
                        MerkleHash::from_hex(&hash).map_err(|error| MetadataError::CorruptObject {
                            path: "repository shard inventory".to_owned(),
                            reason: format!("invalid shard hash: {error}"),
                        })
                    })
                    .collect::<Result<HashSet<_>>>()?;
                Some(CommittedShardAnchor {
                    generation: snapshot.manifest.generation,
                    shard_index_hash,
                    shards,
                })
            }
            Ok(_) => None,
            Err(MetadataError::Storage {
                source: crab_storage::StorageError::NotFound { .. },
            }) => None,
            Err(error) => return Err(error),
        };
        let reader = if use_acceleration {
            let path = file_index_path(repo_prefix);
            match slatedb::DbReader::builder(
                ObjectPath::from(path.as_str()),
                Arc::clone(storage.inner()),
            )
            .build()
            .await
            {
                Ok(reader) => Some(Arc::new(reader)),
                Err(source) if is_manifest_missing(&source) => None,
                Err(source) => {
                    return Err(MetadataError::SlateDbOpen {
                        db: DB_LABEL.to_owned(),
                        path,
                        source,
                    });
                }
            }
        } else {
            None
        };

        Ok(Self {
            reader,
            anchor,
            storage,
            router,
            manifest_fallback: tokio::sync::OnceCell::new(),
        })
    }

    /// Look up one file hash in the open session.
    pub async fn lookup(&self, file_hash: &MerkleHash) -> Result<Option<MerkleHash>> {
        if let Some(reader) = self.reader.as_ref()
            && let Some(record) =
                lookup_committed_record(reader, *file_hash, self.anchor.as_ref()).await?
        {
            return Ok(Some(record.shard_hash));
        }
        Ok(self
            .manifest_scoped_lookup_batch(&[*file_hash])
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    /// Look up file hashes in one bounded-parallel batch.
    ///
    /// The output vector is aligned with the input slice.
    pub async fn lookup_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<MerkleHash>>> {
        if file_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let records = self.lookup_committed_records_batch(file_hashes).await?;
        let mut out = records
            .into_iter()
            .map(|record| record.map(|record| record.shard_hash))
            .collect::<Vec<_>>();
        let misses = out
            .iter()
            .enumerate()
            .filter_map(|(index, value)| value.is_none().then_some((index, file_hashes[index])))
            .collect::<Vec<_>>();
        if misses.is_empty() {
            return Ok(out);
        }
        let missing_hashes = misses.iter().map(|(_, hash)| *hash).collect::<Vec<_>>();
        let repaired = self.manifest_scoped_lookup_batch(&missing_hashes).await?;
        for ((index, _), shard_hash) in misses.into_iter().zip(repaired) {
            out[index] = shard_hash;
        }
        Ok(out)
    }

    /// Return only generation-pinned acceleration hits, without invoking the
    /// manifest-scoped correctness fallback.
    pub async fn lookup_committed_records_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<CommittedFileRecord>>> {
        if file_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let Some(reader) = self.reader.as_ref() else {
            return Ok(vec![None; file_hashes.len()]);
        };
        let anchor = self.anchor.clone();
        let concurrency = GET_BATCH_CONCURRENCY.min(file_hashes.len()).max(1);
        let fetched: Vec<(usize, Option<CommittedFileRecord>)> = stream::iter(
            file_hashes
                .iter()
                .copied()
                .enumerate()
                .map(|(index, file_hash)| {
                    let reader = Arc::clone(reader);
                    let anchor = anchor.clone();
                    async move {
                        lookup_committed_record(&reader, file_hash, anchor.as_ref())
                            .await
                            .map(|record| (index, record))
                    }
                }),
        )
        .buffer_unordered(concurrency)
        .try_collect()
        .await?;
        let mut out = vec![None; file_hashes.len()];
        for (index, record) in fetched {
            out[index] = record;
        }
        Ok(out)
    }

    async fn manifest_scoped_lookup_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<MerkleHash>>> {
        let Some(anchor) = self.anchor.as_ref() else {
            return Ok(vec![None; file_hashes.len()]);
        };
        let found = self
            .manifest_fallback
            .get_or_try_init(|| async {
                Ok::<_, MetadataError>(
                    stream::iter(anchor.shards.iter().copied().map(|shard_hash| {
                        let storage = self.storage.clone();
                        let router = self.router.clone();
                        async move {
                            let path = router.shard_path(&shard_hash);
                            let (body, _) = storage.get_with_etag(&path).await?;
                            if crab_xet::hash::compute_data_hash(&body) != shard_hash {
                                return Err(MetadataError::CorruptObject {
                                    path: path.to_string(),
                                    reason: "manifest-scoped shard body hash mismatch".to_owned(),
                                });
                            }
                            let recipes = crab_xet::shard_parse::extract_file_recipes(&body)?;
                            Ok::<_, MetadataError>(
                                recipes
                                    .into_iter()
                                    .map(|recipe| (recipe.file_hash, shard_hash))
                                    .collect::<Vec<_>>(),
                            )
                        }
                    }))
                    .buffer_unordered(SHARD_SEARCH_CONCURRENCY.min(anchor.shards.len()).max(1))
                    .try_collect::<Vec<Vec<(MerkleHash, MerkleHash)>>>()
                    .await?
                    .into_iter()
                    .flatten()
                    .collect::<HashMap<_, _>>(),
                )
            })
            .await?;
        let repaired_files = file_hashes
            .iter()
            .filter(|file_hash| found.contains_key(*file_hash))
            .count();
        if repaired_files > 0 {
            tracing::warn!(
                repaired_files,
                generation = anchor.generation,
                "file-index miss resolved from one cached manifest-scoped shard scan; run metadb rebuild"
            );
        }
        Ok(file_hashes
            .iter()
            .map(|file_hash| found.get(file_hash).copied())
            .collect())
    }

    /// Close the SlateDB reader opened by this session.
    pub async fn close(self) -> Result<()> {
        let Some(reader) = self.reader else {
            return Ok(());
        };
        reader
            .close()
            .await
            .map_err(|source| MetadataError::SlateDbClose {
                db: DB_LABEL.to_owned(),
                source,
            })
    }
}

struct SharedFileIndexLookupInner {
    store: crab_storage::Store,
    repo_prefix: String,
    session: tokio::sync::RwLock<Option<FileIndexLookupSession>>,
    closed: AtomicBool,
    use_acceleration: bool,
}

/// Cloneable, lazy file-index reader for one repo-scoped operation.
///
/// The first lookup opens the repo's read-only `file_index_db`; later clones
/// reuse that reader until the owner calls [`close`](Self::close).
#[derive(Clone)]
pub struct SharedFileIndexLookup {
    inner: Arc<SharedFileIndexLookupInner>,
}

impl SharedFileIndexLookup {
    /// Create a lazy lookup handle bound to one object store and repo.
    pub fn new(store: Arc<dyn ObjectStore>, repo_prefix: impl Into<String>) -> Self {
        Self::new_with_mode(crab_storage::Store::new(store), repo_prefix, true)
    }

    /// Create a lazy lookup using the storage scope's safe read mode.
    #[must_use]
    pub fn new_for_storage(store: &crab_storage::Store, repo_prefix: impl Into<String>) -> Self {
        Self::new_with_mode(store.clone(), repo_prefix, store.storage_scope().is_none())
    }

    fn new_with_mode(
        store: crab_storage::Store,
        repo_prefix: impl Into<String>,
        use_acceleration: bool,
    ) -> Self {
        Self {
            inner: Arc::new(SharedFileIndexLookupInner {
                store,
                repo_prefix: repo_prefix.into(),
                session: tokio::sync::RwLock::new(None),
                closed: AtomicBool::new(false),
                use_acceleration,
            }),
        }
    }

    /// Look up one file hash, opening the shared reader on first use.
    pub async fn lookup(&self, file_hash: &MerkleHash) -> Result<Option<MerkleHash>> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(MetadataError::Internal(
                "file-index lookup used after close".to_owned(),
            ));
        }

        {
            let guard = self.inner.session.read().await;
            if let Some(session) = guard.as_ref() {
                return session.lookup(file_hash).await;
            }
        }

        let mut guard = self.inner.session.write().await;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(MetadataError::Internal(
                "file-index lookup used after close".to_owned(),
            ));
        }
        if guard.is_none() {
            *guard = Some(
                FileIndexLookupSession::open_with_mode(
                    self.inner.store.clone(),
                    &self.inner.repo_prefix,
                    self.inner.use_acceleration,
                )
                .await?,
            );
        }

        let Some(session) = guard.as_ref() else {
            return Err(MetadataError::Internal(
                "file-index lookup session was not initialized".to_owned(),
            ));
        };
        session.lookup(file_hash).await
    }

    /// Look up file hashes through the shared reader.
    ///
    /// The output vector is aligned with the input slice.
    pub async fn lookup_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<MerkleHash>>> {
        if file_hashes.is_empty() {
            return Ok(Vec::new());
        }

        if self.inner.closed.load(Ordering::Acquire) {
            return Err(MetadataError::Internal(
                "file-index lookup used after close".to_owned(),
            ));
        }

        {
            let guard = self.inner.session.read().await;
            if let Some(session) = guard.as_ref() {
                return session.lookup_batch(file_hashes).await;
            }
        }

        let mut guard = self.inner.session.write().await;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(MetadataError::Internal(
                "file-index lookup used after close".to_owned(),
            ));
        }
        if guard.is_none() {
            *guard = Some(
                FileIndexLookupSession::open_with_mode(
                    self.inner.store.clone(),
                    &self.inner.repo_prefix,
                    self.inner.use_acceleration,
                )
                .await?,
            );
        }

        let Some(session) = guard.as_ref() else {
            return Err(MetadataError::Internal(
                "file-index lookup session was not initialized".to_owned(),
            ));
        };
        session.lookup_batch(file_hashes).await
    }

    /// Close the shared reader if it was opened.
    pub async fn close(self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        let Some(session) = self.inner.session.write().await.take() else {
            return Ok(());
        };
        session.close().await
    }
}

/// Open a one-shot read-only file-index session, look up `file_hash`, and close
/// the session before returning.
pub async fn resolve_file_hash_to_shard(
    store: Arc<dyn ObjectStore>,
    repo_prefix: &str,
    file_hash: &MerkleHash,
) -> Result<Option<MerkleHash>> {
    let session = FileIndexLookupSession::open(store, repo_prefix).await?;
    let result = session.lookup(file_hash).await;
    if let Err(close_err) = session.close().await {
        tracing::warn!(
            error = %close_err,
            "file_index_lookup: SlateDB reader close failed after read"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use bytes::Bytes;
    use object_store::memory::InMemory;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed.wrapping_mul(31), seed.wrapping_mul(97), seed])
    }

    fn shard_with_file(file_hash: MerkleHash) -> (Vec<u8>, MerkleHash) {
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };

        let chunk_hash = hash_from_seed(43);
        let xorb_hash = hash_from_seed(44);
        let xorb = Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, 1, 16),
            chunks: vec![XorbChunkSequenceEntry::new(chunk_hash, 16, 0)],
        });
        let file = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1, false, false),
            segments: vec![FileDataSequenceEntry::new(xorb_hash, 16, 0, 1)],
            verification: Vec::new(),
            metadata_ext: None,
        };
        let mut writer = ShardWriter::new();
        writer.add_xorb(xorb).unwrap();
        writer.add_file(file).unwrap();
        writer.finalize().unwrap()
    }

    async fn seed_file_index(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        entries: &[(MerkleHash, MerkleHash)],
    ) {
        let storage = crab_storage::Store::new(Arc::clone(&store));
        let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
        let shard_hashes: Vec<String> = entries
            .iter()
            .map(|(_, shard_hash)| shard_hash.hex())
            .collect();
        let (shard_index_hash, _, shard_write) = crate::manifests::append_shard_index(
            crate::segmented::SegmentIndex::default(),
            1,
            &shard_hashes,
        )
        .expect("build shard index");
        crate::manifest_store::upload_segmented_bulk(
            &storage,
            &router,
            &crate::manifests::BulkData {
                shard_index: shard_write,
                pack_index: crate::segmented::SegmentWrite::default(),
            },
        )
        .await
        .expect("upload shard index");
        let mut manifest = crate::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash.clone();
        manifest.seal_git_validation();
        crate::manifest_store::create_manifest(&storage, &router, &manifest)
            .await
            .expect("create manifest");
        let shard_index_hash = MerkleHash::from_hex(&shard_index_hash).expect("index hash");
        let db = slatedb::Db::open(
            ObjectPath::from(file_index_path(repo_prefix).as_str()),
            store,
        )
        .await
        .expect("open writer");
        for (file_hash, shard_hash) in entries {
            db.put(
                crate::key_codec::encode_committed_file_key(file_hash, 1),
                crate::value_codec::encode_committed_file_record(
                    &crate::value_codec::CommittedFileRecord {
                        recipe_hash: [7; 32],
                        shard_hash: *shard_hash,
                        committed_generation: 1,
                        shard_index_hash,
                    },
                ),
            )
            .await
            .expect("put entry");
        }
        db.flush().await.expect("flush");
        db.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn resolve_returns_none_on_fresh_store() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let got = resolve_file_hash_to_shard(store, "org/test-repo", &hash_from_seed(1))
            .await
            .expect("lookup succeeds on empty store");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn resolve_round_trips_a_written_entry() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let file_hash = hash_from_seed(42);
        let shard_hash = hash_from_seed(100);
        seed_file_index(Arc::clone(&store), "org/ml", &[(file_hash, shard_hash)]).await;

        let got = resolve_file_hash_to_shard(Arc::clone(&store), "org/ml", &file_hash)
            .await
            .expect("lookup succeeds");
        assert_eq!(got, Some(shard_hash));
    }

    #[tokio::test]
    async fn session_batch_preserves_order() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let file_a = hash_from_seed(1);
        let file_b = hash_from_seed(2);
        let shard_a = hash_from_seed(101);
        let shard_b = hash_from_seed(102);
        seed_file_index(
            Arc::clone(&store),
            "org/ml",
            &[(file_a, shard_a), (file_b, shard_b)],
        )
        .await;

        let session = FileIndexLookupSession::open(Arc::clone(&store), "org/ml")
            .await
            .expect("open session");
        let got = session
            .lookup_batch(&[file_b, file_a])
            .await
            .expect("batch lookup");
        assert_eq!(got, vec![Some(shard_b), Some(shard_a)]);
        session.close().await.expect("close");
    }

    #[tokio::test]
    async fn scoped_storage_lookup_does_not_write_reader_checkpoints() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let file_hash = hash_from_seed(42);
        let shard_hash = hash_from_seed(100);
        seed_file_index(
            Arc::clone(&store),
            "scoped/repo",
            &[(file_hash, shard_hash)],
        )
        .await;
        let manifest_prefix = ObjectPath::from("scoped/repo/file_index_db/manifest");
        let before = store
            .list(Some(&manifest_prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list manifests before lookup")
            .len();
        let scoped = crab_storage::Store::new(Arc::clone(&store)).with_storage_scope(
            crab_storage::StorageScope {
                repo_prefix: "scoped/repo".to_owned(),
                global_prefix: ".crab".to_owned(),
                source_repo: "org/repo".to_owned(),
                scope_hash: "scope".to_owned(),
            },
        );

        let session = FileIndexLookupSession::open_for_storage(&scoped, "ignored")
            .await
            .expect("open scoped lookup");
        session.close().await.expect("close scoped lookup");

        let after = store
            .list(Some(&manifest_prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list manifests after lookup")
            .len();
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn manifest_shard_search_recovers_missing_file_index_entry() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(Arc::clone(&store));
        let router = crab_storage::StoreLayout::new(storage.clone(), "org/recovery".to_owned());
        let file_hash = hash_from_seed(42);
        let (shard_bytes, shard_hash) = shard_with_file(file_hash);
        storage
            .put(&router.shard_path(&shard_hash), Bytes::from(shard_bytes))
            .await
            .unwrap();
        let (shard_index_hash, _, shard_write) = crate::manifests::append_shard_index(
            crate::segmented::SegmentIndex::default(),
            1,
            &[shard_hash.hex()],
        )
        .unwrap();
        crate::manifest_store::upload_segmented_bulk(
            &storage,
            &router,
            &crate::manifests::BulkData {
                shard_index: shard_write,
                pack_index: crate::segmented::SegmentWrite::default(),
            },
        )
        .await
        .unwrap();
        let mut manifest = crate::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        crate::manifest_store::create_manifest(&storage, &router, &manifest)
            .await
            .unwrap();

        let session = FileIndexLookupSession::open(store, "org/recovery")
            .await
            .unwrap();
        assert_eq!(session.lookup(&file_hash).await.unwrap(), Some(shard_hash));
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn journal_shard_search_recovers_before_compaction() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(Arc::clone(&store));
        let router = crab_storage::StoreLayout::new(storage.clone(), "org/journal".to_owned());
        let file_hash = hash_from_seed(52);
        let (shard_bytes, shard_hash) = shard_with_file(file_hash);
        storage
            .put(&router.shard_path(&shard_hash), Bytes::from(shard_bytes))
            .await
            .unwrap();
        let manifest = crate::manifests::Manifest::default_for_repo("refs/heads/main");
        crate::manifest_store::create_manifest(&storage, &router, &manifest)
            .await
            .unwrap();
        let ref_name = "refs/heads/main";
        let head = crate::ref_journal::read_ref_head(&storage, &router, ref_name)
            .await
            .unwrap();
        let transaction = crate::ref_journal::RefJournalTransaction::new(
            BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
            vec![crate::ref_journal::RefJournalEdit {
                ref_name: ref_name.to_owned(),
                old_oid: None,
                new_oid: Some("a".repeat(40)),
                peeled_oid: None,
            }],
            None,
            Vec::new(),
            vec![shard_hash.hex()],
        )
        .unwrap();
        crate::ref_journal::commit_ref_transaction(&storage, &router, &transaction, &[head])
            .await
            .unwrap();

        let session = FileIndexLookupSession::open(store, "org/journal")
            .await
            .unwrap();
        assert_eq!(session.lookup(&file_hash).await.unwrap(), Some(shard_hash));
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn shared_lookup_close_disables_clones() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let lookup = SharedFileIndexLookup::new(store, "org/test-repo");
        let clone = lookup.clone();

        let got = lookup
            .lookup(&hash_from_seed(1))
            .await
            .expect("lookup opens session");
        assert!(got.is_none());

        lookup.close().await.expect("close");
        let err = clone
            .lookup(&hash_from_seed(2))
            .await
            .expect_err("closed clone must reject future lookups");
        assert!(matches!(err, MetadataError::Internal(_)));
    }
}
