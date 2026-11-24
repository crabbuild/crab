use super::*;

async fn token(h: &Harness, actor: &str, repo: &Repo, body: Value) -> reqwest::Response {
    let mut body = body;
    body["owner"] = json!(repo.owner);
    body["repository"] = json!(repo.name);
    h.write(Method::POST, "/api/git-token", actor, body).await
}

async fn remote(h: &Harness, actor: &str, repo: &Repo, body: Value) -> url::Url {
    let result: Value = token(h, actor, repo, body)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git{}.git", h.origin, repo.path())).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(result["token"].as_str()).unwrap();
    remote
}

async fn closing_events(h: &Harness, issue: &Repo, actor: &str) -> Vec<Value> {
    let activity = h.json(&issue.api("/issues/1/activity"), actor).await;
    let conversation = h.json(&issue.api("/issues/1/conversation"), actor).await;
    let actions = |items: Vec<Value>| -> Vec<Value> {
        items
            .into_iter()
            .filter(|action| action["kind"] == "issue_closed")
            .collect()
    };
    let from_activity = actions(
        activity["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["action"].clone())
            .collect(),
    );
    assert_eq!(
        from_activity,
        actions(
            conversation["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["value"]["action"].clone())
                .collect()
        )
    );
    from_activity
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qualified_git_commit_closures_use_opt_in_default_ref_and_live_grants() {
    let (h, alice, organization, issue, source) = setup().await;
    let restricted = Repo::create(&h, &alice, organization, "restricted").await;
    restricted
        .write(
            &h,
            Method::POST,
            "/issues",
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"title":"Restricted issue","body":""
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    for repo in [&issue, &source] {
        repo.set_bob_access(&h, Some(RepositoryAccess::Write)).await;
    }
    restricted
        .set_bob_access(&h, Some(RepositoryAccess::Read))
        .await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        token(
            &h,
            &bob,
            &source,
            json!({"access":"read","close_related_issues":true})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
    );
    let ordinary = remote(&h, &bob, &source, json!({"access":"write"})).await;
    let opted_in = remote(
        &h,
        &bob,
        &source,
        json!({
            "access":"write","close_related_issues":true
        }),
    )
    .await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    git::push_review_branches(path, ordinary.as_str()).await;
    let issue_path = issue.api("/issues/1");
    let message = format!("Fixes {}#1", &issue.path()[1..]);
    git::success(path, &["commit", "--allow-empty", "-m", &message]).await;
    git::success(path, &["push", ordinary.as_str(), "HEAD:refs/heads/main"]).await;
    assert_eq!(h.json(&issue_path, &bob).await["state"], "open");

    let message = format!("{message}\n\nCloses {}#1", &restricted.path()[1..]);
    git::success(path, &["commit", "--allow-empty", "-m", &message]).await;
    let closing_commit = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", opted_in.as_str(), "feature"]).await;
    assert_eq!(h.json(&issue_path, &bob).await["state"], "open");
    git::success(path, &["push", opted_in.as_str(), "HEAD:refs/heads/main"]).await;
    assert_eq!(h.json(&issue_path, &bob).await["state_reason"], "completed");
    assert_eq!(
        h.json(&restricted.api("/issues/1"), &alice).await["state"],
        "open"
    );
    assert_eq!(
        closing_events(&h, &issue, &bob).await,
        vec![json!({
            "kind":"issue_closed","reason":"completed","commit":closing_commit,
            "commit_repository":{"id":source.id,"owner":source.owner,"name":source.name}
        })]
    );
    source.set_bob_access(&h, None).await;
    assert_eq!(
        closing_events(&h, &issue, &bob).await,
        vec![json!({"kind":"issue_closed","reason":"completed"})],
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn squash_message_closure_recovers_the_published_commit_without_creating_a_pull_link() {
    let (store, disconnect) = crate::git::receive::fault_plan_receipt();
    let (h, alice, _, issue, source) = setup_on(store).await;
    let checkout = tempfile::tempdir().unwrap();
    let (pull, remote) = git_pull(&h, &alice, &source, checkout.path(), "").await;
    let repository = h.server.repositories.by_id(source.id).unwrap();
    let merge = json!({
        "request_id":Uuid::now_v7(),"version":pull["version"],"method":"squash",
        "base_oid":pull["base_oid"],"head_oid":pull["head_oid"],
        "message":format!("Ship the service\n\nFixes {}#1", &issue.path()[1..])
    });
    disconnect(true);
    let _reply = source
        .write(&h, Method::POST, "/pulls/1/merge", &alice, merge.clone())
        .await;
    let published = repository
        .ref_oid("refs/heads/main")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(Some(published.as_str()), pull["base_oid"].as_str());
    assert_eq!(
        h.json(&issue.api("/issues/1"), &alice).await["state"],
        "open"
    );
    disconnect(false);
    advance_default(checkout.path(), &remote).await;
    assert_ne!(
        repository
            .ref_oid("refs/heads/main")
            .await
            .unwrap()
            .as_deref(),
        Some(published.as_str()),
    );
    crate::repository::commit_closures::recover(&h.server, &repository)
        .await
        .unwrap();
    let merged: Value = source
        .write(&h, Method::POST, "/pulls/1/merge", &alice, merge)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(merged["state"], "merged");
    assert_eq!(
        h.json(&issue.api("/issues/1"), &alice).await["state_reason"],
        "completed",
    );
    assert_eq!(
        closing_events(&h, &issue, &alice).await,
        vec![json!({
            "kind":"issue_closed","reason":"completed","commit":published,
            "commit_repository":{"id":source.id,"owner":source.owner,"name":source.name}
        })]
    );
    for path in [issue.api("/issues/1/links"), source.api("/pulls/1/links")] {
        assert_eq!(h.json(&path, &alice).await["cross_repository"], json!([]));
    }
    h.close().await;
}
