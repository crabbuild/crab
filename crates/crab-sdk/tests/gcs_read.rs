#![cfg(all(feature = "remote", feature = "write"))]

use crab_sdk::{
    Client, CommitIdentity, CommitOptions, DirectStoreOptions, EntryMode, FileEdit, GcsOptions,
    GitPath, MutationOutcome, OperationOptions, RepositoryLocator, Revision,
};
use futures_util::TryStreamExt as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a dedicated GCS bucket and explicit bearer token"]
async fn live_gcs_read_is_byte_exact_and_read_only() {
    let bucket = std::env::var("CRAB_SDK_TEST_GCS_BUCKET").unwrap();
    let token = std::env::var("CRAB_SDK_TEST_GCS_ACCESS_TOKEN").unwrap();
    let prefix = format!(
        "qualification/sdk-gcs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = crab_storage::build_explicit_store(
        &bucket,
        crab_storage::ObjectStoreCredentials::Gcp {
            access_token: token.clone(),
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
        .direct_store(DirectStoreOptions::gcs(
            GcsOptions::new(&bucket, &token).unwrap(),
        ))
        .build()
        .unwrap();
    seed.initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let bytes = b"exact GCS SDK bytes\0\xff\n".to_vec();
    let identity =
        CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let prepared = seed
        .open_remote(locator.clone())
        .await
        .unwrap()
        .prepare_commit(
            CommitOptions::initial(
                "refs/heads/main",
                identity.clone(),
                identity,
                b"GCS qualification\n".to_vec(),
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
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.execute(OperationOptions::default()).await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    seed.close().await.unwrap();
    let before = inventory().await;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::gcs(
            GcsOptions::new(&bucket, &token).unwrap(),
        ))
        .build()
        .unwrap();
    let repository = client.open_remote(locator).await.unwrap();
    let read = repository
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
