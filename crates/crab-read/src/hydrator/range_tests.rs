use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

use super::tests::reconstruction_fixture;
use crate::ReadError;

#[tokio::test]
async fn range_writer_delivers_exact_bytes_without_a_range_sized_buffer() {
    let directory = tempfile::tempdir().unwrap();
    let (hydrator, pointer, original) =
        reconstruction_fixture(&directory.path().join("cache"), false).await;
    for range in [0..1, 1..pointer.size - 1, 0..pointer.size, 7..7] {
        let destination = directory.path().join("output");
        let file = std::fs::File::create(&destination).unwrap();
        let written = hydrator
            .reconstruct_range_to_writer_with_cancel(
                &pointer,
                range.clone(),
                file,
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            (written, std::fs::read(destination).unwrap()),
            (
                range.end - range.start,
                original[range.start as usize..range.end as usize].to_vec()
            )
        );
    }
}

#[tokio::test]
async fn range_writer_rejects_invalid_bounds_and_pre_cancelled_requests() {
    let directory = tempfile::tempdir().unwrap();
    let (hydrator, pointer, _) =
        reconstruction_fixture(&directory.path().join("cache"), false).await;
    for range in [
        std::ops::Range { start: 8, end: 7 },
        0..pointer.size + 1,
        pointer.size + 1..pointer.size + 1,
    ] {
        let result = hydrator
            .reconstruct_range_to_writer_with_cancel(
                &pointer,
                range,
                std::io::sink(),
                None,
                &CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(ReadError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput)
        );
    }
    let cancel = CancellationToken::new();
    cancel.cancel();
    for range in [0..1, 0..0] {
        assert!(matches!(
            hydrator
                .reconstruct_range_to_writer_with_cancel(
                    &pointer,
                    range,
                    std::io::sink(),
                    None,
                    &cancel,
                )
                .await,
            Err(ReadError::Cancelled)
        ));
    }
}

#[tokio::test]
async fn range_writer_rejects_corrupt_origin() {
    let directory = tempfile::tempdir().unwrap();
    let (hydrator, pointer, _) =
        reconstruction_fixture(&directory.path().join("cache"), true).await;
    assert!(
        hydrator
            .reconstruct_range_to_writer_with_cancel(
                &pointer,
                0..1,
                std::io::sink(),
                None,
                &CancellationToken::new(),
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_drop_release_destination_during_pending_source() {
    struct PendingSource(tokio::sync::Notify);

    #[async_trait::async_trait]
    impl crate::XorbAvailability for PendingSource {
        async fn ensure_available(&self, _: &object_store::path::Path) -> crate::Result<()> {
            self.0.notify_one();
            std::future::pending().await
        }
    }

    struct Destination(Arc<AtomicBool>);

    impl std::io::Write for Destination {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Destination {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    for (range, explicit_cancel) in [
        (None, false),
        (None, true),
        (Some(0..1), false),
        (Some(0..1), true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let source = Arc::new(PendingSource(tokio::sync::Notify::new()));
        let hydrator = hydrator.with_availability(source.clone());
        let closed = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let writer = Destination(closed.clone());
        let mut read = Box::pin(async {
            match range {
                Some(range) => {
                    hydrator
                        .reconstruct_range_to_writer_with_cancel(
                            &pointer, range, writer, None, &cancel,
                        )
                        .await
                }
                None => {
                    hydrator
                        .reconstruct_to_writer_with_cancel(&pointer, writer, None, &cancel)
                        .await
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut read => panic!("pending source completed: {result:?}"),
                () = source.0.notified() => {}
            }
        })
        .await
        .unwrap();
        if explicit_cancel {
            cancel.cancel();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut read)
                .await
                .unwrap();
            assert!(matches!(result, Err(ReadError::Cancelled)));
        }
        drop(read);
        assert!(closed.load(Ordering::SeqCst));
    }
}
