use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};

const ROOT: &str = "/api/repos/team/private";
const COMMENT: &str = "issues/1/comments/1";
const BODY: &str = "The original issue contribution";

async fn grant(h: &Harness, owner: &str, access: &str) {
    let path = format!("{ROOT}/members");
    let current = h.json(&path, owner).await;
    h.write(
        Method::PUT,
        &path,
        owner,
        json!({"expected_revision":current["revision"],"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"},
            {"subject":"bob-id","name":"Bob","access":access}
        ],"groups":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn issue() -> (Harness, String, String) {
    let h = Harness::new(false).await;
    let owner = h.login().await;
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        &owner,
        json!({"request_id":Uuid::now_v7(),"title":"Issue moderation","body":"Description"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{ROOT}/issues/1/comments"),
        &owner,
        json!({"request_id":Uuid::now_v7(),"body":BODY}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let member = h.login().await;
    (h, owner, member)
}

async fn change(
    h: &Harness,
    cookie: &str,
    target: &str,
    version: u64,
    change: Value,
) -> reqwest::Response {
    h.write(
        Method::PUT,
        &format!("{ROOT}/comment-visibility/{target}"),
        cookie,
        json!({"version":version,"change":change}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_minimization_is_versioned_and_visible_to_readers_without_changing_the_comment() {
    let (h, owner, member) = issue().await;
    let hide = json!({"kind":"hide","reason":"off_topic"});
    let unhide = json!({"kind":"unhide"});
    grant(&h, &owner, "triage").await;
    assert_eq!(
        change(&h, &member, COMMENT, 0, hide.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    grant(&h, &owner, "moderate").await;
    assert_eq!(
        change(&h, &member, "issues/1", 0, hide.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    for _ in 0..2 {
        let saved: Value = change(&h, &member, COMMENT, 0, hide.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(saved, json!({"version":1,"reason":"off_topic"}));
    }
    assert_eq!(
        change(&h, &owner, COMMENT, 0, unhide.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    grant(&h, &owner, "read").await;
    let comment = h.json(&format!("{ROOT}/{COMMENT}"), &member).await;
    assert_eq!(
        comment["minimization"],
        json!({"version":1,"reason":"off_topic"})
    );
    assert_eq!(comment["body"], BODY);
    assert_eq!(comment["version"], 1);
    assert_eq!(comment["author"]["name"], "Alice");
    assert_eq!(
        change(&h, &member, COMMENT, 1, unhide.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    change(&h, &owner, COMMENT, 1, unhide)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        change(&h, &owner, COMMENT, 0, hide).await.status(),
        StatusCode::CONFLICT
    );
    let view = h
        .json(&format!("{ROOT}/issues/1/conversation"), &member)
        .await;
    let entries = view["items"].as_array().unwrap();
    let events: Vec<_> = entries
        .iter()
        .filter(|entry| entry["kind"] == "activity")
        .map(|entry| &entry["value"])
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0]["action"],
        json!({"kind":"comment_hidden","reason":"off_topic"})
    );
    assert_eq!(events[0]["actor"]["name"], "Bob");
    assert_eq!(events[1]["action"]["kind"], "comment_restored");
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry["kind"] == "comment")
            .unwrap()["value"]["minimization"],
        json!({"version":2,"reason":null})
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_minimization_covers_comments_reviews_and_inline_contributions() {
    let (h, owner, member) = issue().await;
    grant(&h, &owner, "moderate").await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &owner,
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
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    let pull: Value = h
        .write(
            Method::POST,
            &format!("{ROOT}/pulls"),
            &owner,
            json!({"request_id":Uuid::now_v7(),"title":"Review moderation","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let inline = json!({
        "request_id":Uuid::now_v7(),"body":"Inline original","side":"new","start_line":2,"end_line":2,
        "path_hex":"524541444d452e6d64","base_oid":pull["base_oid"],"head_oid":head,
        "suggested_text":"A suggested change"
    });
    let examples = [
        (
            "pulls/1/comments",
            json!({"request_id":Uuid::now_v7(),"body":"Original comment"}),
        ),
        (
            "pulls/1/reviews",
            json!({"request_id":Uuid::now_v7(),"body":"Original approval","state":"approved","commit_oid":head}),
        ),
        ("pulls/1/threads", inline),
        (
            "pulls/1/threads/1/replies",
            json!({"request_id":Uuid::now_v7(),"body":"Original reply"}),
        ),
    ];
    for (route, input) in &examples {
        h.write(
            Method::POST,
            &format!("{ROOT}/{route}"),
            &member,
            input.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let hide = json!({"kind":"hide","reason":"outdated"});
    assert_eq!(
        change(&h, &member, "pulls/1/threads/1", 0, hide.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN,
    );
    grant(&h, &owner, "write").await;
    for (route, input) in &examples {
        let target = format!("{route}/1");
        change(&h, &member, &target, 0, hide.clone())
            .await
            .error_for_status()
            .unwrap();
        let list = h.json(&format!("{ROOT}/{route}"), &owner).await;
        let item = &list["items"][0];
        assert_eq!(item["body"], input["body"], "{target}");
        assert_eq!(
            item["minimization"],
            json!({"version":1,"reason":"outdated"}),
            "{target}"
        );
        assert_eq!(
            h.json(&format!("{ROOT}/{target}"), &owner).await["minimization"],
            item["minimization"]
        );
        if *route == "pulls/1/reviews" {
            assert_eq!(item["state"], "approved");
        }
        if *route == "pulls/1/threads" {
            assert_eq!(item["suggested_text"], "A suggested change");
            assert_eq!(item["resolved"], false);
        }
    }
    let activity = h
        .json(&format!("{ROOT}/pulls/1/conversation"), &owner)
        .await;
    assert_eq!(
        activity["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["value"]["action"]["kind"] == "comment_hidden")
            .count(),
        4
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn embedded_browser_hides_reveals_and_restores_an_issue_comment() {
    let (h, owner, member) = issue().await;
    grant(&h, &owner, "moderate").await;
    let browser = Browser::new("comment-visibility");
    browser
        .run(&["open", &format!("{}/team/private/issues/1", h.origin)])
        .await;
    let click = async |name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    click("Hide comment").await;
    browser
        .run(&["select", "[role=dialog] select", "spam"])
        .await;
    click("Hide comment").await;
    browser
        .run(&["wait", "--text", "Comment hidden · Spam"])
        .await;
    let snapshot = browser.run(&["snapshot"]).await;
    assert!(!snapshot.contains(BODY));
    assert_eq!(
        h.json(&format!("{ROOT}/{COMMENT}"), &member).await["minimization"]["reason"],
        "spam"
    );

    grant(&h, &owner, "read").await;
    browser.run(&["reload"]).await;
    click("Show comment").await;
    browser.run(&["wait", "--text", BODY]).await;
    assert!(!browser.run(&["snapshot"]).await.contains("Unhide comment"));

    grant(&h, &owner, "moderate").await;
    browser.run(&["reload"]).await;
    click("Show comment").await;
    click("Unhide comment").await;
    browser.run(&["wait", "--text", "Hide comment"]).await;
    assert_eq!(
        h.json(&format!("{ROOT}/{COMMENT}"), &member).await["minimization"],
        json!({"version":2,"reason":null})
    );
    assert!(browser.run(&["snapshot"]).await.contains(BODY));
    browser.run(&["close"]).await;
    h.close().await;
}
