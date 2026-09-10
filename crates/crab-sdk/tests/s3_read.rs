#![cfg(feature = "remote")]

use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, ErrorKind, RepositoryLocator, Revision};
use futures_util::TryStreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit CRAB_SDK_TEST_S3_* bucket, endpoint, access-key and secret-key inputs"]
async fn sdk_explicit_s3_ignores_conflicting_environment() {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let endpoint = std::env::var("CRAB_SDK_TEST_S3_ENDPOINT").unwrap();
    let access = std::env::var("CRAB_SDK_TEST_S3_ACCESS_KEY_ID").unwrap();
    let secret = std::env::var("CRAB_SDK_TEST_S3_SECRET_ACCESS_KEY").unwrap();
    let options = crab_sdk::storage::S3Options::new(&bucket, "us-east-1", &access, &secret)
        .unwrap()
        .with_endpoint(&endpoint);
    if let Ok(prefix) = std::env::var("CRAB_SDK_EXPLICIT_CHILD_PREFIX") {
        let client = Client::builder()
            .direct_store(DirectStoreOptions::s3(options))
            .build()
            .unwrap();
        let repository = client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new(&prefix).unwrap(),
            ))
            .await
            .unwrap();
        let refs = repository.remote().unwrap().refs().await.unwrap();
        client.close().await.unwrap();
        assert_eq!(
            (refs.head(), refs.entries().len()),
            (Some("refs/heads/main"), 0)
        );
        return;
    }
    let store = crab_storage::build_explicit_store(
        &bucket,
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: access,
            secret_access_key: secret,
            session_token: None,
            region: "us-east-1".to_owned(),
        },
        Some(&endpoint),
        endpoint.starts_with("http://"),
    )
    .unwrap();
    let prefix = format!(
        "sdk-explicit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let layout = crab_storage::StoreLayout::new(store.clone(), prefix.clone());
    crab_metadata::manifest_store::create_manifest(
        &store,
        &layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    let path = object_store::path::Path::from(prefix.clone());
    let before = store
        .inner()
        .list(Some(&path))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    // Poison only a child environment: concurrent tests retain their provider
    // settings, and an accidental environment-chain selection cannot reach S3.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "sdk_explicit_s3_ignores_conflicting_environment",
        ])
        .env("CRAB_SDK_EXPLICIT_CHILD_PREFIX", prefix)
        .env("AWS_ENDPOINT_URL_S3", "http://127.0.0.1:1")
        .env("AWS_ENDPOINT_URL", "http://127.0.0.1:1")
        .env("AWS_REGION", "wrong-region")
        .env("AWS_ACCESS_KEY_ID", "wrong-access")
        .env("AWS_SECRET_ACCESS_KEY", "wrong-secret")
        .env("AWS_SESSION_TOKEN", "wrong-token")
        .env("AWS_ALLOW_HTTP", "false")
        .env("AWS_VIRTUAL_HOSTED_STYLE_REQUEST", "true")
        .output()
        .unwrap();
    let after = store
        .inner()
        .list(Some(&path))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    for object in &before {
        store.delete(&object.location).await.unwrap();
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(before, after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_SDK_TEST_S3_BUCKET and provider credentials for a dedicated test bucket"]
async fn sdk_s3_read_preserves_empty_manifest() {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let prefix = format!(
        "sdk-read-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    );
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let layout = crab_storage::StoreLayout::new(store.clone(), prefix.clone());
    let manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
    crab_metadata::manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    let path = object_store::path::Path::from(prefix.clone());
    let inventory = || async {
        let mut objects = store
            .inner()
            .list(Some(&path))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        objects.sort_by(|a, b| a.location.cmp(&b.location));
        objects
            .into_iter()
            .map(|object| (object.location, object.size, object.e_tag))
            .collect::<Vec<_>>()
    };
    let before = inventory().await;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(&bucket).unwrap())
        .build()
        .unwrap();
    let repository = client
        .open(crab_sdk::OpenOptions::remote(
            RepositoryLocator::new(&prefix).unwrap(),
        ))
        .await
        .unwrap();
    let refs = repository.remote().unwrap().refs().await.unwrap();
    let missing = repository
        .remote()
        .unwrap()
        .snapshot(Revision::branch("main").unwrap())
        .await
        .err()
        .unwrap();
    client.close().await.unwrap();
    let after = inventory().await;
    // Delete only objects created in this test's unique repository prefix.
    // The bucket may contain other qualification runs and is never cleared.
    for (path, _, _) in &before {
        store.delete(path).await.unwrap();
    }
    assert_eq!(
        (refs.head(), refs.entries().len(), missing.kind()),
        (Some("refs/heads/main"), 0, ErrorKind::NotFound)
    );
    assert_eq!(before, after);
}
