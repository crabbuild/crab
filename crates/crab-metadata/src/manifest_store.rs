//! Storage-backed manifest pointer and segmented metadata helpers.

use bytes::Bytes;
use crab_storage::{ETag, StorageError, Store, StoreLayout};
use futures_util::{StreamExt, TryStreamExt};

use crate::error::{MetadataError, Result};
use crate::manifests::{
    BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
    validate_manifest_payload, validate_pack_manifest_entry,
};
use crate::ref_journal::{
    RefJournalSnapshot, cleanup_compacted_transactions, list_active_transactions,
    materialize_ref_journal, read_ref_journal_frontier, write_ref_journal_frontier,
};
use crate::segmented::{self, SegmentKind, ShardSegmentEntry};
use crate::segmented_store;

const DEFAULT_HISTORY_READ_CONCURRENCY: usize = 32;

/// One validated immutable historical manifest root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestHistoryEntry {
    /// Manifest generation encoded in both the path and body.
    pub generation: u64,
    /// Blake3 digest of the exact stored JSON body.
    pub digest: String,
    /// Full object-store path for diagnostics and GC rooting.
    pub path: String,
    /// Validated historical manifest.
    pub manifest: Manifest,
    /// Stored JSON body size.
    pub size: u64,
}

/// Coherent repository state materialized from the compacted manifest and ref journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    /// Stored compacted manifest before journal overlay.
    pub manifest: Manifest,
    /// Backend CAS token for the compacted manifest.
    pub manifest_etag: String,
    /// Current refs, packs, and shards after committed journal transactions.
    pub journal: RefJournalSnapshot,
}

fn serialize_manifest(manifest: &Manifest) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(manifest)
        .map_err(|e| MetadataError::Internal(format!("manifest serialize: {e}")))
}

fn manifest_digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn parse_history_name(path: &str, prefix: &str) -> Result<(u64, String)> {
    let name = path
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "historical manifest is outside its repository history prefix".to_owned(),
        })?;
    let stem = name
        .strip_suffix(".json")
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "historical manifest key must end in .json".to_owned(),
        })?;
    let (generation, digest) =
        stem.split_once('-')
            .ok_or_else(|| MetadataError::CorruptObject {
                path: path.to_owned(),
                reason: "historical manifest key is missing its digest".to_owned(),
            })?;
    if generation.len() != 20
        || !generation.bytes().all(|byte| byte.is_ascii_digit())
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "historical manifest key has an invalid generation or digest".to_owned(),
        });
    }
    let generation = generation
        .parse::<u64>()
        .map_err(|error| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: format!("historical manifest generation is invalid: {error}"),
        })?;
    Ok((generation, digest.to_owned()))
}

async fn read_history_entry(
    store: &Store,
    router: &StoreLayout<Store>,
    path: &object_store::path::Path,
) -> Result<ManifestHistoryEntry> {
    let prefix = router.manifest_history_prefix();
    let path_string = path.as_ref().to_owned();
    let (path_generation, path_digest) = parse_history_name(&path_string, prefix.as_ref())?;
    let (body, _) = store.get_with_etag(path).await?;
    let actual_digest = manifest_digest(&body);
    if actual_digest != path_digest {
        return Err(MetadataError::CorruptObject {
            path: path_string,
            reason: format!(
                "historical manifest digest is {actual_digest}, expected {path_digest}"
            ),
        });
    }
    let manifest: Manifest =
        serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
            path: path.as_ref().to_owned(),
            reason: format!("invalid historical manifest JSON: {error}"),
        })?;
    validate_manifest_payload(&manifest)?;
    if manifest.generation != path_generation {
        return Err(MetadataError::CorruptObject {
            path: path.as_ref().to_owned(),
            reason: format!(
                "historical manifest body generation is {}, expected {path_generation}",
                manifest.generation
            ),
        });
    }
    Ok(ManifestHistoryEntry {
        generation: path_generation,
        digest: path_digest,
        path: path.as_ref().to_owned(),
        manifest,
        size: body.len() as u64,
    })
}

