use crab_metadata::layout_descriptor::{ensure_canonical_layout, read_canonical_layout};
use crab_metadata::manifest_store::{create_manifest, read_manifest};
use crab_metadata::manifests::Manifest;
use crab_storage::{StorageError, Store, StoreLayout};
use object_store::path::Path;

use crate::{Result, WriteError};

/// Create or adopt the canonical metadata roots for one repository prefix.
///
/// A missing layout is created only when the repository prefix is empty. An
/// existing canonical layout may receive a missing generation-zero manifest.
/// Concurrent initializers converge through conditional creates.
///
/// # Errors
///
/// Returns a storage or metadata error when canonical roots cannot be read or
/// created, and [`WriteError::CorruptObject`] for a nonempty unowned prefix.
pub async fn initialize_repository(
    store: &Store,
    layout: &StoreLayout<Store>,
    head: &str,
) -> Result<()> {
    match store.head(&layout.layout_descriptor_path()).await {
        Ok(_) => {
            read_canonical_layout(store, layout).await?;
        }
        Err(StorageError::NotFound { .. }) => {
            let prefix = Path::from(layout.repo_prefix().trim_end_matches('/'));
            if store.list_prefix_bounded(&prefix, 0).await?.is_none() {
                return Err(WriteError::CorruptObject {
                    path: layout.layout_descriptor_path().to_string(),
                    reason: "canonical v1 layout descriptor is missing but repository objects already exist; reset this isolated development repository instead of converting it in place".to_owned(),
                });
            }
            ensure_canonical_layout(store, layout).await?;
        }
        Err(error) => return Err(error.into()),
    }

    let manifest = Manifest::default_for_repo(head);
    match create_manifest(store, layout, &manifest).await {
        Ok(()) => Ok(()),
        Err(crab_metadata::error::MetadataError::Storage {
            source: StorageError::StateConflict { .. },
        }) => read_manifest(store, layout)
            .await
            .map(|_| ())
            .map_err(Into::into),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;

    use super::*;

    fn repository() -> (Store, StoreLayout<Store>) {
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "team/project".to_owned());
        (store, layout)
    }

    #[tokio::test]
    async fn initialization_creates_canonical_roots_and_adopts_them() {
        let (store, layout) = repository();

        initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        initialize_repository(&store, &layout, "refs/heads/other")
            .await
            .unwrap();

        read_canonical_layout(&store, &layout).await.unwrap();
        let (manifest, _) = read_manifest(&store, &layout).await.unwrap();
        assert_eq!(manifest.generation, 0);
        assert_eq!(manifest.head, "refs/heads/main");
    }

    #[tokio::test]
    async fn initialization_rejects_nonempty_prefix_without_a_layout() {
        let (store, layout) = repository();
        store
            .put(
                &layout.repo_path("orphan"),
                Bytes::from_static(b"legacy state"),
            )
            .await
            .unwrap();

        let error = initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap_err();

        assert!(matches!(error, WriteError::CorruptObject { .. }));
        assert!(store.head(&layout.layout_descriptor_path()).await.is_err());
        assert!(store.head(&layout.manifest_path()).await.is_err());
    }

    #[tokio::test]
    async fn invalid_existing_layout_cannot_receive_a_manifest() {
        let (store, layout) = repository();
        store
            .put(
                &layout.layout_descriptor_path(),
                Bytes::from_static(br#"{"schema_version":2}"#),
            )
            .await
            .unwrap();

        initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap_err();

        assert!(store.head(&layout.manifest_path()).await.is_err());
    }
}
