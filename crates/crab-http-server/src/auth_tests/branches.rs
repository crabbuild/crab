use super::*;

const ROOT: &str = "/api/repos/team/private/branches";
const DEFAULT_BRANCH: &str = "/api/repos/team/private/settings/default-branch";
const PROTECTIONS: &str = "/api/repos/team/private/settings/branch-protections";

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

async fn delete_branch(h: &Harness, cookie: &str, csrf: &str, body: Value) -> (StatusCode, Value) {
    let response = h
        .http
        .delete(format!("{}{ROOT}", h.origin))
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

async fn set_default_branch(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .patch(format!("{}{DEFAULT_BRANCH}", h.origin))
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

async fn set_branch_protections(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .put(format!("{}{PROTECTIONS}", h.origin))
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

    let policy = create_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"feature/policy","source_oid":commit}),
    )
    .await;
    assert_eq!(policy.0, StatusCode::CREATED, "{}", policy.1);
    let protected = set_branch_protections(
        &h,
        &alice,
        csrf,
        json!({
            "expected_version":0,
            "rules":[
                {"branch":"main","required_approvals":1,"required_checks":["ci/test"]},
                {"branch":"feature/policy","required_approvals":2,"required_checks":["build","security"]}
            ]
        }),
    )
    .await;
    assert_eq!(protected.0, StatusCode::OK, "{}", protected.1);
    assert_eq!(protected.1["version"], 1);
    let catalog = h.json("/api/repos", &alice).await;
    assert_eq!(catalog["repositories"][0]["protection_version"], 1);
    assert_eq!(
        catalog["repositories"][0]["protected_branches"][1]["branch"],
        "feature/policy"
    );
    let persisted = crate::repository_settings::load(repo).await.unwrap();
    assert_eq!(persisted, repo.branch_protections().await.unwrap());

    crate::server::receive_tests::success(source.path(), &["checkout", "-b", "feature/policy"])
        .await;
    std::fs::write(source.path().join("POLICY.md"), "protected by Crab\n").unwrap();
    crate::server::receive_tests::success(source.path(), &["add", "POLICY.md"]).await;
    crate::server::receive_tests::success(source.path(), &["commit", "-m", "policy change"]).await;
    let policy_commit =
        crate::server::receive_tests::success(source.path(), &["rev-parse", "HEAD"]).await;
    let rejected = crate::server::receive_tests::git(
        source.path(),
        &["push", git_url.as_str(), "HEAD:feature/policy"],
    )
    .await;
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("protected branch requires a pull request")
    );
    let stale =
        set_branch_protections(&h, &alice, csrf, json!({"expected_version":0,"rules":[]})).await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["error"]["code"], "settings_changed");
    let unprotected = set_branch_protections(
        &h,
        &alice,
        csrf,
        json!({
            "expected_version":1,
            "rules":[{"branch":"main","required_approvals":1,"required_checks":["ci/test"]}]
        }),
    )
    .await;
    assert_eq!(unprotected.0, StatusCode::OK, "{}", unprotected.1);
    assert_eq!(unprotected.1["version"], 2);
    crate::server::receive_tests::success(
        source.path(),
        &["push", git_url.as_str(), "HEAD:feature/policy"],
    )
    .await;
    assert_eq!(
        crate::server::receive_tests::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/heads/feature/policy"]
        )
        .await,
        format!("{policy_commit}\trefs/heads/feature/policy")
    );

    let external = Repository {
        config: repo.config.clone(),
        store: repo.store.clone(),
        layout: repo.layout.clone(),
        identity: repo.identity.clone(),
        protections: RwLock::new(BranchProtections {
            version: 2,
            rules: repo.config.protected_branches.clone(),
        }),
        pinned: Mutex::new(None),
        maintenance: Mutex::new(None),
    };
    let mut external_rules = repo.config.protected_branches.clone();
    external_rules.push(crate::BranchProtection {
        branch: "feature/policy".into(),
        required_approvals: 1,
        required_checks: vec![],
    });
    let external_policy = crate::repository_settings::replace(&external, 2, external_rules)
        .await
        .unwrap();
    assert_eq!(external_policy.version, 3);
    assert_eq!(repo.protections.read().await.version, 2);

    std::fs::write(source.path().join("POLICY.md"), "changed elsewhere\n").unwrap();
    crate::server::receive_tests::success(source.path(), &["add", "POLICY.md"]).await;
    crate::server::receive_tests::success(source.path(), &["commit", "-m", "external policy"])
        .await;
    let rejected = crate::server::receive_tests::git(
        source.path(),
        &["push", git_url.as_str(), "HEAD:feature/policy"],
    )
    .await;
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("protected branch requires a pull request")
    );
    assert_eq!(repo.protections.read().await.version, 3);
    let catalog = h.json("/api/repos", &alice).await;
    assert_eq!(catalog["repositories"][0]["protection_version"], 3);
    crate::server::receive_tests::success(source.path(), &["checkout", "main"]).await;

    let changed_default = set_default_branch(
        &h,
        &alice,
        csrf,
        json!({
            "name":"feature/browser",
            "expected_head":"refs/heads/main",
            "expected_oid":commit
        }),
    )
    .await;
    assert_eq!(changed_default.0, StatusCode::OK, "{}", changed_default.1);
    assert_eq!(changed_default.1["branch"], "refs/heads/feature/browser");
    let refs = h.json("/api/repos/team/private/refs", &alice).await;
    assert_eq!(refs["head"]["name"], "refs/heads/feature/browser");
    assert_eq!(refs["head"]["oid"], commit);
    let advertised_head = crate::server::receive_tests::success(
        source.path(),
        &["ls-remote", "--symref", git_url.as_str(), "HEAD"],
    )
    .await;
    assert_eq!(
        advertised_head,
        format!("ref: refs/heads/feature/browser\tHEAD\n{commit}\tHEAD")
    );
    let stale_head = set_default_branch(
        &h,
        &alice,
        csrf,
        json!({
            "name":"main",
            "expected_head":"refs/heads/main",
            "expected_oid":commit
        }),
    )
    .await;
    assert_eq!(stale_head.0, StatusCode::CONFLICT);
    assert_eq!(stale_head.1["error"]["code"], "default_branch_changed");
    let stale_tip = set_default_branch(
        &h,
        &alice,
        csrf,
        json!({
            "name":"main",
            "expected_head":"refs/heads/feature/browser",
            "expected_oid":"a".repeat(40)
        }),
    )
    .await;
    assert_eq!(stale_tip.0, StatusCode::CONFLICT);
    assert_eq!(stale_tip.1["error"]["code"], "branch_changed");
    let restored_default = set_default_branch(
        &h,
        &alice,
        csrf,
        json!({
            "name":"main",
            "expected_head":"refs/heads/feature/browser",
            "expected_oid":commit
        }),
    )
    .await;
    assert_eq!(restored_default.0, StatusCode::OK, "{}", restored_default.1);

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

    let protected = delete_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"main","expected_oid":commit}),
    )
    .await;
    assert_eq!(protected.0, StatusCode::FORBIDDEN);
    assert_eq!(protected.1["error"]["code"], "protected_branch");
    let deleted = delete_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"feature/browser","expected_oid":commit}),
    )
    .await;
    assert_eq!(deleted.0, StatusCode::OK, "{}", deleted.1);
    assert_eq!(deleted.1["branch"], "refs/heads/feature/browser");
    assert_eq!(deleted.1["deleted_oid"], commit);
    let refs = h.json("/api/repos/team/private/refs", &alice).await;
    assert!(
        !refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reference| { reference["name"] == "refs/heads/feature/browser" })
    );
    assert!(
        crate::server::receive_tests::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/heads/feature/browser"]
        )
        .await
        .is_empty()
    );
    let stale = delete_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"feature/browser","expected_oid":commit}),
    )
    .await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["error"]["code"], "branch_changed");
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
    for body in [
        json!({"expected_version":0,"rules":[{"branch":"refs/heads/main","required_approvals":0,"required_checks":[]}]}),
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":21,"required_checks":[]}]}),
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci/test","CI/Test"]}]}),
    ] {
        let invalid = set_branch_protections(&h, &alice, csrf, body).await;
        assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
        assert_eq!(invalid.1["error"]["code"], "invalid_request");
    }
    let incomplete = create_branch(&h, &alice, csrf, json!({})).await;
    assert_eq!(incomplete.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(incomplete.1["error"]["code"], "invalid_request");
    let invalid_delete = delete_branch(
        &h,
        &alice,
        csrf,
        json!({"name":"refs/heads/other","expected_oid":&commit}),
    )
    .await;
    assert_eq!(invalid_delete.0, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_delete.1["error"]["code"], "invalid_request");
    for body in [
        json!({"name":"refs/heads/other","expected_head":"refs/heads/main","expected_oid":&commit}),
        json!({"name":"other","expected_head":"main","expected_oid":&commit}),
        json!({"name":"other","expected_head":"refs/heads/main","expected_oid":"not-an-oid"}),
    ] {
        let invalid = set_default_branch(&h, &alice, csrf, body).await;
        assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
        assert_eq!(invalid.1["error"]["code"], "invalid_request");
    }
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
    let denied_delete = delete_branch(
        &h,
        &bob,
        bob_session["csrf"].as_str().unwrap(),
        json!({"name":"other","expected_oid":&commit}),
    )
    .await;
    assert_eq!(denied_delete.0, StatusCode::FORBIDDEN);
    assert_eq!(denied_delete.1["error"]["code"], "forbidden");
    let denied_default = set_default_branch(
        &h,
        &bob,
        bob_session["csrf"].as_str().unwrap(),
        json!({"name":"other","expected_head":"refs/heads/main","expected_oid":&commit}),
    )
    .await;
    assert_eq!(denied_default.0, StatusCode::FORBIDDEN);
    assert_eq!(denied_default.1["error"]["code"], "forbidden");
    let denied_protection = set_branch_protections(
        &h,
        &bob,
        bob_session["csrf"].as_str().unwrap(),
        json!({"expected_version":0,"rules":[]}),
    )
    .await;
    assert_eq!(denied_protection.0, StatusCode::FORBIDDEN);
    assert_eq!(denied_protection.1["error"]["code"], "forbidden");

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
    let hidden_delete = delete_branch(
        &h,
        &outsider,
        outsider_session["csrf"].as_str().unwrap(),
        json!({"name":"other","expected_oid":&commit}),
    )
    .await;
    assert_eq!(hidden_delete.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden_delete.1["error"]["code"], "repository_not_found");
    let hidden_default = set_default_branch(
        &h,
        &outsider,
        outsider_session["csrf"].as_str().unwrap(),
        json!({"name":"other","expected_head":"refs/heads/main","expected_oid":&commit}),
    )
    .await;
    assert_eq!(hidden_default.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden_default.1["error"]["code"], "repository_not_found");
    let hidden_protection = set_branch_protections(
        &h,
        &outsider,
        outsider_session["csrf"].as_str().unwrap(),
        json!({"expected_version":0,"rules":[]}),
    )
    .await;
    assert_eq!(hidden_protection.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden_protection.1["error"]["code"], "repository_not_found");
    h.close().await;
}
