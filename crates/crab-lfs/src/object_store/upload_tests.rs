use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct PartCounts {
    retained: AtomicUsize,
    maximum: AtomicUsize,
    submitted: AtomicUsize,
}

struct RetainedPart(Arc<PartCounts>);

impl Drop for RetainedPart {
    fn drop(&mut self) {
        self.0.retained.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct ObservedUpload {
    inner: Box<dyn MultipartUpload>,
    counts: Arc<PartCounts>,
    fail_next: bool,
}

#[async_trait::async_trait]
impl MultipartUpload for ObservedUpload {
    fn put_part(&mut self, data: PutPayload) -> object_store::UploadPart {
        self.counts.submitted.fetch_add(1, Ordering::SeqCst);
        let fail = std::mem::take(&mut self.fail_next);
        let retained = self.counts.retained.fetch_add(1, Ordering::SeqCst) + 1;
        self.counts.maximum.fetch_max(retained, Ordering::SeqCst);
        let owner = RetainedPart(Arc::clone(&self.counts));
        let part = self.inner.put_part(data);
        Box::pin(async move {
            let _owner = owner;
            if fail {
                return Err(object_store::Error::Generic {
                    store: "part failure fixture",
                    source: Box::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
                });
            }
            part.await
        })
    }

    async fn complete(&mut self) -> object_store::Result<object_store::PutResult> {
        self.inner.complete().await
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.inner.abort().await
    }
}

#[tokio::test]
#[expect(clippy::unwrap_used, reason = "test fixture and assertions")]
async fn full_and_final_parts_share_the_admission_bound() {
    use object_store::memory::InMemory;

    for size in [
        MIN_STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS,
        MIN_STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS + 1,
        MIN_STREAM_PART_SIZE * (MAX_IN_FLIGHT_PARTS + 1) + 1,
    ] {
        let (file, oid) = super::tests::temp_file_of_size(size, 0x42);
        let store = InMemory::new();
        let path = Path::from("upload");
        let counts = Arc::new(PartCounts::default());
        let mut upload = ObservedUpload {
            inner: store.put_multipart(&path).await.unwrap(),
            counts: Arc::clone(&counts),
            fail_next: false,
        };
        let mut reader = tokio::fs::File::open(file.path()).await.unwrap();
        stream_file_parts(
            &mut reader,
            &mut upload,
            &oid,
            Some(size as u64),
            file.path(),
            &path,
            upload_plan(
                size as u64,
                crab_storage::multipart::upload_limits(crab_storage::StorageProviderKind::Local),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        upload.complete().await.unwrap();
        let bytes = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert_eq!(bytes.len(), size);
        assert_eq!(Sha256::digest(&bytes).as_slice(), &oid);
        assert_eq!(counts.retained.load(Ordering::SeqCst), 0);
        assert!(
            counts.maximum.load(Ordering::SeqCst) <= MAX_IN_FLIGHT_PARTS,
            "size {size} retained {} parts",
            counts.maximum.load(Ordering::SeqCst),
        );
    }
}

#[tokio::test]
#[expect(clippy::unwrap_used, reason = "test fixture and assertions")]
async fn synchronous_plan_never_retains_two_payloads() {
    use object_store::memory::InMemory;

    let size = 3 * 1024 + 1;
    let (file, oid) = super::tests::temp_file_of_size(size, 0x24);
    let store = InMemory::new();
    let path = Path::from("synchronous-upload");
    let counts = Arc::new(PartCounts::default());
    let mut upload = ObservedUpload {
        inner: store.put_multipart(&path).await.unwrap(),
        counts: Arc::clone(&counts),
        fail_next: false,
    };
    let mut reader = tokio::fs::File::open(file.path()).await.unwrap();

    stream_file_parts(
        &mut reader,
        &mut upload,
        &oid,
        Some(size as u64),
        file.path(),
        &path,
        UploadPlan {
            part_size: 1024,
            max_pending_parts: 0,
        },
    )
    .await
    .unwrap();

    assert_eq!(counts.submitted.load(Ordering::SeqCst), 4);
    assert_eq!(counts.maximum.load(Ordering::SeqCst), 1);
    assert_eq!(counts.retained.load(Ordering::SeqCst), 0);
    upload.abort().await.unwrap();
}

#[tokio::test]
#[expect(clippy::unwrap_used, reason = "test fixture and assertions")]
async fn failed_part_stops_tail_admission_and_releases_pending_parts() {
    use object_store::memory::InMemory;
    use std::error::Error;

    let size = MIN_STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS + 1;
    let (file, oid) = super::tests::temp_file_of_size(size, 0x42);
    let store = InMemory::new();
    let path = Path::from("failed-upload");
    let counts = Arc::new(PartCounts::default());
    let mut upload = ObservedUpload {
        inner: store.put_multipart(&path).await.unwrap(),
        counts: Arc::clone(&counts),
        fail_next: true,
    };
    let mut reader = tokio::fs::File::open(file.path()).await.unwrap();
    let error = stream_file_parts(
        &mut reader,
        &mut upload,
        &oid,
        Some(size as u64),
        file.path(),
        &path,
        upload_plan(
            size as u64,
            crab_storage::multipart::upload_limits(crab_storage::StorageProviderKind::Local),
        )
        .unwrap(),
    )
    .await
    .unwrap_err();
    let mut source: &(dyn Error + 'static) = &error;
    while let Some(nested) = source.source() {
        source = nested;
    }
    assert_eq!(
        source.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::ConnectionReset,
    );
    assert_eq!(counts.submitted.load(Ordering::SeqCst), MAX_IN_FLIGHT_PARTS);
    assert_eq!(counts.retained.load(Ordering::SeqCst), 0);
    upload.abort().await.unwrap();
    assert!(matches!(
        store.head(&path).await,
        Err(object_store::Error::NotFound { .. })
    ));
}
