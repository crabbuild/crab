use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_admission_follows_the_winner_of_a_concurrent_owner_claim() {
    use crab_cell_runtime::{
        control::{Owner, Transition},
        fleet::placement::PlacementPlanner,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    for at_claim in [false, true] {
        let store = Arc::new(PausedAuthority::default());
        let fixture = Fixture::with_store(2, store.clone()).await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = Arc::new(AtomicBool::new(true));
        let remote = super::provisioning::Remote::with_router(&fixture, |router| {
            let entered = entered.clone();
            let release = release.clone();
            router.layer(axum::middleware::from_fn(
                move |request, next: axum::middleware::Next| {
                    let entered = entered.clone();
                    let release = release.clone();
                    let first = first.clone();
                    async move {
                        if !at_claim && first.swap(false, Ordering::SeqCst) {
                            entered.notify_one();
                            release.notified().await;
                        }
                        next.run(request).await
                    }
                },
            ))
        })
        .await;
        let cell = fixture.data[0].0.cell_id();
        fixture.data[0].0.drain().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let selected = fixture
                    .directory
                    .choose_advertised_placement(&PlacementPlanner::default(), cell, now_ms(), 4)
                    .await
                    .unwrap();
                if selected.is_some_and(|selected| selected.session == remote.session) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        if at_claim {
            *store.path.lock().unwrap() = Some((
                fixture.layout.control_path(cell.as_bytes()),
                PauseOperation::Write,
            ));
        }
        let sdk = super::provisioning::sdk_without_retries(&fixture);
        let item = fixture.data[0].1.clone();
        let key = item["id"].clone();
        let request = tokio::spawn(async move {
            sdk.get_item()
                .table_name("Residency")
                .key("id", key)
                .consistent_read(true)
                .send()
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            if at_claim {
                store.entered.notified()
            } else {
                entered.notified()
            },
        )
        .await
        .unwrap();
        // Placement selected the peer, but another admission wins the authority CAS
        // before activation or during its conditional claim. Leave the winner recovering.
        let authority = CellAuthority::new(fixture.layout.clone());
        let idle = authority.load(cell).await.unwrap().unwrap();
        let claimed = idle
            .value()
            .takeover(Owner {
                session: fixture.session,
                endpoint: fixture.endpoint.clone(),
            })
            .unwrap();
        authority
            .transition(&idle, claimed, Transition::Takeover)
            .await
            .unwrap();
        if at_claim {
            store.release.notify_one();
        } else {
            release.notify_one();
        }
        let read = request.await.unwrap().unwrap();
        assert_eq!(read.item, Some(item));
        assert_eq!(
            authority
                .load(cell)
                .await
                .unwrap()
                .unwrap()
                .value()
                .owner
                .as_ref()
                .unwrap()
                .session,
            fixture.session
        );
        remote.shutdown().await;
        fixture.shutdown().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PauseOperation {
    Read,
    Write,
}

#[derive(Debug, Default)]
struct PausedAuthority {
    inner: InMemory,
    path: std::sync::Mutex<Option<(object_store::path::Path, PauseOperation)>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl PausedAuthority {
    async fn pause(&self, location: &object_store::path::Path, at: PauseOperation) {
        let paused = {
            let mut path = self.path.lock().unwrap();
            if path
                .as_ref()
                .is_some_and(|(path, operation)| path == location && *operation == at)
            {
                path.take();
                true
            } else {
                false
            }
        };
        if paused {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

impl std::fmt::Display for PausedAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PausedAuthority")
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for PausedAuthority {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.pause(location, PauseOperation::Write).await;
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        // Preserve the body and ETag already read while ownership changes.
        let result = self.inner.get_opts(location, options).await?;
        self.pause(location, PauseOperation::Read).await;
        Ok(result)
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
    {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_read_recovers_when_owner_drains_after_authority_lookup() {
    let store = Arc::new(PausedAuthority::default());
    let fixture = Fixture::with_store(2, store.clone()).await;
    let owner = &fixture.data[0].0;
    let item = fixture.data[0].1.clone();
    *store.path.lock().unwrap() = Some((
        fixture.layout.control_path(owner.cell_id().as_bytes()),
        PauseOperation::Read,
    ));
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    let key = item["id"].clone();
    let request = tokio::spawn(async move {
        sdk.get_item()
            .table_name("Residency")
            .key("id", key)
            .consistent_read(true)
            .send()
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), store.entered.notified())
        .await
        .unwrap();
    owner.drain().await.unwrap();
    store.release.notify_one();
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(read.item, Some(item));
    fixture.shutdown().await;
}
