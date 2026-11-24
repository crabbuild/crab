use super::*;

async fn save(h: &Harness, admin: &str, version: u64, rules: Value) {
    h.write(
        Method::PUT,
        RULESETS,
        admin,
        json!({"expected_version":version,"rulesets":rules}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn restricted_updates_allow_ref_creation_and_require_explicit_pull_bypass() {
    use crate::cells::repository::{
        MergeMethod, PullMerge, PullMergeTransition, RepositoryAuthor, TransitionPullMerge,
        TransitionPullMergeInput, TransitionPullMergeOutcome,
    };

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, base) = seed(&h, &alice).await;
    let path = work.path();
    h.write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        &alice,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let mut main = rule("Branch freeze", "branch", "~DEFAULT_BRANCH");
    let mut tags = rule("Release tag freeze", "tag", "v*");
    main["restrict_updates"] = json!(true);
    main["bypass_actors"] = json!([{"actor":{"kind":"repository_admin"},"mode":"pull_request"}]);
    tags["restrict_updates"] = json!(true);
    save(&h, &alice, 0, json!([main, tags])).await;
    git::success(path, &["push", remote.as_str(), "HEAD:refs/tags/v1"]).await;
    git::success(path, &["switch", "-c", "feature"]).await;
    git::success(path, &["commit", "--allow-empty", "-m", "feature"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    for refspec in ["HEAD:refs/heads/main", "HEAD:refs/tags/v1"] {
        let denied = git::git(path, &["push", "--force", remote.as_str(), refspec]).await;
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("restricts ref updates"));
    }
    git::success(path, &["push", remote.as_str(), ":refs/tags/v1"]).await;

    let root = format!("{REPO}/pulls");
    h.write(Method::POST, &root, &alice, json!({
        "request_id":Uuid::now_v7(),"title":"Feature","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"
    })).await.error_for_status().unwrap();
    let pull = format!("{root}/1");
    let merge = format!("{pull}/merge");
    let detail = h.json(&pull, &alice).await;
    assert_eq!(detail["can_merge"], false);
    assert_eq!(detail["merge_requirements"]["updates_restricted"], true);
    assert_eq!(
        detail["merge_requirements"]["bypass_options"][0]["id"],
        main["id"]
    );
    let attempt = |request_id: Uuid, version: &Value, bypass| {
        json!({
            "request_id":request_id,"version":version,"method":"fast_forward",
            "base_oid":base,"head_oid":head,"bypass_rulesets":bypass
        })
    };
    let denied = h
        .write(
            Method::POST,
            &merge,
            &alice,
            attempt(Uuid::now_v7(), &detail["version"], json!([])),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        denied.json::<Value>().await.unwrap()["error"]["code"],
        "ruleset_restricts_updates"
    );

    save(&h, &alice, 1, json!([])).await;
    assert_eq!(h.json(&pull, &alice).await["can_merge"], true);
    let request_id = Uuid::now_v7();
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    let reservation = cell
        .client
        .command::<TransitionPullMerge>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            TransitionPullMergeInput {
                pull: 1,
                transition: PullMergeTransition::Begin,
                merge: PullMerge {
                    request_id: *request_id.as_bytes(),
                    author: RepositoryAuthor {
                        issuer: h.provider.issuer.clone(),
                        subject: "alice-id".into(),
                        name: "Alice".into(),
                    },
                    method: MergeMethod::FastForward,
                    pull_version: detail["version"].as_u64().unwrap(),
                    base_oid: base.clone(),
                    head_oid: head.clone(),
                    commit_oid: head.clone(),
                    message: String::new(),
                    created_at_ms: 1,
                    publication_plan: Some("6".repeat(64)),
                    bypass_rulesets: vec![],
                    tested_candidate: false,
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        reservation.output,
        TransitionPullMergeOutcome::Applied(_)
    ));
    save(&h, &alice, 2, json!([main])).await;
    let denied = h
        .write(
            Method::POST,
            &merge,
            &alice,
            attempt(request_id, &detail["version"], json!([])),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        denied.json::<Value>().await.unwrap()["error"]["code"],
        "ruleset_restricts_updates"
    );
    let detail = h.json(&pull, &alice).await;
    assert!(detail["merge_pending"].is_null());
    let advertised = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(advertised, format!("{base}\trefs/heads/main"));

    h.write(
        Method::POST,
        &merge,
        &alice,
        attempt(Uuid::now_v7(), &detail["version"], json!([main["id"]])),
    )
    .await
    .error_for_status()
    .unwrap();
    let advertised = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(advertised, format!("{head}\trefs/heads/main"));

    main["bypass_actors"][0]["mode"] = json!("always");
    save(&h, &alice, 3, json!([main])).await;
    git::success(path, &["commit", "--allow-empty", "-m", "admin update"]).await;
    git::success(path, &["push", remote.as_str(), "HEAD:main"]).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_git_tokens_keep_the_account_role_without_bypassing_other_protections() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, _) = seed(&h, &alice).await;
    let path = work.path();
    let mut admins = rule("Release creation", "branch", "release/**/*");
    admins["restrict_creations"] = json!(true);
    admins["bypass_actors"] = json!([{"actor":{"kind":"repository_admin"},"mode":"always"}]);
    let mut other = rule("Independent restriction", "branch", "release/*");
    other["restrict_creations"] = json!(true);
    save(&h, &alice, 0, json!([admins, other])).await;
    let restricted = git::git(
        path,
        &["push", remote.as_str(), "HEAD:refs/heads/release/denied"],
    )
    .await;
    assert!(!restricted.status.success());
    assert!(String::from_utf8_lossy(&restricted.stderr).contains("restricts ref creation"));
    // Both authentication and the bypass must still be correct for the nested
    // branch, which is excluded from the other ruleset by its own pattern.
    git::success(
        path,
        &[
            "push",
            remote.as_str(),
            "HEAD:refs/heads/release/team/allowed",
        ],
    )
    .await;
    admins["bypass_actors"][0]["mode"] = json!("pull_request");
    save(&h, &alice, 1, json!([admins])).await;
    let denied = git::git(
        path,
        &[
            "push",
            remote.as_str(),
            "HEAD:refs/heads/release/team/denied",
        ],
    )
    .await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("restricts ref creation"));
    let visible = git::success(
        path,
        &[
            "ls-remote",
            remote.as_str(),
            "refs/heads/release/team/denied",
        ],
    )
    .await;
    assert!(visible.is_empty());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_bypass_is_explicit_actor_bound_and_rechecked_before_publication() {
    use crate::cells::repository::{
        MergeMethod, PullMerge, PullMergeTransition, RepositoryAuthor, TransitionPullMerge,
        TransitionPullMergeInput, TransitionPullMergeOutcome,
    };
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, base) = seed(&h, &alice).await;
    let path = work.path();
    git::success(path, &["switch", "-c", "feature"]).await;
    git::success(path, &["commit", "--allow-empty", "-m", "feature"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    h.write(Method::POST, &format!("{REPO}/pulls"), &alice, json!({
        "request_id":Uuid::now_v7(),"title":"Feature","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
        "expected":{"base_oid":base,"head_oid":head}
    })).await.error_for_status().unwrap();
    let protection = format!("{REPO}/settings/branch-protections");
    let acl = h.json(&format!("{REPO}/members"), &alice).await;
    h.write(Method::PUT, &format!("{REPO}/members"), &alice, json!({
        "expected_revision":acl["revision"],"groups":[],
        "members":[{"subject":"alice-id","name":"Alice","access":"admin"},{"subject":"bob-id","name":"Bob","access":"write"}]
    })).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;

    let mut main = rule("Approval", "branch", "~DEFAULT_BRANCH");
    main["required_approvals"] = json!(1);
    main["bypass_actors"] = json!([{"actor":{"kind":"repository_admin"},"mode":"pull_request"}]);
    let ruleset = Uuid::parse_str(main["id"].as_str().unwrap()).unwrap();
    save(&h, &alice, 0, json!([main])).await;
    let pull = format!("{REPO}/pulls/1");
    let merge_path = format!("{pull}/merge");
    let request = |id: Uuid, version: &Value, bypass: &[Uuid]| {
        json!({
            "request_id":id,"version":version,"method":"fast_forward","base_oid":base,"head_oid":head,
            "bypass_rulesets":bypass,
        })
    };
    let detail = h.json(&pull, &alice).await;
    assert_eq!(detail["merge_requirements"]["bypass_options"], json!([]));
    let blocked = h
        .write(
            Method::POST,
            &merge_path,
            &alice,
            request(Uuid::now_v7(), &detail["version"], &[ruleset]),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "merge_blocked"
    );
    h.write(
        Method::PUT,
        &protection,
        &alice,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let detail = h.json(&pull, &alice).await;
    assert_eq!(detail["can_merge"], false);
    assert_eq!(
        detail["merge_requirements"]["bypass_options"],
        json!([{"id":ruleset,"name":"Approval"}])
    );
    let reader = h.json(RULESETS, &bob).await;
    assert!(reader["rulesets"][0].get("bypass_actors").is_none());
    let denied = h
        .write(
            Method::POST,
            &merge_path,
            &bob,
            request(Uuid::now_v7(), &detail["version"], &[ruleset]),
        )
        .await;
    assert_eq!(
        denied.json::<Value>().await.unwrap()["error"]["code"],
        "ruleset_bypass_unavailable"
    );
    let ordinary = h
        .write(
            Method::POST,
            &merge_path,
            &alice,
            request(Uuid::now_v7(), &detail["version"], &[]),
        )
        .await;
    assert_eq!(
        ordinary.json::<Value>().await.unwrap()["error"]["code"],
        "merge_blocked"
    );

    let plan = Uuid::now_v7();
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    let prepared = cell
        .client
        .command::<TransitionPullMerge>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            TransitionPullMergeInput {
                pull: 1,
                transition: PullMergeTransition::Begin,
                merge: PullMerge {
                    request_id: *plan.as_bytes(),
                    author: RepositoryAuthor {
                        issuer: h.provider.issuer.clone(),
                        subject: "alice-id".into(),
                        name: "Alice".into(),
                    },
                    method: MergeMethod::FastForward,
                    pull_version: detail["version"].as_u64().unwrap(),
                    base_oid: base.clone(),
                    head_oid: head.clone(),
                    commit_oid: head.clone(),
                    message: String::new(),
                    created_at_ms: 1,
                    publication_plan: Some("5".repeat(64)),
                    bypass_rulesets: vec![
                        crate::cells::repository::rulesets::RulesetId::Repository(ruleset),
                    ],
                    tested_candidate: false,
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.output,
        TransitionPullMergeOutcome::Applied(_)
    ));
    let replay = request(plan, &detail["version"], &[ruleset]);
    let response = h
        .write(Method::POST, &merge_path, &bob, replay.clone())
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        h.json(&pull, &alice).await["merge_pending"]["request_id"],
        json!(plan)
    );
    let mut revoked = main.clone();
    revoked["bypass_actors"] = json!([]);
    save(&h, &alice, 1, json!([revoked])).await;
    let response = h.write(Method::POST, &merge_path, &alice, replay).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "ruleset_bypass_unavailable"
    );
    assert!(h.json(&pull, &alice).await["merge_pending"].is_null());
    let advertised = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(advertised, format!("{base}\trefs/heads/main"));

    save(&h, &alice, 2, json!([main])).await;
    let detail = h.json(&pull, &alice).await;
    let request = request(Uuid::now_v7(), &detail["version"], &[ruleset]);
    let result: Value = h
        .write(Method::POST, &merge_path, &alice, request.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["merge"]["bypass_rulesets"], json!([ruleset]));
    assert_eq!(
        h.write(Method::POST, &merge_path, &bob, request)
            .await
            .status(),
        StatusCode::OK
    );
    let advertised = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(advertised, format!("{head}\trefs/heads/main"));
    h.close().await;
}
