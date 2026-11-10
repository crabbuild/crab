use super::*;
use futures_util::StreamExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn probes_fail_once_while_data_stores_retry_transient_responses() {
    for provider in [StorageProviderKind::S3, StorageProviderKind::Azure] {
        for probe in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = requests.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    while !request.ends_with(b"\r\n\r\n") {
                        let count = stream.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let first = observed.fetch_add(1, Ordering::SeqCst) == 0;
                    let (status, body) = if first {
                        ("503 Service Unavailable", "")
                    } else if provider == StorageProviderKind::S3 {
                        (
                            "200 OK",
                            "<ListBucketResult><Name>bucket</Name><IsTruncated>false</IsTruncated></ListBucketResult>",
                        )
                    } else {
                        (
                            "200 OK",
                            "<EnumerationResults><Blobs/><NextMarker/></EnumerationResults>",
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });
            let credentials = if provider == StorageProviderKind::S3 {
                ObjectStoreCredentials::Aws {
                    access_key_id: "fixture".into(),
                    secret_access_key: "fixture".into(),
                    session_token: None,
                    region: "us-east-1".into(),
                }
            } else {
                ObjectStoreCredentials::Azure {
                    account: "account".into(),
                    token: AzureAuthorization::Bearer("fixture".into()),
                }
            };
            let factory = if probe {
                build_probe_store
            } else {
                build_explicit_store
            };
            let store = factory("bucket", credentials, Some(&endpoint), true).unwrap();
            let mut objects = store.list_stream(&Path::from(""));
            let result = tokio::time::timeout(Duration::from_secs(3), objects.next()).await;
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            let listed =
                result.unwrap_or_else(|_| panic!("{provider:?} probe={probe} exceeded deadline"));
            assert_eq!(
                listed.is_some_and(|item| item.is_err()),
                probe,
                "{provider:?} probe={probe}"
            );
            assert_eq!(
                requests.load(Ordering::SeqCst),
                if probe { 1 } else { 2 },
                "{provider:?} probe={probe}"
            );
        }
    }
}
