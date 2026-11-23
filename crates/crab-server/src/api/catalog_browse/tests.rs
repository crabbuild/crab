use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::http::{Harness, browser::Browser},
};

async fn get(h: &Harness, cookie: &str, path: &str) -> reqwest::Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

async fn revoke_bob(h: &Harness, name: &str) {
    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            name,
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn pages_rank_matches_and_bind_continuations_to_account_query_and_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.add_repository("project-notes").await;
    h.add_repository("project-secret").await;
    revoke_bob(&h, "project-secret").await;
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version": 0, "archived": true, "repository": "team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;

    let first = h.json("/api/repos?q=PROJECT&limit=1", &bob).await;
    assert_eq!(first["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(first["repositories"][0]["name"], "project-notes");
    assert!(first["repositories"][0].get("protected_branches").is_none());
    let cursor = first["next"].as_str().unwrap();
    let path = format!("/api/repos?q=project&limit=1&after={cursor}");
    let second = h.json(&path, &bob).await;
    assert_eq!(second["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(second["repositories"][0]["name"], "private");
    assert_eq!(second["repositories"][0]["archived"], true);
    assert_eq!(second["next"], Value::Null);
    for (path, account) in [
        (path.clone(), &alice),
        (format!("/api/repos?q=notes&limit=1&after={cursor}"), &bob),
        (
            format!(
                "/api/repos?organization={}&q=project&after={cursor}",
                Uuid::now_v7()
            ),
            &bob,
        ),
        (format!("/api/repos?q=project&after={cursor}x"), &bob),
        ("/api/repos?q=project&limit=0".to_owned(), &bob),
    ] {
        assert_eq!(
            get(&h, account, &path).await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    let absent = h.json("/api/repos?q=secret", &bob).await;
    assert_eq!(absent, json!({"repositories":[],"next":null}));
    let qualified = h.json("/api/repos?q=TEAM%2FPRIVATE", &bob).await;
    assert_eq!(qualified["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(qualified["repositories"][0]["name"], "private");
    revoke_bob(&h, "project-notes").await;
    assert_eq!(get(&h, &bob, &path).await.status(), StatusCode::CONFLICT);
    let refreshed = h.json("/api/repos?q=project&limit=1", &bob).await;
    assert_eq!(refreshed["repositories"][0]["name"], "private");
    assert_eq!(refreshed["next"], Value::Null);

    let exact = h.json("/api/repos?owner=team&name=private", &bob).await;
    assert_eq!(
        exact["repositories"][0]["protected_branches"],
        json!([{"branch":"main","required_approvals":1,"required_checks":["ci/test"]}])
    );
    assert!(exact.get("next").is_none());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built React UI with the native catalog"]
async fn native_browser_searches_and_opens_an_authorized_repository() {
    let h = Harness::new(false).await;
    h.add_repository("project-notes").await;
    h.add_repository("project-secret").await;
    revoke_bob(&h, "project-secret").await;
    *h.provider.mode.lock().await = "member".into();
    let browser = Browser::new("repository-search");
    browser.run(&["open", &h.origin]).await;
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Your repositories"]).await;
    browser
        .run(&["fill", "input[name=q]", "TEAM/project"])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/?q=TEAM%2Fproject"])
        .await;
    browser.run(&["wait", "--text", "project-notes"]).await;
    let result = browser.run(&["snapshot"]).await;
    assert!(!result.contains("project-secret"));
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "team / project-notes",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/team/project-notes"])
        .await;
    browser.run(&["wait", "--text", "Code"]).await;
    browser.run(&["close"]).await;
    h.close().await;
}
