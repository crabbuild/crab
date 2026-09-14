//! Request-minimal publication through one immutable capsule and one mutable root.

use crab_metadata::request_minimal::{
    Capsule, CapsulePointer, CapsuleTransaction, RepositoryRoot, RootRecord, create_root, load_root,
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
    Ok(create_root(router, record).await?)
}

/// Open and verify the single root used for advertisement and publication CAS.
pub async fn open_root(router: &StoreLayout<Store>) -> Result<RootSnapshot> {
    Ok(load_root(router).await?)
}

/// Publish one capsule after readback verification, then atomically advance the root.
///
/// The caller supplies the root snapshot retained from advertisement and must
/// first prove authorization, exact Git graph closure, pack integrity,
/// fast-forward policy, and every external content dependency against that
/// snapshot. A clean attempt performs capsule PUT, capsule GET, and root CAS;
/// together with [`open_root`] the complete push uses four object-store requests.
pub async fn publish(
    router: &StoreLayout<Store>,
    base: RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<RootSnapshot> {
    validate_capsule_binding(&base, transaction, capsule)?;
    let (refs, peeled_refs) = apply_ref_edits(&base, transaction)?;
    verify_capsule_after_upload(router, capsule).await?;

    let pointer = CapsulePointer::new(
        capsule.hash(),
        capsule.bytes().len() as u64,
        capsule.transaction_id(),
        capsule.base_root_digest(),
    )?;
    let next = base
        .record()
        .root()
        .advance(base.record().digest(), refs, peeled_refs, pointer)?;
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

fn validate_capsule_binding(
    base: &RootSnapshot,
    transaction: &CapsuleTransaction,
    capsule: &Capsule,
) -> Result<()> {
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

async fn verify_capsule_after_upload(router: &StoreLayout<Store>, capsule: &Capsule) -> Result<()> {
    let path = router.request_minimal_capsule_path(capsule.hash());
    let created = router
        .store()
        .put_if_absent(&path, capsule.bytes().clone())
        .await?;
    if !created {
        // put_if_absent accepts an existing object only after hashing its full body.
        return Ok(());
    }
    let maximum = u64::try_from(capsule.bytes().len()).unwrap_or(u64::MAX);
    let (stored, _) = router.store().get_with_etag_bounded(&path, maximum).await?;
    if blake3::hash(&stored).to_hex().as_str() != capsule.hash() {
        return Err(WriteError::CorruptObject {
            path: path.to_string(),
            reason: "uploaded capsule failed cryptographic readback".to_owned(),
        });
    }
    Ok(())
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

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::fmt;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use bytes::Bytes;
    use crab_metadata::request_minimal::{CapsuleRefEdit, CapsuleSection, CapsuleSectionKind};
    use crab_storage::{StorageObservation, StorageObserver, StorageOperation, StorageOutcome};
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
            vec![CapsuleSection::new(
                CapsuleSectionKind::GitPack,
                Bytes::from_static(b"PACK request-minimal test"),
            )],
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
