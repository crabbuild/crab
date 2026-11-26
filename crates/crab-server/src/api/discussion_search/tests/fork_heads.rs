use super::*;
use crate::test_support::{
    git,
    http::{browser::Browser, organizations},
};
use reqwest::Method;
use std::time::Duration;
use uuid::Uuid;

pub(super) async fn ready_search(h: &Harness, actor: &str, path: &str, query: &str) -> Value {
    let mut url = url::Url::parse(&format!("{}{path}", h.origin)).unwrap();
    url.query_pairs_mut().append_pair("q", query);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let response = h
                .http
                .get(url.clone())
                .header(header::COOKIE, actor)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            if status == StatusCode::OK {
                return body;
            }
            assert_eq!(
                (status, body["error"]["code"].as_str()),
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Some("review_index_pending")
                ),
                "{body}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("Git source projection did not become ready")
}

async fn write(
    h: &Harness,
    actor: &str,
    repository: &Value,
    method: Method,
    path: &str,
    body: Value,
) {
    let session = h.json("/api/session", actor).await;
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, actor)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository.as_str().unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.text().await.unwrap()
    );
}

async fn remote(h: &Harness, actor: &str, owner: &str, name: &str) -> url::Url {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            actor,
            json!({"owner":owner,"repository":name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut url = url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn review_and_check_search_follow_the_exact_upstream_and_fork_heads() {
    exercise(None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn browser_reclassifies_a_fork_review_after_a_push() {
    exercise(Some(&Browser::new("fork-review-search"))).await;
}

async fn exercise(browser: Option<&Browser>) {
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "check-status-fork").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    organizations::set_private_forks(&h, organization, &alice, true).await;
    let org = format!("/api/organizations/{organization}");
    let repo: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"upstream","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = repo["owner"].as_str().unwrap();
    let id = &repo["id"];
    let root = format!("/api/repos/{owner}/upstream");
    let pulls = format!("{root}/pulls");
    let upstream = remote(&h, &alice, owner, "upstream").await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let shared = git::push_review_branches(path, upstream.as_str()).await;
    let base = git::success(path, &["rev-parse", "main"]).await;
    git::success(path, &["push", upstream.as_str(), "main:release"]).await;
    let fork_request = Uuid::now_v7();
    write(
        &h,
        &alice,
        id,
        Method::POST,
        &format!("{root}/forks"),
        json!({"request_id":fork_request,"name":"fork"}),
    )
    .await;
    let job = format!("{org}/repositories/imports/git/{fork_request}");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let state = h.json(&job, &alice).await;
            match state["state"].as_str() {
                Some("succeeded") => break,
                Some("queued" | "running") => tokio::time::sleep(Duration::from_millis(100)).await,
                _ => panic!("Fork import did not complete: {state}"),
            }
        }
    })
    .await
    .unwrap();
    let fork = h
        .server
        .repositories
        .get(&(owner.to_owned(), "fork".into()))
        .unwrap();
    for (title, source, base_ref) in [
        ("Upstream change", None, "refs/heads/main"),
        ("Fork change", Some(fork.id), "refs/heads/release"),
    ] {
        write(
            &h,
            &alice,
            id,
            Method::POST,
            &pulls,
            json!({
                "request_id":Uuid::now_v7(),"title":title,"body":"",
                    "base_ref":base_ref,"head_ref":"refs/heads/feature","head_repository":source,
                    "expected":{"base_oid":base,"head_oid":shared},
            }),
        )
        .await;
    }
    let members = format!("{root}/members");
    let acl = h.json(&members, &alice).await;
    write(
        &h,
        &alice,
        id,
        Method::PUT,
        &members,
        json!({
            "expected_revision":acl["revision"],
            "members":[{"subject":"bob-id","name":"Bob","access":"write"}],"groups":[],
        }),
    )
    .await;
    write(
        &h,
        &alice,
        id,
        Method::PUT,
        &format!("{root}/settings/branch-protections"),
        json!({
            "expected_version":0,"rules":[
                {"branch":"main","required_approvals":1,"required_checks":[]},
                {"branch":"release","required_approvals":1,"required_checks":[]},
            ],
        }),
    )
    .await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    for number in 1..=2 {
        write(&h, &bob, id, Method::POST, &format!("{pulls}/{number}/reviews"), json!({
            "request_id":Uuid::now_v7(),"state":"approved","body":"Approved at the shared head","commit_oid":shared,
        })).await;
    }
    let searches = [
        format!("{pulls}?limit=1"),
        format!("/api/discussion-search?scope={owner}%2Fupstream&kind=pull&limit=1"),
    ];
    let status = |sha: &str, scope: &str| format!("{root}/statuses/{sha}{scope}");
    write(
        &h,
        &alice,
        id,
        Method::POST,
        &status(&shared, ""),
        json!({
            "request_id":Uuid::now_v7(),"context":"ci/build","state":"success",
        }),
    )
    .await;
    let approved = "is:open review:approved";
    let upstream_link = format!(".issue-list a[href='/{owner}/upstream/pull/1']");
    let fork_link = format!(".issue-list a[href='/{owner}/upstream/pull/2']");
    let mut continuations = Vec::new();
    for (search, cursor_parameter) in searches.iter().zip(["before", "after"]) {
        for (query, title, has_more) in [
            (
                "base:main head:feat review:approved",
                "Upstream change",
                false,
            ),
            (
                "base:release head:feat review:approved",
                "Fork change",
                false,
            ),
            (
                "(base:main OR status:success) review:approved",
                "Fork change",
                true,
            ),
        ] {
            let result = ready_search(&h, &alice, search, query).await;
            assert_eq!(result["items"][0]["title"], title, "{query}");
            assert_eq!(result["next"].is_string(), has_more, "{query}");
        }
        for query in [
            approved,
            "is:open status:success",
            "review:approved status:success",
        ] {
            let first = ready_search(&h, &alice, search, query).await;
            assert_eq!(first["items"][0]["title"], "Fork change");
            continuations.push((
                search,
                cursor_parameter,
                query,
                first["next"].as_str().unwrap().to_owned(),
            ));
        }
    }
    if let Some(browser) = browser {
        browser
            .run(&["open", &format!("{}/{owner}/upstream/pulls", h.origin)])
            .await;
        browser.sign_in().await;
        browser.run(&["wait", "--text", "Fork change"]).await;
        select_review_filter(browser, "Approved review").await;
        browser
            .run(&[
                "wait",
                "--fn",
                "new URL(location.href).searchParams.get('q')?.includes('review:approved')",
            ])
            .await;
        browser.run(&["wait", &upstream_link]).await;
        browser.run(&["wait", &fork_link]).await;
        let search = "input[aria-label='Search pulls']";
        browser
            .run(&["fill", search, "base:main head:feat review:approved"])
            .await;
        browser.run(&["press", "Enter"]).await;
        browser
            .run(&[
                "wait",
                "--fn",
                &format!(
                    "new URL(location.href).searchParams.get('q')?.includes('base:main') && !document.querySelector({})",
                    serde_json::to_string(&fork_link).unwrap()
                ),
            ])
            .await;
        browser.run(&["wait", &upstream_link]).await;
        browser.run(&["fill", search, approved]).await;
        browser.run(&["press", "Enter"]).await;
        browser.run(&["wait", &fork_link]).await;
    }
    let fork_remote = remote(&h, &alice, owner, "fork").await;
    std::fs::write(path.join("README.md"), "Only the fork advanced\n").unwrap();
    git::success(path, &["commit", "-am", "Advance fork only"]).await;
    let next = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", fork_remote.as_str(), "feature"]).await;

    for (search, cursor_parameter, query, cursor) in continuations {
        let continued = h
            .http
            .get(format!("{}{search}", h.origin))
            .header(header::COOKIE, &alice)
            .query(&[("q", query), (cursor_parameter, cursor.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(continued.status(), StatusCode::CONFLICT);
    }
    for search in &searches {
        for (query, title) in [
            ("status:success", "Upstream change"),
            ("status:pending", "Fork change"),
            ("review:approved", "Upstream change"),
            ("review:required status:pending", "Fork change"),
        ] {
            let result = ready_search(&h, &alice, search, query).await;
            assert_eq!(result["items"][0]["title"], title, "{query}");
            assert_eq!(result["next"], Value::Null, "{query}");
        }
    }
    if let Some(browser) = browser {
        browser.run(&["reload"]).await;
        browser.run(&["wait", &upstream_link]).await;
        browser
            .run(&[
                "wait",
                "--fn",
                &format!(
                    "!document.querySelector({})",
                    serde_json::to_string(&fork_link).unwrap()
                ),
            ])
            .await;
        select_review_filter(browser, "Review required").await;
        browser
            .run(&[
                "wait",
                "--fn",
                "new URL(location.href).searchParams.get('q')?.includes('review:required')",
            ])
            .await;
        browser.run(&["wait", &fork_link]).await;
    }
    let pending = ready_search(&h, &alice, &pulls, "review:required").await;
    assert_eq!(pending["matching"], 1);
    assert_eq!(pending["counts"]["open"], 1);
    assert_eq!(pending["items"][0]["rollup"]["head_oid"], next);
    assert_eq!(pending["items"][0]["rollup"]["reviews"]["outdated"], 1);
    for (context, state) in [("ci/lint", "pending"), ("ci/build", "failure")] {
        write(
            &h,
            &alice,
            id,
            Method::POST,
            &status(&next, "?pull=2"),
            json!({
                "request_id":Uuid::now_v7(),"context":context,"state":state,
            }),
        )
        .await;
    }
    let failed = ready_search(&h, &alice, &format!("{root}/issues"), "status:failure").await;
    assert_eq!(failed["matching"], 1);
    assert_eq!(failed["items"][0]["rollup"]["checks"]["failed"], 1);
    assert_eq!(
        ready_search(&h, &alice, &pulls, "status:pending").await["matching"],
        0
    );
    write(
        &h,
        &alice,
        id,
        Method::POST,
        &status(&next, "?pull=2"),
        json!({
            "request_id":Uuid::now_v7(),"context":"ci/build","state":"success",
        }),
    )
    .await;
    assert_eq!(
        ready_search(&h, &alice, &pulls, "status:pending").await["matching"],
        1
    );
    assert_eq!(
        ready_search(&h, &alice, &pulls, "-status:failure").await["matching"],
        2
    );
    git::success(path, &["push", fork_remote.as_str(), "--delete", "feature"]).await;
    assert_eq!(
        ready_search(&h, &alice, &pulls, "status:pending").await["matching"],
        0
    );
    let first = h.json(&format!("{pulls}/1"), &alice).await;
    write(
        &h,
        &alice,
        id,
        Method::POST,
        &format!("{pulls}/1/merge"),
        json!({
            "request_id":Uuid::now_v7(),"version":first["version"],"method":"merge_commit",
            "base_oid":first["base_oid"],"head_oid":shared,
        }),
    )
    .await;
    git::success(path, &["push", upstream.as_str(), "--delete", "feature"]).await;
    let merged = ready_search(
        &h,
        &alice,
        &pulls,
        "is:merged review:approved status:success",
    )
    .await;
    assert_eq!(merged["matching"], 1);
    assert_eq!(merged["items"][0]["rollup"]["head_oid"], shared);
    h.close().await;
}

async fn select_review_filter(browser: &Browser, name: &str) {
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Filter by reviews",
            "--exact",
        ])
        .await;
    let input = "[role='dialog'] input[placeholder='Find a review filter…']";
    browser.run(&["wait", input]).await;
    browser.run(&["fill", input, name]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.querySelectorAll('[role=listbox][aria-label=Reviews] [role=option]').length === 1",
        ])
        .await;
    browser.run(&["press", "ArrowDown"]).await;
    browser.run(&["press", "Enter"]).await;
}
