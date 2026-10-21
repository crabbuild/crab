use super::*;

#[test]
fn reaction_target_addresses_reject_unscoped_and_malformed_paths() {
    for path in [
        "issues/0",
        "issues/1/extra",
        "pull/1",
        "pulls/1/threads/2/replies",
        "pulls/1/threads/0/replies/1",
        "issues/x/comments/1",
        "../issues/1",
    ] {
        assert!(target(path).is_err(), "{path}");
    }
    assert_eq!(
        target("pulls/3/threads/2/replies/1").unwrap(),
        Target::PullReply {
            pull: 3,
            thread: 2,
            number: 1
        }
    );
}

use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

const ISSUE: &str = "/api/repos/team/private/issues/1";
const REACTIONS: &str = "/api/repos/team/private/reactions/issues/1";

async fn issue(h: &Harness, cookie: &str) {
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Reactions","body":"Discuss"}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test]
async fn read_members_can_react_and_counts_are_personalized_without_leaking_identity() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    issue(&h, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let request = json!({"kind":"heart","active":true});
    let response = h.write(Method::PUT, REACTIONS, &bob, request.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let first: Value = response.json().await.unwrap();
    assert_eq!(first["groups"]["heart"], json!({"count":1,"reacted":true}));
    let retry: Value = h
        .write(Method::PUT, REACTIONS, &bob, request)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first, retry);
    let viewed = h.json(ISSUE, &alice).await;
    assert_eq!(
        viewed["reactions"]["groups"]["heart"],
        json!({"count":1,"reacted":false})
    );
    assert_eq!(viewed["reactions"]["groups"].as_object().unwrap().len(), 8);
    let removed: Value = h
        .write(
            Method::PUT,
            REACTIONS,
            &bob,
            json!({"kind":"heart","active":false}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(removed["revision"].as_u64().unwrap() > first["revision"].as_u64().unwrap());
    assert_eq!(
        h.json(ISSUE, &alice).await["reactions"]["groups"]["heart"]["count"],
        0
    );
    h.close().await;
}

#[tokio::test]
async fn comment_batches_keep_each_reaction_with_its_own_target() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    issue(&h, &cookie).await;
    for body in ["First comment", "Second comment"] {
        h.write(
            Method::POST,
            &format!("{ISSUE}/comments"),
            &cookie,
            json!({"request_id":uuid::Uuid::now_v7(),"body":body}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    for (number, kind) in [(1, "rocket"), (2, "eyes")] {
        h.write(
            Method::PUT,
            &format!("{REACTIONS}/comments/{number}"),
            &cookie,
            json!({"kind":kind,"active":true}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let page = h.json(&format!("{ISSUE}/comments"), &cookie).await;
    for (number, kind) in [(1, "rocket"), (2, "eyes")] {
        let comment = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|comment| comment["number"] == number)
            .unwrap();
        assert_eq!(
            comment["reactions"]["groups"][kind],
            json!({"count":1,"reacted":true})
        );
        let detail = h.json(&format!("{ISSUE}/comments/{number}"), &cookie).await;
        assert_eq!(comment["reactions"], detail["reactions"]);
    }
    h.close().await;
}

#[tokio::test]
async fn malformed_reactions_and_missing_csrf_have_no_effect() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    issue(&h, &cookie).await;
    let request = json!({"kind":"thumbs_up","active":true});
    let response = h
        .http
        .put(format!("{}{REACTIONS}", h.origin))
        .header(header::COOKIE, &cookie)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Axum distinguishes invalid fields in valid JSON (422) from syntax errors.
    for body in [
        json!({"kind":"unknown","active":true}),
        json!({"kind":"heart","active":true,"actor":"someone else"}),
    ] {
        assert_eq!(
            h.write(Method::PUT, REACTIONS, &cookie, body)
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        h.write(
            Method::PUT,
            &format!("{REACTIONS}/comments/99"),
            &cookie,
            request
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let issue = h.json(ISSUE, &cookie).await;
    assert!(
        issue["reactions"]["groups"]
            .as_object()
            .unwrap()
            .values()
            .all(|group| group["count"] == 0)
    );
    h.close().await;
}

#[tokio::test]
async fn pull_discussion_views_preserve_reactions_through_replay_and_edits() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    use crate::test_support::git;
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    git::success(path, &["add", "README.md"]).await;
    git::success(path, &["commit", "-m", "base"]).await;
    git::success(path, &["push", remote.as_str(), "main"]).await;
    git::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("README.md"), "base\nfeature\n").unwrap();
    git::success(path, &["commit", "-am", "feature"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    let root = "/api/repos/team/private/pulls";
    let create_pull = json!({"request_id":uuid::Uuid::now_v7(),"title":"Review reactions","body":"Pull body","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"});
    let pull: Value = h
        .write(Method::POST, root, &alice, create_pull.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let thread = json!({"request_id":uuid::Uuid::now_v7(),"body":"Thread body","base_oid":pull["base_oid"],"head_oid":pull["head_oid"],"path_hex":"524541444d452e6d64","side":"new","start_line":2,"end_line":2});
    let creates = [
        (
            "comments",
            json!({"request_id":uuid::Uuid::now_v7(),"body":"Comment body"}),
        ),
        (
            "reviews",
            json!({"request_id":uuid::Uuid::now_v7(),"body":"Review body","state":"commented"}),
        ),
        ("threads", thread),
        (
            "threads/1/replies",
            json!({"request_id":uuid::Uuid::now_v7(),"body":"Reply body"}),
        ),
    ];
    for (resource, input) in &creates {
        let created: Value = h
            .write(
                Method::POST,
                &format!("{root}/1/{resource}"),
                &bob,
                input.clone(),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            created["reactions"]["groups"]["heart"]["count"], 0,
            "{resource}"
        );
    }
    let paths = [
        "",
        "/comments/1",
        "/reviews/1",
        "/threads/1",
        "/threads/1/replies/1",
    ];
    for (index, suffix) in paths.iter().enumerate() {
        let reaction_path = format!("/api/repos/team/private/reactions/pulls/1{suffix}");
        let selected: Value = h
            .write(
                Method::PUT,
                &reaction_path,
                &bob,
                json!({"kind":"heart","active":true}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            selected["groups"]["heart"],
            json!({"count":1,"reacted":true})
        );
        // Equal-numbered bodies, comments, reviews, threads, and replies have
        // independent namespaces; selecting one must not change the others.
        for other in &paths[index + 1..] {
            let view = h.json(&format!("{root}/1{other}"), &bob).await;
            assert_eq!(view["reactions"]["groups"]["heart"]["count"], 0, "{other}");
        }
        let alice_view = h.json(&format!("{root}/1{suffix}"), &alice).await;
        assert_eq!(
            alice_view["reactions"]["groups"]["heart"],
            json!({"count":1,"reacted":false})
        );
    }
    let replay: Value = h
        .write(Method::POST, root, &alice, create_pull)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["reactions"]["groups"]["heart"]["count"], 1);
    for (resource, input) in creates {
        let replay: Value = h
            .write(Method::POST, &format!("{root}/1/{resource}"), &bob, input)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            replay["reactions"]["groups"]["heart"],
            json!({"count":1,"reacted":true}),
            "{resource}"
        );
        let list = h.json(&format!("{root}/1/{resource}"), &bob).await;
        assert_eq!(
            list["items"][0]["reactions"], replay["reactions"],
            "{resource}"
        );
        let edited: Value = h
            .write(
                Method::PATCH,
                &format!("{root}/1/{resource}/1"),
                &bob,
                json!({"version":replay["version"],"body":"Updated discussion"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            edited["reactions"]["groups"]["heart"],
            json!({"count":1,"reacted":true}),
            "{resource}"
        );
    }
    for suffix in [
        "/comments/1",
        "/reviews/1",
        "/threads/1",
        "/threads/1/replies/1",
    ] {
        let wrong_parent = format!("/api/repos/team/private/reactions/pulls/2{suffix}");
        assert_eq!(
            h.write(
                Method::PUT,
                &wrong_parent,
                &bob,
                json!({"kind":"heart","active":true})
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    h.close().await;
}
