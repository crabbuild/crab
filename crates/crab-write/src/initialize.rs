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
/// HEAD must be a valid fully qualified branch ref; invalid input performs no I/O.
///
/// # Errors
///
/// Returns a storage or metadata error when canonical roots cannot be read or
/// created, [`WriteError::InitialHead`] for invalid HEAD input, and
/// [`WriteError::CorruptObject`] for a nonempty unowned prefix.
pub async fn initialize_repository(
    store: &Store,
    layout: &StoreLayout<Store>,
    head: &str,
) -> Result<()> {
    if !head.starts_with("refs/heads/") {
        return Err(WriteError::InitialHead {
            head: head.to_owned(),
            source: None,
        });
    }
    crab_git::refname::validate_push_refname(head).map_err(|source| WriteError::InitialHead {
        head: head.to_owned(),
        source: Some(source),
    })?;
    match store.head(&layout.layout_descriptor_path()).await {
        Ok(_) => {
            read_canonical_layout(store, layout).await?;
        }
        Err(StorageError::NotFound { .. }) => {
            let prefix = Path::from(layout.repo_prefix().trim_end_matches('/'));
            if store.list_prefix_bounded(&prefix, 0).await?.is_none() {
                // Another initializer may create the descriptor between HEAD
                // and LIST. Adopt only validated canonical ownership; an
                // unrelated nonempty prefix must still remain untouched.
                match store.head(&layout.layout_descriptor_path()).await {
                    Ok(_) => { read_canonical_layout(store, layout).await?; }
                    Err(StorageError::NotFound { .. }) => return Err(WriteError::CorruptObject {
                        path: layout.layout_descriptor_path().to_string(),
                        reason: "the remote prefix contains data but is not an initialized Crab repository; Crab left it unchanged. Verify the remote URL before deleting disposable data".to_owned(),
                    }),
                    Err(error) => return Err(error.into()),
                }
            } else {
                ensure_canonical_layout(store, layout).await?;
            }
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

/// Publish an initial import after its immutable content and Git proof are uploaded.
///
/// The caller must validate the candidate, authorize this unplanned direct write,
/// and hold ref leases and GC fences. Returns None if admission became stale.
/// Await completion: a failed manifest CAS response may have committed remotely.
pub async fn publish_initial_manifest(
    store: &Store,
    layout: &StoreLayout<Store>,
    current: &crab_metadata::manifest_store::RepositorySnapshot,
    manifest: &Manifest,
    ttl: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<String>> {
    crate::with_ref_namespace(store, layout, ttl, cancel, |cancel| async move {
        if cancel.is_cancelled() {
            return Err(WriteError::Cancelled);
        }
        let fresh = crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
        // Uploads can overlap another first import or journal create. The shared
        // namespace gate prevents either path from crossing this recheck.
        if fresh.manifest_etag != current.manifest_etag
            || !fresh.journal.refs.is_empty()
            || !fresh.journal.transactions.is_empty()
            || !fresh.journal.visible_heads.is_empty()
        {
            return Ok(None);
        }
        if cancel.is_cancelled() {
            return Err(WriteError::Cancelled);
        }
        Ok(Some(
            crab_metadata::manifest_store::write_manifest_cas(
                store,
                layout,
                manifest,
                &fresh.manifest_etag,
            )
            .await?,
        ))
    })
    .await
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
    async fn invalid_initial_head_leaves_the_prefix_empty() {
        for head in [
            "",
            "main",
            "refs/tags/v1",
            "refs/heads/",
            "refs/heads/a..b",
            "refs/heads/a b",
        ] {
            let (store, layout) = repository();
            let result = initialize_repository(&store, &layout, head).await;
            assert!(result.is_err(), "accepted {head:?}");
            assert!(
                store
                    .list_prefix(&layout.repo_path(""))
                    .await
                    .unwrap()
                    .is_empty(),
                "wrote objects for {head:?}"
            );
        }
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

    #[derive(Debug)]
    struct InitializeAfterHead {
        inner: Arc<InMemory>,
        fired: std::sync::atomic::AtomicBool,
    }

    impl std::fmt::Display for InitializeAfterHead {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("InitializeAfterHead")
        }
    }

    #[async_trait::async_trait]
    impl object_store::ObjectStore for InitializeAfterHead {
        async fn put_opts(
            &self,
            path: &Path,
            body: object_store::PutPayload,
            options: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(path, body, options).await
        }
        async fn put_multipart_opts(
            &self,
            path: &Path,
            options: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(path, options).await
        }
        async fn get_opts(
            &self,
            path: &Path,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            let head = options.head;
            let result = self.inner.get_opts(path, options).await;
            if head && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                assert!(matches!(result, Err(object_store::Error::NotFound { .. })));
                let store = Store::new(self.inner.clone());
                let layout = StoreLayout::new(store.clone(), "team/project".to_owned());
                initialize_repository(&store, &layout, "refs/heads/main")
                    .await
                    .unwrap();
            }
            result
        }
        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }
        fn delete_stream(
            &self,
            paths: futures_util::stream::BoxStream<'static, object_store::Result<Path>>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(paths)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[tokio::test]
    async fn initializer_adopts_roots_created_between_head_and_list() {
        let store = Store::new(Arc::new(InitializeAfterHead {
            inner: Arc::new(InMemory::new()),
            fired: std::sync::atomic::AtomicBool::new(false),
        }));
        let layout = StoreLayout::new(store.clone(), "team/project".to_owned());
        initialize_repository(&store, &layout, "refs/heads/other")
            .await
            .unwrap();
        let (manifest, _) = read_manifest(&store, &layout).await.unwrap();
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