/// List and validate every immutable historical manifest root for a repository.
pub async fn list_manifest_history(
    store: &Store,
    router: &StoreLayout<Store>,
) -> Result<Vec<ManifestHistoryEntry>> {
    list_manifest_history_with_concurrency(store, router, DEFAULT_HISTORY_READ_CONCURRENCY).await
}

/// List and validate immutable historical roots with bounded read concurrency.
pub async fn list_manifest_history_with_concurrency(
    store: &Store,
    router: &StoreLayout<Store>,
    concurrency: usize,
) -> Result<Vec<ManifestHistoryEntry>> {
    let prefix = router.manifest_history_prefix();
    let objects = store.list_prefix(&prefix).await?;
    let mut entries =
        futures_util::stream::iter(objects.into_iter().map(|object| async move {
            read_history_entry(store, router, &object.location).await
        }))
        .buffer_unordered(concurrency.max(1))
        .try_collect::<Vec<_>>()
        .await?;
    entries.sort_unstable_by(|left, right| {
        left.generation
            .cmp(&right.generation)
            .then_with(|| left.digest.cmp(&right.digest))
    });
    Ok(entries)
}

/// Select one historical generation, rejecting absent or ambiguous matches.
pub async fn select_manifest_history(
    store: &Store,
    router: &StoreLayout<Store>,
    generation: u64,
    digest: Option<&str>,
) -> Result<ManifestHistoryEntry> {
    if let Some(digest) = digest
        && (digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(MetadataError::Internal(
            "historical manifest digest must be 64 lowercase hexadecimal characters".to_owned(),
        ));
    }
    let mut matches = list_manifest_history(store, router)
        .await?
        .into_iter()
        .filter(|entry| {
            entry.generation == generation && digest.is_none_or(|value| entry.digest == value)
        });
    let selected = matches.next().ok_or_else(|| MetadataError::CorruptObject {
        path: router.manifest_history_prefix().as_ref().to_owned(),
        reason: format!("historical manifest generation {generation} was not found"),
    })?;
    if matches.next().is_some() {
        return Err(MetadataError::CorruptObject {
            path: router.manifest_history_prefix().as_ref().to_owned(),
            reason: format!(
                "historical manifest generation {generation} is ambiguous; select a digest"
            ),
        });
    }
    Ok(selected)
}

async fn archive_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
) -> Result<ManifestHistoryEntry> {
    validate_manifest_payload(manifest)?;
    let body = serialize_manifest(manifest)?;
    let digest = manifest_digest(&body);
    let path = router.manifest_history_path(manifest.generation, &digest);
    store.put_exact(&path, Bytes::from(body)).await?;
    read_history_entry(store, router, &path).await
}

/// Read the manifest pointer and return it with the backend CAS token.
pub async fn read_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
) -> Result<(Manifest, String)> {
    let path = router.manifest_path();
    let (body, etag) = store.get_with_etag(&path).await?;
    let manifest: Manifest =
        serde_json::from_slice(&body).map_err(|e| MetadataError::CorruptObject {
            path: path.as_ref().to_string(),
            reason: format!("invalid manifest JSON: {e}"),
        })?;
    validate_manifest_payload(&manifest)?;
    Ok((manifest, etag.e_tag.unwrap_or_default()))
}

/// Read one coherent repository view including independently committed refs.
pub async fn read_repository_snapshot(
    store: &Store,
    router: &StoreLayout<Store>,
) -> Result<RepositorySnapshot> {
    // Markers are captured first so compaction may safely remove them after
    // publishing a newer manifest without stranding an old-manifest reader.
    let active_transactions = list_active_transactions(store, router).await?;
    let (manifest, manifest_etag) = read_manifest(store, router).await?;
    let packs = if manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        read_bulk_pack_list(store, router, &manifest.pack_index_hash).await?
    };
    let shards = if manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        read_bulk_shard_list(store, router, &manifest.shard_index_hash).await?
    };
    let journal = materialize_ref_journal(
        store,
        router,
        &manifest,
        &packs,
        &shards,
        &active_transactions,
    )
    .await?;
    Ok(RepositorySnapshot {
        manifest,
        manifest_etag,
        journal,
    })
}

