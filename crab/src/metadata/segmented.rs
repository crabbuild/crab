//! Compatibility Adapter for storage-backed segmented metadata helpers.

use serde::Deserialize;

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

/// Read a segment index by content hash.
pub async fn read_index(
    store: &Store,
    router: &StoreLayout,
    kind: crab_metadata::segmented::SegmentKind,
    index_hash: &str,
) -> Result<crab_metadata::segmented::SegmentIndex> {
    let router = storage_layout(store, router);
    crab_metadata::segmented_store::read_index(store.as_storage(), &router, kind, index_hash)
        .await
        .map_err(CrabError::from)
}

/// Read every record referenced by an ordered segment index.
pub async fn read_records<T: for<'de> Deserialize<'de>>(
    store: &Store,
    router: &StoreLayout,
    kind: crab_metadata::segmented::SegmentKind,
    index_hash: &str,
) -> Result<Vec<T>> {
    let router = storage_layout(store, router);
    crab_metadata::segmented_store::read_records(store.as_storage(), &router, kind, index_hash)
        .await
        .map_err(CrabError::from)
}

/// Upload an immutable segment or index if it is absent.
pub async fn upload_if_absent(
    store: &Store,
    router: &StoreLayout,
    relative_path: &str,
    bytes: &[u8],
) -> Result<()> {
    let router = storage_layout(store, router);
    crab_metadata::segmented_store::upload_if_absent(
        store.as_storage(),
        &router,
        relative_path,
        bytes,
    )
    .await
    .map_err(CrabError::from)
}

/// Upload all segment objects before their index object.
pub async fn upload_write(
    store: &Store,
    router: &StoreLayout,
    write: &crab_metadata::segmented::SegmentWrite,
) -> Result<()> {
    let router = storage_layout(store, router);
    crab_metadata::segmented_store::upload_write(store.as_storage(), &router, write)
        .await
        .map_err(CrabError::from)
}
