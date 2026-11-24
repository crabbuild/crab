//! Probe the configured catalog and coordinated storage boundary.

use super::GIT_ADMISSION_CAPACITY;
use crate::{Config, Result, catalog::CatalogStore, transfer_admission::TransferAdmission};
use bytes::Bytes;
use crab_storage::StorageError;
use uuid::Uuid;

pub(crate) async fn repository_read_layout(
    root: &crate::storage_root::StorageRoot,
    origin: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<Option<crab_storage::StoreLayout<crab_storage::Store>>> {
    #[cfg(feature = "provider-cdn")]
    if let Some(binding) = root.cdn.get(origin.repo_prefix()) {
        let store = binding
            .read_store(
                origin.store(),
                origin.repo_prefix(),
                env!("CARGO_PKG_VERSION"),
                std::time::Duration::from_secs(300),
                crab_storage::cdn::CdnConsumer::Server,
            )
            .await?;
        return Ok(Some(crab_storage::StoreLayout::with_global_prefix(
            store,
            origin.repo_prefix().to_owned(),
            origin.global_prefix().to_owned(),
        )));
    }
    #[cfg(not(feature = "provider-cdn"))]
    let _ = (root, origin);
    Ok(None)
}

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

    #[cfg(feature = "provider-cdn-qualification")]
    #[tokio::test]
    async fn read_layout_selects_exact_catalog_placement_and_keeps_control_on_origin() {
        use object_store::path::Path;

        let directory = tempfile::tempdir().unwrap();
        let prefix = "repositories/org/models";
        let hash = "a".repeat(64);
        let relative = format!("v2/history/aa/{hash}");
        let approval = crab_storage::cdn::write_publication_fixture(
            directory.path().join("receipt.json"),
            "gs://fixture-bucket",
            "gs://approved-export",
            prefix,
            "https://downloads.example.com",
            &relative,
            6,
        )
        .unwrap();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_target_identity([9; 32])
            .with_bucket_identity(crab_storage::BucketIdentity::new(
                crab_storage::StorageProviderKind::Gcs,
                "fixture-bucket",
                "fixture-bucket",
            ))
            .with_native_cdn_origin(Some("gs://fixture-bucket".into()));
        let mut root = crate::storage_root::StorageRoot::memory(store, "repositories");
        root.cdn = Arc::new(
            toml::from_str(&format!(
                r#"
["repositories/org/models"]
origin = "gs://fixture-bucket"
export_origin = "gs://approved-export"
base_url = "https://downloads.example.com"
access = "public"
publication = {{ path = {path:?}, sha256 = {digest:?} }}
"#,
                path = approval.path,
                digest = approval.sha256,
            ))
            .unwrap(),
        );
        let key = |prefix: &str| Path::from(format!("{prefix}/{relative}"));
        let origin = root.repository_layout(prefix.into());
        let route = repository_read_layout(&root, &origin)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.global_prefix(), "repositories/.crab");
        assert!(origin.store().cdn_attempt(&key(prefix)).is_none());
        assert!(route.store().cdn_attempt(&key(prefix)).is_some());
        assert!(
            route
                .store()
                .cdn_attempt(&Path::from(format!("{prefix}/v2/root")))
                .is_none()
        );
        assert!(
            route
                .store()
                .cdn_attempt(&Path::from(format!("repositories/.crab/xorbs/aa/{hash}")))
                .is_none()
        );

        let sibling = "repositories/org/models-archive";
        let unrelated = repository_read_layout(&root, &root.repository_layout(sibling.into()))
            .await
            .unwrap();
        assert!(unrelated.is_none());
    }
}
