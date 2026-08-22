//! Compatibility Adapter for repository manifest helpers.

pub use crab_metadata::manifest_store::{
    ManifestHistoryEntry, RefJournalCompaction, RepositorySnapshot,
};
pub use crab_metadata::manifests::{BulkData, Manifest, PackManifestEntry};
pub use crab_metadata::ref_journal::{
    RefJournalCommitResult, RefJournalEdit, RefJournalHeadSnapshot, RefJournalTransaction,
};

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

pub async fn compact_ref_journal(
    store: &Store,
    router: &StoreLayout,
    created_at: String,
    pusher: Option<String>,
    session_id: String,
) -> Result<Option<RefJournalCompaction>> {
    let router = storage_layout(store, router);
    crab_metadata::manifest_store::compact_ref_journal(
        store.as_storage(),
        &router,
        created_at,
        pusher,
        session_id,
    )
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
    summary: Option<&crab_metadata::commit_graph::CommitGraphSummary>,
) -> std::collections::HashSet<String> {
    crab_metadata::manifests::manifest_reachable_objects(manifest, summary)
}
