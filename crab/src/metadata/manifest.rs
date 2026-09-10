//! Repository manifest composition helpers.

pub use crab_metadata::manifest_store::{ManifestHistoryEntry, RepositorySnapshot};
pub use crab_metadata::manifests::{BulkData, MANIFEST_VERSION, Manifest, PackManifestEntry};
pub use crab_metadata::ref_journal::{
    RefJournalCommitResult, RefJournalEdit, RefJournalHeadSnapshot, RefJournalTransaction,
};

use std::collections::BTreeSet;

use crate::core::error::{CrabError, Result};
use crate::storage::StoreLayout;
use crate::storage::store::Store;

fn storage_layout(
    store: &Store,
    router: &StoreLayout,
) -> crab_storage::StoreLayout<crab_storage::Store> {
    crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    )
}

pub async fn read_manifest(store: &Store, router: &StoreLayout) -> Result<(Manifest, String)> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_manifest(store.as_storage(), &router)
        .await
        .map_err(CrabError::from)
}

pub async fn read_repository_snapshot(
    store: &Store,
    router: &StoreLayout,
) -> Result<RepositorySnapshot> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_repository_snapshot(store.as_storage(), &router)
        .await
        .map_err(CrabError::from)
}

/// Read a coherent snapshot while caching only immutable metadata objects.
pub async fn read_repository_snapshot_with_cache(
    store: &Store,
    caching_store: Option<&crab_cache_store::CachingStore>,
    router: &StoreLayout,
) -> Result<RepositorySnapshot> {
    let read_store = caching_store
        .map(crab_cache_store::CachingStore::cache_aware_storage)
        .map(Store::from)
        .unwrap_or_else(|| store.clone());
    read_repository_snapshot(&read_store, router).await
}

pub async fn read_ref_journal_head(
    store: &Store,
    router: &StoreLayout,
    ref_name: &str,
) -> Result<RefJournalHeadSnapshot> {
    let router = storage_layout(store, router);
    crab_metadata::ref_journal::read_ref_head(store.as_storage(), &router, ref_name)
        .await
        .map_err(CrabError::from)
}

pub async fn read_ref_journal_transaction(
    store: &Store,
    router: &StoreLayout,
    transaction_id: &str,
) -> Result<RefJournalTransaction> {
    let router = storage_layout(store, router);
    crab_metadata::ref_journal::read_transaction(store.as_storage(), &router, transaction_id)
        .await
        .map_err(CrabError::from)
}

pub async fn commit_ref_journal_transaction(
    store: &Store,
    router: &StoreLayout,
    transaction: &RefJournalTransaction,
    expected_heads: &[RefJournalHeadSnapshot],
) -> Result<RefJournalCommitResult> {
    let router = storage_layout(store, router);
    crab_metadata::ref_journal::commit_ref_transaction(
        store.as_storage(),
        &router,
        transaction,
        expected_heads,
        || false,
    )
    .await
    .map_err(CrabError::from)
}

pub async fn commit_ref_journal_transaction_for_plan(
    store: &Store,
    router: &StoreLayout,
    transaction: &RefJournalTransaction,
    expected_heads: &[RefJournalHeadSnapshot],
    plan_id: &str,
) -> Result<RefJournalCommitResult> {
    let router = storage_layout(store, router);
    crab_metadata::ref_journal::commit_ref_transaction_for_plan(
        store.as_storage(),
        &router,
        transaction,
        expected_heads,
        plan_id,
        || false,
    )
    .await
    .map_err(CrabError::from)
}

pub async fn resolve_mirror_plan_receipt(
    store: &Store,
    router: &StoreLayout,
    plan_id: &str,
) -> Result<Option<crab_metadata::plan_receipt::PlanReceipt>> {
    let router = storage_layout(store, router);
    crab_metadata::plan_receipt::resolve_plan_receipt(store.as_storage(), &router, plan_id)
        .await
        .map_err(CrabError::from)
}

pub async fn read_mirror_plan_manifest(
    store: &Store,
    router: &StoreLayout,
    generation: u64,
    digest: &str,
) -> Result<Option<Manifest>> {
    let router = storage_layout(store, router);
    crab_metadata::plan_receipt::read_manifest_version(
        store.as_storage(),
        &router,
        generation,
        digest,
    )
    .await
    .map_err(CrabError::from)
}

/// Return whether a committed ref transaction still has an active marker.
pub async fn ref_journal_transaction_is_active(
    store: &Store,
    router: &StoreLayout,
    transaction_id: &str,
) -> Result<bool> {
    let router = storage_layout(store, router);
    crab_metadata::ref_journal::transaction_is_active(store.as_storage(), &router, transaction_id)
        .await
        .map_err(CrabError::from)
}

pub async fn list_active_ref_journal_transactions(
    store: &Store,
    router: &StoreLayout,
) -> Result<BTreeSet<String>> {
    let router = storage_layout(store, router);
    crab_metadata::ref_journal::list_active_transactions(store.as_storage(), &router)
        .await
        .map_err(CrabError::from)
}

