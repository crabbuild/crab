use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use object_store::{ObjectStoreExt, memory::InMemory};
use sha2::{Digest as _, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

use super::*;
use crate::{BucketIdentity, StorageProviderKind, Store, layout::StoreLayout};

#[test]
fn exact_immutable_read_families() {
    let layout = StoreLayout::with_global_prefix((), "org/repo".into(), "private/content".into());
    let hash = "abcd".repeat(16);
    for (path, family) in [
        (layout.capsule_path(&hash), CdnReadFamily::Capsule),
        (
            layout.capsule_checkpoint_path(&hash),
            CdnReadFamily::Capsule,
        ),
        (layout.capsule_catalog_path(&hash), CdnReadFamily::Capsule),
        (layout.capsule_pack_layer_path(&hash), CdnReadFamily::Git),
        (
            layout.capsule_history_segment_path(&hash),
            CdnReadFamily::Capsule,
        ),
        (
            layout.capsule_source_segment_path(&hash),
            CdnReadFamily::Capsule,
        ),
        (
            layout.repo_path(&format!("lfs/objects/ab/cd/{hash}")),
            CdnReadFamily::Lfs,
        ),
        (layout.global_path("xorbs", &hash), CdnReadFamily::Xorb),
        (layout.global_path("shards", &hash), CdnReadFamily::Shard),
        (
            layout.global_path("capsule-chunk-pages", &hash),
            CdnReadFamily::ChunkPage,
        ),
    ] {
        assert_eq!(layout.cdn_read_family(&path), Some(family), "{path}");
    }
    for path in [
        layout.capsule_root_path(),
        layout.capsule_ref_head_path(&hash),
        layout.capsule_source_head_path(&hash),
        layout.capsule_source_floor_path(),
        layout.read_retention_object_path("owner", &hash),
        layout.browse_retained_root_path(&hash),
        layout.repo_path(&format!("v2/capsules/xy/ab/{hash}")),
        layout.repo_path(&format!("v2/capsules/ff/{hash}")),
        layout.repo_path(&format!("v2/capsules/ab/{hash}/extra")),
        layout.repo_path(&format!("v2/capsules/AB/{}", hash.to_ascii_uppercase())),
        layout.repo_path(&format!("lfs/objects/ab/ab/{hash}")),
        Path::from(format!("org/repository/v2/capsules/ab/{hash}")),
        Path::from(format!(".crab/xorbs/ab/{hash}")),
    ] {
        assert_eq!(layout.cdn_read_family(&path), None, "{path}");
    }
}

#[test]
fn validates_base_and_preserves_literal_storage_key() {
    for bad in [
        "http://downloads.example.com",
        "https://user:credential@downloads.example.com",
        "https://downloads.example.com/objects?secret=value",
        "https://downloads.example.com/#fragment",
        "https://downloads.example.com/old/../new",
        "https://downloads.example.com/old/%2e%2e/new",
    ] {
        assert!(CdnTransport::new(bad).is_err());
    }
    let cdn = CdnTransport::new("https://downloads.example.com/container/").unwrap();
    let path = Path::parse("org/name with space/é%20/key").unwrap();
    assert_eq!(
        cdn.url_for(&path).unwrap().as_str(),
        "https://downloads.example.com/container/org/name%20with%20space/%C3%A9%2520/key"
    );
}

#[tokio::test]
async fn provider_https_route_requires_an_exact_policy_context() {
    let target = [8; 32];
    let store = Store::new(Arc::new(InMemory::new()))
        .with_target_identity(target)
        .with_bucket_identity(BucketIdentity::new(
            StorageProviderKind::S3,
            "origin",
            "origin",
        ))
        .with_native_cdn_origin(Some("s3://origin".into()));
    let directory = tempfile::tempdir().unwrap();
    let relative = format!("v2/pack-layers/aa/{}", "a".repeat(64));
    let file = write_publication_fixture(
        directory.path().join("receipt.json"),
        "s3://origin",
        "s3://export",
        "org/repo",
        "https://downloads.example.com",
        &relative,
        6,
    )
    .unwrap();
    let route = || {
        CdnRoute::new(
            target,
            "org/repo".into(),
            None,
            CdnAccess::Public,
            CdnTransport::new("https://downloads.example.com").unwrap(),
            Duration::from_secs(1),
        )
    };
    let key = Path::from(format!("org/repo/{relative}"));
    let anonymous = store
        .clone()
        .with_cdn_route(
            route()
                .with_publication(&store, &file, "s3://export")
                .await
                .unwrap(),
        )
        .unwrap();
    assert!(anonymous.cdn_attempt(&key).is_none());
    let unapproved = store
        .clone()
        .with_cdn_route(route().for_consumer(CdnConsumer::Cli, CdnProfile::Direct))
        .unwrap();
    assert!(unapproved.cdn_attempt(&key).is_none());

    let approved = include_str!("qualified-scopes.txt")
        .lines()
        .any(|row| row == "cloudfront/public/cli/git/direct");
    let authorized_route = route()
        .for_consumer(CdnConsumer::Cli, CdnProfile::Direct)
        .with_publication(&store, &file, "s3://export")
        .await
        .unwrap();
    if qualification_enabled() || approved {
        let changed_authority = store
            .clone()
            .with_native_cdn_origin(Some("s3://origin".into()));
        let reusing_route = route()
            .for_consumer(CdnConsumer::Cli, CdnProfile::Direct)
            .with_publication(&store, &file, "s3://export")
            .await
            .unwrap();
        assert!(matches!(
            changed_authority.with_cdn_route(reusing_route),
            Err(StorageError::Forbidden { .. })
        ));
    }
    let scoped = store.with_cdn_route(authorized_route).unwrap();
    assert_eq!(
        scoped.cdn_attempt(&key).is_some(),
        qualification_enabled() || approved
    );
}

#[tokio::test]
async fn public_export_approval_pins_digest_origin_and_viewer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("receipt.json");
    let base = "https://downloads.example.com";
    let file = write_publication_fixture(
        path.clone(),
        "s3://source",
        "s3://export",
        "org/repo",
        base,
        &format!("v2/pack-layers/aa/{}", "a".repeat(64)),
        6,
    )
    .unwrap();
    let route = |viewer| {
        CdnRoute::new(
            [4; 32],
            "org/repo".into(),
            None,
            CdnAccess::Public,
            CdnTransport::new(viewer).unwrap(),
            Duration::from_secs(5),
        )
    };
    let source = Store::new(Arc::new(InMemory::new()))
        .with_target_identity([4; 32])
        .with_native_cdn_origin(Some("s3://source".into()));
    let wrong_host = route("https://other.example.com")
        .with_publication(&source, &file, "s3://export")
        .await;
    assert!(wrong_host.is_err());
    let wrong_export = route(base)
        .with_publication(&source, &file, "s3://other")
        .await;
    assert!(wrong_export.is_err());
    let other = Store::new(Arc::new(InMemory::new()))
        .with_target_identity([4; 32])
        .with_native_cdn_origin(Some("s3://other".into()));
    let wrong_source = route(base)
        .with_publication(&other, &file, "s3://export")
        .await;
    assert!(matches!(wrong_source, Err(StorageError::Forbidden { .. })));
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    receipt["source_placement"]["resource"] =
        serde_json::json!("accounts/123456789012/buckets/other");
    let bytes = serde_json::to_vec(&receipt).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let repinned = CdnPublicationFile {
        path: path.clone(),
        sha256: {
            use sha2::{Digest as _, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        },
    };
    assert!(
        route(base)
            .with_publication(&source, &repinned, "s3://export")
            .await
            .is_err()
    );
    std::fs::write(path, b"tampered").unwrap();
    let tampered = route(base)
        .with_publication(&source, &file, "s3://export")
        .await;
    assert!(matches!(
        tampered,
        Err(StorageError::InvalidStaticEnvTarget { .. })
    ));
}

#[tokio::test]
async fn public_azure_receipt_requires_a_separate_storage_account() {
    let directory = tempfile::tempdir().unwrap();
    let source = "https://fixtureacct.blob.core.windows.net/source";
    let viewer = "https://downloads.example.com";
    let target = [4; 32];
    let origin = Store::new(Arc::new(InMemory::new()))
        .with_target_identity(target)
        .with_native_cdn_origin(Some(source.into()));
    let relative = format!("v2/pack-layers/aa/{}", "a".repeat(64));
    for (name, export, valid) in [
        (
            "same",
            "https://fixtureacct.blob.core.windows.net/export",
            false,
        ),
        (
            "separate",
            "https://exportacct.blob.core.windows.net/source",
            true,
        ),
        ("different-provider", "s3://public-export", false),
    ] {
        let file = write_publication_fixture(
            directory.path().join(format!("{name}.json")),
            source,
            export,
            "org/repo",
            viewer,
            &relative,
            6,
        )
        .unwrap();
        let result = CdnRoute::new(
            target,
            "org/repo".into(),
            None,
            CdnAccess::Public,
            CdnTransport::new(viewer).unwrap(),
            Duration::from_secs(5),
        )
        .with_publication(&origin, &file, export)
        .await;
        assert_eq!(result.is_ok(), valid, "{name}");
    }
}

#[tokio::test]
async fn public_s3_activation_rejects_the_selected_store_in_another_region() {
    let directory = tempfile::tempdir().unwrap();
    let viewer = "https://downloads.example.com";
    let file = write_publication_fixture(
        directory.path().join("receipt.json"),
        "s3://selected-bucket",
        "s3://public-export",
        "org/repo",
        viewer,
        &format!("v2/pack-layers/aa/{}", "a".repeat(64)),
        6,
    )
    .unwrap();
    let store = crate::provider_store::build_explicit_store(
        "selected-bucket",
        crate::provider_store::ObjectStoreCredentials::Aws {
            access_key_id: "fixture".into(),
            secret_access_key: "fixture-key".into(),
            session_token: None,
            region: "us-west-2".into(),
        },
        None,
        false,
    )
    .unwrap();
    let route = CdnRoute::new(
        *store.target_identity().unwrap(),
        "org/repo".into(),
        None,
        CdnAccess::Public,
        CdnTransport::new(viewer).unwrap(),
        Duration::from_secs(5),
    )
    .for_consumer(CdnConsumer::Cli, CdnProfile::Direct);
    let enabled = CdnPolicy {
        provider: StorageProviderKind::S3,
        consumer: CdnConsumer::Cli,
        profile: CdnProfile::Direct,
        visibility: CdnVisibility::Public,
    }
    .enabled();
    let activation = route
        .with_publication(&store, &file, "s3://public-export")
        .await;
    assert_eq!(
        matches!(activation, Err(StorageError::Forbidden { .. })),
        enabled
    );
}

#[tokio::test]
async fn public_export_admits_only_the_approved_key_and_object_size() {
    let (transport, request) = endpoint("HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nserved").await;
    let directory = tempfile::tempdir().unwrap();
    let prefix = "org/repo";
    let relative = format!("v2/pack-layers/aa/{}", "a".repeat(64));
    let listed = Path::from(format!("{prefix}/{relative}"));
    let newer = Path::from(format!("{prefix}/v2/pack-layers/cc/{}", "c".repeat(64)));
    let file = write_publication_fixture(
        directory.path().join("receipt.json"),
        "s3://origin",
        "s3://export",
        prefix,
        transport.base.as_str().trim_end_matches('/'),
        &relative,
        6,
    )
    .unwrap();
    let store = Store::new(Arc::new(InMemory::new()))
        .with_target_identity([9; 32])
        .with_native_cdn_origin(Some("s3://origin".into()));
    let route = CdnRoute::new(
        [9; 32],
        prefix.into(),
        None,
        CdnAccess::Public,
        transport,
        Duration::from_secs(5),
    )
    .with_publication(&store, &file, "s3://export")
    .await
    .unwrap();
    let store = store.with_cdn_route(route).unwrap();
    assert_eq!(store.cdn_public_export_sha256(), Some(file.sha256));
    store
        .put_overwrite(&newer, Bytes::from_static(b"new ref"))
        .await
        .unwrap();
    let body = store
        .read_verified(&newer, 7, None, |bytes| async {
            Ok::<_, StorageError>(bytes)
        })
        .await
        .unwrap();
    assert_eq!(body, Bytes::from_static(b"new ref"));
    let attempt = store.cdn_attempt(&listed).unwrap();
    assert_eq!(attempt.get(8, None).await.unwrap(), None);
    assert_eq!(
        attempt.get(6, None).await.unwrap(),
        Some(Bytes::from_static(b"served"))
    );
    assert!(
        request
            .await
            .unwrap()
            .starts_with(&format!("GET /container/{listed} "))
    );
}

async fn endpoint(response: &'static str) -> (CdnTransport, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!(
        "http://{}/container",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let client = client_builder().no_proxy().build().unwrap();
    let received = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut request = String::new();
        while !request.ends_with("\r\n\r\n") {
            assert!(stream.read_line(&mut request).await.unwrap() != 0);
        }
        let _ = stream.write_all(response.as_bytes()).await;
        request
    });
    (CdnTransport { base, client }, received)
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

