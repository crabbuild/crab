use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::StreamExt as _;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStoreExt as _, PutMode, PutOptions};

use super::*;
use crate::Store;

#[derive(Default)]
struct RecordingObserver {
    active: Mutex<[u64; StorageOperation::ALL.len()]>,
    finished: Mutex<Vec<StorageObservation>>,
}

impl RecordingObserver {
    fn active(&self, operation: StorageOperation) -> u64 {
        self.active.lock().expect("active lock")[operation.index()]
    }

    fn observations(&self) -> Vec<StorageObservation> {
        self.finished.lock().expect("finished lock").clone()
    }
}

impl StorageObserver for RecordingObserver {
    fn started(&self, operation: StorageOperation) {
        self.active.lock().expect("active lock")[operation.index()] += 1;
    }

    fn finished(&self, observation: StorageObservation) {
        self.active.lock().expect("active lock")[observation.operation.index()] -= 1;
        self.finished
            .lock()
            .expect("finished lock")
            .push(observation);
    }
}

fn observed_store(observer: &Arc<RecordingObserver>) -> Store {
    Store::new(Arc::new(InMemory::new()))
        .with_storage_observer(Arc::clone(observer) as Arc<dyn StorageObserver>)
}

struct TestMultipartStore;

#[async_trait::async_trait]
impl MultipartStore for TestMultipartStore {
    async fn create_multipart(&self, _path: &Path) -> object_store::Result<MultipartId> {
        Ok(MultipartId::from("upload"))
    }

    async fn put_part(
        &self,
        _path: &Path,
        _id: &MultipartId,
        _part_idx: usize,
        _data: PutPayload,
    ) -> object_store::Result<PartId> {
        Ok(PartId {
            content_id: "part".into(),
        })
    }

    async fn complete_multipart(
        &self,
        _path: &Path,
        _id: &MultipartId,
        _parts: Vec<PartId>,
    ) -> object_store::Result<PutResult> {
        Ok(PutResult {
            e_tag: None,
            version: None,
            extensions: Default::default(),
        })
    }

    async fn abort_multipart(&self, _path: &Path, _id: &MultipartId) -> object_store::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn body_reads_remain_active_until_eof_and_count_yielded_bytes() {
    let observer = Arc::new(RecordingObserver::default());
    let store = observed_store(&observer);
    let path = Path::from("repository/object");
    store
        .inner()
        .put(&path, Bytes::from_static(b"payload").into())
        .await
        .expect("put");

    let result = store.inner().get(&path).await.expect("get headers");
    assert_eq!(observer.active(StorageOperation::Get), 1);
    assert_eq!(result.bytes().await.expect("get body"), b"payload"[..]);
    assert_eq!(observer.active(StorageOperation::Get), 0);

    let observations = observer.observations();
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::Put
            && observation.outcome == StorageOutcome::Success
            && observation.bytes_written == 7
    }));
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::Get
            && observation.outcome == StorageOutcome::Success
            && observation.bytes_read == 7
    }));
}

#[tokio::test]
async fn range_and_multipart_operations_have_distinct_bounded_classes() {
    let observer = Arc::new(RecordingObserver::default());
    let store = observed_store(&observer);
    let path = Path::from("repository/large");
    let mut upload = store
        .inner()
        .put_multipart(&path)
        .await
        .expect("start multipart");
    upload
        .put_part(Bytes::from_static(b"multipart").into())
        .await
        .expect("multipart part");
    upload.complete().await.expect("complete multipart");
    assert_eq!(
        store.inner().get_range(&path, 2..6).await.expect("range"),
        b"ltip"[..]
    );
    assert_eq!(
        store
            .inner()
            .get_ranges(&path, &[0..2, 7..9])
            .await
            .expect("multiple ranges"),
        vec![Bytes::from_static(b"mu"), Bytes::from_static(b"rt")]
    );

    let observations = observer.observations();
    for operation in [
        StorageOperation::MultipartStart,
        StorageOperation::MultipartPart,
        StorageOperation::MultipartComplete,
        StorageOperation::Range,
    ] {
        assert!(observations.iter().any(|observation| {
            observation.operation == operation && observation.outcome == StorageOutcome::Success
        }));
    }
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::MultipartPart && observation.bytes_written == 9
    }));
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::Range && observation.bytes_read == 4
    }));
    assert_eq!(
        observations
            .iter()
            .filter(|observation| observation.operation == StorageOperation::Range)
            .count(),
        2
    );
}

#[tokio::test]
async fn explicit_resumable_multipart_transport_is_observed() {
    let observer = Arc::new(RecordingObserver::default());
    let multipart = ObservedMultipartStore::new(
        Arc::new(TestMultipartStore),
        Arc::clone(&observer) as Arc<dyn StorageObserver>,
    );
    let path = Path::from("repository/resumable");
    let id = multipart
        .create_multipart(&path)
        .await
        .expect("create multipart");
    let part = multipart
        .put_part(&path, &id, 0, Bytes::from_static(b"resumable").into())
        .await
        .expect("put part");
    multipart
        .complete_multipart(&path, &id, vec![part])
        .await
        .expect("complete multipart");
    multipart
        .abort_multipart(&path, &id)
        .await
        .expect("abort multipart");

    let observations = observer.observations();
    for operation in [
        StorageOperation::MultipartStart,
        StorageOperation::MultipartPart,
        StorageOperation::MultipartComplete,
        StorageOperation::MultipartAbort,
    ] {
        assert!(observations.iter().any(|observation| {
            observation.operation == operation && observation.outcome == StorageOutcome::Success
        }));
    }
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::MultipartPart && observation.bytes_written == 9
    }));
}