pub async fn list_manifest_history(
    store: &Store,
    router: &StoreLayout,
) -> Result<Vec<ManifestHistoryEntry>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::list_manifest_history(store.as_storage(), &router)
        .await
        .map_err(CrabError::from)
}

pub async fn list_manifest_history_for_generation(
    store: &Store,
    router: &StoreLayout,
    generation: u64,
) -> Result<Vec<ManifestHistoryEntry>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::list_manifest_history_for_generation(
        store.as_storage(),
        &router,
        generation,
    )
    .await
    .map_err(CrabError::from)
}

pub async fn select_manifest_history(
    store: &Store,
    router: &StoreLayout,
    generation: u64,
    digest: Option<&str>,
) -> Result<ManifestHistoryEntry> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::select_manifest_history(
        store.as_storage(),
        &router,
        generation,
        digest,
    )
    .await
    .map_err(CrabError::from)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crab_cache::LocalCache;
    use crab_storage::test_support::CountingObjectStore;
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn snapshot_cache_repairs_transactions_and_rereads_mutable_state() {
        let memory: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let counted = Arc::new(CountingObjectStore::new(memory));
        let store = Store::new(Arc::clone(&counted) as Arc<dyn object_store::ObjectStore>);
        let router = StoreLayout::new(store.clone(), "snapshot-cache".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .expect("initialize repository layout");
        crate::cmd::init::create_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .expect("create initial manifest");

        let head = read_ref_journal_head(&store, &router, "refs/heads/main")
            .await
            .expect("read initial ref head");
        let transaction = RefJournalTransaction::new(
            BTreeMap::from([("refs/heads/main".to_owned(), None)]),
            vec![RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: Some("a".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: None,
            }],
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("build ref transaction");
        let transaction_id = transaction.id().expect("transaction identity");
        commit_ref_journal_transaction(&store, &router, &transaction, &[head])
            .await
            .expect("commit ref transaction");

        let tempdir = tempfile::tempdir().expect("cache directory");
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let caching_store = crab_cache_store::CachingStore::new_with_local_cache(
            store.as_storage().clone(),
            crab_cache_store::CacheConfig::default(),
            Arc::clone(&cache),
        )
        .expect("cache store");
        let transaction_path = router.ref_journal_transaction_path(&transaction_id);
        counted.reset();

        let first = read_repository_snapshot_with_cache(&store, Some(&caching_store), &router)
            .await
            .expect("first snapshot");
        assert_eq!(
            first.journal.refs.get("refs/heads/main"),
            Some(&"a".repeat(40))
        );
        assert!(counted.requests().iter().any(|request| {
            request.location == transaction_path.as_ref()
                && request.kind == crab_storage::test_support::ObjectReadKind::Full
        }));

        cache
            .put_unchecked_for_test(
                &crab_cache::CacheKey::RefTransaction(
                    blake3::Hash::from_hex(&transaction_id).expect("transaction hash"),
                ),
                b"corrupt transaction",
            )
            .await
            .expect("corrupt cached transaction");
        counted.reset();
        let repaired = read_repository_snapshot_with_cache(&store, Some(&caching_store), &router)
            .await
            .expect("snapshot after cache repair");
        assert_eq!(first.journal, repaired.journal);
        assert!(counted.requests().iter().any(|request| {
            request.location == transaction_path.as_ref()
                && request.kind == crab_storage::test_support::ObjectReadKind::Full
        }));

        counted.reset();
        counted.block_body_reads_for(&transaction_path);
        let second = read_repository_snapshot_with_cache(&store, Some(&caching_store), &router)
            .await
            .expect("cached snapshot");
        assert_eq!(first.journal, second.journal);
        assert!(
            counted
                .requests()
                .iter()
                .all(|request| request.location != transaction_path.as_ref())
        );
        assert!(counted.requests().iter().any(|request| {
            request.location == router.manifest_path().as_ref()
                && request.kind == crab_storage::test_support::ObjectReadKind::Full
        }));
    }
}

pub async fn read_bulk_shard_list(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
) -> Result<Vec<String>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_bulk_shard_list(store.as_storage(), &router, hash)
        .await
        .map_err(CrabError::from)
}

pub async fn read_bulk_shard_list_with_limit(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
    max_records: u64,
) -> Result<Vec<String>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_bulk_shard_list_with_limit(
        store.as_storage(),
        &router,
        hash,
        max_records,
    )
    .await
    .map_err(CrabError::from)
}

pub async fn read_bulk_pack_list(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
) -> Result<Vec<PackManifestEntry>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_bulk_pack_list(store.as_storage(), &router, hash)
        .await
        .map_err(CrabError::from)
}

