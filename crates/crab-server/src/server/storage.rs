//! Probe the configured catalog and coordinated storage boundary.

use super::GIT_ADMISSION_CAPACITY;
use crate::{Config, Result, catalog::CatalogStore, transfer_admission::TransferAdmission};
use bytes::Bytes;
use crab_storage::StorageError;
use uuid::Uuid;

pub(super) fn transfer_admission(catalog: &CatalogStore) -> TransferAdmission {
    TransferAdmission::new(
        catalog.root().store.clone(),
        catalog
            .root()
            .path(".crab/http-server/v1/admission")
            .to_string(),
        GIT_ADMISSION_CAPACITY,
    )
}

pub(super) async fn probe_storage_contract(
    catalog: &CatalogStore,
    transfer_admission: &TransferAdmission,
) -> Result<()> {
    let root = catalog.root();
    root.store
        .list_prefix_bounded(&root.path(".crab/http-server/v1"), 1)
        .await?;
    transfer_admission.probe().await?;

    // A unique object avoids cross-pod interference. Provider lifecycle rules
    // bound residue if a pod dies between creation and deletion.
    let path = root.path(&format!(
        ".crab/http-server/v1/auth/preflight/{}",
        Uuid::now_v7()
    ));
    root.store
        .put_overwrite(&path, Bytes::from_static(b"crab-storage-probe-v1"))
        .await?;
    match root.store.delete(&path).await {
        Ok(()) | Err(StorageError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    match root.store.head(&path).await {
        Err(StorageError::NotFound { .. }) => Ok(()),
        Ok(_) => Err(crate::Error::StorageProbe(
            "an object remained visible after a successful delete",
        )),
        Err(error) => Err(error.into()),
    }
}

/// Validate the durable catalog and the storage coordination write path.
///
/// Returns the original configuration, storage, catalog, or coordination error.
pub async fn probe_storage(config: &Config) -> Result<()> {
    let catalog = CatalogStore::from_config(config)?;
    catalog.load().await?;
    probe_storage_contract(&catalog, &transfer_admission(&catalog)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_storage::Store;
    use std::sync::Arc;
    #[tokio::test]
    async fn storage_preflight_leaves_no_live_probe_object() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let catalog = CatalogStore::new(crate::storage_root::StorageRoot::memory(
            store.clone(),
            "repositories",
        ));
        let admission = transfer_admission(&catalog);

        probe_storage_contract(&catalog, &admission).await.unwrap();
        probe_storage_contract(&catalog, &admission).await.unwrap();

        let objects = store
            .list_prefix(&catalog.root().path(".crab/http-server/v1/auth/preflight"))
            .await
            .unwrap();
        assert!(objects.is_empty());
    }
}
