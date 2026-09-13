use std::{sync::Arc, time::Duration};

use crab_coordination::{CoordinationError, FixedSlotAdmissionTicket};
use crab_storage::Store;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const RESOURCE_PREFIX: &str = "http-transfer-admission";
const PROBE_RESOURCE_PREFIX: &str = "http-transfer-admission-probe";
const PROBE_CAPACITY: usize = 16;
const LEASE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("deployment-wide transfer capacity is busy")]
    Busy,
    #[error("transfer admission was cancelled")]
    Cancelled,
    #[error("deployment-wide transfer admission failed")]
    Coordination(#[from] CoordinationError),
}

pub(crate) struct TransferAdmission {
    store: Store,
    prefix: String,
    capacity: usize,
    lease_ttl: Duration,
    pub(crate) local: Arc<Semaphore>,
    workers: TaskTracker,
}

impl TransferAdmission {
    pub(crate) fn new(store: Store, prefix: String, capacity: usize) -> Self {
        Self::with_ttl(store, prefix, capacity, LEASE_TTL)
    }

    fn with_ttl(store: Store, prefix: String, capacity: usize, lease_ttl: Duration) -> Self {
        Self {
            store,
            prefix,
            capacity,
            lease_ttl,
            local: Arc::new(Semaphore::new(capacity)),
            workers: TaskTracker::new(),
        }
    }

    pub(crate) async fn probe(&self) -> Result<(), CoordinationError> {
        let mut ticket = FixedSlotAdmissionTicket::new(
            self.store.inner(),
            &self.prefix,
            PROBE_RESOURCE_PREFIX,
            PROBE_CAPACITY,
            self.lease_ttl,
        )?;
        for _ in 0..PROBE_CAPACITY {
            if ticket.try_admit().await? {
                return ticket.release().await;
            }
        }
        Err(CoordinationError::Configuration {
            key: self.prefix.clone(),
            origin: "all startup transfer-admission probes are busy".to_owned(),
        })
    }

    pub(crate) async fn try_acquire(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<TransferPermit, Error> {
        let local = Arc::clone(&self.local)
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let mut ticket = FixedSlotAdmissionTicket::new(
            self.store.inner(),
            &self.prefix,
            RESOURCE_PREFIX,
            self.capacity,
            self.lease_ttl,
        )?;
        for _ in 0..self.capacity {
            let admitted = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                result = ticket.try_admit() => result?,
            };
            if admitted {
                return Ok(TransferPermit::start(
                    local,
                    ticket,
                    cancellation,
                    &self.workers,
                ));
            }
        }
        Err(Error::Busy)
    }

    pub(crate) fn available_permits(&self) -> usize {
        self.local.available_permits()
    }

    pub(crate) fn close(&self) {
        self.local.close();
        self.workers.close();
    }

    pub(crate) async fn wait(&self) {
        self.workers.wait().await;
    }
}

pub(crate) struct TransferPermit {
    _local: OwnedSemaphorePermit,
    stop: CancellationToken,
    _stop_on_drop: tokio_util::sync::DropGuard,
    _worker: Option<tokio::task::JoinHandle<()>>,
}

