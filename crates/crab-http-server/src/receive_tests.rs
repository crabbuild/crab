use super::*;
use std::path::Path;
use std::process::Output;

async fn git(path: &Path, args: &[&str]) -> Output {
    let path = path.to_owned();
    let args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "user.name=Receive test",
                "-c",
                "user.email=receive@example.invalid",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

async fn success(path: &Path, args: &[&str]) -> String {
    let started = Instant::now();
    let output = git(path, args).await;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if args.first() == Some(&"push") {
        eprintln!(
            "native push completed in {} ms",
            started.elapsed().as_millis()
        );
    }
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn disconnected_receive_drains_intake_and_returns_transfer_capacity() {
    use axum::body::Body;
    use tower::ServiceExt;
    let server = maintenance_tests::fixture().await;
    let request = Request::builder()
        .method("POST")
        .uri("/git/team/repo/git-receive-pack")
        .header("host", "localhost:8788")
        .header("content-type", "application/x-git-receive-pack-request")
        .body(Body::from_stream(futures_util::stream::pending::<
            std::result::Result<axum::body::Bytes, std::io::Error>,
        >()))
        .unwrap();
    let app = router(Arc::clone(&server));
    let client = tokio::spawn(app.oneshot(request));
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.receives.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.git_admission.available_permits(), 3);
    client.abort();
    let _ = client.await;
    server.receives.close();
    tokio::time::timeout(Duration::from_secs(2), server.receives.wait())
        .await
        .unwrap();
    assert_eq!(server.git_admission.available_permits(), 4);
    let repo = &server.repositories[&("team".into(), "repo".into())];
    let snapshot =
        crab_metadata::manifest_store::read_repository_snapshot(&repo.store, &repo.layout)
            .await
            .unwrap();
    assert!(snapshot.manifest.refs.is_empty() && snapshot.journal.transactions.is_empty());
    server.cancellation.cancel();
    server.runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_http_push_publishes_exact_objects_and_rejects_rewrites_atomically() {
    exercise(maintenance_tests::fixture().await).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated local RustFS bucket/prefix and environment credentials"]
async fn native_http_push_rustfs() {
    let bucket = std::env::var("QUALIFICATION_BUCKET").unwrap();
    let prefix = std::env::var("QUALIFICATION_PREFIX").unwrap();
    assert!(prefix.starts_with("qualification/http-receive-"));
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let layout = StoreLayout::new(store.clone(), prefix.clone());
    crab_metadata::manifest_store::create_manifest(
        &store,
        &layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    let mut server = maintenance_tests::fixture().await;
    let repo = Arc::get_mut(&mut server)
        .unwrap()
        .repositories
        .get_mut(&("team".into(), "repo".into()))
        .unwrap();
    repo.identity = RepositoryIdentity::new(format!("s3:{bucket}"), prefix.clone(), 1).unwrap();
    repo.config.bucket = bucket;
    repo.config.prefix = prefix;
    repo.store = store;
    repo.layout = layout;
    exercise(server).await;
}

async fn exercise(mut server: Arc<Server>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    Arc::get_mut(&mut server).unwrap().port = port;
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let app = router(Arc::clone(&server));
    let http = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let url = format!("http://127.0.0.1:{port}/git/team/repo");
    success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "first content\n").unwrap();
    success(path, &["add", "README.md"]).await;
    success(path, &["commit", "-m", "first commit"]).await;
    let first = success(path, &["rev-parse", "HEAD"]).await;
    success(path, &["tag", "-a", "v1", "-m", "first tag"]).await;
    success(path, &["push", "--atomic", &url, "main", "refs/tags/v1"]).await;
    std::fs::write(path.join("README.md"), "second content\n").unwrap();
    success(path, &["commit", "-am", "second commit"]).await;
    let second = success(path, &["rev-parse", "HEAD"]).await;
    success(path, &["-c", "http.postBuffer=1", "push", &url, "main"]).await;
    let rejected = git(
        path,
        &[
            "push",
            "--atomic",
            &url,
            &format!("+{first}:refs/heads/main"),
            "HEAD:refs/heads/rejected",
        ],
    )
    .await;
    assert!(
        !rejected.status.success(),
        "non-fast-forward batch was accepted"
    );
    success(path, &["push", &url, "HEAD:refs/tags/existing-object"]).await;
    success(
        path,
        &["push", &url, ":refs/tags/existing-object", ":refs/tags/v1"],
    )
    .await;

    let response = reqwest::get(format!("http://127.0.0.1:{port}/api/repos/team/repo/refs"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let visible: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(visible["refs"][0]["oid"], second);
    let reader = tempfile::tempdir().unwrap();
    success(reader.path(), &["init", "--bare", "."]).await;
    success(
        reader.path(),
        &["-c", "protocol.version=2", "fetch", &url, "main"],
    )
    .await;
    assert_eq!(
        success(reader.path(), &["rev-parse", "FETCH_HEAD"]).await,
        second
    );
    assert_eq!(
        success(reader.path(), &["show", "FETCH_HEAD:README.md"]).await,
        "second content"
    );
    reader.close().unwrap();

    let repo = &server.repositories[&("team".into(), "repo".into())];
    let remote = repo
        .open_current(&server, server.options, &server.cancellation)
        .await
        .unwrap();
    let refs: BTreeMap<_, _> = remote
        .refs()
        .entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.target.to_string()))
        .collect();
    assert_eq!(
        refs,
        BTreeMap::from([("refs/heads/main".into(), second.clone())])
    );
    let listing = success(path, &["rev-list", "--objects", "HEAD"]).await;
    let mut expected = Vec::new();
    for line in listing.lines() {
        let oid = line.split_whitespace().next().unwrap();
        let kind = success(path, &["cat-file", "-t", oid]).await;
        let output = git(path, &["cat-file", &kind, oid]).await;
        assert!(output.status.success());
        expected.push((
            gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap(),
            output.stdout,
        ));
    }
    // The reader must not depend on a surviving client clone or object database.
    directory.close().unwrap();
    let operation = remote
        .operation(
            crab_remote_git::OperationKind::Repository,
            &server.cancellation,
        )
        .await
        .unwrap();
    for (oid, expected) in expected {
        let actual = operation.read_object(oid).await.unwrap();
        assert_eq!(actual.data.as_ref(), expected);
    }
    operation.finish(Ok(())).await.unwrap();
    server.cancellation.cancel();
    stop.cancel();
    http.await.unwrap();
    server.receives.close();
    server.receives.wait().await;
    server.finish_maintenance().await.unwrap();
    server.runtime.shutdown().await;
}
