use super::*;
use crate::test_support::http::Harness;
use reqwest::{Method, header};
use serde_json::{Value, json};

const ENDPOINT: &str = "/api/personal-work";

async fn issue(h: &Harness, repository: &str, cookie: &str, title: &str) -> Value {
    let session = h.json("/api/session", cookie).await;
    let repo = h
        .server
        .repositories
        .get(&("team".into(), repository.into()))
        .unwrap();
    h.http
        .post(format!("{}/api/repos/team/{repository}/issues", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repo.id.to_string())
        .json(&json!({"request_id":Uuid::now_v7(),"title":title,"body":""}))
        .send()
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn revoke(h: &Harness, repository: &str) {
    let catalog = h.server.catalog().unwrap();
    let mut members = catalog
        .membership("team", repository)
        .await
        .unwrap()
        .members;
    members.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", repository, members, true)
        .await
        .unwrap();
}

async fn get(h: &Harness, path: &str, cookie: &str) -> reqwest::Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(axum::http::header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn personal_work_pages_across_current_grants_and_uses_typed_links() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    h.add_repository("restricted").await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let authored = issue(&h, "private", &bob, "Preserve Title Case").await;
    issue(&h, "library", &bob, "Library issue").await;
    issue(&h, "restricted", &bob, "Restricted issue").await;
    revoke(&h, "restricted").await;

    let path = format!("{ENDPOINT}?kind=issue&limit=1");
    let first = h.json(&path, &bob).await;
    let next = first["next"].as_str().unwrap();
    let continuation = format!("{path}&after={next}");
    let second = h.json(&continuation, &bob).await;
    assert_eq!(second["next"], Value::Null);
    let visible: std::collections::BTreeSet<_> = [&first, &second]
        .into_iter()
        .map(|page| {
            assert_eq!(page["items"].as_array().unwrap().len(), 1);
            &page["items"][0]
        })
        .map(|row| {
            (
                row["repository"]["name"].as_str().unwrap(),
                row["title"].as_str().unwrap(),
                row["number"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        visible,
        [
            ("private", "Preserve Title Case", 1),
            ("library", "Library issue", 1)
        ]
        .into()
    );
    assert_eq!(
        get(&h, &continuation, &alice).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        get(&h, &format!("{path}&relation=assigned&after={next}"), &bob)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(get(&h, &path, "").await.status(), StatusCode::UNAUTHORIZED);

    let bob_id = h.assignee_id(&alice, "Bob").await;
    let issue_path = "/api/repos/team/private/issues/1";
    h.write(
        Method::PATCH,
        issue_path,
        &alice,
        json!({
            "version":authored["version"],"assignee_ids":[bob_id],"state":"closed",
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let assigned = h
        .json(
            &format!("{ENDPOINT}?kind=issue&relation=assigned&state=closed"),
            &bob,
        )
        .await;
    assert_eq!(assigned["items"][0]["title"], "Preserve Title Case");
    assert_eq!(assigned["items"][0]["state"], "closed");
    assert_eq!(
        h.json(&format!("{ENDPOINT}?kind=issue&relation=assigned"), &bob)
            .await["items"],
        json!([])
    );

    revoke(&h, "library").await;
    assert_eq!(
        get(&h, &continuation, &bob).await.status(),
        StatusCode::CONFLICT
    );
    let current = h
        .json(&format!("{ENDPOINT}?kind=issue&state=all"), &bob)
        .await;
    assert_eq!(current["items"].as_array().unwrap().len(), 1);
    assert_eq!(current["items"][0]["repository"]["name"], "private");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_opens_personal_work_from_multiple_repositories() {
    use crate::test_support::http::browser::Browser;
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let alice = h.login().await;
    issue(&h, "private", &alice, "Fix the Private docs").await;
    issue(&h, "library", &alice, "Update the Library guide").await;
    let browser = Browser::new("my-work");
    browser.run(&["open", &h.origin]).await;
    browser.sign_in().await;
    browser.run(&["wait", "a[aria-label='My work']"]).await;
    browser.run(&["click", "a[aria-label='My work']"]).await;
    browser.run(&["wait", "nav[aria-label='Work type']"]).await;
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Issues", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Fix the Private docs"])
        .await;
    browser
        .run(&["wait", "--text", "Update the Library guide"])
        .await;
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Fix the Private docs"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Update the Library guide",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/team/library/issues/1*"])
        .await;
    browser
        .run(&["wait", "--text", "Update the Library guide"])
        .await;
    drop(browser);
    h.close().await;
}
