//! Live atomic-publication proof for the automatic S3 multipart path.
use bytes::Bytes;
use crab_storage::{ImmutableCreateOutcome, StorageProviderKind, build_static_env_store};
use futures_util::StreamExt as _;
use object_store::ObjectStoreExt as _;
use object_store::path::Path;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated real S3 bucket; retains the verified object"]
async fn racing_immutable_writers_publish_one_complete_body() {
    let bucket = std::env::var("CRAB_TEST_BUCKET").unwrap();
    let store = build_static_env_store(&bucket, StorageProviderKind::S3).unwrap();
    let path = Path::from(format!(
        "transport-probe/fixed-racing-writers-{}",
        rand::random::<u64>()
    ));
    let first = Bytes::from(vec![0x31; 32 * 1024 * 1024 + 7]);
    let second = Bytes::from(vec![0x92; first.len()]);
    let started = std::time::Instant::now();
    let (left, right) = tokio::join!(
        store.create_or_read_immutable(&path, first.clone(), first.len() as u64),
        store.create_or_read_immutable(&path, second.clone(), second.len() as u64),
    );
    let (winner, occupied, expected) = match (left.unwrap(), right.unwrap()) {
        (
            ImmutableCreateOutcome::Created(winner),
            ImmutableCreateOutcome::Existing { bytes, .. },
        ) => (winner, bytes, first),
        (
            ImmutableCreateOutcome::Existing { bytes, .. },
            ImmutableCreateOutcome::Created(winner),
        ) => (winner, bytes, second),
        other => panic!("expected exactly one successful create: {other:?}"),
    };
    assert_eq!(occupied, expected);
    let object = store.inner().get(&path).await.unwrap();
    assert_eq!(object.meta.e_tag, winner.e_tag);
    let mut stream = object.into_stream();
    let mut hash = blake3::Hasher::new();
    let mut length = 0;
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.unwrap();
        hash.update(&bytes);
        length += bytes.len();
    }
    assert_eq!(
        (length, hash.finalize()),
        (expected.len(), blake3::hash(&expected))
    );
    let conflict = store
        .inner()
        .put_opts(&path, expected.into(), object_store::PutMode::Create.into())
        .await;
    assert!(matches!(
        conflict,
        Err(object_store::Error::AlreadyExists { .. })
    ));
    println!(
        "{}",
        serde_json::json!({"key":path.as_ref(), "bytes":length,
        "elapsed_s":started.elapsed().as_secs_f64(), "successful_writers":1,
        "conflicting_writers":1, "winner_bytes_verified":true,
        "direct_create_error_contract_verified":true})
    );
}
