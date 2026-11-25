use super::*;
use crate::test_support::{
    git,
    http::{browser::Browser, organizations},
};
use reqwest::Method;
use uuid::Uuid;

async fn write(
    h: &Harness,
    cookie: &str,
    method: Method,
    repository: &str,
    path: &str,
    body: Value,
) {
    let session = h.json("/api/session", cookie).await;
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository)
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

#[tokio::test(flavor = "multi_thread")]
async fn named_team_review_search_tracks_rosters_and_visibility() {
    exercise(None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn browser_completes_named_team_review_search() {
    exercise(Some(&Browser::new("named-team-reviews"))).await;
}

async fn exercise(browser: Option<&Browser>) {
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h =
        Harness::new_with_store_and_prefix(false, false, store, "named-team-review-search").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_id) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    *h.provider.mode.lock().await = "valid".into();
    let org = format!("/api/organizations/{organization}");
    h.write(
        Method::PUT,
        &format!("{org}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{org}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    for (actor, handle, name) in [(&alice, "alice", "Alice"), (&bob, "bob", "Bob")] {
        h.write(
            Method::POST,
            "/api/profile",
            actor,
            json!({"request_id":Uuid::now_v7(),"handle":handle,"name":name}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let team = Uuid::now_v7();
    let commands = format!("{org}/teams/commands");
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Private reviewers","description":"","visibility":"secret"}}))
        .await.error_for_status().unwrap();
    let member = |version, role| {
        json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"member","team":team,"member":bob_id,"version":version,"role":role,
        }})
    };
    h.write(Method::POST, &commands, &alice, member(0, Some("member")))
        .await
        .error_for_status()
        .unwrap();
    let repository: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = repository["owner"].as_str().unwrap();
    let id = repository["id"].as_str().unwrap();
    let root = format!("/api/repos/{owner}/library");
    let members = format!("{root}/members");
    let acl = h.json(&members, &alice).await;
    write(
        &h,
        &alice,
        Method::PUT,
        id,
        &members,
        json!({
            "expected_revision":acl["revision"],
            "members":[{"subject":"bob-id","name":"Bob","access":"write"}],"groups":[],
        }),
    )
    .await;
    let access = format!("{root}/team-access");
    let grants = h.json(&access, &alice).await;
    write(
        &h,
        &alice,
        Method::PUT,
        id,
        &access,
        json!({"expected_revision":grants["revision"],"teams":[{"team":team,"access":"write"}]}),
    )
    .await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/library.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    git::push_review_branches(source.path(), remote.as_str()).await;
    git::success(
        source.path(),
        &[
            "push",
            remote.as_str(),
            "HEAD:refs/heads/second",
            "HEAD:refs/heads/direct",
        ],
    )
    .await;
    let pulls = format!("{root}/pulls");
    for (number, branch, title) in [
        (1, "feature", "First team review"),
        (2, "second", "Second team review"),
        (3, "direct", "Direct review only"),
    ] {
        write(&h, &alice, Method::POST, id, &pulls, json!({
            "request_id":Uuid::now_v7(),"base_ref":"refs/heads/main","head_ref":format!("refs/heads/{branch}"),"title":title,"body":"",
        })).await;
        let change = if number == 3 {
            json!({"kind":"request","actor_id":bob_id})
        } else {
            json!({"kind":"request_team","team_id":team})
        };
        write(
            &h,
            &alice,
            Method::PUT,
            id,
            &format!("{pulls}/{number}/review-requests"),
            json!({"version":1,"change":change}),
        )
        .await;
    }
    let search = |target: &str, query: &str| {
        let mut url = url::Url::parse(&format!("{}{target}", h.origin)).unwrap();
        url.query_pairs_mut().append_pair("q", query);
        url
    };
    let team_bob = "is:open team-review-requested-user:bob";
    let team_alice = "is:open team-review-requested-user:alice";
    for (actor, query, count) in [
        (&alice, team_bob, 2),
        (&bob, "team-review-requested-user:@me", 2),
        (&alice, "review-requested:BOB", 3),
        (&alice, "user-review-requested:bob", 1),
        (&alice, "is:pr -team-review-requested-user:bob", 1),
        (&alice, "team-review-requested-user:unknown", 0),
    ] {
        let value: Value = h
            .http
            .get(search(&pulls, query))
            .header(header::COOKIE, actor)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["matching"], count, "{query}");
    }
    let workspace = format!("/api/discussion-search?scope={owner}%2Flibrary&kind=pull&limit=1");
    let mut continuations = Vec::new();
    for (path, parameter) in [(format!("{pulls}?limit=1"), "before"), (workspace, "after")] {
        let mut url = search(&path, team_bob);
        let page: Value = h
            .http
            .get(url.clone())
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page["items"][0]["number"], 2);
        url.query_pairs_mut()
            .append_pair(parameter, page["next"].as_str().unwrap());
        continuations.push(url);
    }
    if let Some(browser) = browser {
        browser
            .run(&["open", &format!("{}/{owner}/library/pulls", h.origin)])
            .await;
        browser.sign_in().await;
        browser.run(&["wait", "--text", "Direct review only"]).await;
        let input = ".discussion-search input[role=combobox]";
        browser
            .run(&["fill", input, "is:open team-review-requested-user:bo"])
            .await;
        browser
            .run(&["wait", "--text", "team-review-requested-user:bob"])
            .await;
        browser.run(&["press", "ArrowDown"]).await;
        browser.run(&["press", "Enter"]).await;
        browser.run(&["press", "Enter"]).await;
        browser.run(&["wait", "--fn", &format!(
            "new URL(location.href).searchParams.get('q') === '{}' && document.querySelector('.issue-list a[href=\"/{owner}/library/pull/2\"]') && !document.querySelector('.issue-list a[href=\"/{owner}/library/pull/3\"]')",
            team_bob,
        )]).await;
    }
    h.write(Method::POST, &commands, &alice, member(1, None))
        .await
        .error_for_status()
        .unwrap();
    for url in continuations {
        assert_eq!(
            h.http
                .get(url)
                .header(header::COOKIE, &alice)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
    }
    for (actor, query, count) in [
        (&alice, team_bob, 0),
        (&alice, "review-requested:bob", 1),
        (&alice, team_alice, 2),
        (&bob, team_alice, 0),
    ] {
        let value: Value = h
            .http
            .get(search(&pulls, query))
            .header(header::COOKIE, actor)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["counts"]["open"], count, "{query}");
    }
    h.close().await;
}
