//! Canonical-v1 repository layout open and initialization boundary.

use crab_metadata::layout_descriptor::{
    LayoutDescriptor, ensure_canonical_layout, read_canonical_layout,
};

use crate::core::error::{CrabError, Result};
use crate::storage::{StoreLayout, store::Store};

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

/// Opens and validates the sole canonical-v1 repository layout.
pub async fn open(store: &Store, router: &StoreLayout) -> Result<LayoutDescriptor> {
    let storage_router = storage_layout(store, router);
    let descriptor = read_canonical_layout(store.as_storage(), &storage_router)
        .await
        .map_err(CrabError::from)?;
    tracing::debug!(
        remote_layout = ?descriptor.layout,
        repo_prefix = %router.repo_prefix(),
        "opened canonical repository layout"
    );
    Ok(descriptor)
}

/// Creates or verifies the canonical-v1 layout before manifest publication.
pub async fn initialize(store: &Store, router: &StoreLayout) -> Result<LayoutDescriptor> {
    let storage_router = storage_layout(store, router);
    let descriptor = ensure_canonical_layout(store.as_storage(), &storage_router)
        .await
        .map_err(CrabError::from)?;
    tracing::info!(
        remote_layout = ?descriptor.layout,
        repo_prefix = %router.repo_prefix(),
        "initialized canonical repository layout"
    );
    Ok(descriptor)
}
