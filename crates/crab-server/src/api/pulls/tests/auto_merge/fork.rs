use super::*;
use crate::test_support::http::organizations;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread")]
async fn fork_auto_merge_requires_every_push_to_be_attributed_to_an_upstream_writer() {
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
    let write = async |method: Method, path: &str, cookie: &str, body: Value| {
        let session = h.json("/api/session", cookie).await;
        let response = h
            .http
            .request(method, format!("{}{root}{path}", h.origin))
            .header("cookie", cookie)
            .header("origin", &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header("x-crab-repository-id", id)
            .json(&body)
            .send()
            .await
            .unwrap();
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
    let inspected = git::push_review_branches(path, upstream.as_str()).await;
    let base = git::success(path, &["rev-parse", "main"]).await;
    let fork_request = Uuid::now_v7();
    write(
        Method::POST,
        "/forks",
        &bob,
        json!({
            "request_id":fork_request,"name":"upstream","destination":"personal",
        }),
    )
    .await;
    let fork = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = h
                .json(&format!("/api/imports/git/{fork_request}"), &bob)
                .await;
            match status["state"].as_str() {
                Some("succeeded") => break status["repository"].clone(),
                Some("queued" | "running") => tokio::time::sleep(Duration::from_millis(100)).await,
                _ => panic!("Fork did not complete: {status}"),
            }
        }
    })
    .await
    .unwrap();
    let fork_owner = fork["owner"].as_str().unwrap();
    let fork_id = h
        .server
        .repositories
        .get(&(fork_owner.to_owned(), "upstream".into()))
        .unwrap()
        .id;
    write(
        Method::POST,
        "/pulls",
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Fork change", "body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature","head_repository":fork_id,
            "expected":{"base_oid":base,"head_oid":inspected},
        }),
    )
    .await;
    let auto = format!("{root}/pulls/1/auto-merge");
    let schedule = async || {
        let pull = h.json(&format!("{root}/pulls/1"), &alice).await;
        write(Method::PUT, "/pulls/1/auto-merge", &alice, json!({
            "request_id":Uuid::now_v7(),"version":pull["version"],"base_oid":pull["base_oid"],
            "head_oid":pull["head_oid"],"method":"merge_commit","message":"Merge tested changes",
        })).await
    };
    let forge_commit =
        async |message: &str| git::success(path, &["commit", "--allow-empty", "-m", message]).await;
    let bob_remote = remote(fork_owner, &bob).await;
    schedule().await;
    forge_commit("Fork owner with upstream Read access").await;
    git::success(path, &["push", bob_remote.as_str(), "feature"]).await;
    set_bob_access("write").await;
    forge_commit("Same account after upstream Write grant").await;
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
    forge_commit("Verified update despite a lost reply").await;
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