/// Fold committed journal transactions into one bounded manifest snapshot.
///
/// Immutable indexes and the matching frontier are written before the
/// manifest CAS. A concurrent journal commit remains above the recorded
/// frontier and is therefore visible in the next repository snapshot.
pub async fn compact_ref_journal(
    store: &Store,
    router: &StoreLayout<Store>,
    created_at: String,
    pusher: Option<String>,
    session_id: String,
) -> Result<Option<Manifest>> {
    let snapshot = read_repository_snapshot(store, router).await?;
    if snapshot.journal.transactions.is_empty() {
        return Ok(None);
    }
    let generation = snapshot
        .manifest
        .generation
        .checked_add(1)
        .ok_or_else(|| MetadataError::Internal("manifest generation overflow".to_owned()))?;
    let (shard_index_hash, _, shard_index) =
        compact_shard_index(generation, &snapshot.journal.shards)?;
    let (pack_index_hash, _, pack_index) = compact_pack_index(generation, &snapshot.journal.packs)?;
    upload_segmented_bulk(
        store,
        router,
        &BulkData {
            shard_index,
            pack_index,
        },
    )
    .await?;

    let compacted_transactions = snapshot.journal.transactions.clone();
    let mut manifest = snapshot.manifest;
    manifest.generation = generation;
    manifest.created_at = created_at;
    manifest.pusher = pusher;
    manifest.session_id = session_id;
    manifest.refs = snapshot.journal.refs;
    manifest.peeled_refs = snapshot.journal.peeled_refs;
    manifest.head = snapshot.journal.head;
    manifest.shard_index_hash = shard_index_hash;
    manifest.pack_index_hash = pack_index_hash;
    // The old summary does not cover journal-only ref advances.
    manifest.commit_graph_hash = None;
    manifest.seal_git_validation();
    write_ref_journal_frontier(store, router, &manifest, &snapshot.journal.visible_heads).await?;
    write_manifest_cas(store, router, &manifest, &snapshot.manifest_etag).await?;
    cleanup_compacted_transactions(store, router, &compacted_transactions).await;
    Ok(Some(manifest))
}

/// Read the segmented shard-index object and parse it into shard hashes.
pub async fn read_bulk_shard_list(
    store: &Store,
    router: &StoreLayout<Store>,
    hash: &str,
) -> Result<Vec<String>> {
    segmented_store::read_records::<ShardSegmentEntry>(store, router, SegmentKind::Shard, hash)
        .await
        .map(|entries| entries.into_iter().map(|entry| entry.shard_hash).collect())
}

/// Read the segmented pack-index object and parse it into pack records.
pub async fn read_bulk_pack_list(
    store: &Store,
    router: &StoreLayout<Store>,
    hash: &str,
) -> Result<Vec<PackManifestEntry>> {
    let packs =
        segmented_store::read_records::<PackManifestEntry>(store, router, SegmentKind::Pack, hash)
            .await?;
    for pack in &packs {
        validate_pack_manifest_entry(pack)?;
    }
    Ok(packs)
}

/// Read a shard segment index by hash.
pub async fn read_shard_index(
    store: &Store,
    router: &StoreLayout<Store>,
    hash: &str,
) -> Result<segmented::SegmentIndex> {
    segmented_store::read_index(store, router, SegmentKind::Shard, hash).await
}

/// Read a pack segment index by hash.
pub async fn read_pack_index(
    store: &Store,
    router: &StoreLayout<Store>,
    hash: &str,
) -> Result<segmented::SegmentIndex> {
    segmented_store::read_index(store, router, SegmentKind::Pack, hash).await
}

/// Upload pending segmented metadata objects if they are absent.
pub async fn upload_segmented_bulk(
    store: &Store,
    router: &StoreLayout<Store>,
    bulk: &BulkData,
) -> Result<()> {
    segmented_store::upload_write(store, router, &bulk.shard_index).await?;
    segmented_store::upload_write(store, router, &bulk.pack_index).await
}

/// Upload a bulk manifest object if it does not already exist.
pub async fn upload_bulk_if_absent(
    store: &Store,
    router: &StoreLayout<Store>,
    prefix: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    let path = router.bulk_manifest_path(prefix, hash);
    match store.head(&path).await {
        Ok(_) => Ok(()),
        Err(StorageError::NotFound { .. }) => store.put(&path, Bytes::from(bytes.to_vec())).await,
        Err(e) => Err(e),
    }
    .map_err(MetadataError::from)
}

