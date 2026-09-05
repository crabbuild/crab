use super::*;

const ROOT: &str = "/api/repos/team/private/branches";

async fn create_branch(h: &Harness, cookie: &str, csrf: &str, body: Value) -> (StatusCode, Value) {
    let response = h
        .http
        .post(format!("{}{ROOT}", h.origin))
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

#[tokio::test(flavor = "multi_thread")]
async fn browser_branch_creation_publishes_an_existing_commit_for_native_git() {
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
    let session = h.json("/api/session", &alice).await;
    let csrf = session["csrf"].as_str().unwrap();
    let token = h
        .http
        .post(format!("{}/api/git-token", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"owner":"team","repository":"private","access":"write"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(token.status(), StatusCode::OK);
    let token: Value = serde_json::from_slice(&token.bytes().await.unwrap()).unwrap();
    let mut git_url = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url
        .set_password(Some(token["token"].as_str().unwrap()))
        .unwrap();
    let source = tempfile::tempdir().unwrap();
    crate::server::receive_tests::success(
        source.path(),
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(source.path().join("README.md"), "browser branch\n").unwrap();
    crate::server::receive_tests::success(source.path(), &["add", "README.md"]).await;
    crate::server::receive_tests::success(source.path(), &["commit", "-m", "base"]).await;
    let commit = crate::server::receive_tests::success(source.path(), &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(source.path(), &["push", git_url.as_str(), "main"]).await;

    let protected =
        create_branch(&h, &alice, csrf, json!({"name":"main","source_oid":commit})).await;
    assert_eq!(protected.0, StatusCode::FORBIDDEN);
    assert_eq!(protected.1["error"]["code"], "protected_branch");

    let created = create_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"feature/browser","source_oid":commit}),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    assert_eq!(created.1["branch"], "refs/heads/feature/browser");
    assert_eq!(created.1["commit"], commit);
    let refs = h.json("/api/repos/team/private/refs", &alice).await;
    assert!(refs["refs"].as_array().unwrap().iter().any(|reference| {
        reference["name"] == "refs/heads/feature/browser" && reference["oid"] == commit
    }));
    let advertised = crate::server::receive_tests::success(
        source.path(),
        &["ls-remote", git_url.as_str(), "refs/heads/feature/browser"],
    )
    .await;
    assert_eq!(advertised, format!("{commit}\trefs/heads/feature/browser"));

    let proposed = h
        .http
        .post(format!("{}/api/repos/team/private/contents", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            json!({
                "branch":"refs/heads/main",
                "expected_head":commit,
                "new_branch":"feature/proposed-edit",
                "path_hex":"50524f504f53414c2e6d64",
                "content":"proposed from protected main\n",
                "message":"Propose browser edit"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(proposed.status(), StatusCode::CREATED);
    let proposed: Value = serde_json::from_slice(&proposed.bytes().await.unwrap()).unwrap();
    assert_eq!(proposed["branch"], "refs/heads/feature/proposed-edit");
    let proposal_commit = proposed["commit"].as_str().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    crate::server::receive_tests::success(checkout.path(), &["init", "--bare", "."]).await;
    crate::server::receive_tests::success(
        checkout.path(),
        &[
            "-c",
            "protocol.version=2",
            "fetch",
            git_url.as_str(),
            "feature/proposed-edit",
        ],
    )
    .await;
    assert_eq!(
        crate::server::receive_tests::success(checkout.path(), &["rev-parse", "FETCH_HEAD"]).await,
        proposal_commit
    );
    assert_eq!(
        crate::server::receive_tests::success(checkout.path(), &["show", "FETCH_HEAD:PROPOSAL.md"])
            .await,
        "proposed from protected main"
    );
    assert_eq!(
        crate::server::receive_tests::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/heads/main"]
        )
        .await,
        format!("{commit}\trefs/heads/main")
    );

    let duplicate = create_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"feature/browser","source_oid":commit}),
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["error"]["code"], "branch_exists");
    h.close().await;
}

#[tokio::test]
async fn branch_creation_rejects_invalid_inputs_and_unauthorized_members() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let session = h.json("/api/session", &alice).await;
    let csrf = session["csrf"].as_str().unwrap();
    let commit = "a".repeat(40);
    for body in [
        json!({"name":"refs/heads/other","source_oid":&commit}),
        json!({"name":"bad name","source_oid":&commit}),
        json!({"name":"other","source_oid":"not-an-oid"}),
    ] {
        let invalid = create_branch(&h, &alice, csrf, body).await;
        assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
        assert_eq!(invalid.1["error"]["code"], "invalid_request");
    }
    let incomplete = create_branch(&h, &alice, csrf, json!({})).await;
    assert_eq!(incomplete.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(incomplete.1["error"]["code"], "invalid_request");
    let rejected_csrf = h
        .http
        .post(format!("{}{ROOT}", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"name":"csrf","source_oid":&commit}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected_csrf.status(), StatusCode::FORBIDDEN);

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let bob_session = h.json("/api/session", &bob).await;
    let denied = create_branch(
        &h,
        &bob,
        bob_session["csrf"].as_str().unwrap(),
        json!({"name":"bob","source_oid":&commit}),
    )
    .await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert_eq!(denied.1["error"]["code"], "forbidden");

    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let outsider_session = h.json("/api/session", &outsider).await;
    let hidden = create_branch(
        &h,
        &outsider,
        outsider_session["csrf"].as_str().unwrap(),
        json!({"name":"outsider","source_oid":&commit}),
    )
    .await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden.1["error"]["code"], "repository_not_found");
    h.close().await;
}
