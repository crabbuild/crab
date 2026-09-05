use super::*;

const RELEASES: &str = "/api/repos/team/private/releases";

async fn publish_release(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .post(format!("{}{RELEASES}", h.origin))
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

async fn edit_release(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    number: u64,
    body: &Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .patch(format!("{}{RELEASES}/{number}", h.origin))
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

async fn delete_release(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    number: u64,
    version: u64,
) -> StatusCode {
    h.http
        .delete(format!("{}{RELEASES}/{number}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"version":version}).to_string())
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_release_publishes_and_recovers_native_git_tags() {
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
    std::fs::write(source.path().join("README.md"), "release fixture\n").unwrap();
    crate::server::receive_tests::success(source.path(), &["add", "README.md"]).await;
    crate::server::receive_tests::success(source.path(), &["commit", "-m", "release base"]).await;
    let commit = crate::server::receive_tests::success(source.path(), &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(source.path(), &["push", git_url.as_str(), "main"]).await;

    let first = json!({
        "request_id":"11111111-1111-4111-8111-111111111111",
        "tag_name":"v1.0.0",
        "target_oid":commit,
        "title":"Crab 1.0",
        "body":"## Changes\n\nFirst release.",
        "prerelease":false
    });
    let rejected_csrf = h
        .http
        .post(format!("{}{RELEASES}", h.origin))
        .header(header::COOKIE, &alice)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(first.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected_csrf.status(), StatusCode::FORBIDDEN);
    let created = publish_release(&h, &alice, csrf, &first).await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    assert_eq!(created.1["number"], 1);
    assert_eq!(created.1["tag_name"], "v1.0.0");
    assert_eq!(created.1["tag_oid"], commit);
    assert_eq!(created.1["target_oid"], commit);
    assert_eq!(created.1["version"], 1);

    crate::server::receive_tests::success(
        source.path(),
        &["push", git_url.as_str(), ":refs/tags/v1.0.0"],
    )
    .await;
    assert!(
        crate::server::receive_tests::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/tags/v1.0.0"],
        )
        .await
        .is_empty()
    );
    let replay = publish_release(&h, &alice, csrf, &first).await;
    assert_eq!(replay, created);
    let list = h.json(RELEASES, &alice).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        h.json(&format!("{RELEASES}/1"), &alice).await["title"],
        "Crab 1.0"
    );
    let advertised = crate::server::receive_tests::success(
        source.path(),
        &["ls-remote", git_url.as_str(), "refs/tags/v1.0.0"],
    )
    .await;
    assert_eq!(advertised, format!("{commit}\trefs/tags/v1.0.0"));
    let client = tempfile::tempdir().unwrap();
    crate::server::receive_tests::success(
        client.path(),
        &["init", "--bare", "--object-format=sha1", "."],
    )
    .await;
    crate::server::receive_tests::success(
        client.path(),
        &["fetch", git_url.as_str(), "refs/tags/v1.0.0"],
    )
    .await;
    assert_eq!(
        crate::server::receive_tests::success(client.path(), &["rev-parse", "FETCH_HEAD"]).await,
        commit
    );

    let mut changed_submission = first.clone();
    changed_submission["title"] = json!("Changed title");
    let conflict = publish_release(&h, &alice, csrf, &changed_submission).await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.1["error"]["code"], "submission_conflict");
    let mut duplicate_tag = first.clone();
    duplicate_tag["request_id"] = json!("22222222-2222-4222-8222-222222222222");
    let conflict = publish_release(&h, &alice, csrf, &duplicate_tag).await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.1["error"]["code"], "release_conflict");

    std::fs::write(source.path().join("NEXT.md"), "next commit\n").unwrap();
    crate::server::receive_tests::success(source.path(), &["add", "NEXT.md"]).await;
    crate::server::receive_tests::success(source.path(), &["commit", "-m", "next commit"]).await;
    let next_commit =
        crate::server::receive_tests::success(source.path(), &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(
        source.path(),
        &["push", git_url.as_str(), "HEAD:refs/heads/release-target"],
    )
    .await;
    crate::server::receive_tests::success(
        source.path(),
        &["tag", "-a", "v2.0.0", &commit, "-m", "annotated release"],
    )
    .await;
    let tag_oid =
        crate::server::receive_tests::success(source.path(), &["rev-parse", "v2.0.0"]).await;
    crate::server::receive_tests::success(
        source.path(),
        &["push", git_url.as_str(), "refs/tags/v2.0.0"],
    )
    .await;
    let incompatible = json!({
        "request_id":"33333333-3333-4333-8333-333333333333",
        "tag_name":"v2.0.0",
        "target_oid":next_commit,
        "title":"Wrong target",
        "body":"This must not claim the existing tag.",
        "prerelease":false
    });
    let incompatible = publish_release(&h, &alice, csrf, &incompatible).await;
    assert_eq!(incompatible.0, StatusCode::CONFLICT);
    assert_eq!(incompatible.1["error"]["code"], "release_conflict");
    let annotated_input = json!({
        "request_id":"44444444-4444-4444-8444-444444444444",
        "tag_name":"v2.0.0",
        "target_oid":commit,
        "title":"Crab 2.0",
        "body":"Annotated tag release.",
        "prerelease":true
    });
    let annotated = publish_release(&h, &alice, csrf, &annotated_input).await;
    assert_eq!(annotated.0, StatusCode::CREATED, "{}", annotated.1);
    assert_eq!(annotated.1["number"], 2);
    assert_eq!(annotated.1["tag_oid"], tag_oid);
    assert_eq!(annotated.1["target_oid"], commit);
    let releases = h.json(RELEASES, &alice).await;
    assert_eq!(releases["items"].as_array().unwrap().len(), 2);
    assert_eq!(releases["items"][0]["number"], annotated.1["number"]);

    let edited = edit_release(
        &h,
        &alice,
        csrf,
        2,
        &json!({
            "version":1,
            "title":"Crab 2.0 final",
            "body":"Updated annotated release notes.",
            "prerelease":false
        }),
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK, "{}", edited.1);
    assert_eq!(edited.1["version"], 2);
    assert_eq!(edited.1["title"], "Crab 2.0 final");
    let stale = edit_release(
        &h,
        &alice,
        csrf,
        2,
        &json!({
            "version":1,
            "title":"Stale edit",
            "body":"Must not replace newer notes.",
            "prerelease":true
        }),
    )
    .await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(
        delete_release(&h, &alice, csrf, 2, 2).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete_release(&h, &alice, csrf, 2, 2).await,
        StatusCode::NO_CONTENT
    );
    let deleted = h
        .http
        .get(format!("{}{RELEASES}/2", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
    let retained_tag = crate::server::receive_tests::success(
        source.path(),
        &["ls-remote", git_url.as_str(), "refs/tags/v2.0.0"],
    )
    .await;
    assert_eq!(retained_tag, format!("{tag_oid}\trefs/tags/v2.0.0"));
    let deleted_replay = publish_release(&h, &alice, csrf, &annotated_input).await;
    assert_eq!(deleted_replay.0, StatusCode::CONFLICT);
    let replacement = json!({
        "request_id":"77777777-7777-4777-8777-777777777777",
        "tag_name":"v2.0.0",
        "target_oid":commit,
        "title":"Crab 2.0 restored",
        "body":"Published again from the retained tag.",
        "prerelease":false
    });
    let replacement = publish_release(&h, &alice, csrf, &replacement).await;
    assert_eq!(replacement.0, StatusCode::CREATED, "{}", replacement.1);
    assert_eq!(replacement.1["number"], 3);
    assert_eq!(replacement.1["tag_oid"], tag_oid);
    assert_eq!(replacement.1["version"], 1);
    assert_eq!(
        h.json(RELEASES, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    crate::repository_settings::replace_lifecycle(repo, 0, true)
        .await
        .unwrap();
    assert_eq!(
        h.json(RELEASES, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut archived_release = first.clone();
    archived_release["request_id"] = json!("55555555-5555-4555-8555-555555555555");
    archived_release["tag_name"] = json!("v3.0.0");
    let blocked = publish_release(&h, &alice, csrf, &archived_release).await;
    assert_eq!(blocked.0, StatusCode::FORBIDDEN);
    assert_eq!(blocked.1["error"]["code"], "repository_archived");
    let blocked_edit = edit_release(
        &h,
        &alice,
        csrf,
        3,
        &json!({
            "version":1,
            "title":"Archived edit",
            "body":"Must remain read-only.",
            "prerelease":false
        }),
    )
    .await;
    assert_eq!(blocked_edit.0, StatusCode::FORBIDDEN);
    assert_eq!(blocked_edit.1["error"]["code"], "repository_archived");
    assert_eq!(
        delete_release(&h, &alice, csrf, 3, 1).await,
        StatusCode::FORBIDDEN
    );
    crate::repository_settings::replace_lifecycle(repo, 1, false)
        .await
        .unwrap();

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(RELEASES, &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let bob_session = h.json("/api/session", &bob).await;
    let mut denied_release = first;
    denied_release["request_id"] = json!("66666666-6666-4666-8666-666666666666");
    let denied = publish_release(
        &h,
        &bob,
        bob_session["csrf"].as_str().unwrap(),
        &denied_release,
    )
    .await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    let denied_edit = edit_release(
        &h,
        &bob,
        bob_session["csrf"].as_str().unwrap(),
        3,
        &json!({
            "version":1,
            "title":"Reader edit",
            "body":"Must be rejected.",
            "prerelease":false
        }),
    )
    .await;
    assert_eq!(denied_edit.0, StatusCode::FORBIDDEN);
    assert_eq!(
        delete_release(&h, &bob, bob_session["csrf"].as_str().unwrap(), 3, 1,).await,
        StatusCode::FORBIDDEN
    );

    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{RELEASES}", h.origin))
        .header(header::COOKIE, outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