#[derive(Clone, Default)]
struct Capture {
    events: Arc<Mutex<Vec<Fields>>>,
    gcs_viewer_log: bool,
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        match metadata.target() {
            "crab_storage::cdn::transfer" | "crab_storage::cdn::public_transition" => true,
            "crab_storage::cdn::gcs_viewer_log" => self.gcs_viewer_log,
            _ => false,
        }
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        assert!(event.is_root());
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.events.lock().unwrap().push(fields);
    }
}

#[tokio::test]
async fn gcs_viewer_log_qualification_tags_each_request_and_retains_only_its_hash() {
    let mut previous = None;
    for enabled in [false, true, true] {
        let capture = Capture {
            gcs_viewer_log: enabled,
            ..Capture::default()
        };
        let dispatcher = tracing::Dispatch::new(capture.clone());
        let _guard = tracing::dispatcher::set_default(&dispatcher);
        let (transport, received) =
            endpoint("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nX-Crab-CDN-Cache: hit\r\n\r\nserved")
                .await;
        let context = (
            CdnPolicy {
                provider: StorageProviderKind::Gcs,
                visibility: CdnVisibility::Public,
                consumer: CdnConsumer::Cli,
                profile: CdnProfile::Direct,
            },
            CdnReadFamily::Git,
        );
        let key = Path::from("test/repo/immutable");
        transport
            .get(
                CdnGet {
                    path: &key,
                    size: 6,
                    range: None,
                    context: Some(context),
                },
                CdnViewer::Public,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let request = received.await.unwrap();
        let tag = request
            .lines()
            .find_map(|line| line.strip_prefix("user-agent: "));
        let events = capture.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let fields = &events[0].0;
        if enabled {
            let tag = tag.unwrap();
            let nonce = tag.strip_prefix("crab-cdn-qualification/").unwrap();
            assert_eq!(nonce.len(), 32);
            assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_ne!(previous.as_deref(), Some(tag));
            assert_eq!(
                fields["request_sha256"],
                format!("\"{:x}\"", Sha256::digest(tag))
            );
            assert!(fields.values().all(|value| !value.contains(tag)));
            previous = Some(tag.to_owned());
        } else {
            assert!(tag.is_none());
            assert_eq!(fields["request_sha256"], "\"\"");
        }
    }
}

#[tokio::test]
async fn product_read_records_one_redacted_transfer_for_its_actual_scope() {
    let capture = Capture::default();
    let dispatcher = tracing::Dispatch::new(capture.clone());
    let _guard = tracing::dispatcher::set_default(&dispatcher);
    let (transport, request) = endpoint(
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-2/6\r\nContent-Length: 2\r\nX-Cache: Hit from cloudfront\r\nX-Amz-Cf-Id: opaque-viewer-id==\r\n\r\n12",
    ).await;
    let directory = tempfile::tempdir().unwrap();
    let relative = format!("v2/pack-layers/aa/{}", "a".repeat(64));
    let approval = write_publication_fixture(
        directory.path().join("publication.json"),
        "s3://origin",
        "s3://export",
        "org/repo",
        transport.base.as_str().trim_end_matches('/'),
        &relative,
        6,
    )
    .unwrap();
    let store = Store::new(Arc::new(InMemory::new()))
        .with_target_identity([8; 32])
        .with_bucket_identity(BucketIdentity::new(
            StorageProviderKind::S3,
            "origin",
            "origin",
        ))
        .with_native_cdn_origin(Some("s3://origin".into()));
    let route = CdnRoute::new(
        [8; 32],
        "org/repo".into(),
        None,
        CdnAccess::Public,
        transport,
        Duration::from_secs(5),
    )
    .for_consumer(CdnConsumer::Cli, CdnProfile::Direct)
    .with_publication(&store, &approval, "s3://export")
    .await
    .unwrap();
    let store = store.with_cdn_route(route).unwrap();
    let missing = Path::from(format!("org/repo/v2/pack-layers/bb/{}", "b".repeat(64)));
    assert!(store.cdn_attempt(&missing).is_none());
    let blob_oid = [0x32; 20];
    store.observe_verified_unlisted_git_blob(&missing, &blob_oid);
    let path = Path::from(format!("org/repo/{relative}"));
    let attempt = store.cdn_attempt(&path).unwrap();
    let body = attempt.get(6, Some(1..3)).await.unwrap();
    assert_eq!(body, Some(Bytes::from_static(b"12")));
    attempt
        .payload_observation()
        .unwrap()
        .record_verified_git_blob(&blob_oid);
    request.await.unwrap();

    let events = capture.events.lock().unwrap();
    let transfers: Vec<_> = events
        .iter()
        .filter(|event| event.0.contains_key("status"))
        .collect();
    let [event] = transfers.as_slice() else {
        panic!("expected one event for one GET")
    };
    let fields = &event.0;
    assert_eq!(fields.len(), 13);
    let fingerprint = format!("\"{:x}\"", Sha256::digest("opaque-viewer-id=="));
    assert_eq!(fields["request_sha256"], fingerprint);
    for (name, expected) in [
        ("provider", "\"s3\""),
        ("family", "\"git\""),
        ("consumer", "\"cli\""),
        ("access", "\"public\""),
        ("profile", "\"direct\""),
        ("cache", "\"hit\""),
        ("status", "206"),
        ("range", "true"),
        ("bytes", "2"),
        ("complete", "true"),
    ] {
        assert_eq!(fields[name], expected);
    }
    assert!(!events.iter().any(|event| {
        event
            .0
            .values()
            .any(|value| value.contains("org/repo") || value.contains("opaque-viewer-id"))
    }));
    let observed_blob = events
        .iter()
        .find_map(|event| event.0.get("git_blob_oid_sha256"));
    assert_eq!(
        observed_blob,
        Some(&format!("\"{:x}\"", Sha256::digest(blob_oid)))
    );
    let transitions: Vec<_> = events
        .iter()
        .filter(|event| event.0.contains_key("observation"))
        .collect();
    let key = format!("{:x}", Sha256::digest(path.as_ref().as_bytes()));
    let missing = format!("{:x}", Sha256::digest(missing.as_ref().as_bytes()));
    let blob = format!("\"{:x}\"", Sha256::digest(blob_oid));
    assert_eq!(transitions.len(), 4);
    for (event, kind, digest, exact_blob) in [
        (transitions[0], "\"unlisted\"", &missing, None),
        (transitions[1], "\"verified_native\"", &missing, Some(&blob)),
        (transitions[2], "\"listed\"", &key, None),
        (transitions[3], "\"verified_edge\"", &key, Some(&blob)),
    ] {
        assert_eq!(event.0["observation"], kind);
        assert_eq!(event.0["key_sha256"], *digest);
        assert_eq!(event.0["approval_sha256"], approval.sha256);
        assert_eq!(event.0.get("git_blob_oid_sha256"), exact_blob);
    }
}

#[tokio::test]
async fn full_and_range_bytes_match_the_origin() {
    let path = Path::from("org/repo/content");
    let origin = Arc::new(InMemory::new());
    origin
        .put(&path, Bytes::from_static(b"012345").into())
        .await
        .unwrap();
    let origin = Store::new(origin);
    let cancel = CancellationToken::new();

    let (cdn, request) = endpoint("HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n012345").await;
    let bytes = cdn
        .get(
            CdnGet::diagnostic(&path, 6, None),
            CdnViewer::Public,
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(bytes, origin.get_immutable_bounded(&path, 6).await.unwrap());
    let request = request.await.unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /container/org/repo/content http/1.1\r\n"));
    assert!(request.contains("accept-encoding: identity\r\n"));
    assert!(!request.contains("authorization:"));

    let (cdn, request) = endpoint("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/6\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n123\r\n0\r\n\r\n").await;
    let bytes = cdn
        .get(
            CdnGet::diagnostic(&path, 6, Some(1..4)),
            CdnViewer::Public,
            Duration::from_secs(5),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(bytes, origin.range_get(&path, 1..4).await.unwrap());
    assert!(
        request
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("range: bytes=1-3\r\n")
    );
}

#[tokio::test]
async fn rejects_wrong_range_and_representation() {
    let path = Path::from("org/repo/content");
    for response in [
        "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n123",
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/6\r\nContent-Length: 3\r\n\r\n123",
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/7\r\nContent-Length: 3\r\n\r\n123",
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/6\r\nContent-Encoding: gzip\r\nContent-Length: 3\r\n\r\n123",
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/6\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n1234\r\n0\r\n\r\n",
        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-3/6\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n12\r\n0\r\n\r\n",
    ] {
        let (cdn, request) = endpoint(response).await;
        let result = cdn
            .get(
                CdnGet::diagnostic(&path, 6, Some(1..4)),
                CdnViewer::Public,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(StorageError::CorruptObject { .. })),
            "{result:?}"
        );
        request.await.unwrap();
    }
}

#[tokio::test]
async fn denial_and_redirect_are_distinct_from_transient_unavailability() {
    let path = Path::from("org/repo/content");
    for response in [
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
    ] {
        let (cdn, request) = endpoint(response).await;
        let result = cdn
            .get(
                CdnGet::diagnostic(&path, 6, None),
                CdnViewer::Public,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await;
        match &response[9..12] {
            "403" => assert!(matches!(result, Err(StorageError::Forbidden { .. }))),
            "302" => assert!(matches!(result, Err(StorageError::ObjectStore { .. }))),
            "503" => assert!(matches!(result, Err(StorageError::NetworkTransient { .. }))),
            _ => unreachable!(),
        }
        request.await.unwrap();
    }
}

struct Admission {
    cancel: CancellationToken,
    request_limit: u64,
    requests: std::sync::atomic::AtomicU64,
    received: std::sync::atomic::AtomicU64,
}

#[async_trait::async_trait]
impl crate::ReadAdmission for Admission {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn request(&self) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            < self.request_limit
        {
            Ok(())
        } else {
            Err("admission request budget exhausted".into())
        }
    }

    async fn bytes(
        &self,
        _bytes: u64,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("admission budget exhausted".into())
    }

    async fn received(&self, bytes: u64) {
        self.received
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::test]
async fn received_bytes_are_counted_before_admission_rejects_them() {
    let (cdn, request) = endpoint("HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n012345").await;
    let admission = Arc::new(Admission {
        cancel: CancellationToken::new(),
        request_limit: 1,
        requests: std::sync::atomic::AtomicU64::new(0),
        received: std::sync::atomic::AtomicU64::new(0),
    });
    let context = transport_read_admission::TransportReadContext::new(admission.clone());
    let path = Path::from("org/repo/content");
    let cancel = CancellationToken::new();
    let bytes = transport_read_admission::scope(
        context,
        cdn.get(
            CdnGet::diagnostic(&path, 6, None),
            CdnViewer::Public,
            Duration::from_secs(5),
            &cancel,
        ),
    )
    .await;
    assert!(matches!(bytes, Err(StorageError::ReadRejected { .. })));
    assert_eq!(
        admission
            .received
            .load(std::sync::atomic::Ordering::Relaxed),
        6
    );
    request.await.unwrap();
}

#[tokio::test]
async fn edge_and_origin_restart_share_request_admission() {
    let (cdn, request) =
        endpoint("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
    let hash = "abcd".repeat(16);
    let path = Path::from(format!("org/repo/v2/history/ab/{hash}"));
    let origin = Arc::new(InMemory::new());
    origin
        .put(&path, Bytes::from_static(b"good").into())
        .await
        .unwrap();
    let admission = Arc::new(Admission {
        cancel: CancellationToken::new(),
        request_limit: 1,
        requests: std::sync::atomic::AtomicU64::new(0),
        received: std::sync::atomic::AtomicU64::new(0),
    });
    let target = [8; 32];
    let store = Store::new(origin)
        .with_target_identity(target)
        .with_read_admission(admission.clone())
        .with_cdn_route(CdnRoute::new(
            target,
            "org/repo".into(),
            None,
            CdnAccess::Public,
            cdn,
            Duration::from_secs(5),
        ))
        .unwrap();

    let result = store
        .read_verified(&path, 4, None, |bytes| {
            std::future::ready(Ok::<_, StorageError>(bytes))
        })
        .await;
    assert!(matches!(result, Err(StorageError::ReadRejected { .. })));
    assert_eq!(
        admission
            .requests
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    request.await.unwrap();
}

#[tokio::test]
async fn pre_cancelled_request_performs_no_network_io() {
    let (cdn, request) = endpoint("HTTP/1.1 500 Internal Server Error\r\n\r\n").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let path = Path::from("org/repo/content");
    let result = cdn
        .get(
            CdnGet::diagnostic(&path, 6, None),
            CdnViewer::Public,
            Duration::from_secs(5),
            &cancel,
        )
        .await;
    assert!(matches!(result, Err(StorageError::Cancelled)));
    assert!(!request.is_finished());
    request.abort();
}

#[tokio::test]
async fn attaches_only_the_selected_private_viewer_header() {
    let path = Path::from("org/repo/content");
    for (viewer, expected, absent) in [
        (
            CdnViewer::Cookie("Cloud-CDN-Cookie=opaque"),
            "cookie: Cloud-CDN-Cookie=opaque",
            "authorization:",
        ),
        (
            CdnViewer::Bearer("opaque.token.value"),
            "authorization: Bearer opaque.token.value",
            "cookie:",
        ),
    ] {
        let (cdn, request) = endpoint("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        cdn.get(
            CdnGet::diagnostic(&path, 0, None),
            viewer,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let request = request.await.unwrap();
        assert!(request.lines().any(|line| line == expected));
        assert!(!request.lines().any(|line| line.starts_with(absent)));
    }
}

#[tokio::test]
async fn scoped_read_checks_edge_bytes_and_keeps_denial_terminal() {
    let hash = "abcd".repeat(16);
    let path = Path::from(format!("org/repo/v2/pack-layers/ab/{hash}"));
    let memory = Arc::new(InMemory::new());
    memory
        .put(&path, Bytes::from_static(b"origin").into())
        .await
        .unwrap();
    let verify = |bytes| std::future::ready(Ok::<_, StorageError>(bytes));
    let target = [7; 32];
    for (response, expected) in [
        (
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/6\r\nContent-Length: 3\r\n\r\ncdn",
            Some("cdn"),
        ),
        (
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/7\r\nContent-Length: 3\r\n\r\ncdn",
            Some("ori"),
        ),
        ("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n", None),
    ] {
        let (cdn, request) = endpoint(response).await;
        let store = Store::new(memory.clone())
            .with_target_identity(target)
            .with_cdn_route(CdnRoute::new(
                target,
                "org/repo".into(),
                None,
                CdnAccess::Public,
                cdn,
                Duration::from_secs(10),
            ))
            .unwrap();
        let result = store.read_verified(&path, 6, Some(0..3), verify).await;
        match expected {
            Some(bytes) => assert_eq!(result.unwrap(), bytes),
            None => {
                let error = result.unwrap_err();
                assert!(matches!(
                    crate::store::cdn_read_rejection(&error),
                    Some(StorageError::Forbidden { .. })
                ));
            }
        }
        request.await.unwrap();
    }
}

#[tokio::test]
async fn edge_verifier_failure_restarts_at_origin() {
    let hash = "abcd".repeat(16);
    let path = Path::from(format!("org/repo/v2/history/ab/{hash}"));
    let memory = Arc::new(InMemory::new());
    memory
        .put(&path, Bytes::from_static(b"good").into())
        .await
        .unwrap();
    let target = [8; 32];
    let (cdn, request) = endpoint("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbad!").await;
    let store = Store::new(memory)
        .with_target_identity(target)
        .with_cdn_route(CdnRoute::new(
            target,
            "org/repo".into(),
            None,
            CdnAccess::Public,
            cdn,
            Duration::from_secs(10),
        ))
        .unwrap();
    let bytes = store
        .read_verified(&path, 4, None, |bytes: Bytes| {
            std::future::ready(if bytes == "good" {
                Ok(bytes)
            } else {
                Err(corrupt(&path, "invalid bytes"))
            })
        })
        .await
        .unwrap();
    assert_eq!(bytes, "good");
    request.await.unwrap();
}

struct RefreshingViewer(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl CdnCredentials for RefreshingViewer {
    async fn credential(&self, _: &Path, rejected: Option<u64>) -> Result<Option<CdnCredential>> {
        let call = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(rejected, (call == 1).then_some(0));
        Ok(Some(CdnCredential::cookie(
            call as u64,
            format!("token={call}"),
        )))
    }
}

#[tokio::test]
async fn unavailable_edge_recheck_after_private_denial_never_reads_origin() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = client_builder().no_proxy().build().unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let serve = tokio::spawn(async move {
        for (i, status) in ["403 Forbidden", "503 Service Unavailable"]
            .into_iter()
            .enumerate()
        {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert!(stream.read_line(&mut request).await.unwrap() != 0);
            }
            assert!(
                request
                    .lines()
                    .any(|line| line == format!("cookie: token={i}"))
            );
            stream
                .write_all(
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let hash = "abcd".repeat(16);
    let path = Path::from(format!("org/repo/v2/history/ab/{hash}"));
    let memory = Arc::new(InMemory::new());
    memory
        .put(&path, Bytes::from_static(b"readable origin").into())
        .await
        .unwrap();
    let viewer = Arc::new(RefreshingViewer(std::sync::atomic::AtomicUsize::new(0)));
    let target = [9; 32];
    let store = Store::new(memory)
        .with_target_identity(target)
        .with_cdn_route(CdnRoute::new(
            target,
            "org/repo".into(),
            None,
            CdnAccess::Private(viewer.clone()),
            CdnTransport { base, client },
            Duration::from_secs(10),
        ))
        .unwrap();
    let result = store
        .read_verified(&path, 15, None, |body| {
            std::future::ready(Ok::<_, StorageError>(body))
        })
        .await;
    assert!(matches!(result, Err(StorageError::ObjectStore { .. })));
    assert_eq!(viewer.0.load(std::sync::atomic::Ordering::Relaxed), 2);
    serve.await.unwrap();
}

#[tokio::test]
async fn file_download_hashes_bounded_ranges_in_object_order() {
    const FIRST_LENGTH: usize = 8 * 1024 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let client = client_builder().no_proxy().build().unwrap();
    let serve = tokio::spawn(async move {
        let mut offsets = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert!(stream.read_line(&mut request).await.unwrap() != 0);
            }
            let value = request
                .lines()
                .find_map(|line| line.strip_prefix("range: bytes="))
                .unwrap();
            let (start, end) = value.split_once('-').unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap();
            offsets.push(start);
            let reply = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                FIRST_LENGTH + 1,
                end - start + 1
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
            let chunk = if start == 0 {
                vec![b'a'; FIRST_LENGTH]
            } else {
                vec![b'b']
            };
            stream.write_all(&chunk).await.unwrap();
        }
        offsets.sort_unstable();
        offsets
    });
    let hash = "abcd".repeat(16);
    let path = Path::from(format!("org/repo/v2/pack-layers/ab/{hash}"));
    let target = [11; 32];
    let store = Store::new(Arc::new(InMemory::new()))
        .with_target_identity(target)
        .with_cdn_route(CdnRoute::new(
            target,
            "org/repo".into(),
            None,
            CdnAccess::Public,
            CdnTransport { base, client },
            Duration::from_secs(10),
        ))
        .unwrap();
    let staging = tempfile::tempdir().unwrap();
    let destination = staging.path().join("pack");
    let size = (FIRST_LENGTH + 1) as u64;
    let mut digest = blake3::Hasher::new();
    let complete = store
        .cdn_attempt(&path)
        .unwrap()
        .download(
            size,
            0..size,
            &destination,
            &CancellationToken::new(),
            |bytes| {
                digest.update(bytes);
            },
        )
        .await
        .unwrap();
    assert!(complete);
    assert_eq!(serve.await.unwrap(), [0, FIRST_LENGTH]);
    let mut expected = vec![b'a'; FIRST_LENGTH];
    expected.push(b'b');
    assert_eq!(digest.finalize(), blake3::hash(&expected));
    assert_eq!(std::fs::read(destination).unwrap(), expected);
}
