use std::error::Error;
use std::sync::Arc;

use super::tests::reconstruction_fixture;
use crate::{ReadError, XorbAvailability};

#[derive(Debug, thiserror::Error)]
#[error("read denied for operation {0}")]
struct Denied(usize);

struct Gate {
    barrier: Arc<tokio::sync::Barrier>,
    denied: Option<usize>,
    cancelled: bool,
}

#[async_trait::async_trait]
impl XorbAvailability for Gate {
    async fn ensure_available(&self, _: &object_store::path::Path) -> crate::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.barrier.wait())
            .await
            .expect("all concurrent operations reach the source");
        if self.cancelled {
            return Err(ReadError::Cancelled);
        }
        if let Some(id) = self.denied {
            return Err(ReadError::availability(Denied(id)));
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reconstructions_keep_success_cancellation_and_failure_isolated() {
    let directory = tempfile::tempdir().unwrap();
    let (mut hydrator, pointer, expected) =
        reconstruction_fixture(&directory.path().join("cache"), false).await;
    // Every operation must exercise its source; a warm decoded hit bypasses it.
    hydrator.chunk_cache = None;
    // The fixture defaults to two permits; this rendezvous needs three readers.
    hydrator.concurrency = super::fixed_hydrate_concurrency(3).unwrap();
    for id in 0..64 {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let denied = hydrator.clone().with_availability(Arc::new(Gate {
            barrier: Arc::clone(&barrier),
            denied: Some(id),
            cancelled: false,
        }));
        let cancelled = hydrator.clone().with_availability(Arc::new(Gate {
            barrier: Arc::clone(&barrier),
            denied: None,
            cancelled: true,
        }));
        let healthy = hydrator.clone().with_availability(Arc::new(Gate {
            barrier,
            denied: None,
            cancelled: false,
        }));
        let bytes = pointer.serialize();
        let (denied, cancelled, healthy) = tokio::join!(
            denied.reconstruct_from_pointer(&bytes),
            cancelled.reconstruct_range_from_pointer(&bytes, 0, 1024),
            healthy.reconstruct_from_pointer(&bytes),
        );
        let error = denied.unwrap_err();
        let cause = std::iter::successors(error.source(), |error| (*error).source())
            .find_map(|error| error.downcast_ref::<Denied>())
            .unwrap();
        assert_eq!(cause.0, id);
        assert!(matches!(cancelled, Err(ReadError::Cancelled)));
        assert_eq!(healthy.unwrap(), expected);
    }
}
