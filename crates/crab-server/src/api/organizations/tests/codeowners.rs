use super::{repositories::repository_write, *};
use crate::test_support::git;

#[tokio::test]
async fn codeowners_requests_reviews_and_enforces_base_policy_at_merge() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h =
        Harness::new_with_store_and_prefix(false, false, store, "organization-codeowners").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
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
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let created: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"owners","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let repo_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let owner = created["owner"].as_str().unwrap();
    let repo = format!("/api/repos/{owner}/owners");
    let team = Uuid::now_v7();
    let commands = format!("{org}/teams/commands");
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Release Engineering","description":"","visibility":"closed"}}))
        .await.error_for_status().unwrap();
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":bob_id,"version":0,"role":"member"}}))
        .await.error_for_status().unwrap();
    let teams = format!("{repo}/team-access");
    let revision = h.json(&teams, &alice).await["revision"].clone();
    repository_write(
        &h,
        Method::PUT,
        &teams,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"write"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let direct = format!("{repo}/members");
    let revision = h.json(&direct, &alice).await["revision"].clone();
    repository_write(&h, Method::PUT, &direct, &alice, repo_id,
        json!({"expected_revision":revision,"groups":[],"members":[{"subject":"bob-id","name":"Bob","access":"write"}]}))
        .await.error_for_status().unwrap();

    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"owners","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/owners.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let working = tempfile::tempdir().unwrap();
    let path = working.path();
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::create_dir(path.join(".github")).unwrap();
    let policy =
        format!("*.md @bob\n/src/ @{owner}/release-engineering\n/base-only.txt @unavailable\n");
    std::fs::write(path.join(".github/CODEOWNERS"), policy).unwrap();
    std::fs::write(path.join("CODEOWNERS"), "* @unknown\n").unwrap();
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    std::fs::create_dir(path.join("src")).unwrap();
    std::fs::write(
        path.join("src/owned.txt"),
        "owned on the original revision\n",
    )
    .unwrap();
    git::success(path, &["add", "."]).await;
    git::success(path, &["commit", "-m", "Ownership policy"]).await;
    let historical = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["branch", "feature"]).await;
    std::fs::write(path.join("base-only.txt"), "base branch only\n").unwrap();
    git::success(path, &["add", "base-only.txt"]).await;
    git::success(path, &["commit", "-m", "Base-only change"]).await;
    git::success(path, &["push", remote.as_str(), "main"]).await;
    git::success(path, &["checkout", "feature"]).await;
    std::fs::write(path.join("src/example.txt"), "feature\n").unwrap();
    std::fs::write(path.join("README.md"), "feature\n").unwrap();
    std::fs::write(
        path.join(".github/CODEOWNERS"),
        "# Remove owners in proposed change\n",
    )
    .unwrap();
    git::success(path, &["add", "."]).await;
    git::success(path, &["commit", "-m", "Proposed change"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;

    let file_owners = |rev: &str, path: &str| {
        let path = crate::api::encode_hex(path.as_bytes());
        format!("{repo}/file-codeowners?rev={rev}&path_hex={path}")
    };
    let source = file_owners(&historical, "src/owned.txt");
    let report = h.json(&source, &bob).await;
    assert_eq!(report["commit"], historical);
    assert_eq!(report["source"]["path"], ".github/CODEOWNERS");
    assert_eq!(report["source"]["line"], 2);
    assert_eq!(
        report["source"]["owners"],
        json!([{"reference":format!("@{owner}/release-engineering"),"eligible":true}])
    );
    let individual = h.json(&file_owners(&historical, "README.md"), &bob).await;
    assert_eq!(
        individual["source"]["owners"],
        json!([{"reference":"@bob","eligible":true}])
    );
    assert_eq!(
        h.json(&file_owners(&head, "src/owned.txt"), &bob).await["source"],
        Value::Null
    );
    for (rev, file) in [(&historical, "src/example.txt"), (&head, "missing.txt")] {
        let url = file_owners(rev, file);
        assert_eq!(
            h.http
                .get(format!("{}{url}", h.origin))
                .header(header::COOKIE, &bob)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    let pulls = format!("{repo}/pulls");
    let request = json!({"request_id":Uuid::now_v7(),"title":"Protected changes","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"});
    for _ in 0..2 {
        repository_write(&h, Method::POST, &pulls, &alice, repo_id, request.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    let pull = format!("{repo}/pulls/1");
    let requests = h.json(&format!("{pull}/reviewers"), &alice).await;
    assert_eq!(requests["items"][0]["request"]["code_owner"], true);
    assert_eq!(requests["teams"][0]["code_owner"], true);
    let inbox = format!("/api/notifications?repository={repo_id}");
    h.deliver_notifications(owner, "owners").await;
    let notifications = h.json(&inbox, &bob).await;
    assert_eq!(notifications["items"].as_array().unwrap().len(), 1);
    assert_eq!(notifications["items"][0]["reason"], "review_requested");

    let mut draft = request;
    git::success(
        path,
        &["push", remote.as_str(), "feature:refs/heads/draft-topic"],
    )
    .await;
    draft["request_id"] = json!(Uuid::now_v7());
    draft["head_ref"] = json!("refs/heads/draft-topic");
    draft["title"] = json!("Draft proposal");
    draft["draft"] = json!(true);
    repository_write(&h, Method::POST, &pulls, &alice, repo_id, draft)
        .await
        .error_for_status()
        .unwrap();
    let draft_reviewers = format!("{pulls}/2/reviewers");
    let draft_requests = format!("{pulls}/2/review-requests");
    let readiness = format!("{pulls}/2/readiness");
    let initial = h.json(&draft_reviewers, &alice).await;
    assert_eq!(initial["items"], json!([]));
    assert_eq!(initial["teams"][0]["state"], Value::Null);
    assert_eq!(h.json(&inbox, &bob).await, notifications);
    let ready = json!({"draft":false,"version":initial["version"]});
    for _ in 0..2 {
        repository_write(&h, Method::PUT, &readiness, &alice, repo_id, ready.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    let queued = h.json(&draft_reviewers, &alice).await;
    assert_eq!(queued["items"][0]["request"]["code_owner"], true);
    assert_eq!(queued["teams"][0]["code_owner"], true);
    h.deliver_notifications(owner, "owners").await;
    let delivered = h.json(&inbox, &bob).await;
    assert_eq!(delivered["items"].as_array().unwrap().len(), 2);
    assert!(
        delivered["items"][0]["href"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/pull/2?repository_id={repo_id}"))
    );
    repository_write(
        &h,
        Method::PUT,
        &draft_requests,
        &alice,
        repo_id,
        json!({"version":queued["version"],"change":{"kind":"cancel_team","team_id":team}}),
    )
    .await
    .error_for_status()
    .unwrap();
    let queued = h.json(&draft_reviewers, &alice).await;
    repository_write(&h, Method::PUT, &draft_requests, &alice, repo_id,
        json!({"version":queued["version"],"change":{"kind":"cancel","id":queued["items"][0]["request"]["id"]}}))
        .await.error_for_status().unwrap();
    for draft in [true, false] {
        let current = h.json(&draft_reviewers, &alice).await;
        repository_write(
            &h,
            Method::PUT,
            &readiness,
            &alice,
            repo_id,
            json!({"draft":draft,"version":current["version"]}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let after = h.json(&draft_reviewers, &alice).await;
    assert_eq!(after["items"], json!([]));
    assert_eq!(after["teams"][0]["state"], Value::Null);
    assert_eq!(h.json(&inbox, &bob).await, delivered);

    let protection = format!("{repo}/settings/branch-protections");
    let saved: Value = repository_write(&h, Method::PUT, &protection, &alice, repo_id,
        json!({"expected_version":0,"rules":[{"branch":"main","required_code_owner_reviews":true}]}))
        .await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(saved["rules"][0]["required_code_owner_reviews"], true);
    let merge = format!("{pull}/merge");
    let current = h.json(&pull, &alice).await;
    let report = &current["merge_requirements"]["code_owners"];
    assert_eq!(report["path"], ".github/CODEOWNERS");
    assert_eq!(report["affected_paths"], 2);
    assert_eq!(report["missing_paths"], 2);
    assert_eq!(h.json(&format!("{pull}/codeowners"), &alice).await, *report);
    let attempt = |current: &Value| {
        json!({"request_id":Uuid::now_v7(),"version":current["version"],
        "base_oid":current["base_oid"],"head_oid":current["head_oid"],"method":"merge_commit","message":"Protected merge"})
    };
    assert_eq!(
        repository_write(&h, Method::POST, &merge, &alice, repo_id, attempt(&current))
            .await
            .status(),
        StatusCode::CONFLICT
    );

    repository_write(
        &h,
        Method::POST,
        &format!("{pull}/reviews"),
        &bob,
        repo_id,
        json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed","commit_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();
    git::success(
        path,
        &["commit", "--allow-empty", "-m", "New review target"],
    )
    .await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    let stale = h.json(&pull, &alice).await;
    assert_eq!(
        stale["merge_requirements"]["code_owners"]["missing_paths"],
        2
    );
    repository_write(&h, Method::POST, &format!("{pull}/reviews"), &bob, repo_id,
        json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed the current head","commit_oid":head}))
        .await.error_for_status().unwrap();
    let current = h.json(&pull, &alice).await;
    assert_eq!(
        current["merge_requirements"]["code_owners"]["missing_paths"],
        0
    );
    let revision = h.json(&teams, &alice).await["revision"].clone();
    repository_write(
        &h,
        Method::PUT,
        &teams,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"read"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&source, &bob).await["source"]["owners"][0]["eligible"],
        false
    );
    let blocked = h.json(&pull, &alice).await;
    assert_eq!(
        blocked["merge_requirements"]["code_owners"]["missing_paths"],
        1
    );
    assert_eq!(
        blocked["merge_requirements"]["code_owners"]["groups"][0]["has_eligible_owner"],
        false
    );
    assert_eq!(
        repository_write(&h, Method::POST, &merge, &alice, repo_id, attempt(&current))
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let revision = h.json(&teams, &alice).await["revision"].clone();
    repository_write(
        &h,
        Method::PUT,
        &teams,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"write"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let outcome: Value =
        repository_write(&h, Method::POST, &merge, &alice, repo_id, attempt(&current))
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(outcome["state"], "merged");
    let refs = h.json(&format!("{repo}/refs"), &alice).await;
    assert_eq!(refs["head"]["oid"], outcome["merge"]["commit_oid"]);
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{source}", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
