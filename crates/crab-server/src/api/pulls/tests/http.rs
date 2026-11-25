use crate::test_support::http::Harness;
use axum::http::StatusCode;
use axum::http::header;
use serde_json::Value;
use serde_json::json;
use url::Url;

mod sarif;

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

async fn report_status(h: &Harness, token: &str, oid: &str, body: Value) -> (StatusCode, Value) {
    let response = h
        .http
        .post(format!(
            "{}/api/repos/team/private/statuses/{oid}",
            h.origin
        ))
        .basic_auth("crab", Some(token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

async fn report_check(
    h: &Harness,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .basic_auth("crab", Some(token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

async fn set_bob_access(h: &Harness, alice: &str, access: &str) {
    let url = "/api/repos/team/private/members";
    let mut current = h.json(url, alice).await;
    current["members"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|member| member["subject"] == "bob-id")
        .unwrap()["access"] = json!(access);
    h.write(reqwest::Method::PUT, url, alice,
        json!({"expected_revision":current["revision"],"members":current["members"],"groups":current["groups"]}))
        .await.error_for_status().unwrap();
}

#[tokio::test]
async fn pull_routes_require_membership_and_csrf_before_repository_access() {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
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
            .header(
                "x-crab-repository-id",
                h.server.repositories.values()[0].id.to_string(),
            )
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
async fn viewed_files_are_private_and_track_the_diff_identity() {
    use crate::test_support::git;
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let token: Value = h
        .write(
            reqwest::Method::POST,
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
    h.write(
        reqwest::Method::POST,
        ROOT,
        &alice,
        json!({
            "request_id":uuid::Uuid::now_v7(), "title":"Track review progress", "body":"",
            "base_ref":"refs/heads/main", "head_ref":"refs/heads/feature",
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let preview = "/api/repos/team/private/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true";
    let reviewed = format!("{ROOT}/1/viewed");
    let first = h.json(preview, &bob).await;
    let first = first["changes"][0]["view_id"].as_str().unwrap();
    let (path, revision) = first.split_at(64);
    let mark = |file: &str, version: u64| json!({"file":file,"version":version,"viewed":true});
    let marked: Value = h
        .write(reqwest::Method::PUT, &reviewed, &bob, mark(first, 0))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(marked["version"], 1);
    assert_eq!(h.json(&reviewed, &bob).await["files"][path], revision);
    assert_eq!(h.json(&reviewed, &alice).await["files"], json!({}));
    let stale_noop = h
        .write(reqwest::Method::PUT, &reviewed, &bob, mark(first, 0))
        .await;
    assert_eq!(stale_noop.status(), StatusCode::CONFLICT);

    git::success(
        source.path(),
        &["commit", "--allow-empty", "-m", "Unchanged reviewed file"],
    )
    .await;
    git::success(source.path(), &["push", remote.as_str(), "feature"]).await;
    assert_eq!(h.json(preview, &bob).await["changes"][0]["view_id"], first);
    std::fs::write(source.path().join("README.md"), "base\nchanged again\n").unwrap();
    git::success(
        source.path(),
        &["commit", "-am", "Change the reviewed file"],
    )
    .await;
    git::success(source.path(), &["push", remote.as_str(), "feature"]).await;
    let second = h.json(preview, &bob).await;
    let second = second["changes"][0]["view_id"].as_str().unwrap();
    let (second_path, second_revision) = second.split_at(64);
    assert_eq!(second_path, path);
    assert_ne!(second_revision, revision);
    let conflict = h
        .write(reqwest::Method::PUT, &reviewed, &bob, mark(second, 0))
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let updated: Value = h
        .write(reqwest::Method::PUT, &reviewed, &bob, mark(second, 1))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated, json!({"version": 2}));
    assert_eq!(
        h.json(&reviewed, &bob).await["files"],
        json!({(path):second_revision}),
    );
    h.close().await;
}

#[tokio::test]
async fn pending_review_keeps_comments_private_and_recovers_a_published_batch() {
    use crate::test_support::git;
    use reqwest::Method;
    use uuid::Uuid;

    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &alice,
        json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
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
    let git_source = tempfile::tempdir().unwrap();
    git::push_review_branches(git_source.path(), remote.as_str()).await;
    h.write(
        Method::POST,
        ROOT,
        &alice,
        json!({
            "request_id":Uuid::now_v7(), "title":"Review together", "body":"",
            "base_ref":"refs/heads/main", "head_ref":"refs/heads/feature",
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let subscription = format!("{ROOT}/1/subscription");
    let draft = format!("{ROOT}/1/pending-review");
    let submit = format!("{draft}/submit");
    let reviews = format!("{ROOT}/1/reviews");
    let threads = format!("{ROOT}/1/threads");
    let comparison_path = "/api/repos/team/private/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true";
    let comparison = h.json(comparison_path, &bob).await;
    let comment = |expected, request_id, body, suggested_text| {
        json!({
            "expected":expected, "request_id":request_id,
            "edit":{"action":"save","comment":{
                "request_id":request_id,"body":body,"suggested_text":suggested_text,
                "path_hex":comparison["changes"][0]["path_hex"],
                "base_oid":comparison["base"],"head_oid":comparison["commit"],
                "side":"new","start_line":2,"end_line":2,
            }},
        })
    };
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let first_edit = comment(
        None,
        first,
        "@alice Consider explaining this.",
        None::<&str>,
    );
    for input in [
        first_edit.clone(),
        first_edit.clone(),
        comment(
            Some(first),
            second,
            "Suggested help text.",
            Some("updated feature"),
        ),
    ] {
        h.write(Method::PUT, &draft, &bob, input)
            .await
            .error_for_status()
            .unwrap();
    }
    assert_eq!(
        h.json(&draft, &bob).await["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        h.json(&draft, &alice).await,
        json!({"version":null,"comments":[]})
    );
    assert_eq!(h.json(&reviews, &alice).await["items"], json!([]));
    assert_eq!(h.json(&threads, &alice).await["items"], json!([]));
    assert_eq!(
        h.json(&subscription, &bob).await,
        json!({"mode":"inherit","version":0})
    );
    h.deliver_notifications("team", "private").await;
    assert_eq!(
        h.json("/api/notifications", &alice).await["items"],
        json!([])
    );
    assert_eq!(
        h.write(Method::PUT, &draft, &bob, first_edit)
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let request = json!({
        "request_id":Uuid::now_v7(),"expected":second,"body":"Summary for @alice","state":"commented",
    });
    let submitted: Value = h
        .write(Method::POST, &submit, &bob, request.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(submitted, json!({"review":1,"threads":[1,2]}));
    h.deliver_notifications("team", "private").await;
    let inbox = h.json("/api/notifications", &alice).await;
    let [row] = inbox["items"].as_array().unwrap().as_slice() else {
        panic!("one published pull request should occupy one inbox row");
    };
    assert_eq!(row["reason"], "mention");
    assert!(row["href"].as_str().unwrap().contains("/pull/1"));
    assert_eq!(
        h.json(&subscription, &bob).await,
        json!({"mode":"follow","version":1})
    );
    assert_eq!(h.json(&draft, &bob).await["comments"], json!([]));
    assert_eq!(
        h.json(&reviews, &alice).await["items"][0]["commit_oid"],
        comparison["commit"]
    );
    let visible = h.json(&threads, &alice).await;
    assert_eq!(visible["items"].as_array().unwrap().len(), 2);
    assert!(
        visible["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["suggested_text"] == "updated feature")
    );

    let stale = Uuid::now_v7();
    h.write(
        Method::PUT,
        &draft,
        &bob,
        comment(
            Some(request["request_id"].as_str().unwrap().parse().unwrap()),
            stale,
            "New private comment",
            None,
        ),
    )
    .await
    .error_for_status()
    .unwrap();
    git::success(
        git_source.path(),
        &["commit", "--allow-empty", "-m", "Move source"],
    )
    .await;
    git::success(git_source.path(), &["push", remote.as_str(), "feature"]).await;
    let conflict = h
        .write(
            Method::POST,
            &submit,
            &bob,
            json!({"request_id":Uuid::now_v7(),"expected":stale,"body":"","state":"commented"}),
        )
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let replayed: Value = h
        .write(Method::POST, &submit, &bob, request.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed, submitted);
    h.deliver_notifications("team", "private").await;
    assert_eq!(h.json("/api/notifications", &alice).await, inbox);
    assert_eq!(h.json(&draft, &bob).await["version"], json!(stale));
    let discarded = Uuid::now_v7();
    h.write(
        Method::PUT,
        &draft,
        &bob,
        json!({"request_id":discarded,"expected":stale,"edit":{"action":"discard"}}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&draft, &bob).await,
        json!({"version":discarded,"comments":[]})
    );
    assert_eq!(
        h.json(&reviews, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.close().await;
}

#[tokio::test]
async fn suggestion_batches_publish_one_git_commit_or_leave_the_head_unchanged() {
    use crate::test_support::git;
    use reqwest::Method;

    let h = Harness::new(false).await;
    h.initialize_directory().await;
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
    let path = source.path();
    git::push_review_branches(path, remote.as_str()).await;
    std::fs::write(path.join("second.txt"), "alpha\r\nbeta\n").unwrap();
    git::success(path, &["add", "second.txt"]).await;
    git::success(path, &["commit", "-m", "Add a second file"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    h.write(
        Method::POST,
        ROOT,
        &alice,
        json!({
            "request_id":uuid::Uuid::now_v7(), "title":"Update two files", "body":"",
            "base_ref":"refs/heads/main", "head_ref":"refs/heads/feature",
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let diff = h.json("/api/repos/team/private/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true", &alice).await;
    for (file, line, replacement) in [
        ("README.md", 2, "new feature"),
        ("second.txt", 2, "new beta"),
        ("README.md", 2, "overlapping edit"),
    ] {
        let change = diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == file)
            .unwrap();
        h.write(
            Method::POST,
            &format!("{ROOT}/1/threads"),
            &alice,
            json!({
                "request_id":uuid::Uuid::now_v7(),
                "base_oid":diff["base"],"head_oid":head,"path_hex":change["path_hex"],
                "side":"new","start_line":line,"end_line":line,
                "body":"Suggested revision","suggested_text":replacement
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let endpoint = format!("{ROOT}/1/suggestions");
    let denied = h
        .write(
            Method::POST,
            &endpoint,
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"expected_head":head,"threads":[
                {"number":1,"version":1},{"number":2,"version":1},{"number":3,"version":1}
            ]}),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    let unchanged = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/feature"]).await;
    assert_eq!(unchanged, format!("{head}\trefs/heads/feature"));
    let input = json!({
        "request_id":uuid::Uuid::now_v7(),
        "expected_head":head,
        "threads":[{"number":1,"version":1},{"number":2,"version":1}],
    });
    let mut stale = input.clone();
    stale["threads"][1]["version"] = json!(2);
    assert_eq!(
        h.write(Method::POST, &endpoint, &alice, stale)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.write(Method::POST, &endpoint, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let submit = || h.write(Method::POST, &endpoint, &alice, input.clone());
    let (first, second) = tokio::join!(submit(), submit());
    let mut outcomes = Vec::<Value>::new();
    for response in [first, second] {
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            continue;
        }
        outcomes.push(response.error_for_status().unwrap().json().await.unwrap());
    }
    let applied = outcomes.first().unwrap();
    assert!(outcomes.iter().all(|value| value == applied));
    git::success(path, &["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(
        git::success(path, &["rev-parse", "FETCH_HEAD"]).await,
        applied["commit"]
    );
    assert_eq!(
        git::success(path, &["rev-parse", "FETCH_HEAD^"]).await,
        head
    );
    assert_eq!(
        git::success(path, &["show", "FETCH_HEAD:README.md"]).await,
        "base\nnew feature"
    );
    assert_eq!(
        git::success(path, &["show", "FETCH_HEAD:second.txt"]).await,
        "alpha\r\nnew beta"
    );
    let mut reordered = input.clone();
    reordered["threads"].as_array_mut().unwrap().reverse();
    let replayed: Value = h
        .write(Method::POST, &endpoint, &alice, reordered)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(&replayed, applied);
    let mut changed = input;
    changed["message"] = json!("A different commit message");
    assert_eq!(
        h.write(Method::POST, &endpoint, &alice, changed)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn pull_review_decisions_require_another_member_and_follow_the_exact_head() {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let alice_session = h.json("/api/session", &alice).await;
    let alice_csrf = alice_session["csrf"].as_str().unwrap();
    let profile: Value = h.write(reqwest::Method::POST, "/api/profile", &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"handle":"public-alice","name":"Public Alice"}))
        .await.error_for_status().unwrap().json().await.unwrap();
    let alice_author = json!({"name":"Alice","profile":{"id":profile["id"],"handle":"public-alice","name":"Public Alice"}});
    let bob_author = json!({"name":"Bob","profile":null});
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
    let git_token = token.1["token"].as_str().unwrap().to_owned();
    let mut git_url = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url.set_password(Some(&git_token)).unwrap();
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    let first_head = crate::test_support::git::push_review_branches(path, git_url.as_str()).await;
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
    assert_eq!(created.1["author"], alice_author);
    assert_eq!(created.1["can_decide"], false);
    assert_eq!(created.1["can_merge"], false);
    let blocked = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/merge"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000014",
            "version":created.1["version"],
            "method":"fast_forward",
            "base_oid":created.1["base_oid"],
            "head_oid":created.1["head_oid"]
        }),
    )
    .await;
    assert_eq!(blocked.0, StatusCode::CONFLICT);
    assert_eq!(blocked.1["error"]["code"], "merge_blocked");

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let bob_session = h.json("/api/session", &bob).await;
    let bob_csrf = bob_session["csrf"].as_str().unwrap();
    let missing_csrf = h
        .http
        .post(format!("{}{ROOT}/1/threads", h.origin))
        .header(header::COOKIE, &bob)
        .header(header::ORIGIN, &h.origin)
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            json!({
                "request_id":"00000000-0000-4000-8000-000000000030",
                "body":"missing csrf",
                "base_oid":created.1["base_oid"],
                "head_oid":created.1["head_oid"],
                "path_hex":"524541444d452e6d64",
                "side":"new",
                "start_line":2,
                "end_line":2
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);
    let thread = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/threads"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000031",
            "body":"Please make this line more explicit.",
            "suggested_text":"feature improved",
            "base_oid":created.1["base_oid"],
            "head_oid":created.1["head_oid"],
            "path_hex":"524541444d452e6d64",
            "side":"new",
            "start_line":2,
            "end_line":2
        }),
    )
    .await;
    assert_eq!(thread.0, StatusCode::CREATED, "{}", thread.1);
    assert_eq!(thread.1["author"], bob_author);
    assert_eq!(thread.1["number"], 1);
    assert_eq!(thread.1["path_hex"], "524541444d452e6d64");
    assert_eq!(thread.1["new_blob_oid"].as_str().unwrap().len(), 40);
    assert_eq!(thread.1["can_reply"], true);
    let member_reply = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/threads/1/replies"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000034",
            "body":"A read member can participate in the conversation."
        }),
    )
    .await;
    assert_eq!(member_reply.0, StatusCode::CREATED, "{}", member_reply.1);
    assert_eq!(member_reply.1["author"], bob_author);
    assert_eq!(member_reply.1["number"], 1);
    let old_suggestion = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/threads"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000032",
            "body":"old-side suggestions are invalid",
            "suggested_text":"replacement",
            "base_oid":created.1["base_oid"],
            "head_oid":created.1["head_oid"],
            "path_hex":"524541444d452e6d64",
            "side":"old",
            "start_line":1,
            "end_line":1
        }),
    )
    .await;
    assert_eq!(old_suggestion.0, StatusCode::BAD_REQUEST);
    let reply = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/threads/1/replies"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000033",
            "body":"I agree; I will update it."
        }),
    )
    .await;
    assert_eq!(reply.0, StatusCode::CREATED, "{}", reply.1);
    assert_eq!(reply.1["author"], alice_author);
    assert_eq!(reply.1["number"], 2);
    let resolved = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1/threads/1"),
        json!({"version":thread.1["version"],"resolved":true}),
    )
    .await;
    assert_eq!(resolved.0, StatusCode::OK, "{}", resolved.1);
    assert_eq!(resolved.1["resolved"], true);
    assert_eq!(resolved.1["resolved_by"], alice_author);
    let unauthorized_resolution = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1/threads/1"),
        json!({"version":thread.1["version"],"resolved":false}),
    )
    .await;
    assert_eq!(unauthorized_resolution.0, StatusCode::FORBIDDEN);
    let reopened = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1/threads/1"),
        json!({"version":resolved.1["version"],"resolved":false}),
    )
    .await;
    assert_eq!(reopened.0, StatusCode::OK);
    assert_eq!(reopened.1["resolved"], false);
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
            "state":"approved", "commit_oid":first_head
        }),
    )
    .await;
    assert_eq!(approved.0, StatusCode::CREATED);
    assert_eq!(approved.1["commit_oid"], first_head);
    assert_eq!(approved.1["author"], bob_author);
    assert_eq!(approved.1["current"], true);
    h.write(
        reqwest::Method::PATCH,
        "/api/profile",
        &alice,
        json!({"version":1,"handle":"renamed-alice","name":"Renamed Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let renamed = json!({"name":"Alice","profile":{"id":profile["id"],"handle":"renamed-alice","name":"Renamed Alice"}});
    assert_eq!(h.json(&format!("{ROOT}/1"), &bob).await["author"], renamed);
    assert_eq!(
        h.json(&format!("{ROOT}/1/threads/1/replies/2"), &bob).await["author"],
        renamed
    );
    for path in ["reviews/1", "threads/1", "threads/1/replies/1"] {
        assert_eq!(
            h.json(&format!("{ROOT}/1/{path}"), &alice).await["author"],
            bob_author
        );
    }
    for path in ["reviews", "threads", "threads/1/replies"] {
        let page = h.json(&format!("{ROOT}/1/{path}"), &alice).await;
        for record in page["items"].as_array().unwrap() {
            assert!(record["author"].is_object());
            assert!(!record["author"].to_string().contains("issuer"));
            assert!(!record["author"].to_string().contains("subject"));
        }
    }
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["version"], 2);
    assert_eq!(ready["merge_requirements"]["approvals"], 0);
    assert_eq!(
        h.json(&format!("{ROOT}/1"), &bob).await["review_counts_for_merge"],
        false
    );
    assert_eq!(
        ready["merge_requirements"]["checks"][0]["state"],
        Value::Null
    );
    assert_eq!(ready["merge_requirements"]["checks_satisfied"], false);
    assert_eq!(ready["merge_requirements"]["satisfied"], false);
    assert_eq!(ready["can_merge"], false);
    let blocked = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/merge"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000021",
            "version":ready["version"],
            "method":"fast_forward",
            "base_oid":ready["base_oid"],
            "head_oid":ready["head_oid"]
        }),
    )
    .await;
    assert_eq!(blocked.0, StatusCode::CONFLICT);
    assert_eq!(blocked.1["error"]["code"], "merge_blocked");
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::POST,
            &format!("{ROOT}/1/merge"),
            json!({
                "request_id":"00000000-0000-4000-8000-000000000015",
                "version":1,
                "method":"fast_forward",
                "base_oid":created.1["base_oid"],
                "head_oid":created.1["head_oid"]
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let pending = report_status(
        &h,
        &git_token,
        &first_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000017",
            "context":"ci/test",
            "state":"pending",
            "description":"Tests are running",
            "target_url":"https://ci.example.test/build/17"
        }),
    )
    .await;
    assert_eq!(pending.0, StatusCode::CREATED, "{}", pending.1);
    assert_eq!(pending.1["state"], "pending");
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::POST,
            &format!("/api/repos/team/private/statuses/{first_head}"),
            json!({
                "request_id":"00000000-0000-4000-8000-000000000018",
                "context":"ci/test",
                "state":"success"
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let success = report_status(
        &h,
        &git_token,
        &first_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000019",
            "context":"CI/Test",
            "state":"success",
            "description":"Tests passed",
            "target_url":"https://ci.example.test/build/19"
        }),
    )
    .await;
    assert_eq!(success.0, StatusCode::CREATED);
    assert_eq!(success.1["state"], "success");
    set_bob_access(&h, &alice, "write").await;
    assert_eq!(
        h.json(&format!("{ROOT}/1"), &bob).await["review_counts_for_merge"],
        true
    );
    let replay = report_status(
        &h,
        &git_token,
        &first_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000017",
            "context":"ci/test",
            "state":"pending",
            "description":"Tests are running",
            "target_url":"https://ci.example.test/build/17"
        }),
    )
    .await;
    assert_eq!(replay.0, StatusCode::CREATED);
    assert_eq!(replay.1["state"], "pending");
    let statuses = h
        .json(
            &format!("/api/repos/team/private/commits/{first_head}/status"),
            &bob,
        )
        .await;
    assert_eq!(statuses["state"], "success");
    assert_eq!(statuses["statuses"].as_array().unwrap().len(), 1);
    assert_eq!(statuses["statuses"][0]["context"], "CI/Test");
    let created_check = report_check(
        &h,
        &git_token,
        reqwest::Method::POST,
        "/api/repos/team/private/check-runs",
        json!({
            "request_id":"00000000-0000-4000-8000-000000000023",
            "head_sha":first_head,
            "name":"ci/test",
            "status":"queued",
            "conclusion":null,
            "details_url":"https://ci.example.test/runs/23",
            "output":{
                "title":"Tests are queued",
                "summary":"Waiting for a runner.",
                "text":null,
                "steps":[]
            }
        }),
    )
    .await;
    assert_eq!(created_check.0, StatusCode::CREATED, "{}", created_check.1);
    let run_id = created_check.1["id"].as_u64().unwrap();
    let run_path = format!("/api/repos/team/private/commits/{first_head}/check-runs/{run_id}");
    let waiting = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(
        waiting["merge_requirements"]["checks"][0]["state"],
        "pending"
    );
    assert_eq!(waiting["merge_requirements"]["checks"][0]["run_id"], run_id);
    let listed = h
        .json(
            &format!("/api/repos/team/private/commits/{first_head}/check-runs"),
            &bob,
        )
        .await;
    assert_eq!(listed["items"][0]["name"], "ci/test");
    assert_eq!(listed["next"], Value::Null);
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::PATCH,
            &run_path,
            json!({
                "request_id":"00000000-0000-4000-8000-000000000024",
                "version":1,
                "status":"in_progress",
                "conclusion":null,
                "details_url":null,
                "output":{"title":"Denied","summary":"Denied","text":null,"steps":[]}
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let running_input = json!({
        "request_id":"00000000-0000-4000-8000-000000000025",
        "version":1,
        "status":"in_progress",
        "conclusion":null,
        "details_url":"https://ci.example.test/runs/23",
        "output":{
            "title":"Tests are running",
            "summary":"One step has completed.",
            "text":"Live output is available below.",
            "steps":[{
                "name":"Build",
                "status":"completed",
                "conclusion":"success",
                "log":"Compiling crab-server\nFinished release build\n"
            }]
        }
    });
    let running = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        running_input.clone(),
    )
    .await;
    assert_eq!(running.0, StatusCode::OK, "{}", running.1);
    assert_eq!(running.1["version"], 2);
    let blocked_status = report_status(
        &h,
        &git_token,
        &first_head,
        json!({"request_id":uuid::Uuid::now_v7(), "context":"ci/test", "state":"failure"}),
    )
    .await;
    assert_eq!(blocked_status.0, StatusCode::CREATED);
    let completed = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000026",
            "version":2,
            "status":"completed",
            "conclusion":"success",
            "details_url":"https://ci.example.test/runs/23",
            "output":{
                "title":"All tests passed",
                "summary":"The required test suite passed.",
                "text":"No failures were reported.",
                "steps":[{
                    "name":"Build",
                    "status":"completed",
                    "conclusion":"success",
                    "log":"Compiling crab-server\nFinished release build\n"
                },{
                    "name":"Test",
                    "status":"completed",
                    "conclusion":"success",
                    "log":"44 passed; 0 failed\n"
                }],
                "annotations":[{
                    "path":"src/lib.rs",
                    "start_line":42,
                    "end_line":44,
                    "level":"warning",
                    "title":"Slow assertion",
                    "message":"This assertion took longer than expected."
                }]
            }
        }),
    )
    .await;
    assert_eq!(completed.0, StatusCode::OK, "{}", completed.1);
    assert_eq!(completed.1["version"], 3);
    assert_eq!(
        completed.1["output"]["steps"][1]["log"],
        "44 passed; 0 failed\n"
    );
    assert_eq!(completed.1["output"]["annotations"][0]["start_line"], 42);
    let replay = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        running_input,
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["version"], 2);
    assert_eq!(h.json(&run_path, &bob).await["version"], 3);
    let immutable = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000027",
            "version":3,
            "status":"in_progress",
            "conclusion":null,
            "details_url":null,
            "output":{"title":"Restarted","summary":"Restarted","text":null}
        }),
    )
    .await;
    assert_eq!(immutable.0, StatusCode::BAD_REQUEST);
    let invalid_annotation = report_check(
        &h,
        &git_token,
        reqwest::Method::POST,
        "/api/repos/team/private/check-runs",
        json!({
            "request_id":"00000000-0000-4000-8000-000000000028",
            "head_sha":first_head,
            "name":"ci/lint",
            "status":"completed",
            "conclusion":"success",
            "details_url":null,
            "output":{
                "title":"Lint passed",
                "summary":"No findings.",
                "text":null,
                "annotations":[{
                    "path":"src/lib.rs",
                    "start_line":2,
                    "end_line":1,
                    "level":"failure",
                    "title":null,
                    "message":"Invalid range"
                }]
            }
        }),
    )
    .await;
    assert_eq!(invalid_annotation.0, StatusCode::BAD_REQUEST);
    let blocked = h.json(&format!("{ROOT}/1"), &alice).await;
    let check = &blocked["merge_requirements"]["checks"][0];
    assert_eq!(check["state"], "failure");
    assert_eq!(check["provider"], "crab_status");
    assert_eq!(check["head_sha"], first_head);
    assert_eq!(check["additional_report"]["provider"], "crab_check_run");
    assert_eq!(check["additional_report"]["state"], "success");
    assert_eq!(check["additional_report"]["conclusion"], "success");
    assert_eq!(check["additional_report"]["run_id"], run_id);
    assert_eq!(blocked["can_merge"], false);
    let pass_status = report_status(
        &h,
        &git_token,
        &first_head,
        json!({"request_id":uuid::Uuid::now_v7(), "context":"ci/test", "state":"success"}),
    )
    .await;
    assert_eq!(pass_status.0, StatusCode::CREATED);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["merge_requirements"]["checks"][0]["state"], "success");
    assert_eq!(
        ready["merge_requirements"]["checks"][0]["additional_report"]["state"],
        "success"
    );
    assert_eq!(ready["merge_requirements"]["checks_satisfied"], true);
    assert_eq!(ready["merge_requirements"]["satisfied"], true);
    assert_eq!(ready["can_merge"], true);
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
                "state":"approved", "commit_oid":first_head
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    std::fs::write(path.join("README.md"), "base\nfeature\nupdated\n").unwrap();
    crate::test_support::git::success(path, &["commit", "-am", "updated"]).await;
    let second_head = crate::test_support::git::success(path, &["rev-parse", "HEAD"]).await;
    crate::test_support::git::success(path, &["push", git_url.as_str(), "feature"]).await;
    let current_threads = h
        .json(&format!("{ROOT}/1/threads?outdated=false"), &alice)
        .await;
    assert!(current_threads["items"].as_array().unwrap().is_empty());
    let outdated_threads = h
        .json(&format!("{ROOT}/1/threads?outdated=true"), &alice)
        .await;
    assert_eq!(outdated_threads["items"].as_array().unwrap().len(), 1);
    assert_eq!(outdated_threads["items"][0]["outdated"], true);
    assert_eq!(
        outdated_threads["items"][0]["path_hex"],
        "524541444d452e6d64"
    );
    let replies = h.json(&format!("{ROOT}/1/threads/1/replies"), &alice).await;
    assert_eq!(replies["items"][0]["body"], "I agree; I will update it.");
    assert_eq!(
        replies["items"][1]["body"],
        "A read member can participate in the conversation."
    );
    let stale = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(stale["merge_requirements"]["approvals"], 0);
    let check = &stale["merge_requirements"]["checks"][0];
    assert_eq!(check["state"], Value::Null);
    assert_eq!(check["head_sha"], second_head);
    assert_eq!(check["provider"], "crab_status_or_check_run");
    assert!(check.get("additional_report").is_none());
    assert_eq!(stale["merge_requirements"]["satisfied"], false);
    assert_eq!(stale["can_merge"], false);
    let reviews = h.json(&format!("{ROOT}/1/reviews"), &bob).await;
    assert_eq!(reviews["items"][0]["current"], false);
    let stale_review = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":uuid::Uuid::now_v7(),
            "body":"Do not approve unseen changes",
            "state":"approved",
            "commit_oid":first_head
        }),
    )
    .await;
    assert_eq!(stale_review.0, StatusCode::CONFLICT);
    assert_eq!(stale_review.1["error"]["code"], "conflict");
    let replay_review = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000011",
            "body":"",
            "state":"approved",
            "commit_oid":first_head
        }),
    )
    .await;
    assert_eq!(replay_review.0, StatusCode::CREATED);
    assert_eq!(replay_review.1["commit_oid"], first_head);
    assert_eq!(replay_review.1["current"], false);
    let changed_replay = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000011",
            "body":"",
            "state":"approved",
            "commit_oid":second_head
        }),
    )
    .await;
    assert_eq!(changed_replay.0, StatusCode::CONFLICT);
    assert_eq!(changed_replay.1["error"]["code"], "submission_conflict");
    assert_eq!(
        h.json(&format!("{ROOT}/1/reviews"), &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.json(&format!("{ROOT}/1"), &bob).await["version"],
        stale["version"]
    );
    let requested = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000013",
            "body":"Please address the remaining concern.",
            "state":"changes_requested", "commit_oid":second_head
        }),
    )
    .await;
    assert_eq!(requested.0, StatusCode::CREATED);
    assert_eq!(requested.1["commit_oid"], second_head);
    assert_eq!(requested.1["current"], true);
    let blocked = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(blocked["version"], 3);
    assert_eq!(blocked["merge_requirements"]["changes_requested"], 1);
    assert_eq!(blocked["merge_requirements"]["satisfied"], false);
    let approved = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000016",
            "body":"",
            "state":"approved", "commit_oid":second_head
        }),
    )
    .await;
    assert_eq!(approved.0, StatusCode::CREATED);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["version"], 4);
    assert_eq!(ready["merge_requirements"]["approvals"], 1);
    assert_eq!(ready["merge_requirements"]["changes_requested"], 0);
    assert_eq!(ready["merge_requirements"]["checks_satisfied"], false);
    assert_eq!(ready["merge_requirements"]["satisfied"], false);
    let success = report_status(
        &h,
        &git_token,
        &second_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000020",
            "context":"ci/test",
            "state":"success"
        }),
    )
    .await;
    assert_eq!(success.0, StatusCode::CREATED);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["merge_requirements"]["satisfied"], true);
    let label = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        "/api/repos/team/private/labels",
        json!({
            "request_id":"00000000-0000-4000-8000-000000000022",
            "name":"ready for merge",
            "color":"1f883d",
            "description":"All required checks passed"
        }),
    )
    .await;
    assert_eq!(label.0, StatusCode::CREATED);
    let labeled = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1"),
        json!({"version":ready["version"],"label_ids":[label.1["id"]]}),
    )
    .await;
    assert_eq!(labeled.0, StatusCode::OK);
    assert_eq!(labeled.1["labels"][0]["name"], "ready for merge");
    let bob_id = h.assignee_id(&alice, "Bob").await;
    let assigned = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1"),
        json!({"version":labeled.1["version"],"assignee_ids":[bob_id]}),
    )
    .await;
    assert_eq!(assigned.0, StatusCode::OK);
    assert_eq!(
        assigned.1["assignees"],
        json!([{"id":bob_id,"name":"Bob","profile":null}])
    );
    assert_eq!(
        h.json(ROOT, &bob).await["items"][0],
        json!({
            "kind":"pull",
            "rollup": {
                "number":1,"head_oid":second_head,
                "checks":{"success":1,"neutral":0,"skipped":0,"pending":0,"failed":0},
                "reviews":{"approved":1,"changes_requested":0,"requested":0,"outdated":0}
            },
            "number":1,
            "draft":false,
            "title":"Proposed change",
            "state":"open",
            "author":renamed,
            "base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature",
            "created_at":assigned.1["created_at"],
            "closed_at":null,
            "merged_at":null,
            "updated_at":assigned.1["updated_at"],
            "labels":[{"id":label.1["id"],"name":"ready for merge","color":"1f883d"}],
            "version":assigned.1["version"],
            "comment_count":0,
            "reaction_count":0,
            "assignees":[{"id":bob_id,"name":"Bob","profile":null}]
        })
    );
    set_bob_access(&h, &alice, "read").await;
    let downgraded = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(downgraded["merge_requirements"]["approvals"], 0);
    assert_eq!(downgraded["can_merge"], false);
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::PATCH,
            &format!("{ROOT}/1"),
            json!({"version":assigned.1["version"],"assignee_ids":[]}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    crate::test_support::git::success(path, &["push", git_url.as_str(), ":feature"]).await;
    let replay = report_status(
        &h,
        &git_token,
        &second_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000020",
            "context":"ci/test",
            "state":"success"
        }),
    )
    .await;
    assert_eq!(replay.0, StatusCode::CREATED);
    source.close().unwrap();
    h.close().await;
}
