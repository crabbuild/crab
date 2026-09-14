//! Request-minimal publication through one immutable capsule and one mutable root.

use crab_metadata::request_minimal::{
    Capsule, CapsulePointer, CapsuleRun, CapsuleTransaction, Checkpoint, CheckpointPointer,
    RepositoryRoot, RootRecord, create_root, load_root,
};
use crab_storage::{StorageError, Store, StoreLayout};

use crate::{Result, WriteError};

pub use crab_metadata::request_minimal::RootSnapshot;

/// Initialize a request-minimal repository with one unborn generation-zero root.
pub async fn initialize(
    router: &StoreLayout<Store>,
    repository_id: &str,
    head: &str,
) -> Result<RootSnapshot> {
    let record = RootRecord::encode(RepositoryRoot::initial(repository_id, head)?)?;
    match load_root(router).await {
        Ok(existing) => return Ok(existing),
        Err(crab_metadata::error::MetadataError::Storage {
            source: StorageError::NotFound { .. },
        }) => {}
        Err(error) => return Err(error.into()),
    }
    let prefix =
        object_store::path::Path::from(format!("{}/", router.repo_prefix().trim_end_matches('/')));
    let existing = router.store().list_prefix_bounded(&prefix, 1).await?;
    if !existing.is_some_and(|objects| objects.is_empty()) {
        return match load_root(router).await {
            Ok(root) => Ok(root),
            Err(crab_metadata::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => Err(WriteError::CorruptObject {
                path: prefix.to_string(),
                reason: "repository prefix contains data but has no request-minimal root; Crab left it unchanged"
                    .to_owned(),
            }),
            Err(error) => Err(error.into()),
        };
    }
    match create_root(router, record).await {
        Ok(created) => Ok(created),
        Err(crab_metadata::error::MetadataError::Storage {
            source: StorageError::StateConflict { .. },
        }) => Ok(load_root(router).await?),
        Err(error) => Err(error.into()),
    }
}

/// Open and verify the single root used for advertisement and publication CAS.
pub async fn open_root(router: &StoreLayout<Store>) -> Result<RootSnapshot> {
    Ok(load_root(router).await?)
}

/// Publish one verified capsule, then atomically advance the root.
///
/// The caller supplies the root snapshot retained from advertisement and must
/// first prove authorization, exact Git graph closure, pack integrity,
/// fast-forward policy, and every external content dependency against that
/// snapshot. A clean attempt performs capsule PUT, optional capsule readback,
/// and root CAS; together with [`open_root`] the complete push uses three
/// requests for a checksum-qualified provider and four otherwise.
pub async fn publish(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<RootSnapshot> {
    validate_capsule_binding(&base, transaction, capsule)?;
    let (refs, peeled_refs) = apply_ref_edits(&base, transaction)?;
    let mut run = CapsuleRun::leaf(capsule.clone())?;
    let mut frontier = base.record().root().capsule_frontier().to_vec();
    while frontier
        .last()
        .is_some_and(|existing| existing.level() == run.level())
    {
        let pointer = frontier
            .pop()
            .ok_or_else(|| WriteError::Internal("capsule run frontier became empty".to_owned()))?;
        let older = load_run(router, &pointer).await?;
        run = older.merge(&run)?;
    }
    let capsule_path = router.request_minimal_capsule_path(run.hash());
    router
        .store()
        .put_if_absent_verified(&capsule_path, run.bytes().clone())
        .await?;

    let pointer = CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.level(),
        run.transaction_ids(),
        run.newest_base_root_digest(),
    )?;
    frontier.push(pointer);
    let transaction_id = transaction.id()?;
    let next = base.record().root().advance(
        base.record().digest(),
        refs,
        peeled_refs,
        frontier,
        &transaction_id,
    )?;
    let candidate = RootRecord::encode(next)?;
    let root_path = router.request_minimal_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_successor(candidate, etag)?),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::RequestMinimalRootChanged {
            path: root_path.to_string(),
        }),
        Err(source) => {
            reconcile_root_update(router, base.record(), candidate, transaction, source).await
        }
    }
}

