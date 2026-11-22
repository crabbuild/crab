use crab_storage::cdn::{CdnAccess, CdnRoute, CdnTransport};
use futures_util::TryStreamExt as _;
use object_store::{ObjectStoreExt as _, path::Path};
use sha2::Digest as _;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorized_gateway_reads_verified_git_and_lfs_from_the_edge() {
    let directory = tempfile::tempdir().unwrap();
    let secret = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
    std::fs::write(secret.path(), "fixture-key-secret").unwrap();
    let hash = "a".repeat(64);
    let relative = format!("v2/history/aa/{hash}");
    let approval = crab_storage::cdn::write_publication_fixture(
        directory.path().join("receipt.json"),
        "s3://fixture-bucket",
        "s3://approved-export",
        "org/repo",
        "https://downloads.example.com",
        &relative,
        6,
    )
    .unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
listen = "127.0.0.1:0"
management_listen = "127.0.0.1:1"
[cache]
directory = {cache:?}
max_bytes = 67108864
[[credentials]]
access_key = "fixture-key"
secret_key_file = {secret:?}
principal = "reader"
[[repositories]]
name = "repo"
bucket = "fixture-bucket"
prefix = "org/repo"
members = [{{ principal = "reader", access = "write" }}]
[repositories.cdn]
origin = "s3://fixture-bucket"
export_origin = "s3://approved-export"
base_url = "https://downloads.example.com"
access = "public"
publication = {{ path = {receipt:?}, sha256 = {digest:?} }}
"#,
        cache = directory.path().join("cache"),
        secret = secret.path(),
        receipt = approval.path,
        digest = approval.sha256,
    ))
    .unwrap();
    config.validate().unwrap();
    let entry = config.repositories[0].clone();
    let target = [7; 32];
    let origin = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_native_cdn_origin(Some("s3://fixture-bucket".into()))
        .with_bucket_identity(crab_storage::BucketIdentity::new(
            crab_storage::StorageProviderKind::S3,
            "fixture-bucket",
            "fixture-bucket",
        ))
        .with_target_identity(target);
    let eligible = Path::from(format!("org/repo/{relative}"));
    let configured = repository_read_store(&entry, &origin, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        configured.cdn_attempt(&eligible).is_some(),
        crab_storage::cdn::qualification_enabled()
    );
    let inactive = repository_read_store(&entry, &origin, false).await.unwrap();
    assert!(inactive.is_none());
    let wrong = origin
        .clone()
        .with_native_cdn_origin(Some("gs://fixture-bucket".into()));
    assert!(repository_read_store(&entry, &wrong, true).await.is_err());

    let cache = Arc::new(LocalCache::with_limits(
        config.cache.directory.clone(),
        config.cache.max_bytes,
        Some(config.cache.max_bytes),
    ));
    let metrics = Metrics::new().unwrap();
    let writer =
        Repository::new_with_cache(entry.clone(), origin.clone(), Arc::clone(&cache)).unwrap();
    crab_write::capsule_protocol::initialize(&writer.layout, &hash, "refs/heads/main")
        .await
        .unwrap();
    let runtime = Arc::new(RemoteGitRuntime::default());
    let options = RepositoryOptions::default();
    let cancellation = CancellationToken::new();
    let mutations = Arc::new(mutation::Coordinator::new(
        Arc::clone(&runtime),
        options,
        cancellation.clone(),
        metrics.clone(),
    ));
    mutations
        .apply(
            &writer,
            "refs/heads/main",
            &crab_remote_git::GitPath::new(b"file".to_vec()).unwrap(),
            mutation::Change::Put {
                bytes: Bytes::from_static(b"original"),
                track_lfs: false,
                attributes: Box::new(crate::attributes::PutAttributes {
                    etag_override: Some("fixture-etag".into()),
                    ..Default::default()
                }),
                condition: mutation::PutCondition::None,
            },
            "reader",
            &cancellation,
        )
        .await
        .unwrap();
    // Exercise more than one local verification block with a short final block.
    let lfs_bytes = Bytes::from(vec![b'l'; 1024 * 1024 + 19]);
    let lfs_size = lfs_bytes.len() as u64;
    let oid = sha2::Sha256::digest(&lfs_bytes).into();
    writer.lfs.put(&oid, lfs_bytes.clone()).await.unwrap();
    mutations
        .apply(
            &writer,
            "refs/heads/main",
            &crab_remote_git::GitPath::new(b"large".to_vec()).unwrap(),
            mutation::Change::Put {
                bytes: lfs_pointer_content(oid, lfs_size).bytes,
                track_lfs: true,
                attributes: Box::default(),
                condition: mutation::PutCondition::None,
            },
            "reader",
            &cancellation,
        )
        .await
        .unwrap();
    mutations
        .apply(
            &writer,
            "refs/heads/main",
            &crab_remote_git::GitPath::new(b"other".to_vec()).unwrap(),
            mutation::Change::Put {
                bytes: Bytes::from_static(b"unrelated"),
                track_lfs: false,
                attributes: Box::default(),
                condition: mutation::PutCondition::None,
            },
            "reader",
            &cancellation,
        )
        .await
        .unwrap();
    // Ordinary commits start in ref-local runs. Publish a physical layer so
    // this fixture proves the optimized body path, beyond metadata requests.
    crab_remote::checkpoint::maintain_capsule_repository(
        &writer.layout,
        1,
        16 * 1024 * 1024,
        &cancellation,
    )
    .await
    .unwrap();
    assert!(
        !origin
            .list_prefix(&Path::from("org/repo/v2/pack-layers"))
            .await
            .unwrap()
            .is_empty()
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
    let read_store = origin
        .clone()
        .with_cdn_route(
            CdnRoute::new(
                target,
                entry.prefix.clone(),
                None,
                CdnAccess::Public,
                transport,
                Duration::from_secs(10),
            )
            .for_consumer(
                crab_storage::cdn::CdnConsumer::S3Gateway,
                crab_storage::cdn::CdnProfile::Direct,
            ),
        )
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let edge = serve_edge(
        listener,
        origin.clone(),
        Arc::clone(&requests),
        cancellation.clone(),
    );
    let reader = Repository::new_with_cache_observer(
        entry,
        origin,
        Some(read_store),
        Arc::clone(&cache),
        metrics.cache_observer(),
    )
    .unwrap();
    let gateway = Gateway {
        repositories: Arc::new(BTreeMap::from([("repo".to_owned(), reader)])),
        runtime,
        options,
        mutations,
        auth: GatewayAuth::load(&config).unwrap(),
        region: Arc::from("us-east-1"),
        admission: Admission::new(8, cancellation.clone(), metrics.clone()),
        metrics,
        local_cache: cache,
        cancellation: cancellation.clone(),
    };

    let reader = &gateway.repositories["repo"];
    let origin_view = reader
        .read_views
        .current(
            reader,
            Arc::clone(&gateway.runtime),
            gateway.options,
            &cancellation,
        )
        .await
        .unwrap();
    let operation = origin_view
        .remote()
        .operation(OperationKind::Repository, &cancellation)
        .await
        .unwrap();
    let snapshot = origin_view.snapshot("main", &operation).await.unwrap();
    let blob = snapshot
        .read_blob(
            &crab_remote_git::GitPath::new(b"file".to_vec()).unwrap(),
            &operation,
        )
        .await
        .unwrap();
    assert_eq!(blob.bytes, Bytes::from_static(b"original"));
    operation.finish(Ok(())).await.unwrap();
    assert!(requests.lock().unwrap().is_empty());

    let request = |authorized: bool| S3Request {
        input: GetObjectInput {
            bucket: "repo".into(),
            key: "main/file".into(),
            if_match: Some(ETagCondition::ETag(ETag::Strong("fixture-etag".into()))),
            range: Some(Range::parse("bytes=2-4").unwrap()),
            ..Default::default()
        },
        method: http::Method::GET,
        uri: http::Uri::from_static("/repo/main/file"),
        headers: http::HeaderMap::new(),
        extensions: http::Extensions::new(),
        credentials: authorized.then(|| s3s::auth::Credentials {
            access_key: "fixture-key".into(),
            secret_key: "fixture-key-secret".into(),
        }),
        region: None,
        service: None,
        trailing_headers: None,
    };
    let denied = gateway.get_object(request(false)).await.err().unwrap();
    assert_eq!(denied.code().as_str(), "AccessDenied");
    assert!(requests.lock().unwrap().is_empty());
    let output = gateway.get_object(request(true)).await.unwrap().output;
    assert_eq!(output.content_range.as_deref(), Some("bytes 2-4/8"));
    assert!(matches!(output.e_tag, Some(ETag::Strong(etag)) if etag == "fixture-etag"));
    let bytes = output
        .body
        .unwrap()
        .try_fold(Vec::new(), |mut body, bytes| async move {
            body.extend_from_slice(&bytes);
            Ok(body)
        })
        .await
        .unwrap();
    assert_eq!(bytes, b"igi");
    let edge_paths = requests.lock().unwrap().clone();
    assert!(
        edge_paths
            .iter()
            .any(|path| path.contains("/v2/pack-layers/")),
        "{edge_paths:?}"
    );
    requests.lock().unwrap().clear();
    let mut full = request(true);
    full.input.range = None;
    let cached = gateway.get_object(full).await.unwrap().output;
    let bytes = cached
        .body
        .unwrap()
        .try_fold(Vec::new(), |mut body, bytes| async move {
            body.extend_from_slice(&bytes);
            Ok(body)
        })
        .await
        .unwrap();
    assert_eq!(bytes, b"original");
    assert!(requests.lock().unwrap().is_empty());
    let lfs_request = |range| {
        let mut request = request(true);
        request.uri = http::Uri::from_static("/repo/main/large");
        request.input.key = "main/large".into();
        request.input.range = range;
        request.input.if_match = None;
        request
    };
    for (uses_edge, corrupt_cache) in [(true, false), (false, false), (true, true)] {
        if corrupt_cache {
            let lfs = &reader.edge_reader.as_ref().unwrap().lfs;
            let namespace = lfs
                .store()
                .cdn_lfs_cache_namespace(&lfs.object_path_for(&oid), lfs_size)
                .unwrap();
            let cache = gateway.local_cache.for_authorized_reads(&namespace);
            let path = cache
                .open_lfs_file(&oid, lfs_size)
                .await
                .unwrap()
                .unwrap()
                .key()
                .to_owned();
            std::fs::write(path, vec![0; lfs_bytes.len()]).unwrap();
        }
        let output = gateway.get_object(lfs_request(None)).await.unwrap().output;
        assert_eq!(read_lfs_body(output).await, lfs_bytes.as_ref());
        let edge_paths = std::mem::take(&mut *requests.lock().unwrap());
        assert_eq!(
            edge_paths.iter().any(|key| key.contains("/lfs/objects/")),
            uses_edge
        );
    }
    let output = gateway
        .get_object(lfs_request(Some(Range::parse("bytes=0-7").unwrap())))
        .await
        .unwrap()
        .output;
    assert_eq!(read_lfs_body(output).await, &lfs_bytes[..8]);
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|key| key.contains("/lfs/objects/"))
    );
    gateway.shutdown().await;
    cancellation.cancel();
    edge.await.unwrap();
}

