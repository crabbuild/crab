use super::*;
use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const BATCH: &str = "/git/team/repo.git/info/lfs/objects/batch";

async fn request(
    server: &Arc<Server>,
    method: &str,
    path: &str,
    body: Body,
) -> axum::response::Response {
    router(Arc::clone(server))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost:8788")
                .header(
                    "content-type",
                    "application/vnd.git-lfs+json; charset=utf-8",
                )
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn value(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn lfs_batch_upload_download_is_verified_and_idempotent() {
    let server = maintenance_tests::fixture().await;
    let batch = |operation| {
        Body::from(json!({"operation":operation,"objects":[{"oid":HELLO,"size":5}]}).to_string())
    };
    let response = request(&server, "POST", BATCH, batch("upload")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.git-lfs+json"
    );
    let object = value(response).await;
    let target = url::Url::parse(
        object["objects"][0]["actions"]["upload"]["href"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let path = format!("{}?{}", target.path(), target.query().unwrap());
    let invalid = request(&server, "PUT", &path, Body::from("wrong")).await;
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let response = request(&server, "POST", BATCH, batch("download")).await;
    assert_eq!(value(response).await["objects"][0]["error"]["code"], 404);
    for body in ["tiny", "too many bytes"] {
        assert_eq!(
            request(&server, "PUT", &path, Body::from(body))
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        request(&server, "PUT", &path, Body::from("hello"))
            .await
            .status(),
        StatusCode::OK
    );
    let response = request(&server, "POST", BATCH, batch("upload")).await;
    assert!(value(response).await["objects"][0].get("actions").is_none());
    let response = request(&server, "GET", &path, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "5");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "hello"
    );
    assert_eq!(server.git_admission.available_permits(), 4);
    assert_eq!(
        request(
            &server,
            "POST",
            "/git/team/repo.git/info/lfs/locks/verify",
            Body::empty()
        )
        .await
        .status(),
        StatusCode::NOT_IMPLEMENTED
    );
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn archived_repository_rejects_lfs_writes_and_keeps_reads_available() {
    let server = maintenance_tests::fixture().await;
    let repository = &server.repositories[&("team".into(), "repo".into())];
    repository_settings::replace_lifecycle(repository, 0, true)
        .await
        .unwrap();
    let batch = |operation| {
        Body::from(json!({"operation":operation,"objects":[{"oid":HELLO,"size":5}]}).to_string())
    };

    assert_eq!(
        request(&server, "POST", BATCH, batch("upload"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&server, "POST", BATCH, batch("download"))
            .await
            .status(),
        StatusCode::OK
    );
    let path = format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5");
    assert_eq!(
        request(&server, "PUT", &path, Body::from("hello"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_rejects_invalid_batches_and_releases_disconnected_uploads() {
    let server = maintenance_tests::fixture().await;
    for body in [
        json!({"operation":"upload","hash_algo":"sha1","objects":[]}),
        json!({"operation":"upload","transfers":["tus"],"objects":[]}),
        json!({"operation":"upload","objects":[{"oid":"invalid","size":5}]}),
        json!({"operation":"upload","objects":[{"oid":HELLO,"size":2147483649_u64}]}),
    ] {
        assert!(
            request(&server, "POST", BATCH, Body::from(body.to_string()))
                .await
                .status()
                .is_client_error()
        );
    }
    let path = format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5");
    let client_server = Arc::clone(&server);
    let client = tokio::spawn(async move {
        request(
            &client_server,
            "PUT",
            &path,
            Body::from_stream(futures_util::stream::pending::<
                std::result::Result<axum::body::Bytes, std::io::Error>,
            >()),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.receives.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    client.abort();
    let _ = client.await;
    server.receives.close();
    tokio::time::timeout(Duration::from_secs(2), server.receives.wait())
        .await
        .unwrap();
    assert_eq!(server.git_admission.available_permits(), 4);
    server.runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_git_lfs_push_and_clone_transfer_exact_large_file() {
    use receive_tests::success;
    let mut server = maintenance_tests::fixture().await;
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
    let source = directory.path().join("source");
    std::fs::create_dir(&source).unwrap();
    success(&source, &["init", "--initial-branch=main", "."]).await;
    success(&source, &["lfs", "install", "--local"]).await;
    success(&source, &["lfs", "track", "*.bin"]).await;
    let content = vec![b'x'; 10 * 1024 * 1024];
    std::fs::write(source.join("asset.bin"), &content).unwrap();
    success(&source, &["add", "."]).await;
    success(&source, &["commit", "-m", "LFS fixture"]).await;
    success(
        &source,
        &[
            "remote",
            "add",
            "origin",
            &format!("http://127.0.0.1:{port}/git/team/repo.git"),
        ],
    )
    .await;
    success(&source, &["push", "origin", "main"]).await;
    let destination = directory.path().join("client");
    success(
        directory.path(),
        &[
            "-c",
            "filter.lfs.process=git-lfs filter-process",
            "-c",
            "filter.lfs.required=true",
            "clone",
            &format!("http://127.0.0.1:{port}/git/team/repo.git"),
            destination.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(
        std::fs::read(destination.join("asset.bin")).unwrap(),
        content
    );
    stop.cancel();
    http.await.unwrap();
    server.receives.close();
    server.receives.wait().await;
    server.finish_maintenance().await.unwrap();
    server.runtime.shutdown().await;
}
