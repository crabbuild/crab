use super::*;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::lfs::{Error as LfsHttpError, parse_byte_range, requested_range};

const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const BATCH: &str = "/git/team/repo.git/info/lfs/objects/batch";

async fn request(
    server: &Arc<Server>,
    method: &str,
    path: &str,
    body: Body,
) -> axum::response::Response {
    request_with_headers(server, method, path, body, &[]).await
}

async fn request_with_headers(
    server: &Arc<Server>,
    method: &str,
    path: &str,
    body: Body,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:8788")
        .header(
            "content-type",
            "application/vnd.git-lfs+json; charset=utf-8",
        );
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    router(Arc::clone(server))
        .oneshot(request.body(body).unwrap())
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
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_action_urls_preserve_the_validated_loopback_authority() {
    let server = maintenance_tests::fixture().await;
    let response = router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(BATCH)
                .header("host", "127.0.0.1:18791")
                .header("content-type", "application/vnd.git-lfs+json")
                .body(Body::from(
                    json!({"operation":"upload","objects":[{"oid":HELLO,"size":5}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let response = value(response).await;
    assert_eq!(
        response["objects"][0]["actions"]["upload"]["href"],
        format!("http://127.0.0.1:18791/git/team/repo.git/info/lfs/objects/{HELLO}?size=5")
    );
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_lock_lifecycle_is_paginated_partitioned_and_retry_safe() {
    let server = maintenance_tests::fixture().await;
    let locks = "/git/team/repo.git/info/lfs/locks";
    let created = request(
        &server,
        "POST",
        locks,
        Body::from(json!({"path":"models/mine.bin"}).to_string()),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(
        created.headers()["content-type"],
        "application/vnd.git-lfs+json"
    );
    let created = value(created).await;
    let id = created["lock"]["id"].as_str().unwrap();
    time::OffsetDateTime::parse(
        created["lock"]["locked_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(
        (&created["lock"]["path"], &created["lock"]["owner"]["name"]),
        (&json!("models/mine.bin"), &json!("operator"))
    );

    let duplicate = value(
        request(
            &server,
            "POST",
            locks,
            Body::from(json!({"path":"models/mine.bin"}).to_string()),
        )
        .await,
    )
    .await;
    assert_eq!(duplicate["lock"]["id"], id);

    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let manager = crab_lfs::LfsLockManager::lfs(repo.store.clone(), &repo.config.prefix);
    let other = manager
        .lock("models/theirs.bin", "another-subject")
        .await
        .unwrap();
    let conflict = request(
        &server,
        "POST",
        locks,
        Body::from(json!({"path":"models/theirs.bin"}).to_string()),
    )
    .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(value(conflict).await["lock"]["id"], other.id);

    let first =
        value(request(&server, "GET", &format!("{locks}?limit=1"), Body::empty()).await).await;
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = value(
        request(
            &server,
            "GET",
            &format!("{locks}?limit=1&cursor={cursor}"),
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            first["locks"].as_array().unwrap().len(),
            second["locks"].as_array().unwrap().len(),
            first["locks"][0]["id"] != second["locks"][0]["id"],
            second.get("next_cursor").is_none(),
        ),
        (1, 1, true, true)
    );

    let verified = value(
        request(
            &server,
            "POST",
            &format!("{locks}/verify"),
            Body::from("{}"),
        )
        .await,
    )
    .await;
    assert_eq!(
        (&verified["ours"][0]["id"], &verified["theirs"][0]["id"],),
        (&json!(id), &json!(other.id))
    );

    let denied = request(
        &server,
        "POST",
        &format!("{locks}/{}/unlock", other.id),
        Body::from("{}"),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let forced = request(
        &server,
        "POST",
        &format!("{locks}/{}/unlock", other.id),
        Body::from(json!({"force":true}).to_string()),
    )
    .await;
    assert_eq!(forced.status(), StatusCode::OK);

    let unlock_path = format!("{locks}/{id}/unlock");
    let first_unlock = request(&server, "POST", &unlock_path, Body::from("{}")).await;
    let retry_unlock = request(&server, "POST", &unlock_path, Body::from("{}")).await;
    assert_eq!(
        (
            first_unlock.status(),
            value(first_unlock).await["lock"]["id"].clone(),
            retry_unlock.status(),
            value(retry_unlock).await["lock"]["id"].clone(),
        ),
        (StatusCode::OK, json!(id), StatusCode::OK, json!(id),)
    );
    assert!(
        value(request(&server, "GET", locks, Body::empty()).await).await["locks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_lock_mutations_share_the_receive_publication_guard() {
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let lease = crab_coordination::PushLock::acquire_internal(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        crab_coordination::LFS_LOCKS_RESOURCE,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let locks = "/git/team/repo.git/info/lfs/locks";
    let blocked = request(
        &server,
        "POST",
        locks,
        Body::from(json!({"path":"models/guarded.bin"}).to_string()),
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::SERVICE_UNAVAILABLE);
    lease.release().await.unwrap();
    let created = request(
        &server,
        "POST",
        locks,
        Body::from(json!({"path":"models/guarded.bin"}).to_string()),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_lock_inputs_are_bounded() {
    let server = maintenance_tests::fixture().await;
    let locks = "/git/team/repo.git/info/lfs/locks";
    let invalid_path = request(
        &server,
        "POST",
        locks,
        Body::from(json!({"path":"/absolute"}).to_string()),
    )
    .await;
    let invalid_limit = request(&server, "GET", &format!("{locks}?limit=0"), Body::empty()).await;
    let missing = request(
        &server,
        "POST",
        &format!("{locks}/missing/unlock"),
        Body::from("{}"),
    )
    .await;
    assert_eq!(
        (
            invalid_path.status(),
            invalid_limit.status(),
            missing.status(),
        ),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::NOT_FOUND,
        )
    );
    server.runtime.shutdown().await;
}

#[test]
fn lfs_byte_ranges_are_single_bounded_and_validator_aware() {
    for (value, expected) in [
        ("bytes=1-3", 1..4),
        ("bytes=2-", 2..5),
        ("bytes=-2", 3..5),
        ("ByTeS=0-99", 0..5),
    ] {
        assert_eq!(parse_byte_range(value, 5).unwrap(), expected, "{value}");
    }
    for value in [
        "bytes=5-",
        "bytes=4-3",
        "bytes=-0",
        "bytes=0-0,2-2",
        "items=0-1",
        "bytes=18446744073709551616-",
    ] {
        assert!(
            matches!(
                parse_byte_range(value, 5),
                Err(LfsHttpError::RangeNotSatisfiable { size: 5 })
            ),
            "{value}"
        );
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-"));
    headers.insert(header::IF_RANGE, HeaderValue::from_static("\"current\""));
    assert_eq!(
        (
            requested_range(&headers, 5, "\"current\"").unwrap(),
            requested_range(&headers, 5, "\"stale\"").unwrap(),
        ),
        (Some(2..5), None)
    );
    headers.append(header::RANGE, HeaderValue::from_static("bytes=3-"));
    assert_eq!(requested_range(&headers, 5, "\"current\"").unwrap(), None);
    headers.clear();
    headers.insert(header::RANGE, HeaderValue::from_static("items=0-1"));
    assert_eq!(requested_range(&headers, 5, "\"current\"").unwrap(), None);
}

#[tokio::test]
async fn lfs_range_response_is_resumable_and_rejects_unsatisfiable_ranges() {
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let oid: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(b"hello").into();
    crab_lfs::LfsObjectStore::new(repo.store.clone(), &repo.config.prefix)
        .put(&oid, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let path = format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5");

    let response = request_with_headers(
        &server,
        "GET",
        &path,
        Body::empty(),
        &[("range", "bytes=2-")],
    )
    .await;
    let partial = (
        response.status(),
        response.headers()["accept-ranges"].clone(),
        response.headers()["content-range"].clone(),
        response.headers()["content-length"].clone(),
        response.headers()["etag"].clone(),
        response.into_body().collect().await.unwrap().to_bytes(),
    );
    let rejected = request_with_headers(
        &server,
        "GET",
        &path,
        Body::empty(),
        &[("range", "bytes=5-")],
    )
    .await;
    let rejected = (
        rejected.status(),
        rejected.headers()["accept-ranges"].clone(),
        rejected.headers()["content-range"].clone(),
    );
    assert_eq!(
        (partial, rejected),
        (
            (
                StatusCode::PARTIAL_CONTENT,
                HeaderValue::from_static("bytes"),
                HeaderValue::from_static("bytes 2-4/5"),
                HeaderValue::from_static("3"),
                HeaderValue::from_str(&format!("\"{HELLO}\"")).unwrap(),
                Bytes::from_static(b"llo"),
            ),
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                HeaderValue::from_static("bytes"),
                HeaderValue::from_static("bytes */5"),
            ),
        )
    );
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_ignores_multi_range_and_unknown_range_units() {
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let oid: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(b"hello").into();
    crab_lfs::LfsObjectStore::new(repo.store.clone(), &repo.config.prefix)
        .put(&oid, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let path = format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5");

    for range in ["bytes=0-0,2-2", "items=0-1"] {
        let response =
            request_with_headers(&server, "GET", &path, Body::empty(), &[("range", range)]).await;
        assert_eq!(
            (
                response.status(),
                response.headers().get(header::CONTENT_RANGE).cloned(),
                response.into_body().collect().await.unwrap().to_bytes(),
            ),
            (StatusCode::OK, None, Bytes::from_static(b"hello")),
            "{range}"
        );
    }
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_head_ignores_range_and_describes_the_complete_object() {
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let oid: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(b"hello").into();
    crab_lfs::LfsObjectStore::new(repo.store.clone(), &repo.config.prefix)
        .put(&oid, Bytes::from_static(b"hello"))
        .await
        .unwrap();

    let response = request_with_headers(
        &server,
        "HEAD",
        &format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5"),
        Body::empty(),
        &[("range", "bytes=2-")],
    )
    .await;
    assert_eq!(
        (
            response.status(),
            response.headers().get(header::CONTENT_RANGE).cloned(),
            response.headers()[header::ACCEPT_RANGES].clone(),
            response.headers()[header::CONTENT_LENGTH].clone(),
            response.headers()[header::ETAG].clone(),
            response.into_body().collect().await.unwrap().to_bytes(),
        ),
        (
            StatusCode::OK,
            None,
            HeaderValue::from_static("bytes"),
            HeaderValue::from_static("5"),
            HeaderValue::from_str(&format!("\"{HELLO}\"")).unwrap(),
            Bytes::new(),
        )
    );
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_range_verifies_the_complete_object_before_partial_delivery() {
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let oid = crab_git::lfs_pointer::LfsPointer::parse(
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{HELLO}\nsize 5\n")
            .as_bytes(),
    )
    .unwrap()
    .oid;
    let object_path = crab_lfs::LfsObjectStore::object_path_for_prefix(&repo.config.prefix, &oid);
    repo.store
        .put(&object_path, Bytes::from_static(b"wrong"))
        .await
        .unwrap();

    let response = request_with_headers(
        &server,
        "GET",
        &format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5"),
        Body::empty(),
        &[("range", "bytes=0-1")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn lfs_download_does_not_publish_verification_receipts() {
    use futures_util::TryStreamExt;
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let oid = crab_git::lfs_pointer::LfsPointer::parse(
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{HELLO}\nsize 5\n")
            .as_bytes(),
    )
    .unwrap()
    .oid;
    let path = crab_lfs::LfsObjectStore::object_path_for_prefix(&repo.config.prefix, &oid);
    repo.store
        .put(&path, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let before: Vec<_> = repo.store.inner().list(None).try_collect().await.unwrap();

    let response = request(
        &server,
        "GET",
        &format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5"),
        Body::empty(),
    )
    .await;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let after: Vec<_> = repo.store.inner().list(None).try_collect().await.unwrap();
    server.runtime.shutdown().await;
    assert_eq!((bytes, after), (Bytes::from_static(b"hello"), before));
}

#[tokio::test]
async fn lfs_corrupt_download_fails_its_body_and_releases_admission() {
    let server = maintenance_tests::fixture().await;
    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let oid = crab_git::lfs_pointer::LfsPointer::parse(
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{HELLO}\nsize 5\n")
            .as_bytes(),
    )
    .unwrap()
    .oid;
    let path = crab_lfs::LfsObjectStore::object_path_for_prefix(&repo.config.prefix, &oid);
    repo.store
        .put(&path, Bytes::from_static(b"wrong"))
        .await
        .unwrap();

    let response = request(
        &server,
        "GET",
        &format!("/git/team/repo.git/info/lfs/objects/{HELLO}?size=5"),
        Body::empty(),
    )
    .await;
    let result = response.into_body().collect().await;
    server.runtime.shutdown().await;
    assert_eq!(
        (result.is_err(), server.git_admission.available_permits()),
        (true, 4)
    );
}

#[tokio::test]
async fn archived_repository_rejects_lfs_writes_and_keeps_reads_available() {
    let server = maintenance_tests::fixture().await;
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    repository_settings::replace_lifecycle(&repository, 0, true)
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
    let server = maintenance_tests::fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
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

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_lfs_response_fails_http_body_and_releases_capacity() {
    for cancel in [false, true] {
        use futures_util::StreamExt;

        let server = maintenance_tests::fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let repo = server
            .repositories
            .get(&("team".into(), "repo".into()))
            .unwrap();
        let oid: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(b"hello").into();
        crab_lfs::LfsObjectStore::new(repo.store.clone(), &repo.config.prefix)
            .put(&oid, bytes::Bytes::from_static(b"hello"))
            .await
            .unwrap();

        let gate = Arc::new(tokio::sync::Notify::new());
        let body_gate = Arc::clone(&gate);
        let app = router(Arc::clone(&server)).layer(axum::middleware::from_fn(
            move |request: Request, next: axum::middleware::Next| {
                let gate = Arc::clone(&body_gate);
                async move {
                    let (parts, body) = next.run(request).await.into_parts();
                    // Keep the real handler's body unpolled until headers reach the
                    // client, making cancellation timing independent of socket speed.
                    let delayed = futures_util::stream::once(async move {
                        gate.notified().await;
                        body
                    })
                    .flat_map(Body::into_data_stream);
                    axum::response::Response::from_parts(parts, Body::from_stream(delayed))
                }
            },
        ));
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let http = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder()
            .http1_only()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .get(format!(
                "http://127.0.0.1:{port}/git/team/repo.git/info/lfs/objects/{HELLO}?size=5"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(server.git_admission.available_permits(), 3);
        if cancel {
            server.cancellation.cancel();
        }
        gate.notify_one();
        let result = response.bytes().await;
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), http)
            .await
            .unwrap()
            .unwrap();
        server.runtime.shutdown().await;
        if cancel {
            assert!(
                matches!(result, Err(ref error) if !error.is_timeout()),
                "cancelled body must fail without relying on the client timeout"
            );
        } else {
            assert_eq!(&result.unwrap()[..], b"hello");
        }
        assert_eq!(server.git_admission.available_permits(), 4);
    }
}