/// Publish a complete checkpoint and atomically replace the covered root's frontier.
pub async fn publish_checkpoint(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    checkpoint: &Checkpoint,
) -> Result<RootSnapshot> {
    if let Some(fence) = base.record().root().gc_fence() {
        return Err(WriteError::RequestMinimalGcFenced {
            fence_id: fence.id().to_owned(),
            expires_at_unix: fence.expires_at_unix(),
        });
    }
    if checkpoint.covered_generation() != base.record().root().generation()
        || checkpoint.covered_root_digest() != base.record().digest()
    {
        return Err(WriteError::CorruptObject {
            path: "request-minimal checkpoint".to_owned(),
            reason: "checkpoint does not cover the exact CAS base".to_owned(),
        });
    }
    let object_count = checkpoint
        .git_packs()
        .iter()
        .try_fold(0_u64, |total, pack| {
            total.checked_add(pack.object_count()).ok_or_else(|| {
                WriteError::Internal("checkpoint object count overflowed".to_owned())
            })
        })?;
    let pack_count = u32::try_from(checkpoint.git_packs().len())
        .map_err(|_| WriteError::Internal("checkpoint pack count overflowed".to_owned()))?;
    let path = router.request_minimal_checkpoint_path(checkpoint.hash());
    router
        .store()
        .put_if_absent_verified(&path, checkpoint.bytes().clone())
        .await?;
    let pointer = CheckpointPointer::new(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        pack_count,
        object_count,
    )?;
    let next = base
        .record()
        .root()
        .install_checkpoint(base.record().digest(), pointer)?;
    let candidate = RootRecord::encode(next)?;
    let root_path = router.request_minimal_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_checkpoint(candidate, etag)?),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::RequestMinimalRootChanged {
            path: root_path.to_string(),
        }),
        Err(source) => {
            reconcile_checkpoint_update(router, base.record(), candidate, checkpoint, source).await
        }
    }
}

/// Atomically fence a root for one exclusive GC sweep.
pub async fn begin_gc(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    fence: crab_metadata::request_minimal::GcFence,
) -> Result<RootSnapshot> {
    let fence_id = fence.id().to_owned();
    let next = base
        .record()
        .root()
        .begin_gc(base.record().digest(), fence)?;
    update_maintenance_root(router, base, RootRecord::encode(next)?, &fence_id).await
}

/// Atomically clear the exact GC fence after a sweep.
pub async fn end_gc(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    fence_id: &str,
) -> Result<RootSnapshot> {
    let next = base
        .record()
        .root()
        .end_gc(base.record().digest(), fence_id)?;
    update_maintenance_root(router, base, RootRecord::encode(next)?, fence_id).await
}

async fn update_maintenance_root(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    candidate: RootRecord,
    fence_id: &str,
) -> Result<RootSnapshot> {
    let root_path = router.request_minimal_root_path();
    match router
        .store()
        .update(&root_path, candidate.bytes().clone(), base.etag().clone())
        .await
    {
        Ok(etag) => Ok(base.committed_maintenance(candidate, etag)?),
        Err(StorageError::StateConflict { .. }) => Err(WriteError::RequestMinimalRootChanged {
            path: root_path.to_string(),
        }),
        Err(source) => {
            let verification = open_root(router).await;
            match verification {
                Ok(snapshot) if snapshot.record().digest() == candidate.digest() => Ok(snapshot),
                Ok(snapshot) if snapshot.record().digest() == base.record().digest() => {
                    Err(source.into())
                }
                Ok(_) => Err(WriteError::RequestMinimalMaintenanceCommitUncertain {
                    fence_id: fence_id.to_owned(),
                    source: Box::new(source),
                    verification: None,
                }),
                Err(verification) => Err(WriteError::RequestMinimalMaintenanceCommitUncertain {
                    fence_id: fence_id.to_owned(),
                    source: Box::new(source),
                    verification: Some(Box::new(verification)),
                }),
            }
        }
    }
}