pub async fn read_bulk_pack_list_with_limit(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
    max_records: u64,
) -> Result<Vec<PackManifestEntry>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_bulk_pack_list_with_limit(
        store.as_storage(),
        &router,
        hash,
        max_records,
    )
    .await
    .map_err(CrabError::from)
}

pub async fn read_shard_index(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
) -> Result<crab_metadata::segmented::SegmentIndex> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_shard_index(store.as_storage(), &router, hash)
        .await
        .map_err(CrabError::from)
}

pub async fn read_pack_index(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
) -> Result<crab_metadata::segmented::SegmentIndex> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::read_pack_index(store.as_storage(), &router, hash)
        .await
        .map_err(CrabError::from)
}

pub fn append_shard_index(
    base: crab_metadata::segmented::SegmentIndex,
    generation: u64,
    shard_hashes: &[String],
) -> Result<(
    String,
    crab_metadata::segmented::SegmentIndex,
    crab_metadata::segmented::SegmentWrite,
)> {
    crab_metadata::manifests::append_shard_index(base, generation, shard_hashes)
        .map_err(CrabError::from)
}

pub fn append_pack_index(
    base: crab_metadata::segmented::SegmentIndex,
    generation: u64,
    packs: &[PackManifestEntry],
) -> Result<(
    String,
    crab_metadata::segmented::SegmentIndex,
    crab_metadata::segmented::SegmentWrite,
)> {
    crab_metadata::manifests::append_pack_index(base, generation, packs).map_err(CrabError::from)
}

pub fn compact_shard_index(
    generation: u64,
    shard_hashes: &[String],
) -> Result<(
    String,
    crab_metadata::segmented::SegmentIndex,
    crab_metadata::segmented::SegmentWrite,
)> {
    crab_metadata::manifests::compact_shard_index(generation, shard_hashes).map_err(CrabError::from)
}

pub fn compact_pack_index(
    generation: u64,
    packs: &[PackManifestEntry],
) -> Result<(
    String,
    crab_metadata::segmented::SegmentIndex,
    crab_metadata::segmented::SegmentWrite,
)> {
    crab_metadata::manifests::compact_pack_index(generation, packs).map_err(CrabError::from)
}

pub async fn upload_segmented_bulk(
    store: &Store,
    router: &StoreLayout,
    bulk: &BulkData,
) -> Result<()> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::upload_segmented_bulk(store.as_storage(), &router, bulk)
        .await
        .map_err(CrabError::from)
}

pub async fn upload_bulk_if_absent(
    store: &Store,
    router: &StoreLayout,
    prefix: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::upload_bulk_if_absent(
        store.as_storage(),
        &router,
        prefix,
        hash,
        bytes,
    )
    .await
    .map_err(CrabError::from)
}

pub async fn write_manifest_cas(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
    etag: &str,
) -> Result<String> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::write_manifest_cas(store.as_storage(), &router, manifest, etag)
        .await
        .map_err(CrabError::from)
}

pub async fn create_manifest(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<()> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::create_manifest(store.as_storage(), &router, manifest)
        .await
        .map_err(CrabError::from)
}

pub async fn create_manifest_with_etag(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<String> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::create_manifest_with_etag(store.as_storage(), &router, manifest)
        .await
        .map_err(CrabError::from)
}

pub async fn materialize_active_active_manifest_projection(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<()> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::materialize_active_active_manifest_projection(
        store.as_storage(),
        &router,
        manifest,
    )
    .await
    .map_err(CrabError::from)
}

#[must_use]
pub fn serialize_shard_list(hashes: &[String]) -> Vec<u8> {
    crab_metadata::manifests::serialize_shard_list(hashes)
}

pub fn parse_shard_list(bytes: &[u8]) -> Result<Vec<String>> {
    crab_metadata::manifests::parse_shard_list(bytes).map_err(CrabError::from)
}

#[must_use]
pub fn serialize_pack_list(packs: &[PackManifestEntry]) -> Vec<u8> {
    crab_metadata::manifests::serialize_pack_list(packs)
}

pub fn parse_pack_list(bytes: &[u8]) -> Result<Vec<PackManifestEntry>> {
    crab_metadata::manifests::parse_pack_list(bytes).map_err(CrabError::from)
}

pub fn parse_pack_segment_entries(
    segment: &crab_metadata::segmented::SegmentRef,
    bytes: &[u8],
    path: &str,
) -> Result<Vec<PackManifestEntry>> {
    crab_metadata::manifests::parse_pack_segment_entries(segment, bytes, path)
        .map_err(CrabError::from)
}

pub fn validate_manifest_payload(manifest: &Manifest) -> Result<()> {
    crab_metadata::manifests::validate_manifest_payload(manifest).map_err(CrabError::from)
}

#[must_use]
pub fn manifest_reachable_objects(
    manifest: &Manifest,
    graph: Option<&dyn crab_metadata::commit_graph::CommitGraphTraversal>,
) -> std::collections::HashSet<String> {
    crab_metadata::manifests::manifest_reachable_objects(manifest, graph)
}
