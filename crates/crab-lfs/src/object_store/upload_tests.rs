use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct PartCounts {
    retained: AtomicUsize,
    maximum: AtomicUsize,
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
}

#[async_trait::async_trait]
impl MultipartUpload for ObservedUpload {
    fn put_part(&mut self, data: PutPayload) -> object_store::UploadPart {
        let retained = self.counts.retained.fetch_add(1, Ordering::SeqCst) + 1;
        self.counts.maximum.fetch_max(retained, Ordering::SeqCst);
        let owner = RetainedPart(Arc::clone(&self.counts));
        let part = self.inner.put_part(data);
        Box::pin(async move {
            let _owner = owner;
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
        STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS,
        STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS + 1,
        STREAM_PART_SIZE * (MAX_IN_FLIGHT_PARTS + 1) + 1,
    ] {
        let (file, oid) = super::tests::temp_file_of_size(size, 0x42);
        let store = InMemory::new();
        let path = Path::from("upload");
        let counts = Arc::new(PartCounts::default());
        let mut upload = ObservedUpload {
            inner: store.put_multipart(&path).await.unwrap(),
            counts: Arc::clone(&counts),
        };
        let mut reader = tokio::fs::File::open(file.path()).await.unwrap();
        stream_file_parts(
            &mut reader,
            &mut upload,
            &oid,
            Some(size as u64),
            file.path(),
            &path,
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
