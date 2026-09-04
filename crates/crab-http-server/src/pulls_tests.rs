use super::*;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn json_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = client
        .request(method, url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, value)
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_requests_follow_live_branches_and_persist_discussion_state() {
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
    let source = tempfile::tempdir().unwrap();
    let source_path = source.path();
    let git_url = format!("http://127.0.0.1:{port}/git/team/repo.git");
    receive_tests::success(
        source_path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(source_path.join("README.md"), "base\n").unwrap();
    receive_tests::success(source_path, &["add", "README.md"]).await;
    receive_tests::success(source_path, &["commit", "-m", "base"]).await;
    receive_tests::success(source_path, &["push", &git_url, "main"]).await;
    let feature_source = tempfile::tempdir().unwrap();
    receive_tests::success(
        feature_source.path(),
        &["clone", "--depth=1", "--branch", "main", &git_url, "."],
    )
    .await;
    let path = feature_source.path();
    assert!(path.join(".git/shallow").is_file());
    receive_tests::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("README.md"), "base\nfeature one\n").unwrap();
    receive_tests::success(path, &["commit", "-am", "feature one"]).await;
    let first_head = receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    receive_tests::success(path, &["push", &git_url, "feature"]).await;

    let client = reqwest::Client::new();
    let root = format!("http://127.0.0.1:{port}/api/repos/team/repo/pulls");
    let input = json!({
        "request_id":"00000000-0000-4000-8000-000000000001",
        "title":"Improve the README",
        "body":"Please review **this change**.",
        "base_ref":"refs/heads/main",
        "head_ref":"refs/heads/feature"
    });
    let (status, pull) = json_request(&client, reqwest::Method::POST, &root, input.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(pull["number"], 1);
    assert_eq!(pull["head_oid"], first_head);
    assert_eq!(pull["branches_available"], true);
    assert_eq!(pull["can_decide"], false);
    let pull_url = format!("{root}/1");
    let compare = client
        .get(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/changes?rev={}&base={}",
            pull["head_oid"].as_str().unwrap(),
            pull["base_oid"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(compare.status(), StatusCode::OK);
    let compare: Value = serde_json::from_slice(&compare.bytes().await.unwrap()).unwrap();
    assert_eq!(compare["changes"][0]["path"], "README.md");

    let comment = json!({
        "request_id":"00000000-0000-4000-8000-000000000002",
        "body":"The exact diff looks good."
    });
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::POST,
            &format!("{pull_url}/comments"),
            comment.clone(),
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::POST,
            &format!("{pull_url}/comments"),
            comment,
        )
        .await
        .1["number"],
        1
    );

    let review = json!({
        "request_id":"00000000-0000-4000-8000-000000000003",
        "body":"I left notes without deciding on my own change.",
        "state":"commented"
    });
    let review_url = format!("{pull_url}/reviews");
    let created_review =
        json_request(&client, reqwest::Method::POST, &review_url, review.clone()).await;
    assert_eq!(created_review.0, StatusCode::CREATED);
    assert_eq!(created_review.1["commit_oid"], first_head);
    assert_eq!(created_review.1["current"], true);
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::POST,
            &review_url,
            json!({
                "request_id":"00000000-0000-4000-8000-000000000004",
                "body":"Self approval is forbidden.",
                "state":"approved"
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    std::fs::write(path.join("README.md"), "base\nfeature one\nfeature two\n").unwrap();
    receive_tests::success(path, &["commit", "-am", "feature two"]).await;
    let second_head = receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    receive_tests::success(path, &["push", &git_url, "feature"]).await;
    let replay = json_request(&client, reqwest::Method::POST, &root, input).await;
    assert_eq!(replay.0, StatusCode::CREATED);
    assert_eq!(replay.1["number"], 1);
    assert_eq!(replay.1["original_head_oid"], first_head);
    assert_eq!(replay.1["head_oid"], second_head);
    let reviews: Value = serde_json::from_slice(
        &client
            .get(&review_url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reviews["items"][0]["commit_oid"], first_head);
    assert_eq!(reviews["items"][0]["current"], false);

    assert_eq!(
        json_request(
            &client,
            reqwest::Method::PATCH,
            &pull_url,
            json!({"version":0,"state":"closed"}),
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let closed = json_request(
        &client,
        reqwest::Method::PATCH,
        &pull_url,
        json!({"version":1,"state":"closed"}),
    )
    .await;
    assert_eq!(closed.0, StatusCode::OK);
    assert_eq!(closed.1["state"], "closed");
    let recovered_review = json_request(&client, reqwest::Method::POST, &review_url, review).await;
    assert_eq!(recovered_review.0, StatusCode::CREATED);
    assert_eq!(recovered_review.1["number"], 1);
    assert_eq!(recovered_review.1["current"], false);
    let closed_list: Value = serde_json::from_slice(
        &client
            .get(format!("{root}?state=closed"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(closed_list["items"][0]["number"], 1);
    let searched: Value = serde_json::from_slice(
        &client
            .get(format!("{root}?state=closed&q=PLEASE%20REVIEW"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(searched["items"][0]["number"], 1);

    receive_tests::success(path, &["checkout", "main"]).await;
    receive_tests::success(path, &["push", &git_url, ":refs/heads/feature"]).await;
    feature_source.close().unwrap();
    source.close().unwrap();
    let detail: Value = serde_json::from_slice(
        &client
            .get(&pull_url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(detail["branches_available"], false);
    assert_eq!(detail["original_head_oid"], first_head);

    let repo = &server.repositories[&("team".into(), "repo".into())];
    assert!(
        repo.store
            .list_prefix(&repo.layout.repo_path("app/v1/pulls"))
            .await
            .unwrap()
            .iter()
            .any(|object| object.location.as_ref().ends_with("pull.json"))
    );
    server.cancellation.cancel();
    stop.cancel();
    http.await.unwrap();
    server.receives.close();
    server.receives.wait().await;
    server.finish_maintenance().await.unwrap();
    server.runtime.shutdown().await;
}

#[tokio::test]
async fn pull_creation_rejects_invalid_branch_pairs_before_writing_app_state() {
    let server = maintenance_tests::fixture().await;
    let repo = &server.repositories[&("team".into(), "repo".into())];
    for (base, head) in [
        ("refs/heads/main", "refs/heads/main"),
        ("main", "refs/heads/feature"),
        ("refs/heads/main", "refs/tags/v1"),
    ] {
        let response = router(Arc::clone(&server))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/repos/team/repo/pulls")
                    .header("host", "localhost:8788")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({
                            "request_id":"00000000-0000-4000-8000-000000000001",
                            "title":"Title",
                            "body":"",
                            "base_ref":base,
                            "head_ref":head,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(
        repo.store
            .list_prefix(&repo.layout.repo_path("app/v1/pulls"))
            .await
            .unwrap()
            .is_empty()
    );
    server.cancellation.cancel();
    server.runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_request_fast_forward_merge_uses_canonical_ref_publication() {
    let mut server = maintenance_tests::fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mutable = Arc::get_mut(&mut server).unwrap();
    mutable.port = port;
    mutable
        .repositories
        .get_mut(&("team".into(), "repo".into()))
        .unwrap()
        .config
        .protected_branches = vec![crate::BranchProtection {
        branch: "main".into(),
        required_approvals: 0,
        required_checks: vec![],
    }];
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let app = router(Arc::clone(&server));
    let http = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
    });
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    let git_url = format!("http://127.0.0.1:{port}/git/team/repo.git");
    receive_tests::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    receive_tests::success(path, &["add", "README.md"]).await;
    receive_tests::success(path, &["commit", "-m", "base"]).await;
    let base = receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    receive_tests::success(path, &["push", &git_url, "main"]).await;
    receive_tests::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("FEATURE.md"), "merged through Crab\n").unwrap();
    receive_tests::success(path, &["add", "FEATURE.md"]).await;
    receive_tests::success(path, &["commit", "-m", "feature"]).await;
    let head = receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    receive_tests::success(path, &["push", &git_url, "feature"]).await;

    let direct = receive_tests::git(
        path,
        &[
            "push",
            "--atomic",
            &git_url,
            "feature:main",
            "feature:protected-batch-side",
        ],
    )
    .await;
    assert!(!direct.status.success());
    assert!(
        String::from_utf8_lossy(&direct.stderr)
            .contains("protected branch requires a pull request")
    );
    let protected_refs = receive_tests::success(
        path,
        &[
            "ls-remote",
            &git_url,
            "refs/heads/main",
            "refs/heads/protected-batch-side",
        ],
    )
    .await;
    assert_eq!(protected_refs, format!("{base}\trefs/heads/main"));

    let catalog: Value = serde_json::from_slice(
        &reqwest::get(format!("http://127.0.0.1:{port}/api/repos"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        catalog["repositories"][0]["protected_branches"][0]["branch"],
        "main"
    );

    let client = reqwest::Client::new();
    let root = format!("http://127.0.0.1:{port}/api/repos/team/repo/pulls");
    let created = json_request(
        &client,
        reqwest::Method::POST,
        &root,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000030",
            "title":"Merge the feature",
            "body":"Use the existing verified commits.",
            "base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature"
        }),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    let merge_input = json!({
        "request_id":"00000000-0000-4000-8000-000000000031",
        "version":created.1["version"],
        "method":"fast_forward",
        "base_oid":created.1["base_oid"],
        "head_oid":created.1["head_oid"]
    });
    let busy = Arc::clone(&server.git_admission)
        .acquire_many_owned(4)
        .await
        .unwrap();
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::POST,
            &format!("{root}/1/merge"),
            merge_input.clone(),
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let pending: Value = serde_json::from_slice(
        &client
            .get(format!("{root}/1"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        pending["merge_pending"]["request_id"],
        "00000000-0000-4000-8000-000000000031"
    );
    assert_eq!(pending["can_manage"], false);
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::PATCH,
            &format!("{root}/1"),
            json!({"version":pending["version"],"state":"closed"}),
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    drop(busy);
    let merged = json_request(
        &client,
        reqwest::Method::POST,
        &format!("{root}/1/merge"),
        merge_input.clone(),
    )
    .await;
    assert_eq!(merged.0, StatusCode::OK);
    assert_eq!(merged.1["state"], "merged");
    assert_eq!(merged.1["base_oid"], base);
    assert_eq!(merged.1["head_oid"], head);
    assert_eq!(merged.1["merge"]["commit_oid"], head);
    assert_eq!(merged.1["can_merge"], false);
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::POST,
            &format!("{root}/1/merge"),
            merge_input,
        )
        .await
        .1["version"],
        merged.1["version"]
    );
    assert_eq!(
        json_request(
            &client,
            reqwest::Method::PATCH,
            &format!("{root}/1"),
            json!({"version":merged.1["version"],"state":"open"}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let refs: Value = serde_json::from_slice(
        &client
            .get(format!("http://127.0.0.1:{port}/api/repos/team/repo/refs"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        refs["refs"].as_array().unwrap().iter().any(|reference| {
            reference["name"] == "refs/heads/main" && reference["oid"] == head
        })
    );
    let compare = client
        .get(format!(
            "http://127.0.0.1:{port}/api/repos/team/repo/changes?rev={head}&base={base}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(compare.status(), StatusCode::OK);
    let compare: Value = serde_json::from_slice(&compare.bytes().await.unwrap()).unwrap();
    assert_eq!(compare["changes"][0]["path"], "FEATURE.md");
    receive_tests::success(path, &["push", &git_url, ":refs/heads/feature"]).await;
    let detail: Value = serde_json::from_slice(
        &client
            .get(format!("{root}/1"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(detail["branches_available"], true);
    assert_eq!(detail["base_oid"], base);
    assert_eq!(detail["head_oid"], head);

    receive_tests::success(path, &["checkout", "-b", "conflict", &base]).await;
    std::fs::write(path.join("CONFLICT.md"), "diverged\n").unwrap();
    receive_tests::success(path, &["add", "CONFLICT.md"]).await;
    receive_tests::success(path, &["commit", "-m", "diverged"]).await;
    receive_tests::success(path, &["push", &git_url, "conflict"]).await;
    let second = json_request(
        &client,
        reqwest::Method::POST,
        &root,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000032",
            "title":"Diverged change",
            "body":"This cannot fast-forward.",
            "base_ref":"refs/heads/main",
            "head_ref":"refs/heads/conflict"
        }),
    )
    .await;
    assert_eq!(second.0, StatusCode::CREATED);
    let conflict = json_request(
        &client,
        reqwest::Method::POST,
        &format!("{root}/2/merge"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000033",
            "version":second.1["version"],
            "method":"fast_forward",
            "base_oid":second.1["base_oid"],
            "head_oid":second.1["head_oid"]
        }),
    )
    .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);

    source.close().unwrap();
    server.cancellation.cancel();
    stop.cancel();
    http.await.unwrap();
    server.receives.close();
    server.receives.wait().await;
    server.finish_maintenance().await.unwrap();
    server.runtime.shutdown().await;
}
