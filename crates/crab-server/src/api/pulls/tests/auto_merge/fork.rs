use super::*;
use crate::test_support::http::{browser::Browser, organizations};
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread")]
async fn fork_auto_merge_requires_every_push_to_be_attributed_to_an_upstream_writer() {
    exercise(None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn native_browser_read_only_contributor_opens_a_pull_from_their_fork() {
    exercise(Some(Browser::new("fork-contribution"))).await;
}

async fn exercise(browser: Option<Browser>) {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "fork-auto-merge").await;
    h.initialize_directory().await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_id) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
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
    organizations::set_private_forks(&h, organization, &alice, true).await;
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
    let id = repo["id"].as_str().unwrap();
    let root = format!("/api/repos/{owner}/upstream");
    let send = async |method: Method, path: &str, cookie: &str, body: Value| {
        let session = h.json("/api/session", cookie).await;
        h.http
            .request(method, format!("{}{root}{path}", h.origin))
            .header("cookie", cookie)
            .header("origin", &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header("x-crab-repository-id", id)
            .json(&body)
            .send()
            .await
            .unwrap()
    };
    let write = async |method: Method, path: &str, cookie: &str, body: Value| {
        let response = send(method, path, cookie, body).await;
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "{path}: {status} {body}");
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_str::<Value>(&body).unwrap()
        }
    };
    let set_bob_access = async |access: &str| {
        let acl = h.json(&format!("{root}/members"), &alice).await;
        write(Method::PUT, "/members", &alice, json!({
            "expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":access}],"groups":[],
        })).await;
    };
    set_bob_access("read").await;
    write(
        Method::PUT,
        "/settings/merge-methods",
        &alice,
        json!({"expected_version":0,"auto_merge":true,"enabled":["merge_commit"]}),
    )
    .await;
    write(Method::PUT, "/settings/branch-protections", &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci"]}]})).await;
    let remote = async |owner: &str, cookie: &str| {
        let token: Value = h
            .write(
                Method::POST,
                "/api/git-token",
                cookie,
                json!({"owner":owner,"repository":"upstream","access":"write"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut remote =
            url::Url::parse(&format!("{}/git/{owner}/upstream.git", h.origin)).unwrap();
        remote.set_username("crab").unwrap();
        remote.set_password(token["token"].as_str()).unwrap();
        remote
    };
    let upstream = remote(owner, &alice).await;
    let local = tempfile::tempdir().unwrap();
    let path = local.path();
    let initial_head = git::push_review_branches(path, upstream.as_str()).await;
    let base = git::success(path, &["rev-parse", "main"]).await;
    let create_fork = async |cookie: &str| {
        let request = Uuid::now_v7();
        write(
            Method::POST,
            "/forks",
            cookie,
            json!({
                "request_id":request,"name":"upstream","destination":"personal",
            }),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let status = h.json(&format!("/api/imports/git/{request}"), cookie).await;
                match status["state"].as_str() {
                    Some("succeeded") => {
                        break status["repository"]["owner"].as_str().unwrap().to_owned();
                    }
                    Some("queued" | "running") => {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                    _ => panic!("Fork did not complete: {status}"),
                }
            }
        })
        .await
        .unwrap()
    };
    let fork_owner = create_fork(&bob).await;
    let fork_id = h
        .server
        .repositories
        .get(&(fork_owner.to_owned(), "upstream".into()))
        .unwrap()
        .id;
    let commit =
        async |message: &str| git::success(path, &["commit", "--allow-empty", "-m", message]).await;
    let bob_remote = remote(&fork_owner, &bob).await;
    commit("Prepare a contribution from the personal fork").await;
    git::success(path, &["push", bob_remote.as_str(), "feature"]).await;
    let inspected = git::success(path, &["rev-parse", "feature"]).await;
    let recent = h
        .json(
            &format!("/api/repos/{fork_owner}/upstream/pulls/recent-pushes"),
            &bob,
        )
        .await;
    assert_eq!(
        recent["items"],
        json!([{"name":"refs/heads/feature","oid":inspected}])
    );
    let request = |head: &str| {
        json!({
                "request_id":Uuid::now_v7(),"title":"Fork change", "body":"",
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature","head_repository":fork_id,
                "expected":{"base_oid":base,"head_oid":head},
        })
    };
    if let Some(browser) = &browser {
        browser
            .run(&["open", &format!("{}/{fork_owner}/upstream", h.origin)])
            .await;
        browser.sign_in().await;
        browser
            .run(&["wait", "--text", "You recently pushed"])
            .await;
        browser
            .run(&[
                "find",
                "role",
                "link",
                "click",
                "--name",
                "Compare & pull request for feature",
                "--exact",
            ])
            .await;
        browser.run(&["wait", "#discussion-title"]).await;
        browser
            .run(&["fill", "#discussion-title", "Fork change"])
            .await;
        browser
            .run(&[
                "find",
                "role",
                "button",
                "click",
                "--name",
                "Create pull request",
                "--exact",
            ])
            .await;
        browser
            .run(&["wait", "--fn", "location.pathname.endsWith('/pull/1')"])
            .await;
    } else {
        let mut unrelated = request(&inspected);
        unrelated["head_repository"] = json!(
            h.server
                .repositories
                .values()
                .iter()
                .find(|repo| repo.config.owner == "team")
                .unwrap()
                .id
        );
        assert_eq!(
            send(Method::POST, "/pulls", &bob, unrelated).await.status(),
            StatusCode::NOT_FOUND
        );
        let other_owner = create_fork(&alice).await;
        let other_fork = h
            .server
            .repositories
            .get(&(other_owner, "upstream".into()))
            .unwrap();
        let mut unowned = request(&initial_head);
        unowned["head_repository"] = json!(other_fork.id);
        assert!(matches!(
            send(Method::POST, "/pulls", &bob, unowned).await.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ));
        assert_eq!(
            send(Method::POST, "/pulls", &bob, request(&initial_head))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let reviewed = request(&inspected);
        let first = write(Method::POST, "/pulls", &bob, reviewed.clone()).await;
        let replayed = write(Method::POST, "/pulls", &bob, reviewed).await;
        assert_eq!(first["number"], replayed["number"]);
    }
    let pull = h.json(&format!("{root}/pulls/1"), &bob).await;
    assert_eq!(pull["author"]["name"], "Bob");
    assert_eq!(pull["head_repository"]["id"], json!(fork_id));
    assert_eq!(pull["original_head_oid"], inspected);
    assert_eq!(pull["can_edit"], true);
    assert_eq!(pull["can_merge"], false);
    if let Some(browser) = browser {
        drop(browser);
        h.close().await;
        return;
    }
    let auto = format!("{root}/pulls/1/auto-merge");
    let schedule = async || {
        let pull = h.json(&format!("{root}/pulls/1"), &alice).await;
        write(Method::PUT, "/pulls/1/auto-merge", &alice, json!({
            "request_id":Uuid::now_v7(),"version":pull["version"],"base_oid":pull["base_oid"],
            "head_oid":pull["head_oid"],"method":"merge_commit","message":"Merge tested changes",
        })).await
    };
    schedule().await;
    commit("Fork owner with upstream Read access").await;
    git::success(path, &["push", bob_remote.as_str(), "feature"]).await;
    set_bob_access("write").await;
    commit("Same account after upstream Write grant").await;
    git::success(path, &["push", bob_remote.as_str(), "feature"]).await;
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    let await_state = async |stopped: Value, head: Option<&str>| {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let state = h.json(&auto, &alice).await;
                if state["request"]["stopped"] == stopped
                    && head.is_none_or(|head| state["request"]["head_oid"] == head)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
    };
    await_state(json!("head_changed"), None).await;
    let armed = schedule().await;
    commit("Verified update despite a lost reply").await;
    let head = git::success(path, &["rev-parse", "feature"]).await;
    h.drop_next_git_push_reply.store(true, Ordering::SeqCst);
    let _reply = git::git(path, &["push", bob_remote.as_str(), "feature"]).await;
    assert!(!h.drop_next_git_push_reply.load(Ordering::SeqCst));
    git::success(path, &["fetch", bob_remote.as_str(), "feature"]).await;
    assert_eq!(git::success(path, &["rev-parse", "FETCH_HEAD"]).await, head);
    await_state(Value::Null, Some(&head)).await;
    assert_eq!(
        h.json(&auto, &alice).await["request"]["request_id"],
        armed["request"]["request_id"]
    );
    write(
        Method::POST,
        &format!("/statuses/{head}?pull=1"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"context":"ci","state":"success"}),
    )
    .await;
    await_state(json!("merged"), Some(&head)).await;
    git::success(path, &["fetch", upstream.as_str(), "main"]).await;
    assert_eq!(
        git::success(path, &["rev-parse", "FETCH_HEAD^2"]).await,
        head
    );
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
}
