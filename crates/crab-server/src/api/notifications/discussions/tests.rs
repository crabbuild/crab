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
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Subscriptions"]).await;
    click("link", "Subscriptions").await;
    browser
        .run(&["wait", "--text", "Quiet followed discussion"])
        .await;
    browser
        .run(&["select", "select:has(option[value=oldest])", "oldest"])
        .await;
    browser
        .run(&[
            "select",
            "select:has(option[value=participation])",
            "participation",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Quiet followed discussion"])
        .await;
    let url = browser.run(&["get", "url"]).await;
    assert!(url.contains("sort=oldest") && url.contains("reason=participation"));
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

const BULK: &str = "/api/notifications/subscriptions/unsubscribe";

#[tokio::test]
async fn bulk_discussion_unsubscribe_preserves_newer_follows_and_saved_activity() {
    let h = Harness::new(false).await;
    for name in ["library", "restricted"] {
        h.add_repository(name).await;
    }
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let id = |name: &str| {
        h.server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap()
            .id
    };
    let put = async |cookie: &str,
                     repository: &str,
                     method: Method,
                     suffix: &str,
                     body: serde_json::Value| {
        let session = h.json("/api/session", cookie).await;
        h.http
            .request(
                method,
                format!("{}/api/repos/team/{repository}/{suffix}", h.origin),
            )
            .header("cookie", cookie)
            .header("origin", &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header("x-crab-repository-id", id(repository).to_string())
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    };
    for (repository, number) in [
        ("private", 1),
        ("private", 2),
        ("library", 1),
        ("restricted", 1),
    ] {
        put(&alice, repository, Method::POST, "issues", json!({
            "request_id": Uuid::now_v7(), "title": format!("Quiet {repository} {number}"), "body": ""
        })).await;
        put(
            &bob,
            repository,
            Method::PUT,
            &format!("issues/{number}/subscription"),
            json!({
                "mode":"follow","version":0
            }),
        )
        .await;
    }

    let comment = async |text: &str| {
        put(
            &alice,
            "private",
            Method::POST,
            "issues/1/comments",
            json!({"request_id":Uuid::now_v7(),"body":text}),
        )
        .await;
        h.deliver_notifications("team", "private").await;
    };
    comment("Existing inbox activity").await;
    let notice = h.json("/api/notifications", &bob).await["items"][0].clone();
    h.write(
        Method::PUT,
        &format!(
            "/api/notifications/{}/state",
            notice["id"].as_str().unwrap()
        ),
        &bob,
        json!({"repository_id":id("private"),"expected":notice["updated"],"action":{"save":true}}),
    )
    .await
    .error_for_status()
    .unwrap();
    let saved_before = h.json("/api/notifications?view=saved", &bob).await;
    assert_eq!(saved_before["items"][0]["id"], notice["id"]);
    put(
        &bob,
        "private",
        Method::PUT,
        "issues/2/subscription",
        json!({"mode":"state_changes","version":1}),
    )
    .await;
    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "restricted",
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();

    let selection = |repository: &str, number| {
        json!({
            "repository_id":id(repository),"subject":{"kind":"issue","number":number},"version":1
        })
    };
    let batch = json!({"items":[
        selection("private",1), selection("library",1), selection("private",2),
        selection("restricted",1), selection("library",99)
    ]});
    assert_eq!(
        h.http
            .post(format!("{}{BULK}", h.origin))
            .header("cookie", &bob)
            .header("origin", &h.origin)
            .json(&batch)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.write(
            Method::POST,
            BULK,
            &bob,
            json!({"items":[selection("private",1),selection("private",1)]})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );

    let status = |repository: &str, number, status: &str| {
        json!({
            "repository_id":id(repository),"subject":{"kind":"issue","number":number},"status":status
        })
    };
    let expected = json!({"items":[
        status("private",1,"unsubscribed"),status("library",1,"unsubscribed"),
        status("private",2,"changed"),status("restricted",1,"unavailable"),status("library",99,"unavailable")
    ]});
    let submit = async || {
        h.write(Method::POST, BULK, &bob, batch.clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };
    assert_eq!(submit().await, expected);
    assert_eq!(submit().await, expected);
    let page = h.json(LIST, &bob).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["items"][0]["preference"],
        json!({"mode":"state_changes","version":2})
    );
    comment("Ordinary update after unsubscribe").await;
    assert_eq!(
        h.json("/api/notifications?view=saved", &bob).await,
        saved_before
    );

    put(
        &bob,
        "private",
        Method::PUT,
        "issues/1/subscription",
        json!({"mode":"follow","version":2}),
    )
    .await;
    assert_eq!(submit().await["items"][0]["status"], "changed");
    assert_eq!(
        h.json(&format!("{REPO}/issues/1/subscription"), &bob).await,
        json!({"mode":"follow","version":3})
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_unsubscribes_selected_quiet_discussions() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    issue(&h, &alice, "First followed discussion").await;
    issue(&h, &alice, "Second followed discussion").await;

    let browser = Browser::new("unfollow-bulk");
    browser
        .run(&[
            "open",
            &format!("{}/-/notifications/subscriptions", h.origin),
        ])
        .await;
    let click = async |role: &str, name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    click("checkbox", "Select all loaded subscriptions").await;
    click("button", "Unsubscribe selected").await;
    browser
        .run(&["wait", "--text", "Unsubscribe from discussions"])
        .await;
    assert_eq!(
        h.json(LIST, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    browser
        .run(&[
            "wait",
            "--fn",
            r#"(() => {
                const button = document.querySelector('[role=dialog] button[data-variant=destructive]');
                if (!button) return false;
                const box = button.getBoundingClientRect();
                return button.contains(document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2));
            })()"#,
        ])
        .await;
    click("button", "Unsubscribe").await;
    browser
        .run(&["wait", "--text", "Unsubscribed from 2 discussions."])
        .await;
    for number in 1..=2 {
        assert_eq!(
            h.json(&format!("{REPO}/issues/{number}/subscription"), &alice)
                .await,
            json!({"mode":"unsubscribed","version":2})
        );
    }
    click("button", "Close").await;
    browser
        .run(&[
            "wait",
            "--text",
            "No discussion subscriptions match this view.",
        ])
        .await;
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
