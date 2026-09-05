use super::*;
use base64::Engine;
use std::path::Path;
use std::process::Output;

pub(super) async fn git(path: &Path, args: &[&str]) -> Output {
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

pub(super) async fn success(path: &Path, args: &[&str]) -> String {
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
        .uri("/git/team/repo.git/git-receive-pack")
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
    for branch in ["main", "trunk"] {
        exercise(maintenance_tests::fixture().await, branch).await;
    }
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
    exercise(server, "trunk").await;
}

async fn exercise(mut server: Arc<Server>, branch: &str) {
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
    let url = format!("http://127.0.0.1:{port}/git/team/repo.git");
    success(
        path,
        &[
            "init",
            &format!("--initial-branch={branch}"),
            "--object-format=sha1",
            ".",
        ],
    )
    .await;
    std::fs::write(path.join("README.md"), "first content\n").unwrap();
    success(path, &["add", "README.md"]).await;
    success(path, &["commit", "-m", "first commit"]).await;
    let first = success(path, &["rev-parse", "HEAD"]).await;
    success(path, &["tag", "-a", "v1", "-m", "first tag"]).await;
    success(path, &["push", &url, "refs/tags/v1"]).await;
    let response = reqwest::get(format!("http://127.0.0.1:{port}/api/repos/team/repo/refs"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let tag_only: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert!(tag_only["head"].is_null());
    assert_eq!(tag_only["unborn_head"], "refs/heads/main");
    let reader = tempfile::tempdir().unwrap();
    success(
        reader.path(),
        &["-c", "protocol.version=2", "clone", "--bare", &url, "."],
    )
    .await;
    assert_eq!(
        success(reader.path(), &["symbolic-ref", "HEAD"]).await,
        "refs/heads/main"
    );
    assert_eq!(
        success(reader.path(), &["show", "refs/tags/v1:README.md"]).await,
        "first content"
    );
    reader.close().unwrap();
    success(path, &["push", "--atomic", &url, branch, "refs/tags/v1"]).await;
    std::fs::write(path.join("README.md"), "second content\n").unwrap();
    success(path, &["commit", "-am", "second commit"]).await;
    let second = success(path, &["rev-parse", "HEAD"]).await;
    success(path, &["-c", "http.postBuffer=1", "push", &url, branch]).await;
    let rejected = git(
        path,
        &[
            "push",
            "--atomic",
            &url,
            &format!("+{first}:refs/heads/{branch}"),
            "HEAD:refs/heads/rejected",
        ],
    )
    .await;
    assert!(
        !rejected.status.success(),
        "non-fast-forward batch was accepted"
    );
    for extra in [None, Some("HEAD:refs/heads/replacement")] {
        let deletion = format!(":refs/heads/{branch}");
        let mut args = vec!["push", "--atomic", &url, &deletion];
        args.extend(extra);
        let rejected = git(path, &args).await;
        assert!(
            !rejected.status.success(),
            "default branch deletion was accepted"
        );
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("deletion is prohibited"));
    }
    success(path, &["push", &url, "HEAD:refs/heads/temporary"]).await;
    success(path, &["push", &url, ":refs/heads/temporary"]).await;
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
    assert_eq!(visible["head"]["name"], format!("refs/heads/{branch}"));
    let readme_path = "524541444d452e6d64";
    let latest = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/commit?rev={second}&path_hex={readme_path}"
    ))
    .await
    .unwrap();
    assert_eq!(latest.status(), StatusCode::OK);
    let latest: serde_json::Value = serde_json::from_slice(&latest.bytes().await.unwrap()).unwrap();
    assert_eq!(latest["oid"], second);
    assert_eq!(latest["message"], "second commit\n");
    assert_eq!(latest["change_kind"], "Modified");
    let path_history = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/commits?rev={second}&path_hex={readme_path}&limit=1"
    ))
    .await
    .unwrap();
    assert_eq!(path_history.status(), StatusCode::OK);
    let path_history: serde_json::Value =
        serde_json::from_slice(&path_history.bytes().await.unwrap()).unwrap();
    assert_eq!(path_history["items"][0]["oid"], second);
    assert_eq!(path_history["items"][0]["change_kind"], "Modified");
    assert_eq!(path_history["items"].as_array().unwrap().len(), 1);
    assert!(path_history["next"].is_string());
    let cursor = path_history["next"].as_str().unwrap();
    let older_history = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/commits?rev={second}&path_hex={readme_path}&limit=1&cursor={cursor}"
    ))
    .await
    .unwrap();
    assert_eq!(older_history.status(), StatusCode::OK);
    let older_history: serde_json::Value =
        serde_json::from_slice(&older_history.bytes().await.unwrap()).unwrap();
    assert_eq!(older_history["items"][0]["oid"], first);
    assert_eq!(older_history["items"][0]["change_kind"], "Added");
    assert!(older_history["next"].is_null());
    let reader = tempfile::tempdir().unwrap();
    success(reader.path(), &["init", "--bare", "."]).await;
    success(
        reader.path(),
        &["-c", "protocol.version=2", "fetch", &url, branch],
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

    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/contents"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": second,
                "path_hex": "646f63732f62726f777365722e747874",
                "content": "created without a checkout\n",
                "message": "Create browser file"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let third = created["commit"].as_str().unwrap().to_owned();
    assert_eq!(created["branch"], format!("refs/heads/{branch}"));
    let stale = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/contents"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": second,
                "path_hex": "7374616c652e747874",
                "content": "stale\n",
                "message": "Stale commit"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let browser_path = "646f63732f62726f777365722e747874";
    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/search?rev={third}&q=browser&limit=10"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let search: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(search["items"].as_array().unwrap().len(), 1);
    assert_eq!(search["items"][0]["path_hex"], browser_path);
    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/file?rev={third}&path_hex={browser_path}"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let file: serde_json::Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let created_blob = file["oid"].as_str().unwrap();
    let response = reqwest::Client::new()
        .patch(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/contents"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": third,
                "expected_blob": created_blob,
                "path_hex": browser_path,
                "content": "edited without a checkout\n",
                "message": "Edit browser file"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let edited: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let fourth = edited["commit"].as_str().unwrap().to_owned();
    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/file?rev={fourth}&path_hex={browser_path}"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let file: serde_json::Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(file["text"], "edited without a checkout\n");
    let edited_blob = file["oid"].as_str().unwrap();
    let reader = tempfile::tempdir().unwrap();
    success(reader.path(), &["init", "--bare", "."]).await;
    success(
        reader.path(),
        &["-c", "protocol.version=2", "fetch", &url, branch],
    )
    .await;
    assert_eq!(
        success(reader.path(), &["show", "FETCH_HEAD:docs/browser.txt"]).await,
        "edited without a checkout"
    );
    let stale_file = reqwest::Client::new()
        .delete(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/contents"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": fourth,
                "expected_blob": created_blob,
                "path_hex": browser_path,
                "message": "Delete stale browser file"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(stale_file.status(), StatusCode::CONFLICT);
    let response = reqwest::Client::new()
        .delete(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/contents"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": fourth,
                "expected_blob": edited_blob,
                "path_hex": browser_path,
                "message": "Delete browser file"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let deleted: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let fifth = deleted["commit"].as_str().unwrap().to_owned();
    let response = reqwest::get(format!(
        "http://127.0.0.1:{port}/api/repos/team/repo/search?rev={fifth}&q=browser&limit=10"
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let search: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert!(search["items"].as_array().unwrap().is_empty());
    success(
        reader.path(),
        &["-c", "protocol.version=2", "fetch", &url, branch],
    )
    .await;
    for path in ["FETCH_HEAD:docs/browser.txt", "FETCH_HEAD:docs"] {
        assert!(
            !git(reader.path(), &["cat-file", "-e", path])
                .await
                .status
                .success(),
            "deleted browser path remained visible"
        );
    }

    let binary = [0_u8, 255, 10, 0, 128, 1, 2];
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/uploads"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": fifth,
                "files": [
                    {
                        "path_hex": "6173736574732f7261772e62696e",
                        "content_base64": base64::engine::general_purpose::STANDARD.encode(binary)
                    },
                    {
                        "path_hex": "646f63732f75706c6f61642e747874",
                        "content_base64": base64::engine::general_purpose::STANDARD.encode("uploaded without a checkout\n")
                    }
                ],
                "message": "Upload browser files"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let uploaded: serde_json::Value =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let sixth = uploaded["commit"].as_str().unwrap().to_owned();
    assert_eq!(uploaded["paths_hex"].as_array().unwrap().len(), 2);
    success(
        reader.path(),
        &["-c", "protocol.version=2", "fetch", &url, branch],
    )
    .await;
    let raw = git(reader.path(), &["show", "FETCH_HEAD:assets/raw.bin"]).await;
    assert!(raw.status.success());
    assert_eq!(raw.stdout, binary);
    assert_eq!(
        success(reader.path(), &["show", "FETCH_HEAD:docs/upload.txt"]).await,
        "uploaded without a checkout"
    );

    let collision = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/uploads"
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "branch": format!("refs/heads/{branch}"),
                "expected_head": sixth,
                "files": [
                    {"path_hex": "524541444d452e6d64", "content_base64": "Y29sbGlkZQ=="},
                    {"path_hex": "61746f6d69632e747874", "content_base64": "bmV2ZXI="}
                ],
                "message": "Reject an atomic collision"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(collision.status(), StatusCode::CONFLICT);
    let advertised = success(path, &["ls-remote", &url, branch]).await;
    assert_eq!(advertised, format!("{sixth}\trefs/heads/{branch}"));
    assert!(
        !git(reader.path(), &["cat-file", "-e", "FETCH_HEAD:atomic.txt"])
            .await
            .status
            .success()
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
        BTreeMap::from([(format!("refs/heads/{branch}"), sixth)])
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