/// Conditional-PUT the manifest pointer with `If-Match: {etag}`.
pub async fn write_manifest_cas(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    etag: &str,
) -> Result<String> {
    validate_manifest_payload(manifest)?;
    let (current, current_etag) = read_manifest(store, router).await?;
    if current_etag != etag {
        return Err(MetadataError::ManifestCasConflict {
            path: router.manifest_path().as_ref().to_owned(),
            expected_etag: Some(etag.to_owned()),
        });
    }
    carry_ref_journal_frontier(store, router, &current, manifest).await?;
    archive_manifest(store, router, &current).await?;
    let path = router.manifest_path();
    let body = serialize_manifest(manifest)?;
    let update_version = ETag {
        e_tag: Some(etag.to_owned()),
        version: None,
    };
    let new_etag = store
        .update(&path, Bytes::from(body), update_version)
        .await?;
    Ok(new_etag.e_tag.unwrap_or_default())
}

async fn carry_ref_journal_frontier(
    store: &Store,
    router: &StoreLayout<Store>,
    current: &Manifest,
    next: &Manifest,
) -> Result<()> {
    if read_ref_journal_frontier(store, router, next)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let Some(frontier) = read_ref_journal_frontier(store, router, current).await? else {
        return Ok(());
    };
    write_ref_journal_frontier(store, router, next, &frontier.heads).await
}

/// PUT the manifest pointer with `If-None-Match: *` for first-time creation.
pub async fn create_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
) -> Result<()> {
    create_manifest_with_etag(store, router, manifest)
        .await
        .map(|_| ())
}

/// PUT the manifest pointer with `If-None-Match: *`, returning the backend CAS token.
pub async fn create_manifest_with_etag(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
) -> Result<String> {
    validate_manifest_payload(manifest)?;
    let path = router.manifest_path();
    let body = serialize_manifest(manifest)?;
    let etag = store
        .create_strict_with_etag(&path, Bytes::from(body))
        .await?;
    Ok(etag.e_tag.unwrap_or_default())
}

