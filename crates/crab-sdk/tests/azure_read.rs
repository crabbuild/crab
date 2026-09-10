#![cfg(feature = "remote")]

#[cfg(feature = "write")]
use crab_sdk::remote::EntryMode;
#[cfg(feature = "write")]
use crab_sdk::remote::write::{CommitIdentity, CommitOptions, FileEdit, MutationOutcome};
use crab_sdk::storage::{AzureOptions, DirectStoreOptions};
use crab_sdk::{Client, ErrorKind, RepositoryLocator};
#[cfg(feature = "write")]
use crab_sdk::{GitPath, Revision};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_azure_requests_ignore_environment_and_report_missing_repository() {
    const CHILD: &str = "CRAB_SDK_AZURE_REQUEST_CHILD";
    let configured = |kind| match kind {
        "bearer" => AzureOptions::bearer("account", "container", "fixture-bearer").unwrap(),
        "sas" => AzureOptions::sas("account", "container", "sig=abc%2Bdef%2Fghi%3D").unwrap(),
        _ => panic!("invalid test credential kind"),
    };
    if let Ok(kind) = std::env::var(CHILD) {
        let endpoint = std::env::var("CRAB_SDK_AZURE_TEST_ENDPOINT").unwrap();
        let client = Client::builder()
            .direct_store(DirectStoreOptions::azure(
                configured(&kind).with_endpoint(&endpoint),
            ))
            .build()
            .unwrap();
        let error = client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("missing").unwrap(),
            ))
            .await
            .err()
            .unwrap();
        client.close().await.unwrap();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        return;
    }
    for kind in ["bearer", "sas"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                let mut requests = Vec::new();
                for missing in [false, true] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                        assert!(request.len() < 16 * 1024);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let (status, body) = if missing {
                        assert!(request.starts_with("GET /container/missing/"));
                        ("404 Not Found", "")
                    } else {
                        assert!(request.starts_with("GET /container?"));
                        assert!(request.lines().next().unwrap().contains("comp=list"));
                        ("200 OK", "<EnumerationResults><Blobs/><NextMarker/></EnumerationResults>")
                    };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    stream.write_all(response.as_bytes()).await.unwrap();
                    requests.push(request);
                }
                requests
            })
            .await
            .unwrap()
        });
        // Poison provider policy in a child so no concurrently running test sees
        // modified process state. Only the explicit endpoint can reach the server.
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "explicit_azure_requests_ignore_environment_and_report_missing_repository",
                ])
                .env(CHILD, kind)
                .env("CRAB_SDK_AZURE_TEST_ENDPOINT", endpoint)
                .env("AZURE_STORAGE_ACCOUNT_NAME", "wrong-account")
                .env("AZURE_STORAGE_CONTAINER_NAME", "wrong-container")
                .env("AZURE_STORAGE_ENDPOINT", "http://127.0.0.1:1")
                .env("AZURE_STORAGE_TOKEN", "wrong-bearer")
                .env("AZURE_STORAGE_SAS_KEY", "sig=wrong")
                .env("AZURE_STORAGE_ACCESS_KEY", "invalid-base64")
                .env("AZURE_STORAGE_USE_EMULATOR", "true")
                .env("AZURITE_BLOB_STORAGE_URL", "http://127.0.0.1:1")
                .env("AWS_ALLOW_HTTP", "false")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let requests = server.await.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for request in requests {
            match kind {
                "bearer" => assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer fixture-bearer\r\n")
                ),
                "sas" => assert!(
                    request
                        .lines()
                        .next()
                        .unwrap()
                        .contains("sig=abc%2Bdef%2Fghi%3D")
                ),
                _ => unreachable!(),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(feature = "write")]
#[ignore = "requires a dedicated Azure container and explicit bearer token"]
async fn live_azure_read_is_byte_exact_and_read_only() {
    use futures_util::TryStreamExt as _;

    let account = std::env::var("CRAB_SDK_TEST_AZURE_ACCOUNT").unwrap();
    let container = std::env::var("CRAB_SDK_TEST_AZURE_CONTAINER").unwrap();
    let token = std::env::var("CRAB_SDK_TEST_AZURE_BEARER_TOKEN").unwrap();
    let prefix = format!(
        "qualification/sdk-azure-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = crab_storage::build_explicit_store(
        &container,
        crab_storage::ObjectStoreCredentials::Azure {
            account: account.clone(),
            token: crab_storage::AzureAuthorization::Bearer(token.clone()),
        },
        None,
        false,
    )
    .unwrap();
    let path = object_store::path::Path::from(prefix.clone());
    let inventory = || async {
        store
            .inner()
            .list(Some(&path))
            .map_ok(|object| (object.location, object.size, object.e_tag))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
    };
    let locator = RepositoryLocator::new(&prefix).unwrap();
    let seed = Client::builder()
        .direct_store(DirectStoreOptions::azure(
            AzureOptions::bearer(&account, &container, &token).unwrap(),
        ))
        .build()
        .unwrap();
    seed.initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let bytes = b"exact Azure SDK bytes\0\xff\n".to_vec();
    let identity =
        CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let prepared = seed
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .prepare_commit(
            CommitOptions::initial(
                "refs/heads/main",
                identity.clone(),
                identity,
                b"Azure qualification\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new("exact.bin").unwrap(),
                    EntryMode::Regular,
                    bytes.len() as u64,
                    std::io::Cursor::new(bytes.clone()),
                )
                .unwrap(),
            ],
            scratch.path().to_owned(),
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.execute().await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    seed.close().await.unwrap();
    let before = inventory().await;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::azure(
            AzureOptions::bearer(&account, &container, &token).unwrap(),
        ))
        .build()
        .unwrap();
    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator))
        .await
        .unwrap();
    let read = repository
        .remote()
        .unwrap()
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("exact.bin").unwrap())
        .await
        .unwrap();
    client.close().await.unwrap();
    let after = inventory().await;
    assert_eq!(read.as_ref(), bytes);
    assert_eq!(before, after);
    for (location, _, _) in before {
        store.delete(&location).await.unwrap();
    }
}
