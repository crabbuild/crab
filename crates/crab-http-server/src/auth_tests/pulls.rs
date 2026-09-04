use super::*;

const ROOT: &str = "/api/repos/team/private/pulls";

async fn mutate(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn pull_routes_require_membership_and_csrf_before_repository_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let input = json!({
        "request_id":"00000000-0000-4000-8000-000000000001",
        "title":"Proposed change",
        "body":"Please review",
        "base_ref":"refs/heads/main",
        "head_ref":"refs/heads/feature"
    });
    assert_eq!(
        h.http
            .post(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        let session = h.json("/api/session", &outsider).await;
        let response = h
            .http
            .request(method, format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &outsider)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    h.close().await;
}

#[tokio::test]
async fn pull_review_decisions_require_another_member_and_follow_the_exact_head() {
    let h = Harness::new(false).await;
    let repo = &h.server.repositories[&("team".into(), "private".into())];
    crab_metadata::manifest_store::create_manifest(
        &repo.store,
        &repo.layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();

    let alice = h.login().await;
    let alice_session = h.json("/api/session", &alice).await;
    let alice_csrf = alice_session["csrf"].as_str().unwrap();
    let token = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        "/api/git-token",
        json!({"owner":"team","repository":"private","access":"write"}),
    )
    .await;
    assert_eq!(token.0, StatusCode::OK);
    let mut git_url = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url
        .set_password(Some(token.1["token"].as_str().unwrap()))
        .unwrap();
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    crate::server::receive_tests::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    crate::server::receive_tests::success(path, &["add", "README.md"]).await;
    crate::server::receive_tests::success(path, &["commit", "-m", "base"]).await;
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), "main"]).await;
    crate::server::receive_tests::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("README.md"), "base\nfeature\n").unwrap();
    crate::server::receive_tests::success(path, &["commit", "-am", "feature"]).await;
    let first_head = crate::server::receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), "feature"]).await;
    let created = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        ROOT,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000010",
            "title":"Proposed change",
            "body":"Please review",
            "base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature"
        }),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert_eq!(created.1["can_decide"], false);

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let bob_session = h.json("/api/session", &bob).await;
    let bob_csrf = bob_session["csrf"].as_str().unwrap();
    assert_eq!(h.json(&format!("{ROOT}/1"), &bob).await["can_decide"], true);
    let approved = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000011",
            "body":"",
            "state":"approved"
        }),
    )
    .await;
    assert_eq!(approved.0, StatusCode::CREATED);
    assert_eq!(approved.1["commit_oid"], first_head);
    assert_eq!(approved.1["current"], true);
    let edited = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1/reviews/1"),
        json!({"version":1,"body":"Approved after exact comparison."}),
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK);
    assert_eq!(edited.1["state"], "approved");
    assert_eq!(edited.1["version"], 2);
    assert_eq!(
        mutate(
            &h,
            &alice,
            alice_csrf,
            reqwest::Method::POST,
            &format!("{ROOT}/1/reviews"),
            json!({
                "request_id":"00000000-0000-4000-8000-000000000012",
                "body":"",
                "state":"approved"
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    std::fs::write(path.join("README.md"), "base\nfeature\nupdated\n").unwrap();
    crate::server::receive_tests::success(path, &["commit", "-am", "updated"]).await;
    let second_head = crate::server::receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), "feature"]).await;
    let reviews = h.json(&format!("{ROOT}/1/reviews"), &bob).await;
    assert_eq!(reviews["items"][0]["current"], false);
    let requested = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000013",
            "body":"Please address the remaining concern.",
            "state":"changes_requested"
        }),
    )
    .await;
    assert_eq!(requested.0, StatusCode::CREATED);
    assert_eq!(requested.1["commit_oid"], second_head);
    assert_eq!(requested.1["current"], true);
    source.close().unwrap();
    h.close().await;
}
