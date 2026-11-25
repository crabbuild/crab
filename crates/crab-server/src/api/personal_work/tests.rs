use super::*;
use crate::test_support::http::Harness;
use reqwest::{Method, header};
use serde_json::{Value, json};
use std::cmp::Reverse;

const ENDPOINT: &str = "/api/personal-work";
const SEARCH: &str = "/api/discussion-search";

async fn issue(h: &Harness, repository: &str, cookie: &str, title: &str) -> Value {
    write(
        h,
        repository,
        cookie,
        Method::POST,
        "issues",
        json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
    )
    .await
}

async fn write(
    h: &Harness,
    repository: &str,
    cookie: &str,
    method: Method,
    path: &str,
    body: Value,
) -> Value {
    let session = h.json("/api/session", cookie).await;
    let repo = h
        .server
        .repositories
        .get(&("team".into(), repository.into()))
        .unwrap();
    h.http
        .request(
            method,
            format!("{}/api/repos/team/{repository}/{path}", h.origin),
        )
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repo.id.to_string())
        .json(&body)
        .send()
        .await
        .unwrap()
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
async fn workspace_discussion_search_combines_types_and_rechecks_repository_scope() {
    use crate::test_support::git;

    let h = Harness::new(false).await;
    h.add_repository("library").await;
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
    let clone = tempfile::tempdir().unwrap();
    git::push_review_branches(clone.path(), remote.as_str()).await;
    h.write(
        Method::POST,
        "/api/repos/team/private/pulls",
        &alice,
        json!({"request_id":Uuid::now_v7(), "title":"Release pipeline",
            "body":"", "base_ref":"refs/heads/main", "head_ref":"refs/heads/feature"}),
    )
    .await
    .error_for_status()
    .unwrap();
    issue(&h, "private", &alice, "Release guide").await;
    issue(&h, "library", &alice, "Release checklist").await;

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let query =
        format!("{SEARCH}?q=release+AND+%28guide+OR+pipeline+OR+checklist%29&sort=updated&limit=1");
    let mut next = query.clone();
    let mut rows = Vec::new();
    let mut continuation = None;
    for _ in 0..3 {
        let response = h.json(&next, &bob).await;
        let item = &response["items"][0];
        assert_eq!(item["number"], 1);
        rows.push((
            item["kind"].as_str().unwrap().to_owned(),
            item["title"].as_str().unwrap().to_owned(),
        ));
        if let Some(cursor) = response["next"].as_str() {
            next = format!("{query}&after={cursor}");
            continuation.get_or_insert_with(|| next.clone());
        } else {
            assert_eq!(rows.len(), 3);
        }
    }
    rows.sort_unstable();
    assert_eq!(
        rows,
        [
            ("issue".into(), "Release checklist".into()),
            ("issue".into(), "Release guide".into()),
            ("pull".into(), "Release pipeline".into()),
        ]
    );
    let exact = format!("{SEARCH}?q=release&scope=team%2Flibrary&kind=issue");
    assert_eq!(
        h.json(&exact, &bob).await["items"][0]["title"],
        "Release checklist"
    );
    let branches = format!("{SEARCH}?q=base%3Amain+head%3Afeat&scope=team");
    let matching = h.json(&branches, &bob).await;
    let items = matching["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "Release pipeline");
    assert_eq!(
        h.json(&format!("{SEARCH}?q=author%3A%40me&scope=team"), &alice)
            .await["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        h.json(&format!("{SEARCH}?q=author%3A%40me&scope=team"), &bob)
            .await["items"],
        json!([])
    );
    assert_eq!(get(&h, &query, "").await.status(), StatusCode::UNAUTHORIZED);

    write(
        &h,
        "library",
        &alice,
        Method::POST,
        "issues/1/comments",
        json!({"request_id":Uuid::now_v7(),"body":"Review the checklist"}),
    )
    .await;
    let mut issue_reactions = Vec::new();
    for (repository, title) in [
        ("private", "Release guide"),
        ("library", "Release checklist"),
    ] {
        write(
            &h,
            repository,
            &alice,
            Method::PUT,
            "reactions/issues/1",
            json!({"kind":"heart","active":true}),
        )
        .await;
        let repo = h
            .server
            .repositories
            .get(&("team".into(), repository.into()))
            .unwrap();
        issue_reactions.push((repo.id, (title, 1)));
    }
    issue_reactions.sort_unstable_by_key(|(id, _)| Reverse(*id));
    let most_reactions = [
        issue_reactions[0].1,
        issue_reactions[1].1,
        ("Release pipeline", 0),
    ];
    let most_comments = [
        ("Release checklist", 1),
        ("Release pipeline", 0),
        ("Release guide", 0),
    ];
    let mut comment_cursor = None;
    for (sort, descending) in [("comments", most_comments), ("reactions", most_reactions)] {
        for direction in ["asc", "desc"] {
            let route = format!("{SEARCH}?q=release&sort={sort}&direction={direction}&limit=1");
            let mut expected = descending;
            if direction == "asc" {
                expected.reverse();
            }
            let mut continuation = route.clone();
            for (offset, (title, count)) in expected.into_iter().enumerate() {
                let result = h.json(&continuation, &bob).await;
                assert_eq!(result["items"].as_array().unwrap().len(), 1);
                assert_eq!(result["items"][0]["title"], title);
                assert_eq!(result["items"][0]["sort_value"], count);
                if offset == 2 {
                    assert_eq!(result["next"], Value::Null);
                } else {
                    let cursor = result["next"].as_str().unwrap();
                    continuation = format!("{route}&after={cursor}");
                    if offset == 0 && sort == "comments" && direction == "asc" {
                        let reverse = continuation.replace("direction=asc", "direction=desc");
                        assert_eq!(get(&h, &reverse, &bob).await.status(), StatusCode::CONFLICT);
                        comment_cursor = Some(continuation.clone());
                    }
                }
            }
        }
    }
    write(
        &h,
        "private",
        &alice,
        Method::POST,
        "issues/1/comments",
        json!({"request_id":Uuid::now_v7(),"body":"A new comment moves this issue"}),
    )
    .await;
    assert_eq!(
        get(&h, &comment_cursor.unwrap(), &bob).await.status(),
        StatusCode::CONFLICT
    );

    revoke(&h, "library").await;
    assert_eq!(
        get(&h, &continuation.unwrap(), &bob).await.status(),
        StatusCode::CONFLICT
    );
    let missing = format!("{SEARCH}?q=release&scope=team%2Fmissing&kind=issue");
    assert_eq!(h.json(&exact, &bob).await, h.json(&missing, &bob).await);
    revoke(&h, "private").await;
    assert_eq!(h.json(&branches, &bob).await["items"], json!([]));
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
    write(
        &h,
        "library",
        &alice,
        Method::POST,
        "issues/1/comments",
        json!({"request_id":Uuid::now_v7(),"body":"Review the guide"}),
    )
    .await;
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
            "Search issues and PRs",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#discussion-search-terms"]).await;
    browser
        .run(&["fill", "#discussion-search-terms", "is:open"])
        .await;
    browser
        .choose_select("#discussion-search-sort", "comments")
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Update the Library guide"])
        .await;
    let first = browser
        .run(&[
            "get",
            "text",
            "ul[aria-label='Matching work'] > li:first-child",
        ])
        .await;
    assert!(first.contains("Update the Library guide"));
    assert!(first.contains("1 comment"));
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Update the Library guide"])
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
