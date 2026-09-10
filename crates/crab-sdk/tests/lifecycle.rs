#![cfg(feature = "remote")]

pub mod support;

use std::future::IntoFuture;
use std::time::Duration;

use crab_sdk::operation::Options as OperationOptions;
use crab_sdk::storage::{DirectStoreOptions, S3Options};
use crab_sdk::{Client, ErrorKind, RepositoryLocator};
use tokio::io::AsyncReadExt;

async fn pending_store() -> (
    Client,
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (started, receive) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await.unwrap());
            assert!(headers.len() < 16 * 1024);
        }
        started.send(()).unwrap();
        // A request that never receives headers must release its connection on
        // cancellation; merely dropping the SDK-facing receiver is insufficient.
        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
    });
    let options = S3Options::new("bucket", "us-east-1", "fixture", "fixture")
        .unwrap()
        .with_endpoint(&endpoint);
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3(options))
        .build()
        .unwrap();
    (client, receive, server)
}

#[tokio::test]
async fn close_drains_dropped_operations() {
    let (client, started, server) = pending_store().await;
    let mut request = Box::pin(
        client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("repository").unwrap(),
            ))
            .into_future(),
    );
    tokio::select! {
        result = &mut request => panic!("request unexpectedly completed: {:?}", result.err()),
        started = tokio::time::timeout(Duration::from_secs(2), started) => started.unwrap().unwrap(),
    }
    drop(request);
    tokio::time::timeout(Duration::from_secs(2), client.close())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn open_timeout_drains_pending_transport() {
    let (client, started, server) = pending_store().await;
    let options = OperationOptions::default()
        .with_timeout(Duration::from_millis(100))
        .unwrap();
    let mut request = Box::pin(
        client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("repository").unwrap(),
            ))
            .with_options(options)
            .into_future(),
    );
    tokio::select! {
        result = &mut request => panic!("request unexpectedly completed: {:?}", result.err()),
        started = tokio::time::timeout(Duration::from_secs(2), started) => started.unwrap().unwrap(),
    }
    let error = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .unwrap()
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::Timeout);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    client.close().await.unwrap();
}

#[tokio::test]
async fn warm_cache_respects_limits() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    assert_eq!(
        fixture
            .snapshot
            .read_blob(fixture.path.clone())
            .await
            .unwrap(),
        fixture.original
    );
    let options = crab_sdk::operation::ReadOptions::default()
        .with_limits(crab_sdk::operation::ReadLimits {
            max_response_bytes: 1,
            ..Default::default()
        })
        .unwrap();
    let error = fixture
        .snapshot
        .read_blob(fixture.path.clone())
        .with_options(options)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::LimitExceeded);
    assert_eq!(
        fixture
            .snapshot
            .read_blob(fixture.path.clone())
            .await
            .unwrap(),
        fixture.original
    );
    fixture.client.close().await.unwrap();
}

#[cfg(feature = "content")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_close_reports_integrity_failure() {
    use std::sync::Arc;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    struct Failure {
        failed: Arc<tokio::sync::Notify>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    struct ContentOperation(bool);
    #[derive(Default)]
    struct Fields {
        content_operation: bool,
        consumer_failure: bool,
    }
    impl Visit for Fields {
        fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "operation" {
                self.content_operation = value == "content";
            }
            if field.name() == "error_category" {
                self.consumer_failure = value == "consumer";
            }
        }
    }
    impl<S> Layer<S> for Failure
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
            let mut fields = Fields::default();
            attributes.record(&mut fields);
            if let Some(span) = context.span(id) {
                span.extensions_mut()
                    .insert(ContentOperation(fields.content_operation));
            }
        }

        fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, context: Context<'_, S>) {
            let Some(span) = context.span(id) else {
                return;
            };
            if span.metadata().target() != "crab_remote_git::telemetry"
                || !span
                    .extensions()
                    .get::<ContentOperation>()
                    .is_some_and(|operation| operation.0)
            {
                return;
            }
            let mut fields = Fields::default();
            values.record(&mut fields);
            if fields.consumer_failure {
                self.failed.notify_one();
                let _ = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2));
            }
        }
    }

    let fixture = support::read_fixture::ReadFixture::new().await;
    let pointer = crab_git::LfsPointer::parse(&fixture.pointer).unwrap();
    let path = crab_lfs::LfsObjectStore::object_path_for_prefix("repository", &pointer.oid);
    let mut corrupt = fixture.updated.to_vec();
    corrupt[0] ^= 1;
    std::fs::write(
        fixture.directory.path().join("storage").join(path.as_ref()),
        corrupt,
    )
    .unwrap();
    let failed = Arc::new(tokio::sync::Notify::new());
    let (release, released) = std::sync::mpsc::channel();
    let subscriber = tracing_subscriber::registry().with(Failure {
        failed: failed.clone(),
        release: std::sync::Mutex::new(released),
    });
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let mut stream = fixture
        .snapshot
        .open_file(crab_sdk::GitPath::new(b"z-lfs".to_vec()).unwrap())
        .await
        .unwrap();
    let mut delivered = 0;
    // Owner completion is recorded after the failure and locator close are
    // retained. The observer holds task completion until this consumer stops
    // polling, leaving the terminal error unconsumed for explicit close.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                biased;
                () = failed.notified() => break,
                result = stream.next() => {
                    delivered += result.expect("terminal failure must remain unconsumed")
                        .expect("corrupt content cannot reach EOF").len();
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(delivered < fixture.updated.len());
    release.send(()).unwrap();
    let error = stream.close().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Corruption);
    fixture.client.close().await.unwrap();
}