/// Writes a regional active-active manifest projection without making it write authority.
pub async fn materialize_active_active_manifest_projection(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
) -> Result<()> {
    validate_manifest_payload(manifest)?;
    for _attempt in 0..8 {
        match read_manifest(store, router).await {
            Ok((current, etag)) => {
                if current.generation > manifest.generation {
                    return Ok(());
                }
                if current.generation == manifest.generation {
                    if current == *manifest {
                        return Ok(());
                    }
                    return Err(MetadataError::ManifestCasConflict {
                        path: router.manifest_path().as_ref().to_owned(),
                        expected_etag: Some(etag),
                    });
                }
                match write_manifest_cas(store, router, manifest, &etag).await {
                    Ok(_) => return Ok(()),
                    Err(MetadataError::Storage {
                        source: StorageError::StateConflict { .. },
                    }) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => match create_manifest(store, router, manifest).await {
                Ok(()) => return Ok(()),
                Err(MetadataError::Storage {
                    source: StorageError::StateConflict { .. },
                }) => {}
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }

    Err(MetadataError::Internal(
        "active-active repair manifest materialization failed after repeated CAS conflicts"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fmt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use futures_util::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };

    use crate::manifests::{compact_pack_index, compact_shard_index};
    use crate::ref_journal::{
        RefJournalEdit, RefJournalTransaction, commit_ref_transaction, read_ref_head,
    };

    fn memory_store() -> Store {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    struct DelayedGetStore {
        inner: Arc<InMemory>,
        active_gets: AtomicUsize,
        max_active_gets: AtomicUsize,
    }

    impl DelayedGetStore {
        fn new(inner: Arc<InMemory>) -> Self {
            Self {
                inner,
                active_gets: AtomicUsize::new(0),
                max_active_gets: AtomicUsize::new(0),
            }
        }

        fn max_active_gets(&self) -> usize {
            self.max_active_gets.load(Ordering::Acquire)
        }
    }

    impl fmt::Debug for DelayedGetStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("DelayedGetStore")
        }
    }

    impl fmt::Display for DelayedGetStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("DelayedGetStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for DelayedGetStore {
        async fn put_opts(
            &self,
            location: &object_store::path::Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &object_store::path::Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &object_store::path::Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            let active = self.active_gets.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active_gets.fetch_max(active, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(10)).await;
            let result = self.inner.get_opts(location, options).await;
            self.active_gets.fetch_sub(1, Ordering::AcqRel);
            result
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<object_store::path::Path>>,
        ) -> BoxStream<'static, object_store::Result<object_store::path::Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&object_store::path::Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &object_store::path::Path,
            to: &object_store::path::Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn test_layout(store: Store) -> StoreLayout<Store> {
        StoreLayout::new(store, "org/models".to_string())
    }

    fn next_manifest(current: &Manifest) -> Manifest {
        let mut next = current.clone();
        next.generation += 1;
        next.session_id = format!("session-{}", next.generation);
        next.seal_git_validation();
        next
    }

    #[tokio::test]
    async fn write_then_read_manifest_round_trip() {
        let store = memory_store();
        let router = test_layout(store.clone());

        let shard_hashes: Vec<String> = (0..10).map(|i| format!("{i:064x}")).collect();
        let pack_id = "e".repeat(64);
        let packs = vec![PackManifestEntry {
            pack_id: pack_id.clone(),
            size: 1024,
            content_hash: pack_id,
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }];
        let (shard_hash, _shard_index, shard_write) =
            compact_shard_index(1, &shard_hashes).expect("shard index");
        let (pack_hash, _pack_index, pack_write) =
            compact_pack_index(1, &packs).expect("pack index");
        let bulk = BulkData {
            shard_index: shard_write,
            pack_index: pack_write,
        };

        let mut refs = BTreeMap::new();
        refs.insert("refs/heads/main".to_owned(), "aaaa".repeat(10));
        let mut manifest = Manifest {
            version: 2,
            generation: 1,
            created_at: "2025-07-01T00:00:00Z".to_owned(),
            pusher: Some("test".to_owned()),
            session_id: "session-1".to_owned(),
            refs,
            peeled_refs: BTreeMap::new(),
            head: "refs/heads/main".to_owned(),
            shard_index_hash: shard_hash.clone(),
            pack_index_hash: pack_hash.clone(),
            git_validation_digest: String::new(),
            commit_graph_hash: None,
            ref_registry_hash: None,
        };
        manifest.seal_git_validation();

        upload_segmented_bulk(&store, &router, &bulk).await.unwrap();
        create_manifest(&store, &router, &manifest).await.unwrap();

        let (stored, _etag) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(stored, manifest);
        assert_eq!(
            read_bulk_shard_list(&store, &router, &shard_hash)
                .await
                .unwrap(),
            shard_hashes
        );
        assert_eq!(
            read_bulk_pack_list(&store, &router, &pack_hash)
                .await
                .unwrap(),
            packs
        );
    }

    #[tokio::test]
    async fn create_and_cas_reject_stale_git_validation_digest() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let base = Manifest::default_for_repo("refs/heads/main");

        let mut invalid_create = base.clone();
        invalid_create.generation = 1;
        assert!(
            create_manifest(&store, &router, &invalid_create)
                .await
                .is_err()
        );
        assert!(store.head(&router.manifest_path()).await.is_err());

        create_manifest(&store, &router, &base).await.unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let mut invalid_cas = base;
        invalid_cas.generation = 1;
        assert!(
            write_manifest_cas(&store, &router, &invalid_cas, &etag)
                .await
                .is_err()
        );
        assert_eq!(
            read_manifest(&store, &router).await.unwrap().0.generation,
            0
        );
    }

    #[tokio::test]
    async fn read_bulk_pack_list_rejects_malformed_pack_entries() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let packs = vec![PackManifestEntry {
            pack_id: "bad-pack-id".to_owned(),
            size: 1024,
            content_hash: "e".repeat(64),
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }];
        let (pack_hash, _pack_index, pack_write) =
            compact_pack_index(1, &packs).expect("pack index");
        let bulk = BulkData {
            shard_index: segmented::SegmentWrite::default(),
            pack_index: pack_write,
        };
        upload_segmented_bulk(&store, &router, &bulk).await.unwrap();

        assert!(
            read_bulk_pack_list(&store, &router, &pack_hash)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stale_manifest_cas_returns_storage_conflict() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let manifest_v1 = Manifest::default_for_repo("refs/heads/main");
        create_manifest(&store, &router, &manifest_v1)
            .await
            .unwrap();
        let (_m, etag_v1) = read_manifest(&store, &router).await.unwrap();

        let mut manifest_v2 = manifest_v1.clone();
        manifest_v2.generation = 1;
        manifest_v2.seal_git_validation();
        write_manifest_cas(&store, &router, &manifest_v2, &etag_v1)
            .await
            .unwrap();

        let mut manifest_v3 = manifest_v2;
        manifest_v3.generation = 2;
        manifest_v3.seal_git_validation();
        let err = write_manifest_cas(&store, &router, &manifest_v3, &etag_v1)
            .await
            .expect_err("stale etag must conflict");
        assert!(matches!(
            err,
            MetadataError::Storage {
                source: StorageError::StateConflict { .. }
            } | MetadataError::ManifestCasConflict { .. }
        ));
    }

    #[tokio::test]
    async fn manifest_cas_archives_displaced_committed_manifest() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let current = Manifest::default_for_repo("refs/heads/main");
        create_manifest(&store, &router, &current).await.unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();

        write_manifest_cas(&store, &router, &next_manifest(&current), &etag)
            .await
            .unwrap();

        let history = list_manifest_history(&store, &router).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].manifest, current);
    }

    #[tokio::test]
    async fn stale_manifest_cas_does_not_archive_proposed_manifest() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let first = Manifest::default_for_repo("refs/heads/main");
        create_manifest(&store, &router, &first).await.unwrap();
        let (_, stale_etag) = read_manifest(&store, &router).await.unwrap();
        let second = next_manifest(&first);
        write_manifest_cas(&store, &router, &second, &stale_etag)
            .await
            .unwrap();

        let proposed = next_manifest(&second);
        assert!(
            write_manifest_cas(&store, &router, &proposed, &stale_etag)
                .await
                .is_err()
        );

        let history = list_manifest_history(&store, &router).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].manifest, first);
    }

    #[tokio::test]
    async fn manifest_archival_is_idempotent() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let manifest = Manifest::default_for_repo("refs/heads/main");

        let first = archive_manifest(&store, &router, &manifest).await.unwrap();
        let second = archive_manifest(&store, &router, &manifest).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(
            list_manifest_history(&store, &router).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn manifest_history_validates_entries_concurrently_and_sorts_results() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone() as Arc<dyn ObjectStore>);
        let seed_router = test_layout(seed_store.clone());
        for generation in (0..64).rev() {
            let mut manifest = Manifest::default_for_repo("refs/heads/main");
            manifest.generation = generation;
            manifest.session_id = format!("session-{generation}");
            manifest.seal_git_validation();
            archive_manifest(&seed_store, &seed_router, &manifest)
                .await
                .unwrap();
        }

        let delayed = Arc::new(DelayedGetStore::new(inner));
        let store = Store::new(delayed.clone() as Arc<dyn ObjectStore>);
        let router = test_layout(store.clone());
        let entries = list_manifest_history_with_concurrency(&store, &router, 4)
            .await
            .unwrap();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.generation)
                .collect::<Vec<_>>(),
            (0..64).collect::<Vec<_>>()
        );
        assert!(delayed.max_active_gets() > 1);
        assert!(delayed.max_active_gets() <= 4);
    }

    #[tokio::test]
    async fn manifest_history_rejects_path_body_digest_mismatch() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let manifest = Manifest::default_for_repo("refs/heads/main");
        let body = serialize_manifest(&manifest).unwrap();
        let path = router.manifest_history_path(manifest.generation, &"0".repeat(64));
        store.put(&path, Bytes::from(body)).await.unwrap();

        assert!(list_manifest_history(&store, &router).await.is_err());
    }

    #[tokio::test]
    async fn manifest_cas_fails_closed_when_history_key_is_poisoned() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let current = Manifest::default_for_repo("refs/heads/main");
        create_manifest(&store, &router, &current).await.unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let body = serialize_manifest(&current).unwrap();
        let digest = manifest_digest(&body);
        let history_path = router.manifest_history_path(current.generation, &digest);
        store
            .put(&history_path, Bytes::from_static(b"poisoned"))
            .await
            .unwrap();

        assert!(
            write_manifest_cas(&store, &router, &next_manifest(&current), &etag)
                .await
                .is_err()
        );
        let (still_current, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(still_current, current);
    }

    #[tokio::test]
    async fn active_active_projection_writes_missing_manifest() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 42;
        manifest
            .refs
            .insert("refs/heads/main".into(), "a".repeat(40));
        manifest.seal_git_validation();

        materialize_active_active_manifest_projection(&store, &router, &manifest)
            .await
            .unwrap();

        let (written, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(written, manifest);
    }

    #[tokio::test]
    async fn active_active_projection_rejects_same_generation_different_manifest() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 42;
        manifest.seal_git_validation();
        create_manifest(&store, &router, &manifest).await.unwrap();

        manifest
            .refs
            .insert("refs/heads/main".into(), "b".repeat(40));
        manifest.seal_git_validation();
        let err = materialize_active_active_manifest_projection(&store, &router, &manifest)
            .await
            .expect_err("same generation with different contents must conflict");

        assert!(matches!(
            err,
            MetadataError::ManifestCasConflict {
                path,
                expected_etag: Some(_),
            } if path == "org/models/manifest"
        ));
    }

    #[tokio::test]
    async fn active_active_projection_rejects_stale_proof_before_newer_generation_short_circuit() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let mut current = Manifest::default_for_repo("refs/heads/main");
        current.generation = 42;
        current.seal_git_validation();
        create_manifest(&store, &router, &current).await.unwrap();

        let mut invalid = Manifest::default_for_repo("refs/heads/main");
        invalid.generation = 41;

        let error = materialize_active_active_manifest_projection(&store, &router, &invalid)
            .await
            .expect_err("stale proof must fail before generation comparison");

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn journal_compaction_publishes_bounded_repository_snapshot() {
        let store = memory_store();
        let router = test_layout(store.clone());
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        base.seal_git_validation();
        create_manifest(&store, &router, &base).await.unwrap();
        let head = read_ref_head(&store, &router, "refs/heads/main")
            .await
            .unwrap();
        let transaction = RefJournalTransaction::new(
            BTreeMap::from([("refs/heads/main".to_owned(), None)]),
            vec![RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some("a".repeat(40)),
                new_oid: Some("b".repeat(40)),
                peeled_oid: None,
            }],
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        commit_ref_transaction(&store, &router, &transaction, &[head])
            .await
            .unwrap();

        let compacted = compact_ref_journal(
            &store,
            &router,
            "2026-08-20T00:00:00Z".to_owned(),
            Some("test".to_owned()),
            "compact-1".to_owned(),
        )
        .await
        .unwrap()
        .expect("one journal transaction should compact");
        let snapshot = read_repository_snapshot(&store, &router).await.unwrap();

        assert_eq!(compacted.refs["refs/heads/main"], "b".repeat(40));
        assert!(snapshot.journal.transactions.is_empty());
        assert_eq!(snapshot.journal.refs["refs/heads/main"], "b".repeat(40));
        assert!(
            crate::ref_journal::list_active_transactions(&store, &router)
                .await
                .unwrap()
                .is_empty()
        );

        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let rewritten = next_manifest(&compacted);
        write_manifest_cas(&store, &router, &rewritten, &etag)
            .await
            .unwrap();
        let head = read_ref_head(&store, &router, "refs/heads/main")
            .await
            .unwrap();
        let second = RefJournalTransaction::new(
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                head.visible_transaction.clone(),
            )]),
            vec![RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some("b".repeat(40)),
                new_oid: Some("c".repeat(40)),
                peeled_oid: None,
            }],
            None,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        commit_ref_transaction(&store, &router, &second, &[head])
            .await
            .unwrap();

        let after_rewrite = read_repository_snapshot(&store, &router).await.unwrap();

        assert_eq!(
            after_rewrite.journal.refs["refs/heads/main"],
            "c".repeat(40)
        );
        assert_eq!(
            after_rewrite.journal.transactions,
            vec![second.id().unwrap()]
        );
    }
}