impl TransferPermit {
    fn start(
        local: OwnedSemaphorePermit,
        mut ticket: FixedSlotAdmissionTicket,
        cancellation: &CancellationToken,
        workers: &TaskTracker,
    ) -> Self {
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let operation = cancellation.clone();
        let worker = workers.spawn(async move {
            let interval = (ticket.ttl() / 3).max(Duration::from_secs(1));
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                tokio::select! {
                    biased;
                    () = stopped.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(error) = ticket.renew().await {
                            operation.cancel();
                            tracing::warn!(error = %error, "deployment-wide transfer admission lease was lost");
                            break;
                        }
                    }
                }
            }
            if let Err(error) = ticket.release().await {
                tracing::warn!(error = %error, "deployment-wide transfer admission release failed");
            }
        });
        Self {
            _local: local,
            stop: stop.clone(),
            _stop_on_drop: stop.drop_guard(),
            _worker: Some(worker),
        }
    }

    #[cfg(test)]
    pub(crate) async fn release(mut self) {
        self.stop.cancel();
        if let Some(worker) = self._worker.take() {
            let _ = worker.await;
        }
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::BoxStream;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
    };

    #[derive(Debug)]
    struct WriteDeniedStore {
        inner: InMemory,
    }

    impl std::fmt::Display for WriteDeniedStore {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("write-denied-store")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for WriteDeniedStore {
        async fn put_opts(
            &self,
            _location: &Path,
            _payload: PutPayload,
            _options: PutOptions,
        ) -> object_store::Result<PutResult> {
            Err(object_store::Error::Generic {
                store: "write-denied",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "conditional writes are denied",
                )),
            })
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
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

    fn fixture(capacity: usize) -> TransferAdmission {
        TransferAdmission::new(
            Store::new(Arc::new(InMemory::new())),
            "root/.crab/http-server/v1/admission".into(),
            capacity,
        )
    }

    #[tokio::test]
    async fn independent_instances_share_capacity_and_release() {
        let store = Store::new(Arc::new(InMemory::new()));
        let first = TransferAdmission::new(
            store.clone(),
            "root/.crab/http-server/v1/admission".into(),
            2,
        );
        let second = TransferAdmission::new(store, "root/.crab/http-server/v1/admission".into(), 2);
        let cancellation = CancellationToken::new();
        let first_permit = first.try_acquire(&cancellation).await.unwrap();
        let second_permit = second.try_acquire(&cancellation).await.unwrap();

        assert!(matches!(
            first.try_acquire(&cancellation).await,
            Err(Error::Busy)
        ));

        first_permit.release().await;
        let replacement = first.try_acquire(&cancellation).await.unwrap();
        replacement.release().await;
        second_permit.release().await;
        first.close();
        second.close();
        first.wait().await;
        second.wait().await;
    }

    #[tokio::test]
    async fn startup_probe_is_separate_from_live_capacity_and_releases_its_slot() {
        let admission = fixture(1);
        let cancellation = CancellationToken::new();
        let permit = admission.try_acquire(&cancellation).await.unwrap();

        admission.probe().await.unwrap();

        permit.release().await;
        admission.close();
        admission.wait().await;
    }

    #[tokio::test]
    async fn startup_probe_rejects_read_only_storage() {
        let admission = TransferAdmission::new(
            Store::new(Arc::new(WriteDeniedStore {
                inner: InMemory::new(),
            })),
            "root/.crab/http-server/v1/admission".into(),
            1,
        );

        let error = admission.probe().await.unwrap_err();

        assert!(matches!(error, CoordinationError::ObjectStore { .. }));
    }

    #[tokio::test]
    async fn cancellation_does_not_claim_a_slot() {
        let admission = fixture(1);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            admission.try_acquire(&cancellation).await,
            Err(Error::Cancelled)
        ));
        assert_eq!(admission.available_permits(), 1);
        admission.close();
        admission.wait().await;
    }

    #[tokio::test]
    async fn lost_lease_cancels_the_owned_transfer() {
        let store = Store::new(Arc::new(InMemory::new()));
        let admission = TransferAdmission::with_ttl(
            store.clone(),
            "root/.crab/http-server/v1/admission".into(),
            1,
            Duration::from_secs(3),
        );
        let cancellation = CancellationToken::new();
        let permit = admission.try_acquire(&cancellation).await.unwrap();
        let path = object_store::path::Path::from(
            "root/.crab/http-server/v1/admission/locks/internal/http-transfer-admission-0/lock",
        );

        store
            .inner()
            .put_opts(
                &path,
                bytes::Bytes::from_static(b"replaced").into(),
                object_store::PutOptions::from(object_store::PutMode::Overwrite),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), cancellation.cancelled())
            .await
            .unwrap();
        permit.release().await;
        admission.close();
        admission.wait().await;
    }
}
