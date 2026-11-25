use super::*;
use crate::test_support::{git, http::organizations};
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

async fn write(h: &Harness, actor: &str, repository: &Value, path: &str, body: Value) {
    let session = h.json("/api/session", actor).await;
    let response = h
        .http
        .post(format!("{}{path}", h.origin))
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
async fn check_status_search_follows_the_exact_upstream_and_fork_heads() {
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
    let fork_request = Uuid::now_v7();
    write(
        &h,
        &alice,
        id,
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
    for (title, source) in [("Upstream change", None), ("Fork change", Some(fork.id))] {
        write(&h, &alice, id, &pulls, json!({
            "request_id":Uuid::now_v7(),"title":title,"body":"",
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature","head_repository":source,
                "expected":{"base_oid":base,"head_oid":shared},
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
        &status(&shared, ""),
        json!({
            "request_id":Uuid::now_v7(),"context":"ci/build","state":"success",
        }),
    )
    .await;
    let mut cursors = Vec::new();
    for search in &searches {
        let first = ready_search(&h, &alice, search, "is:open status:success").await;
        assert_eq!(first["items"][0]["title"], "Fork change");
        cursors.push(first["next"].as_str().unwrap().to_owned());
    }
    let fork_remote = remote(&h, &alice, owner, "fork").await;
    std::fs::write(path.join("README.md"), "Only the fork advanced\n").unwrap();
    git::success(path, &["commit", "-am", "Advance fork only"]).await;
    let next = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", fork_remote.as_str(), "feature"]).await;

    for (index, search) in searches.iter().enumerate() {
        let continued = h
            .http
            .get(format!("{}{search}", h.origin))
            .header(header::COOKIE, &alice)
            .query(&[
                ("q", "is:open status:success"),
                (if index == 0 { "before" } else { "after" }, &cursors[index]),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(continued.status(), StatusCode::CONFLICT);
        let success = ready_search(&h, &alice, search, "status:success").await;
        assert_eq!(success["items"][0]["title"], "Upstream change");
        let pending = ready_search(&h, &alice, search, "status:pending").await;
        assert_eq!(pending["items"][0]["title"], "Fork change");
    }
    let pending = ready_search(&h, &alice, &pulls, "status:pending").await;
    assert_eq!(pending["matching"], 1);
    assert_eq!(pending["counts"]["open"], 1);
    assert_eq!(pending["items"][0]["rollup"]["head_oid"], next);
    for (context, state) in [("ci/lint", "pending"), ("ci/build", "failure")] {
        write(
            &h,
            &alice,
            id,
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
        &format!("{pulls}/1/merge"),
        json!({
            "request_id":Uuid::now_v7(),"version":first["version"],"method":"merge_commit",
            "base_oid":first["base_oid"],"head_oid":shared,
        }),
    )
    .await;
    git::success(path, &["push", upstream.as_str(), "--delete", "feature"]).await;
    let merged = ready_search(&h, &alice, &pulls, "is:merged status:success").await;
    assert_eq!(merged["matching"], 1);
    assert_eq!(merged["items"][0]["rollup"]["head_oid"], shared);
    h.close().await;
}
