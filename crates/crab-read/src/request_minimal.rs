//! Verified loading of one request-minimal root and its bounded capsule frontier.

use crab_metadata::request_minimal::{Capsule, CapsulePointer, CapsuleRun, RootRecord, load_root};
use crab_storage::{Store, StoreLayout};
use futures_util::future::try_join_all;

use crate::{ReadError, Result};

/// Caller-owned memory admission for one request-minimal repository view.
#[derive(Debug, Clone, Copy)]
pub struct RequestMinimalReadLimits {
    /// Largest individual capsule body accepted by this reader.
    pub max_capsule_bytes: u64,
    /// Largest aggregate capsule frontier accepted by this reader.
    pub max_frontier_bytes: u64,
}

/// One authenticated repository root and every post-checkpoint capsule it names.
#[derive(Debug, Clone)]
pub struct RequestMinimalView {
    root: RootRecord,
    capsules: Vec<Capsule>,
}

impl RequestMinimalView {
    /// Return the authoritative repository generation and ref state.
    #[must_use]
    pub fn root(&self) -> &RootRecord {
        &self.root
    }

    /// Return every verified post-checkpoint capsule in publication order.
    #[must_use]
    pub fn capsules(&self) -> &[Capsule] {
        &self.capsules
    }
}

/// Load a root and its bounded capsule frontier with one request per object.
///
/// Capsule bodies are fetched concurrently, then checked against the exact
/// size, content identity, transaction, and base-root bindings in the root.
pub async fn open_view(
    router: &StoreLayout<Store>,
    limits: RequestMinimalReadLimits,
) -> Result<RequestMinimalView> {
    let snapshot = load_root(router).await?;
    let root = snapshot.record().clone();
    admit_frontier(root.root().capsule_frontier(), limits)?;
    let runs = try_join_all(
        root.root()
            .capsule_frontier()
            .iter()
            .map(|pointer| load_run(router, pointer)),
    )
    .await?;
    let capsules = runs
        .into_iter()
        .flat_map(|run| run.capsules().to_vec())
        .collect();
    Ok(RequestMinimalView { root, capsules })
}

fn admit_frontier(pointers: &[CapsulePointer], limits: RequestMinimalReadLimits) -> Result<()> {
    let mut total = 0u64;
    for pointer in pointers {
        if pointer.size() > limits.max_capsule_bytes {
            return Err(ReadError::RequestMinimalLimit {
                resource: "individual capsule bytes",
                maximum: limits.max_capsule_bytes,
            });
        }
        total = total
            .checked_add(pointer.size())
            .ok_or(ReadError::RequestMinimalLimit {
                resource: "frontier bytes",
                maximum: limits.max_frontier_bytes,
            })?;
        if total > limits.max_frontier_bytes {
            return Err(ReadError::RequestMinimalLimit {
                resource: "frontier bytes",
                maximum: limits.max_frontier_bytes,
            });
        }
    }
    Ok(())
}

async fn load_run(router: &StoreLayout<Store>, pointer: &CapsulePointer) -> Result<CapsuleRun> {
    let path = router.request_minimal_capsule_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let actual_size = u64::try_from(bytes.len())
        .map_err(|_| ReadError::internal("capsule size cannot be represented as u64"))?;
    if actual_size != pointer.size() {
        return Err(corrupt(
            &path,
            format!(
                "capsule size is {actual_size} bytes; root declares {}",
                pointer.size()
            ),
        ));
    }
    let run = CapsuleRun::decode(bytes)?;
    if run.hash() != pointer.hash()
        || run.level() != pointer.level()
        || run.transaction_ids() != pointer.transaction_ids()
        || run.newest_base_root_digest() != pointer.newest_base_root_digest()
    {
        return Err(corrupt(
            &path,
            "capsule run does not match its authenticated root pointer",
        ));
    }
    Ok(run)
}

fn corrupt(path: &object_store::path::Path, reason: impl Into<String>) -> ReadError {
    ReadError::CorruptObject {
        path: path.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use crab_metadata::request_minimal::{
        CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction, RepositoryRoot,
    };
    use crab_storage::{StorageObservation, StorageObserver, StorageOperation, StorageOutcome};
    use object_store::memory::InMemory;

    use super::*;

    const TEST_LIMITS: RequestMinimalReadLimits = RequestMinimalReadLimits {
        max_capsule_bytes: 1024 * 1024,
        max_frontier_bytes: 8 * 1024 * 1024,
    };

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

    async fn seed_one_capsule(inner: Arc<InMemory>, pointer_transaction_id: Option<String>) {
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK request-minimal read test"),
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
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        store
            .put(
                &router.request_minimal_capsule_path(run.hash()),
                run.bytes().clone(),
            )
            .await
            .unwrap();
        let mut transaction_ids = run.transaction_ids();
        if let Some(pointer_transaction_id) = pointer_transaction_id {
            transaction_ids[0] = pointer_transaction_id;
        }
        let pointer_transaction_id = transaction_ids[0].clone();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.level(),
            transaction_ids,
            run.newest_base_root_digest(),
        )
        .unwrap();
        let next = initial
            .root()
            .advance(
                initial.digest(),
                std::collections::BTreeMap::from([("refs/heads/main".to_owned(), "2".repeat(40))]),
                std::collections::BTreeMap::new(),
                vec![pointer],
                &pointer_transaction_id,
            )
            .unwrap();
        let root = RootRecord::encode(next).unwrap();
        store
            .create_strict(&router.request_minimal_root_path(), root.bytes().clone())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn one_capsule_view_uses_exactly_two_gets() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let view = open_view(&router, TEST_LIMITS).await.unwrap();

        assert_eq!(view.root().root().generation(), 1);
        assert_eq!(view.capsules().len(), 1);
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
            vec![StorageOperation::Get, StorageOperation::Get]
        );
    }

    #[tokio::test]
    async fn capsule_must_match_every_root_pointer_identity() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), Some("3".repeat(64))).await;
        let store = Store::new(inner);
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let error = open_view(&router, TEST_LIMITS)
            .await
            .expect_err("mismatched transaction identity must fail");

        assert!(matches!(error, ReadError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn frontier_admission_fails_before_capsule_gets() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let error = open_view(
            &router,
            RequestMinimalReadLimits {
                max_capsule_bytes: 1,
                max_frontier_bytes: 1,
            },
        )
        .await
        .expect_err("oversized frontier must fail admission");

        assert!(matches!(error, ReadError::RequestMinimalLimit { .. }));
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(operations, vec![StorageOperation::Get]);
    }
}