async fn load_run(router: &StoreLayout<Store>, pointer: &CapsulePointer) -> Result<CapsuleRun> {
    let path = router.request_minimal_capsule_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let actual_size = u64::try_from(bytes.len())
        .map_err(|_| WriteError::Internal("capsule run size cannot be represented".to_owned()))?;
    let run = CapsuleRun::decode(bytes)?;
    if actual_size != pointer.size()
        || run.hash() != pointer.hash()
        || run.level() != pointer.level()
        || run.transaction_ids() != pointer.transaction_ids()
        || run.newest_base_root_digest() != pointer.newest_base_root_digest()
    {
        return Err(WriteError::CorruptObject {
            path: path.to_string(),
            reason: "capsule run does not match its authenticated root pointer".to_owned(),
        });
    }
    Ok(run)
}

fn validate_capsule_binding(
    base: &RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<()> {
    if let Some(fence) = base.record().root().gc_fence() {
        return Err(WriteError::RequestMinimalGcFenced {
            fence_id: fence.id().to_owned(),
            expires_at_unix: fence.expires_at_unix(),
        });
    }
    if transaction.base_root_digest() != base.record().digest()
        || capsule.base_root_digest() != base.record().digest()
        || capsule.transaction_id() != transaction.id()?
    {
        return Err(WriteError::CorruptObject {
            path: "request-minimal capsule".to_owned(),
            reason: "capsule, transaction, and advertised root are not cryptographically bound"
                .to_owned(),
        });
    }
    Ok(())
}

fn apply_ref_edits(
    base: &RootSnapshot,
    transaction: &CapsuleTransaction,
) -> Result<(
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
)> {
    let mut refs = base.record().root().refs().clone();
    let mut peeled_refs = base.record().root().peeled_refs().clone();
    for edit in transaction.edits() {
        let observed = refs.get(edit.ref_name()).map(String::as_str);
        if observed != edit.expected_old() {
            return Err(WriteError::RefChanged {
                ref_name: edit.ref_name().to_owned(),
                path: "request-minimal root".to_owned(),
            });
        }
        match edit.new_oid() {
            Some(new_oid) => {
                refs.insert(edit.ref_name().to_owned(), new_oid.to_owned());
                match edit.peeled_oid() {
                    Some(peeled_oid) => {
                        peeled_refs.insert(edit.ref_name().to_owned(), peeled_oid.to_owned());
                    }
                    None => {
                        peeled_refs.remove(edit.ref_name());
                    }
                }
            }
            None => {
                refs.remove(edit.ref_name());
                peeled_refs.remove(edit.ref_name());
            }
        }
    }
    Ok((refs, peeled_refs))
}

async fn reconcile_root_update(
    router: &StoreLayout<Store>,
    base: &RootRecord,
    candidate: RootRecord,
    transaction: &CapsuleTransaction,
    source: StorageError,
) -> Result<RootSnapshot> {
    let verification = open_root(router).await;
    match verification {
        Ok(snapshot) if snapshot.record().digest() == candidate.digest() => Ok(snapshot),
        Ok(snapshot)
            if snapshot
                .record()
                .root()
                .contains_transaction(&transaction.id()?) =>
        {
            Ok(snapshot)
        }
        Ok(snapshot) if snapshot.record().digest() == base.digest() => Err(source.into()),
        Ok(_) => Err(WriteError::RequestMinimalCommitUncertain {
            transaction_id: transaction.id()?,
            source: Box::new(source),
            verification: None,
        }),
        Err(verification) => Err(WriteError::RequestMinimalCommitUncertain {
            transaction_id: transaction.id()?,
            source: Box::new(source),
            verification: Some(Box::new(verification)),
        }),
    }
}

async fn reconcile_checkpoint_update(
    router: &StoreLayout<Store>,
    base: &RootRecord,
    candidate: RootRecord,
    checkpoint: &Checkpoint,
    source: StorageError,
) -> Result<RootSnapshot> {
    let verification = open_root(router).await;
    match verification {
        Ok(snapshot) if snapshot.record().digest() == candidate.digest() => Ok(snapshot),
        Ok(snapshot) if snapshot.record().digest() == base.digest() => Err(source.into()),
        Ok(_) => Err(WriteError::RequestMinimalCheckpointCommitUncertain {
            checkpoint_hash: checkpoint.hash().to_owned(),
            source: Box::new(source),
            verification: None,
        }),
        Err(verification) => Err(WriteError::RequestMinimalCheckpointCommitUncertain {
            checkpoint_hash: checkpoint.hash().to_owned(),
            source: Box::new(source),
            verification: Some(Box::new(verification)),
        }),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::fmt;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use bytes::Bytes;
    use crab_metadata::request_minimal::{CapsuleGitPack, CapsuleRefEdit};
    use crab_storage::{
        ImmutableWriteVerification, StorageObservation, StorageObserver, StorageOperation,
        StorageOutcome,
    };
    use futures_util::stream::BoxStream;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory,
        path::Path,
    };

    use super::*;

    #[derive(Default)]
    struct RecordingObserver {
        observations: Mutex<Vec<StorageObservation>>,
    }

    impl StorageObserver for RecordingObserver {
        fn started(&self, _operation: StorageOperation) {}

        fn finished(&self, observation: StorageObservation) {
            self.observations.lock().unwrap().push(observation);
        }
    }

    #[derive(Debug)]
    struct LostRootReplyStore {
        inner: Arc<InMemory>,
        root_path: String,
        lost: AtomicBool,
    }

    impl fmt::Display for LostRootReplyStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("LostRootReplyStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for LostRootReplyStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            let lose_reply = location.as_ref() == self.root_path
                && matches!(options.mode, PutMode::Update(_))
                && !self.lost.swap(true, Ordering::AcqRel);
            let result = self.inner.put_opts(location, payload, options).await?;
            if lose_reply {
                return Err(object_store::Error::Generic {
                    store: "request-minimal-root-test",
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "lost root update reply",
                    )),
                });
            }
            Ok(result)
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn transaction(base: &RootSnapshot, old: Option<&str>, new: &str) -> CapsuleTransaction {
        CapsuleTransaction::new(
            base.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                old.map(str::to_owned),
                Some(new.to_owned()),
                None,
            )],
        )
        .unwrap()
    }

    fn capsule(transaction: &CapsuleTransaction) -> Capsule {
        Capsule::build(
            transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK request-minimal test"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            Vec::new(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn clean_publication_uses_four_requests_including_root_open() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
        initialize(&seed_router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();

        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));
        let capsule = capsule(&transaction);

        let published = publish(&router, base, &transaction, &capsule)
            .await
            .unwrap();

        assert_eq!(published.record().root().generation(), 1);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Put,
                StorageOperation::Get,
                StorageOperation::Put,
            ]
        );
    }

    #[tokio::test]
    async fn checksum_qualified_publication_uses_three_requests_including_root_open() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store, "repositories/test".to_owned());
        initialize(&seed_router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();

        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner)
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));
        let capsule = capsule(&transaction);

        let published = publish(&router, base, &transaction, &capsule)
            .await
            .unwrap();

        assert_eq!(published.record().root().generation(), 1);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Put,
                StorageOperation::Put,
            ]
        );
    }

    #[tokio::test]
    async fn binary_carry_adds_one_get_without_an_intermediate_put() {
        let inner = Arc::new(InMemory::new());
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner)
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let base = open_root(&router).await.unwrap();
        let first = transaction(&base, None, &"2".repeat(40));
        publish(&router, base, &first, &capsule(&first))
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();

        let base = open_root(&router).await.unwrap();
        let second = transaction(&base, Some(&"2".repeat(40)), &"3".repeat(40));
        let published = publish(&router, base, &second, &capsule(&second))
            .await
            .unwrap();

        assert_eq!(published.record().root().capsule_frontier().len(), 1);
        assert_eq!(published.record().root().capsule_frontier()[0].level(), 1);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Get,
                StorageOperation::Put,
                StorageOperation::Put,
            ]
        );
    }

    #[tokio::test]
    async fn five_hundred_small_pushes_average_fewer_than_four_qualified_requests() {
        let inner = Arc::new(InMemory::new());
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner)
            .with_immutable_write_verification(ImmutableWriteVerification::Sha256Checksum)
            .with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();

        let mut previous = None;
        let mut published = None;
        for sequence in 1..=500_u64 {
            let base = open_root(&router).await.unwrap();
            let next = format!("{sequence:040x}");
            let transaction = transaction(&base, previous.as_deref(), &next);
            published = Some(
                publish(&router, base, &transaction, &capsule(&transaction))
                    .await
                    .unwrap(),
            );
            previous = Some(next);
        }

        let published = published.unwrap();
        assert_eq!(published.record().root().generation(), 500);
        assert_eq!(published.record().root().capsule_frontier().len(), 6);
        let request_count = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .count();
        assert_eq!(request_count, 1_994);
        assert!((request_count as f64 / 500.0) < 4.0);
    }

    #[tokio::test]
    async fn stale_root_cannot_publish_over_a_winner() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let first_base = open_root(&router).await.unwrap();
        let stale_base = open_root(&router).await.unwrap();
        let first_transaction = transaction(&first_base, None, &"2".repeat(40));
        publish(
            &router,
            first_base,
            &first_transaction,
            &capsule(&first_transaction),
        )
        .await
        .unwrap();
        let stale_transaction = transaction(&stale_base, None, &"3".repeat(40));

        let error = publish(
            &router,
            stale_base,
            &stale_transaction,
            &capsule(&stale_transaction),
        )
        .await
        .expect_err("stale root CAS must fail");

        assert!(matches!(
            error,
            WriteError::RequestMinimalRootChanged { .. }
        ));
        let visible = open_root(&router).await.unwrap();
        assert_eq!(
            visible.record().root().refs().get("refs/heads/main"),
            Some(&"2".repeat(40))
        );
    }

    #[tokio::test]
    async fn lost_root_update_reply_reconciles_as_committed_success() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
        initialize(&seed_router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let fault_store = Store::with_retry(
            Arc::new(LostRootReplyStore {
                inner,
                root_path: seed_router.request_minimal_root_path().to_string(),
                lost: AtomicBool::new(false),
            }),
            crab_storage::RetryPolicy {
                max_attempts: 1,
                base: std::time::Duration::ZERO,
                cap: std::time::Duration::ZERO,
            },
        );
        let router = StoreLayout::new(fault_store.clone(), "repositories/test".to_owned());
        let base = open_root(&router).await.unwrap();
        let transaction = transaction(&base, None, &"2".repeat(40));

        let published = publish(&router, base, &transaction, &capsule(&transaction))
            .await
            .unwrap();

        assert_eq!(published.record().root().generation(), 1);
        assert!(
            published
                .record()
                .root()
                .contains_transaction(&transaction.id().unwrap())
        );
    }

    #[tokio::test]
    async fn ref_mismatch_fails_before_capsule_upload() {
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(Arc::new(InMemory::new())).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let initial = initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let transaction = transaction(&initial, Some(&"9".repeat(40)), &"2".repeat(40));
        let capsule = capsule(&transaction);
        observer.observations.lock().unwrap().clear();

        let error = publish(&router, initial, &transaction, &capsule)
            .await
            .expect_err("expected-old mismatch must fail");

        assert!(matches!(error, WriteError::RefChanged { .. }));
        assert!(observer.observations.lock().unwrap().is_empty());
    }
}
