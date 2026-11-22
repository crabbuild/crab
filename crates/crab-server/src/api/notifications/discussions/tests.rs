use axum::http::StatusCode;
use reqwest::Method;
use serde_json::json;
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::{
        git,
        http::{Harness, browser::Browser},
    },
};

const LIST: &str = "/api/notifications/subscriptions";
const REPO: &str = "/api/repos/team/private";

async fn issue(h: &Harness, cookie: &str, title: &str) {
    h.write(
        Method::POST,
        &format!("{REPO}/issues"),
        cookie,
        json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test]
async fn discussion_subscriptions_page_quiet_follows_and_recheck_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for number in 1..=33 {
        issue(&h, &alice, &format!("Quiet issue {number}")).await;
    }

    let token: serde_json::Value = h
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
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let checkout = tempfile::tempdir().unwrap();
    git::push_review_branches(checkout.path(), remote.as_str()).await;
    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Quiet pull","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
    )
    .await
    .error_for_status()
    .unwrap();

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    for discussion in ["issues/33", "pulls/1"] {
        h.write(
            Method::PUT,
            &format!("{REPO}/{discussion}/subscription"),
            &bob,
            json!({"mode":"state_changes","version":0}),
        )
        .await
        .error_for_status()
        .unwrap();
    }

    let repository = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap()
        .id;
    let first = h.json(LIST, &bob).await;
    assert_eq!(
        first,
        json!({"items":[],"next":format!("{repository}:issue:32")})
    );
    let next = format!("{LIST}?after={repository}%3Aissue%3A32");
    let page = h.json(&next, &bob).await;
    let rows = page["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["subject"], json!({"kind":"issue","number":33}));
    assert_eq!(rows[0]["title"], "Quiet issue 33");
    assert_eq!(
        rows[0]["preference"],
        json!({"mode":"state_changes","version":1})
    );
    assert_eq!(rows[1]["subject"], json!({"kind":"pull","number":1}));
    assert_eq!(page["next"], serde_json::Value::Null);
    assert_eq!(h.json("/api/notifications", &bob).await["items"], json!([]));

    let pulls = format!("{LIST}?repository={repository}&kind=pull");
    assert_eq!(
        h.json(&pulls, &alice).await["items"][0]["preference"]["mode"],
        "follow"
    );
    assert_eq!(h.json(&pulls, &bob).await["items"], json!([rows[1]]));
    h.write(
        Method::PUT,
        &format!("{REPO}/subscription"),
        &bob,
        json!({"mode":"muted","version":0}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&pulls, &bob).await["items"][0]["muted"], true);

    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "private",
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    assert_eq!(h.json(LIST, &bob).await, json!({"items":[],"next":null}));
    for (path, status) in [
        (&pulls, StatusCode::NOT_FOUND),
        (&next, StatusCode::CONFLICT),
    ] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    h.close().await;
}

#[tokio::test]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_manages_quiet_discussion_subscriptions() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    issue(&h, &alice, "Quiet followed discussion").await;
    assert_eq!(
        h.json("/api/notifications", &alice).await["items"],
        json!([])
    );

    let browser = Browser::new("subscriptions");
    browser
        .run(&["open", &format!("{}/-/notifications", h.origin)])
        .await;
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    click("button", "Continue to sign in").await;
    browser.run(&["wait", "--text", "Subscriptions"]).await;
    click("link", "Subscriptions").await;
    browser
        .run(&["wait", "--text", "Quiet followed discussion"])
        .await;
    let settings = "Notification settings for team/private issue #1";
    click("button", settings).await;
    browser.run(&["wait", "[role=dialog] select"]).await;
    browser
        .run(&["select", "[role=dialog] select", "state_changes"])
        .await;
    click("button", "Save preference").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    assert_eq!(
        h.json(LIST, &alice).await["items"][0]["preference"]["mode"],
        "state_changes"
    );

    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Quiet followed discussion"])
        .await;
    click("button", settings).await;
    browser.run(&["wait", "[role=dialog] select"]).await;
    browser
        .run(&["select", "[role=dialog] select", "unsubscribed"])
        .await;
    click("button", "Save preference").await;
    browser
        .run(&[
            "wait",
            "--text",
            "No discussion subscriptions match this view.",
        ])
        .await;
    assert_eq!(
        h.json(&format!("{REPO}/issues/1/subscription"), &alice)
            .await["mode"],
        "unsubscribed"
    );
    browser.run(&["reload"]).await;
    browser
        .run(&[
            "wait",
            "--text",
            "No discussion subscriptions match this view.",
        ])
        .await;
    drop(browser);
    h.close().await;
}