async fn read_lfs_body(output: GetObjectOutput) -> Vec<u8> {
    output
        .body
        .unwrap()
        .try_fold(Vec::new(), |mut body, bytes| async move {
            body.extend_from_slice(&bytes);
            Ok(body)
        })
        .await
        .unwrap()
}

fn serve_edge(
    listener: tokio::net::TcpListener,
    origin: Store,
    requests: Arc<Mutex<Vec<String>>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, _) = tokio::select! {
                () = cancel.cancelled() => break,
                stream = listener.accept() => stream.unwrap(),
            };
            let mut stream = BufReader::new(stream);
            let mut head = String::new();
            while !head.ends_with("\r\n\r\n") {
                assert!(stream.read_line(&mut head).await.unwrap() > 0);
            }
            let mut line = head.lines().next().unwrap().split_whitespace();
            assert_eq!(line.next(), Some("GET"));
            let key = line.next().unwrap().strip_prefix('/').unwrap();
            assert!(key.starts_with("org/repo/v2/") || key.starts_with("org/repo/lfs/objects/"));
            assert_ne!(key, "org/repo/v2/root");
            requests.lock().unwrap().push(key.to_owned());
            let body = origin
                .inner()
                .get(&Path::from(key))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let range = head
                .lines()
                .find_map(|header| header.strip_prefix("range: bytes="));
            let (status, content_range, bytes) = if let Some(range) = range {
                let (start, end) = range.split_once('-').unwrap();
                let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
                (
                    "206 Partial Content",
                    format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()),
                    body.slice(start..=end),
                )
            } else {
                ("200 OK", String::new(), body)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\n{content_range}Content-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len(),
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        }
    })
}