#[tokio::test]
async fn failed_write_does_not_credit_payload_bytes() {
    let observer = Arc::new(RecordingObserver::default());
    let store = observed_store(&observer);
    let path = Path::from("repository/existing");
    store
        .inner()
        .put(&path, Bytes::from_static(b"original").into())
        .await
        .expect("initial put");
    let error = store
        .inner()
        .put_opts(
            &path,
            Bytes::from_static(b"replacement").into(),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
        .expect_err("create must conflict");
    assert!(matches!(error, object_store::Error::AlreadyExists { .. }));

    assert!(observer.observations().iter().any(|observation| {
        observation.operation == StorageOperation::Put
            && observation.outcome == StorageOutcome::Conflict
            && observation.bytes_written == 0
    }));
}

#[tokio::test]
async fn configured_read_routes_and_staging_writes_are_observed() {
    let observer = Arc::new(RecordingObserver::default());
    let route = Arc::new(InMemory::new());
    let staging = Arc::new(InMemory::new());
    let routed_path = Path::from("route/object");
    route
        .put(&routed_path, Bytes::from_static(b"routed").into())
        .await
        .expect("route fixture");
    let store = Store::new(Arc::new(InMemory::new()))
        .with_read_routes(vec![("route".into(), route)])
        .with_staging_write_store("staging".into(), staging.clone())
        .with_storage_observer(Arc::clone(&observer) as Arc<dyn StorageObserver>);

    let (body, _) = store
        .get_with_etag(&routed_path)
        .await
        .expect("routed read");
    assert_eq!(body, b"routed"[..]);
    store
        .put_exact(
            &Path::from("canonical/object"),
            Bytes::from_static(b"staged"),
        )
        .await
        .expect("staged write");
    staging
        .head(&Path::from("staging/objects/canonical/object"))
        .await
        .expect("staged object");

    let observations = observer.observations();
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::Get
            && observation.outcome == StorageOutcome::Success
            && observation.bytes_read == 6
    }));
    assert!(observations.iter().any(|observation| {
        observation.operation == StorageOperation::Put
            && observation.outcome == StorageOutcome::Success
            && observation.bytes_written == 6
    }));
}

#[tokio::test]
async fn dropped_streams_release_active_operations_as_cancelled() {
    let observer = Arc::new(RecordingObserver::default());
    let store = observed_store(&observer);
    let listing = store.inner().list(None);
    assert_eq!(observer.active(StorageOperation::List), 1);

    drop(listing);

    assert_eq!(observer.active(StorageOperation::List), 0);
    assert!(observer.observations().iter().any(|observation| {
        observation.operation == StorageOperation::List
            && observation.outcome == StorageOutcome::Cancelled
    }));
}

#[tokio::test]
async fn dropped_body_reports_bytes_already_delivered() {
    let observer = Arc::new(RecordingObserver::default());
    let store = observed_store(&observer);
    let path = Path::from("repository/partial");
    store
        .inner()
        .put(&path, Bytes::from_static(b"payload").into())
        .await
        .expect("put");
    let mut body = store
        .inner()
        .get(&path)
        .await
        .expect("get headers")
        .into_stream();
    assert_eq!(
        body.next().await.expect("body item").expect("body"),
        b"payload"[..]
    );

    drop(body);

    assert!(observer.observations().iter().any(|observation| {
        observation.operation == StorageOperation::Get
            && observation.outcome == StorageOutcome::Cancelled
            && observation.bytes_read == 7
    }));
}

#[tokio::test]
async fn missing_objects_are_classified_without_exposing_their_path() {
    let observer = Arc::new(RecordingObserver::default());
    let store = observed_store(&observer);
    let error = store
        .inner()
        .get(&Path::from("secret/repository/path"))
        .await
        .expect_err("missing get");
    assert!(matches!(error, object_store::Error::NotFound { .. }));

    let observations = observer.observations();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].operation, StorageOperation::Get);
    assert_eq!(observations[0].outcome, StorageOutcome::NotFound);
}

#[test]
fn provider_failures_use_bounded_outcomes() {
    let throttled = object_store::Error::Generic {
        store: "test",
        source: "503 SlowDown".into(),
    };
    let forbidden = object_store::Error::PermissionDenied {
        path: "private/path".into(),
        source: "denied".into(),
    };
    let cancelled = object_store::Error::NotSupported {
        source: Box::new(crate::StorageError::ReadRejected {
            source: Box::new(crate::StorageError::Cancelled),
        }),
    };
    assert_eq!(classify_error(&throttled), StorageOutcome::Throttled);
    assert_eq!(classify_error(&forbidden), StorageOutcome::Auth);
    assert_eq!(classify_error(&cancelled), StorageOutcome::Cancelled);
}
