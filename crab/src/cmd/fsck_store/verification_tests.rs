use super::tests::{shard_with_file, test_store, test_xorb, upload_shard, write_manifest};
use super::*;
use futures_util::stream::{self, BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy)]
enum SnapshotChange {
    Manifest,
    Journal,
    MissingLayout,
    CorruptLayout,
    UnsupportedLayout,
    LayoutFormatting,
}

#[derive(Debug)]
struct MovingSnapshotStore {
    inner: Arc<dyn ObjectStore>,
    repo_prefix: String,
    trigger: Path,
    change: SnapshotChange,
    moved: AtomicBool,
    writes: AtomicUsize,
}

impl MovingSnapshotStore {
    async fn move_snapshot(&self) -> Result<()> {
        use crate::metadata::manifest::{
            RefJournalEdit, RefJournalTransaction, commit_ref_journal_transaction,
            read_ref_journal_head, write_manifest_cas,
        };
        let store = Store::new(Arc::clone(&self.inner));
        let router = StoreLayout::new(store.clone(), self.repo_prefix.clone());
        if matches!(self.change, SnapshotChange::Journal) {
            let name = "refs/heads/main";
            let head = read_ref_journal_head(&store, &router, name).await?;
            let transaction = RefJournalTransaction::new(
                std::collections::BTreeMap::from([(
                    name.to_owned(),
                    head.visible_transaction.clone(),
                )]),
                vec![RefJournalEdit {
                    ref_name: name.to_owned(),
                    old_oid: None,
                    new_oid: Some("a".repeat(40)),
                    peeled_oid: None,
                    lock_holder: None,
                    visibility_evidence_hash: None,
                }],
                None,
                Vec::new(),
                Vec::new(),
            )?;
            commit_ref_journal_transaction(&store, &router, &transaction, &[head]).await?;
        } else if matches!(self.change, SnapshotChange::Manifest) {
            let (mut manifest, etag) = read_manifest(&store, &router).await?;
            manifest.generation += 1;
            manifest.seal_git_validation();
            write_manifest_cas(&store, &router, &manifest, &etag).await?;
        } else if matches!(self.change, SnapshotChange::MissingLayout) {
            store.delete(&router.layout_descriptor_path()).await?;
        } else {
            let path = router.layout_descriptor_path();
            let (body, etag) = store.get_with_etag(&path).await?;
            let mut layout: crab_metadata::layout_descriptor::LayoutDescriptor =
                serde_json::from_slice(&body).unwrap();
            match self.change {
                SnapshotChange::CorruptLayout => layout.digest = "0".repeat(64),
                SnapshotChange::UnsupportedLayout => layout.recipe_page_entries += 1,
                SnapshotChange::LayoutFormatting => {}
                _ => unreachable!(),
            }
            store
                .update(
                    &path,
                    serde_json::to_vec_pretty(&layout).unwrap().into(),
                    etag,
                )
                .await?;
        }
        Ok(())
    }

    fn deny_write(&self) -> object_store::Error {
        self.writes.fetch_add(1, Ordering::Relaxed);
        object_store::Error::PermissionDenied {
            path: self.repo_prefix.clone(),
            source: Box::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        }
    }
}

impl std::fmt::Display for MovingSnapshotStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("moving-snapshot fixture")
    }
}

#[async_trait::async_trait]
impl ObjectStore for MovingSnapshotStore {
    async fn put_opts(
        &self,
        _: &Path,
        _: PutPayload,
        _: PutOptions,
    ) -> object_store::Result<PutResult> {
        Err(self.deny_write())
    }

    async fn put_multipart_opts(
        &self,
        _: &Path,
        _: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(self.deny_write())
    }

    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        if path == &self.trigger && !self.moved.swap(true, Ordering::Relaxed) {
            // A separate writer changes metadata while the reader is suspended
            // on a deterministic object read, before its final revalidation.
            self.move_snapshot()
                .await
                .map_err(|source| object_store::Error::Generic {
                    store: "moving-snapshot fixture",
                    source: Box::new(source),
                })?;
        }
        self.inner.get_opts(path, options).await
    }

    fn delete_stream(
        &self,
        _: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        stream::iter([Err(self.deny_write())]).boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, _: &Path, _: &Path, _: CopyOptions) -> object_store::Result<()> {
        Err(self.deny_write())
    }
}

