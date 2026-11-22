use crate::test_support::http::Harness;
use axum::http::StatusCode;
use axum::http::header;
use serde_json::Value;
use serde_json::json;
use url::Url;

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
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
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
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
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
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
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
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
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
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
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
    crate::test_support::git::success(
        source.path(),
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(source.path().join("README.md"), "release fixture\n").unwrap();
    crate::test_support::git::success(source.path(), &["add", "README.md"]).await;
    crate::test_support::git::success(source.path(), &["commit", "-m", "release base"]).await;
    let commit = crate::test_support::git::success(source.path(), &["rev-parse", "HEAD"]).await;
    crate::test_support::git::success(source.path(), &["push", git_url.as_str(), "main"]).await;

    for path in [
        "/team/private/blob/main/README.md",
        "/team/private/blame/main/README.md",
        "/team/private/tree/main",
        "/team/private/commits/main",
        "/team/private/commit/",
        "/team/private/issues/1",
        "/team/private/pull/1/files",
        "/team/private/releases/tag/v1.0.0",
        "/team/private/settings/access",
    ] {
        let address = if path.ends_with("/commit/") {
            format!("{path}{commit}")
        } else {
            path.to_owned()
        };
        let response = h
            .http
            .get(format!("{}{address}", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{address}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }

    let first = json!({
        "request_id":"11111111-1111-4111-8111-111111111111",
        "tag_name":"v1.0.0",
        "target_oid":commit,
        "title":"Crab 1.0",
        "body":"## Changes\n\nFirst release.",
        "prerelease":false,
        "draft":false
    });
    let rejected_csrf = h
        .http
        .post(format!("{}{RELEASES}", h.origin))
        .header(header::COOKIE, &alice)
        .header("x-csrf-token", csrf)
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
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
    assert_eq!(
        h.json(&format!("{RELEASES}/tag/v1.0.0"), &alice).await,
        created.1
    );
    assert!(created.1["assets"].as_array().unwrap().is_empty());

    let asset_url = format!(
        "{}{RELEASES}/1/assets?request_id=99999999-9999-4999-8999-999999999999&name=crab-linux.tar.gz&version=1",
        h.origin
    );
    let asset = h
        .http
        .post(&asset_url)
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
        .header(header::CONTENT_TYPE, "application/gzip")
        .body("release asset bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::CREATED);
    let asset: Value = serde_json::from_slice(&asset.bytes().await.unwrap()).unwrap();
    assert_eq!(asset["version"], 2);
    assert_eq!(asset["assets"][0]["name"], "crab-linux.tar.gz");
    assert_eq!(asset["assets"][0]["content_type"], "application/gzip");
    assert_eq!(asset["assets"][0]["size"], 19);
    assert_eq!(
        asset["assets"][0]["digest"],
        "sha256:b1fb5a929aa04a033823f4d88f3d5d046edb4cf775aaea7abff6970589c99f1a"
    );
    let replay = h
        .http
        .post(&asset_url)
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
        .header(header::CONTENT_TYPE, "application/gzip")
        .body("release asset bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let duplicate = h
        .http
        .post(format!(
            "{}{RELEASES}/1/assets?request_id=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa&name=crab-linux.tar.gz&version=2",
            h.origin
        ))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header("x-crab-repository-id", h.server.repositories.values()[0].id.to_string())
        .header(header::CONTENT_TYPE, "application/gzip")
        .body("other bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let download = h
        .http
        .get(format!(
            "{}{RELEASES}/1/assets/99999999-9999-4999-8999-999999999999",
            h.origin
        ))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.headers()[header::CONTENT_TYPE], "application/gzip");
    assert_eq!(
        download.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"crab-linux.tar.gz\""
    );
    assert_eq!(&download.bytes().await.unwrap()[..], b"release asset bytes");
    let removed = h
        .http
        .delete(format!(
            "{}{RELEASES}/1/assets/99999999-9999-4999-8999-999999999999",
            h.origin
        ))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(
            "x-crab-repository-id",
            h.server.repositories.values()[0].id.to_string(),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"version":2}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
    let removed: Value = serde_json::from_slice(&removed.bytes().await.unwrap()).unwrap();
    assert_eq!(removed["version"], 3);
    assert!(removed["assets"].as_array().unwrap().is_empty());
    let missing_asset = h
        .http
        .get(format!(
            "{}{RELEASES}/1/assets/99999999-9999-4999-8999-999999999999",
            h.origin
        ))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_asset.status(), StatusCode::NOT_FOUND);

    crate::test_support::git::success(
        source.path(),
        &["push", git_url.as_str(), ":refs/tags/v1.0.0"],
    )
    .await;
    assert!(
        crate::test_support::git::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/tags/v1.0.0"],
        )
        .await
        .is_empty()
    );
    let replay = publish_release(&h, &alice, csrf, &first).await;
    assert_eq!(replay.0, created.0, "{}", replay.1);
    assert_eq!(replay.1["number"], created.1["number"]);
    assert_eq!(replay.1["version"], 3);
    assert!(replay.1["assets"].as_array().unwrap().is_empty());
    let list = h.json(RELEASES, &alice).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        h.json(&format!("{RELEASES}/1"), &alice).await["title"],
        "Crab 1.0"
    );
    let advertised = crate::test_support::git::success(
        source.path(),
        &["ls-remote", git_url.as_str(), "refs/tags/v1.0.0"],
    )
    .await;
    assert_eq!(advertised, format!("{commit}\trefs/tags/v1.0.0"));
    let client = tempfile::tempdir().unwrap();
    crate::test_support::git::success(
        client.path(),
        &["init", "--bare", "--object-format=sha1", "."],
    )
    .await;
    crate::test_support::git::success(
        client.path(),
        &["fetch", git_url.as_str(), "refs/tags/v1.0.0"],
    )
    .await;
    assert_eq!(
        crate::test_support::git::success(client.path(), &["rev-parse", "FETCH_HEAD"]).await,
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
    crate::test_support::git::success(source.path(), &["add", "NEXT.md"]).await;
    crate::test_support::git::success(source.path(), &["commit", "-m", "next commit"]).await;
    let next_commit =
        crate::test_support::git::success(source.path(), &["rev-parse", "HEAD"]).await;
    crate::test_support::git::success(
        source.path(),
        &["push", git_url.as_str(), "HEAD:refs/heads/release-target"],
    )
    .await;
    crate::test_support::git::success(
        source.path(),
        &["tag", "-a", "v2.0.0", &commit, "-m", "annotated release"],
    )
    .await;
    let tag_oid = crate::test_support::git::success(source.path(), &["rev-parse", "v2.0.0"]).await;
    crate::test_support::git::success(
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
        "prerelease":false,
        "draft":false
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
        "prerelease":true,
        "draft":false
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
            "prerelease":false,
            "draft":false
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
            "prerelease":true,
            "draft":false
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
    let missing_tag = h
        .http
        .get(format!("{}{RELEASES}/tag/v2.0.0", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_tag.status(), StatusCode::NOT_FOUND);
    let retained_tag = crate::test_support::git::success(
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
        "prerelease":false,
        "draft":false
    });
    let replacement = publish_release(&h, &alice, csrf, &replacement).await;
    assert_eq!(replacement.0, StatusCode::CREATED, "{}", replacement.1);
    assert_eq!(replacement.1["number"], 3);
    assert_eq!(replacement.1["tag_oid"], tag_oid);
    assert_eq!(replacement.1["version"], 1);
    assert_eq!(
        h.json(&format!("{RELEASES}/tag/v2.0.0"), &alice).await["number"],
        3
    );
    assert_eq!(
        h.json(RELEASES, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let draft_input = json!({
        "request_id":"88888888-8888-4888-8888-888888888888",
        "tag_name":"v3.0.0",
        "target_oid":next_commit,
        "title":"Crab 3.0 draft",
        "body":"Private draft notes.",
        "prerelease":true,
        "draft":true
    });
    let draft = publish_release(&h, &alice, csrf, &draft_input).await;
    assert_eq!(draft.0, StatusCode::CREATED, "{}", draft.1);
    assert_eq!(draft.1["number"], 4);
    assert_eq!(draft.1["draft"], true);
    assert!(draft.1["tag_oid"].is_null());
    assert!(draft.1["published_at"].is_null());
    assert_eq!(
        h.json(&format!("{RELEASES}/tag/v3.0.0"), &alice).await,
        draft.1
    );
    assert!(
        crate::test_support::git::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/tags/v3.0.0"],
        )
        .await
        .is_empty()
    );
    let draft = edit_release(
        &h,
        &alice,
        csrf,
        4,
        &json!({
            "version":1,
            "title":"Crab 3.0 ready",
            "body":"Reviewed draft notes.",
            "prerelease":false,
            "draft":true
        }),
    )
    .await;
    assert_eq!(draft.0, StatusCode::OK, "{}", draft.1);
    assert_eq!(draft.1["version"], 2);
    let matching_draft = h.json(&format!("{RELEASES}?query=READY"), &alice).await;
    assert_eq!(matching_draft["items"].as_array().unwrap().len(), 1);
    assert_eq!(matching_draft["items"][0]["number"], 4);
    assert!(
        h.json(&format!("{RELEASES}?query=missing"), &alice).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    crate::repository::settings::replace_lifecycle(&h.server, &repo, 0, true)
        .await
        .unwrap();
    assert_eq!(
        h.json(RELEASES, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        3
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
            "prerelease":false,
            "draft":false
        }),
    )
    .await;
    assert_eq!(blocked_edit.0, StatusCode::FORBIDDEN);
    assert_eq!(blocked_edit.1["error"]["code"], "repository_archived");
    assert_eq!(
        delete_release(&h, &alice, csrf, 3, 1).await,
        StatusCode::FORBIDDEN
    );
    crate::repository::settings::replace_lifecycle(&h.server, &repo, 1, false)
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
    let hidden_draft = h
        .http
        .get(format!("{}{RELEASES}/4", h.origin))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(hidden_draft.status(), StatusCode::NOT_FOUND);
    let hidden_tag = h
        .http
        .get(format!("{}{RELEASES}/tag/v3.0.0", h.origin))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(hidden_tag.status(), StatusCode::NOT_FOUND);
    assert!(
        h.json(&format!("{RELEASES}?query=ready"), &bob).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let watch = "/api/repos/team/private/subscription";
    let inbox = "/api/notifications?reason=release";
    let choose = async |cookie: &str, version: u64, releases: bool| {
        h.write(
            reqwest::Method::PUT,
            watch,
            cookie,
            json!({
                "mode":"direct", "version":version, "releases":releases
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
    };
    for cookie in [&alice, &bob] {
        assert_eq!(
            choose(cookie, 0, true).await,
            json!({"mode":"direct","version":1,"releases":true})
        );
    }
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(inbox, &bob).await["items"], json!([]));

    let published = edit_release(
        &h,
        &alice,
        csrf,
        4,
        &json!({
            "version":2,
            "title":"Crab 3.0 ready",
            "body":"Reviewed draft notes.",
            "prerelease":false,
            "draft":false
        }),
    )
    .await;
    assert_eq!(published.0, StatusCode::OK, "{}", published.1);
    assert_eq!(published.1["version"], 3);
    assert_eq!(published.1["draft"], false);
    assert_eq!(published.1["tag_oid"], next_commit);
    assert!(published.1["published_at"].is_number());
    assert_eq!(
        h.json(&format!("{RELEASES}/tag/v3.0.0"), &bob).await,
        published.1
    );
    assert_eq!(
        crate::test_support::git::success(
            source.path(),
            &["ls-remote", git_url.as_str(), "refs/tags/v3.0.0"],
        )
        .await,
        format!("{next_commit}\trefs/tags/v3.0.0")
    );
    assert_eq!(
        h.json(RELEASES, &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        h.json(&format!("{RELEASES}?query=REVIEWED"), &bob).await["items"][0]["number"],
        4
    );
    let cell = crate::app::repository_cell(&h.server, &repo).unwrap();
    let pending = cell
        .client
        .query::<crate::cells::repository::collaboration::mentions::Pending>(&cell.target, None, ())
        .await
        .unwrap()
        .output;
    assert_eq!(pending.releases.len(), 1);
    let event = &pending.releases[0];
    h.deliver_notifications("team", "private").await;
    let release_inbox = h.json(inbox, &bob).await;
    let [notice] = release_inbox["items"].as_array().unwrap().as_slice() else {
        panic!("publishing a draft must produce one notification");
    };
    assert_eq!(notice["title"], "Crab 3.0 ready");
    assert_eq!(notice["subject"], json!({"kind":"release","number":4}));
    assert_eq!(
        notice["href"],
        format!(
            "/team/private/releases/tag/v3.0.0?repository_id={}&release_number=4",
            repo.id
        )
    );
    assert_eq!(h.json(inbox, &alice).await["items"], json!([]));
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":true})
    );
    let mark = format!(
        "/api/notifications/{}/state",
        notice["id"].as_str().unwrap()
    );
    h.write(
        reqwest::Method::PUT,
        &mark,
        &bob,
        json!({
            "repository_id":repo.id, "expected":notice["updated"], "action":{"read":true}
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    // Retry the identical Directory event after the user reads it.
    use crate::cells::directory::notifications::{
        Access, Deliver, Delivery, Notice, NoticeTarget, Reason, Recipient, ReleaseKind,
    };
    let actor = h.assignee_id(&alice, "Bob").await;
    cell.client
        .command::<Deliver>(
            crate::cells::Address::Directory,
            crate::app::mutation_identity().unwrap(),
            Delivery {
                notice: Notice {
                    id: event.id,
                    repository: repo.id,
                    target: NoticeTarget::Release {
                        kind: ReleaseKind::Release,
                        number: event.number,
                    },
                    created_at_ms: event.published_at_ms,
                    sender: event.publisher.name.clone(),
                },
                recipients: vec![Recipient {
                    actor: serde_json::from_value(actor).unwrap(),
                    access: Access::Direct,
                    reason: Reason::Release,
                }],
            },
        )
        .await
        .unwrap();
    let read_inbox = h.json(inbox, &bob).await;
    assert_eq!(read_inbox["items"].as_array().unwrap().len(), 1);
    assert_eq!(read_inbox["items"][0]["read"], true);
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
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
            "prerelease":false,
            "draft":false
        }),
    )
    .await;
    assert_eq!(denied_edit.0, StatusCode::FORBIDDEN);
    assert_eq!(
        delete_release(&h, &bob, bob_session["csrf"].as_str().unwrap(), 3, 1,).await,
        StatusCode::FORBIDDEN
    );

    let slash_tag = publish_release(
        &h,
        &alice,
        csrf,
        &json!({
            "request_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "tag_name":"release/2026",
            "target_oid":next_commit,
            "title":"Slash tag",
            "body":"",
            "prerelease":false,
            "draft":true
        }),
    )
    .await;
    assert_eq!(slash_tag.0, StatusCode::CREATED, "{}", slash_tag.1);
    for tag in ["release/2026", "release%2F2026"] {
        assert_eq!(
            h.json(&format!("{RELEASES}/tag/{tag}"), &alice).await,
            slash_tag.1
        );
    }

    let publish_slash = |version| {
        json!({
            "version":version, "title":"Slash tag", "body":"", "prerelease":false, "draft":false
        })
    };
    let slash_number = slash_tag.1["number"].as_u64().unwrap();
    assert_eq!(choose(&bob, 1, false).await["version"], 2);
    let slash = edit_release(&h, &alice, csrf, slash_number, &publish_slash(1)).await;
    assert_eq!(slash.0, StatusCode::OK, "{}", slash.1);
    assert_eq!(choose(&bob, 2, true).await["version"], 3);
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json(inbox, &bob).await, read_inbox);

    let draft = edit_release(
        &h,
        &alice,
        csrf,
        slash_number,
        &json!({
            "version":2, "title":"Slash tag", "body":"", "prerelease":false, "draft":true
        }),
    )
    .await;
    assert_eq!(draft.0, StatusCode::OK, "{}", draft.1);
    let slash = edit_release(&h, &alice, csrf, slash_number, &publish_slash(3)).await;
    assert_eq!(slash.0, StatusCode::OK, "{}", slash.1);
    h.deliver_notifications("team", "private").await;
    let slash_inbox = h.json(inbox, &bob).await;
    assert_eq!(
        slash_inbox["items"][0]["href"],
        format!(
            "/team/private/releases/tag/release%2F2026?repository_id={}&release_number={slash_number}",
            repo.id
        )
    );
    assert_eq!(
        h.json(
            &format!("{RELEASES}/tag/release%2F2026?release_number={slash_number}"),
            &bob
        )
        .await["number"],
        slash_number
    );
    let wrong_release = h
        .http
        .get(format!(
            "{}{RELEASES}/tag/release%2F2026?release_number=4",
            h.origin
        ))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_release.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        delete_release(&h, &alice, csrf, slash_number, 4).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.json(inbox, &bob).await, read_inbox);
    assert_eq!(
        h.json("/api/notifications/unread", &bob).await,
        json!({"unread":false})
    );

    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{RELEASES}", h.origin))
        .header(header::COOKIE, &outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let tag_response = h
        .http
        .get(format!("{}{RELEASES}/tag/v1.0.0", h.origin))
        .header(header::COOKIE, &outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(tag_response.status(), StatusCode::NOT_FOUND);
    assert!(
        repo.store
            .list_prefix(&repo.layout.repo_path("app/v1/releases"))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        repo.store
            .list_prefix(&repo.layout.repo_path("release-assets/v1/sha256"))
            .await
            .unwrap()
            .len(),
        1
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_watches_releases_and_opens_the_published_version() {
    use crate::test_support::{git, http::browser::Browser};
    use reqwest::Method;
    use uuid::Uuid;

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    git::push_review_branches(source.path(), remote.as_str()).await;
    let target = git::success(source.path(), &["rev-parse", "main"]).await;

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let browser = Browser::new("release-notifications");
    browser
        .run(&["open", &format!("{}/team/private", h.origin)])
        .await;
    let click = async |role: &str, name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    click("button", "Continue to sign in").await;
    click("button", "Watch").await;
    click("checkbox", "New releases").await;
    click("button", "Save preference").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    assert_eq!(
        h.json("/api/repos/team/private/subscription", &bob).await,
        json!({"mode":"direct","version":1,"releases":true})
    );

    h.write(
        Method::POST,
        RELEASES,
        &alice,
        json!({
            "request_id":Uuid::now_v7(), "tag_name":"release/observed", "target_oid":target,
            "title":"A release for watchers", "body":"Published release notes",
            "prerelease":false, "draft":false
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
    browser
        .run(&["open", &format!("{}/-/notifications", h.origin)])
        .await;
    browser.run(&["wait", "--text", "Release published"]).await;
    click("link", "A release for watchers").await;
    browser
        .run(&["wait", "--text", "Published release notes"])
        .await;
    assert!(
        browser
            .run(&["get", "url"])
            .await
            .contains("release_number=1")
    );
    drop(browser);
    h.close().await;
}