#[tokio::test]
async fn verification_rejects_manifest_and_uncompacted_journal_movement() {
    for journal_commit in [false, true] {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let content = [0x27; 1024];
        let file_bytes = blake3::hash(&content).into();
        let (xorb_hash, xorb_bytes) = test_xorb(&content);
        let (shard_bytes, shard_hash) = shard_with_file(MerkleHash::from(file_bytes), xorb_hash);
        upload_shard(&store, &router, &shard_hash, shard_bytes).await;
        store
            .put(&router.xorb_path(&xorb_hash), xorb_bytes)
            .await
            .unwrap();
        write_manifest(&store, &prefix, &[shard_hash.hex().as_str()], &[]).await;
        let snapshot = read_repository_snapshot(&store, &router).await.unwrap();
        let moving = Arc::new(MovingSnapshotStore {
            inner: Arc::clone(store.inner()),
            repo_prefix: prefix.clone(),
            trigger: router.xorb_path(&xorb_hash),
            change: if journal_commit {
                SnapshotChange::Journal
            } else {
                SnapshotChange::Manifest
            },
            moved: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        });
        let result = StoreChecker::new(Store::new(moving.clone()), prefix)
            .verify_pointer_data(
                &snapshot,
                &[Pointer {
                    file_hash: file_bytes,
                    size: 1024,
                    shard_hint: None,
                }],
                &CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(CrabError::Protocol(ref reason)) if reason.contains("snapshot changed")),
            "journal_commit={journal_commit}: {result:?}"
        );
        assert_eq!(moving.writes.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn layout_changes_during_capture_or_verification_fail_without_checker_writes() {
    for scoped in [false, true] {
        for during_capture in [false, true] {
            for change in [
                SnapshotChange::MissingLayout,
                SnapshotChange::CorruptLayout,
                SnapshotChange::UnsupportedLayout,
                SnapshotChange::LayoutFormatting,
            ] {
                let (mut store, prefix) = test_store();
                let scope = scoped.then(|| crab_types::storage::StorageScope {
                    repo_prefix: "views/layout-check".to_owned(),
                    global_prefix: "views/layout-check/.crab".to_owned(),
                    source_repo: prefix.clone(),
                    scope_hash: "read-scope".to_owned(),
                });
                if let Some(scope) = &scope {
                    store = store.with_storage_scope(scope.clone());
                }
                let router = StoreLayout::new(store.clone(), prefix.clone());
                let content = [0x47; 1024];
                let file_hash = blake3::hash(&content).into();
                let (xorb_hash, xorb_bytes) = test_xorb(&content);
                let (shard_bytes, shard_hash) =
                    shard_with_file(MerkleHash::from(file_hash), xorb_hash);
                upload_shard(&store, &router, &shard_hash, shard_bytes).await;
                store
                    .put(&router.xorb_path(&xorb_hash), xorb_bytes)
                    .await
                    .unwrap();
                write_manifest(&store, &prefix, &[shard_hash.hex().as_str()], &[]).await;
                let snapshot = read_repository_snapshot(&store, &router).await.unwrap();
                let moving = Arc::new(MovingSnapshotStore {
                    inner: Arc::clone(store.inner()),
                    repo_prefix: router.repo_prefix().to_owned(),
                    trigger: if during_capture {
                        router.ref_journal_frontier_path(&snapshot.manifest.git_validation_digest)
                    } else {
                        router.xorb_path(&xorb_hash)
                    },
                    change,
                    moved: AtomicBool::new(false),
                    writes: AtomicUsize::new(0),
                });
                let mut reader = Store::new(moving.clone());
                if let Some(scope) = scope {
                    reader = reader.with_storage_scope(scope);
                }
                let result = if during_capture {
                    let reader_layout = StoreLayout::new(reader.clone(), prefix.clone());
                    read_repository_snapshot(&reader, &reader_layout)
                        .await
                        .map(|_| ())
                } else {
                    StoreChecker::new(reader, prefix)
                        .verify_pointer_data(
                            &snapshot,
                            &[Pointer {
                                file_hash,
                                size: 1024,
                                shard_hint: None,
                            }],
                            &CancellationToken::new(),
                        )
                        .await
                        .map(|proof| assert_eq!((proof.verified, proof.issues.len()), (1, 0)))
                };
                assert_eq!(
                    result.is_ok(),
                    matches!(change, SnapshotChange::LayoutFormatting),
                    "scoped={scoped}, capture={during_capture}, change={change:?}: {result:?}"
                );
                assert!(moving.moved.load(Ordering::Relaxed));
                assert_eq!(moving.writes.load(Ordering::Relaxed), 0);
                assert_eq!(
                    read_manifest(&store, &router).await.unwrap().0,
                    snapshot.manifest
                );
            }
        }
    }
}
