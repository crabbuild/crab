use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::test_support::{git, http::Harness};

mod force_pushes;

const REPO: &str = "/api/repos/team/private";
const RULESETS: &str = "/api/repos/team/private/rulesets";

fn rule(name: &str, target: &str, pattern: &str) -> Value {
    json!({
        "id": Uuid::now_v7(),
        "name": name,
        "target": target,
        "enforcement": "active",
        "include": [pattern],
        "exclude": [],
        "restrict_creations": false,
        "restrict_deletions": false,
        "require_pull_request": false,
        "required_approvals": 0,
        "required_code_owner_reviews": false,
        "required_checks": [],
        "required_integrations": [],
    })
}

async fn seed(h: &Harness, alice: &str) -> (tempfile::TempDir, Url, String) {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut url = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    let work = tempfile::tempdir().unwrap();
    let path = work.path();
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    git::success(path, &["commit", "--allow-empty", "-m", "base"]).await;
    git::success(path, &["push", url.as_str(), "main"]).await;
    let oid = git::success(path, &["rev-parse", "HEAD"]).await;
    (work, url, oid)
}

#[tokio::test(flavor = "multi_thread")]
async fn branch_and_tag_rulesets_guard_http_and_native_publication() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, base) = seed(&h, &alice).await;
    let path = work.path();
    git::success(
        path,
        &[
            "push",
            remote.as_str(),
            "HEAD:refs/heads/release/team/stable",
        ],
    )
    .await;
    let mut releases = rule("Release branches", "branch", "release/**/*");
    releases["exclude"] = json!(["release/sandbox/**/*"]);
    releases["restrict_creations"] = json!(true);
    releases["restrict_deletions"] = json!(true);
    let mut tags = rule("Version tags", "tag", "v[0-9]*");
    tags["restrict_creations"] = json!(true);
    let payload = json!({"expected_version":0,"rulesets":[releases,tags]});
    let response = h
        .write(Method::PUT, RULESETS, &alice, payload.clone())
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        response.text().await.unwrap()
    );
    assert_eq!(h.json(RULESETS, &alice).await["version"], 1);
    assert_eq!(
        h.write(Method::PUT, RULESETS, &alice, payload)
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let branch = h
        .write(
            Method::POST,
            &format!("{REPO}/branches"),
            &alice,
            json!({"name":"release/team/rejected","source_oid":base}),
        )
        .await;
    assert_eq!(branch.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        branch.json::<Value>().await.unwrap()["error"]["code"],
        "repository_ruleset"
    );
    git::success(
        path,
        &[
            "push",
            remote.as_str(),
            "HEAD:refs/heads/release/sandbox/allowed",
        ],
    )
    .await;
    for (spec, denied_ref, reason) in [
        (
            ":refs/heads/release/team/stable",
            "refs/heads/release/team/stable",
            "ref deletion",
        ),
        ("HEAD:refs/tags/v2", "refs/tags/v2", "ref creation"),
    ] {
        let result = git::git(path, &["push", remote.as_str(), spec]).await;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(reason));
        let advertised = git::success(path, &["ls-remote", remote.as_str(), denied_ref]).await;
        assert_eq!(advertised.is_empty(), spec.starts_with("HEAD:"));
    }
    let release = json!({
        "request_id":Uuid::now_v7(), "tag_name":"v3", "target_oid":base,
        "title":"Version 3", "body":"", "prerelease":false, "draft":false
    });
    let blocked = h
        .write(
            Method::POST,
            &format!("{REPO}/releases"),
            &alice,
            release.clone(),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "repository_ruleset"
    );
    assert!(
        git::success(path, &["ls-remote", remote.as_str(), "refs/tags/v3"])
            .await
            .is_empty()
    );

    let mut saved = h.json(RULESETS, &alice).await;
    saved["rulesets"][1]["enforcement"] = json!("disabled");
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":1,"rulesets":saved["rulesets"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    git::success(path, &["push", remote.as_str(), "HEAD:refs/tags/v2"]).await;
    let advertised = git::success(path, &["ls-remote", remote.as_str(), "refs/tags/v2"]).await;
    assert_eq!(advertised, format!("{base}\trefs/tags/v2"));
    let published = h
        .write(Method::POST, &format!("{REPO}/releases"), &alice, release)
        .await;
    assert_eq!(
        published.status(),
        StatusCode::CREATED,
        "{:?}",
        published.text().await.unwrap()
    );
    let advertised = git::success(path, &["ls-remote", remote.as_str(), "refs/tags/v3"]).await;
    assert_eq!(advertised, format!("{base}\trefs/tags/v3"));

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let visible = h.json(RULESETS, &bob).await;
    assert_eq!(visible["can_manage"], false);
    assert_eq!(visible["rulesets"].as_array().unwrap().len(), 1);
    assert_eq!(visible["rulesets"][0]["name"], "Release branches");
    assert_eq!(
        h.write(
            Method::PUT,
            RULESETS,
            &bob,
            json!({"expected_version":2,"rulesets":[]})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_requirements_layer_classic_and_matching_rulesets_at_publication() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, base) = seed(&h, &alice).await;
    let path = work.path();
    git::success(path, &["switch", "-c", "feature"]).await;
    git::success(path, &["commit", "--allow-empty", "-m", "feature"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Feature","body":"","base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature","expected":{"base_oid":base,"head_oid":head}}),
    )
    .await
    .error_for_status()
    .unwrap();
    let pull = format!("{REPO}/pulls/1");
    h.write(Method::PUT, &format!("{REPO}/settings/branch-protections"), &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":["build"]}]}))
        .await.error_for_status().unwrap();
    let mut reviews = rule("Default branch reviews", "branch", "~DEFAULT_BRANCH");
    reviews["required_approvals"] = json!(2);
    let mut checks = rule("All branch checks", "branch", "~ALL");
    checks["required_checks"] = json!(["build", "lint"]);
    let rulesets = json!([reviews, checks]);
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":0,"rulesets":rulesets}),
    )
    .await
    .error_for_status()
    .unwrap();
    let current = h.json(&pull, &alice).await;
    let requirements = &current["merge_requirements"];
    assert_eq!(
        requirements["rulesets"],
        json!(["Default branch reviews", "All branch checks"])
    );
    assert_eq!(requirements["required_approvals"], 2);
    let names: Vec<_> = requirements["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["context"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["build", "lint"]);
    let merge = |version: &Value| json!({"request_id":Uuid::now_v7(),"version":version,"method":"fast_forward","base_oid":base,"head_oid":head});
    let blocked = h
        .write(
            Method::POST,
            &format!("{pull}/merge"),
            &alice,
            merge(&current["version"]),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "merge_blocked"
    );
    let refs = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(refs, format!("{base}\trefs/heads/main"));
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":1,"rulesets":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        &alice,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let members = h.json(&format!("{REPO}/members"), &alice).await;
    h.write(Method::PUT, &format!("{REPO}/members"), &alice, json!({
        "groups":[], "expected_revision":members["revision"],
        "members":[{"subject":"alice-id","name":"Alice","access":"admin"},{"subject":"bob-id","name":"Bob","access":"write"}]
    })).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let review: Value = h.write(Method::POST, &format!("{pull}/reviews"), &bob, json!({
        "request_id":Uuid::now_v7(), "state":"approved", "body":"Approved", "commit_oid":head
    })).await.error_for_status().unwrap().json().await.unwrap();
    let mut rule = rule("Dismissal protection", "branch", "~DEFAULT_BRANCH");
    rule["require_pull_request"] = json!(true);
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":2,"rulesets":[rule]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let rejected = git::git(path, &["push", remote.as_str(), "HEAD:refs/heads/main"]).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("ruleset requires a pull request"));
    let review_path = format!("{pull}/reviews/1");
    assert_eq!(h.json(&review_path, &bob).await["can_dismiss"], false);
    let response = h.write(Method::PUT, &format!("{review_path}/dismissals"), &bob,
        json!({"request_id":Uuid::now_v7(),"version":review["version"],"body":"Dismiss approval"})).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.json(&review_path, &bob).await["state"], "approved");
    let current = h.json(&pull, &alice).await;
    let response = h
        .write(
            Method::POST,
            &format!("{pull}/merge"),
            &alice,
            merge(&current["version"]),
        )
        .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{:?}",
        response.text().await.unwrap()
    );
    let refs = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(refs, format!("{head}\trefs/heads/main"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_resolution_guards_new_and_pending_merges() {
    use crate::cells::repository::{
        MergeMethod, PullMerge, PullMergeTransition, RepositoryAuthor, TransitionPullMerge,
        TransitionPullMergeInput, TransitionPullMergeOutcome,
    };

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, base) = seed(&h, &alice).await;
    let path = work.path();
    git::success(path, &["switch", "-c", "feature"]).await;
    std::fs::write(path.join("topic.txt"), "first version\n").unwrap();
    git::success(path, &["add", "topic.txt"]).await;
    git::success(path, &["commit", "-m", "add topic"]).await;
    let original_head = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Review the topic","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
            "expected":{"base_oid":base,"head_oid":original_head}
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let pull = format!("{REPO}/pulls/1");
    let threads = format!("{pull}/threads");
    let input = |oid: &str| {
        json!({
            "request_id":Uuid::now_v7(),"base_oid":base,"head_oid":oid,
            "path_hex":"746f7069632e747874","side":"new","start_line":1,"end_line":1,
            "body":"Please clarify the topic"
        })
    };
    let first = input(&original_head);
    let response = h.write(Method::POST, &threads, &alice, first.clone()).await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "{:?}",
        response.text().await.unwrap()
    );
    std::fs::write(path.join("topic.txt"), "changed version\n").unwrap();
    git::success(path, &["commit", "-am", "update topic"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    let second = input(&head);
    let new_thread: Value = h
        .write(Method::POST, &threads, &alice, second.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let resolved: Value = h
        .write(
            Method::PATCH,
            &format!("{threads}/2"),
            &alice,
            json!({"version":new_thread["version"],"resolved":true}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    h.write(Method::POST, &threads, &alice, input(&head))
        .await
        .error_for_status()
        .unwrap();

    // The unresolved thread remains required after its anchor goes outdated.
    let outdated = h
        .json(&format!("{threads}?resolved=false&outdated=true"), &alice)
        .await;
    assert_eq!(outdated["items"][0]["number"], 1);
    let protection = format!("{REPO}/settings/branch-protections");
    h.write(Method::PUT, &protection, &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","require_conversation_resolution":true}]}))
        .await.error_for_status().unwrap();
    let detail = h.json(&pull, &alice).await;
    assert_eq!(detail["merge_requirements"]["unresolved_conversations"], 2);
    let request = |request_id: Uuid, version: &Value| {
        json!({
            "request_id":request_id, "version":version, "method":"fast_forward",
            "base_oid":base, "head_oid":head,
        })
    };
    let endpoint = format!("{pull}/merge");
    let response = h
        .write(
            Method::POST,
            &endpoint,
            &alice,
            request(Uuid::now_v7(), &detail["version"]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "unresolved_conversations"
    );

    // Stage an intent under the old policy. Retrying after the rule is enabled
    // exercises the actual publication check and must clear the rejected intent.
    h.write(
        Method::PUT,
        &protection,
        &alice,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let detail = h.json(&pull, &alice).await;
    assert!(detail["merge_requirements"]["unresolved_conversations"].is_null());
    let merge_id = Uuid::now_v7();
    let merge = PullMerge {
        request_id: *merge_id.as_bytes(),
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
        publication_plan: Some("4".repeat(64)),
        bypass_rulesets: Vec::new(),
        tested_candidate: false,
    };
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    let result = cell
        .client
        .command::<TransitionPullMerge>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            TransitionPullMergeInput {
                pull: 1,
                merge,
                transition: PullMergeTransition::Begin,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        result.output,
        TransitionPullMergeOutcome::Applied(_)
    ));
    let retry = h.write(Method::POST, &threads, &alice, first).await;
    assert_eq!(retry.status(), StatusCode::CREATED);
    for (method, target, body) in [
        (Method::POST, threads.clone(), input(&head)),
        (
            Method::PATCH,
            format!("{threads}/2"),
            json!({"version":resolved["version"],"resolved":false}),
        ),
    ] {
        let response = h.write(method, &target, &alice, body).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "merge_pending"
        );
    }
    h.write(
        Method::PATCH,
        &format!("{threads}/3"),
        &alice,
        json!({"version":1,"resolved":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let mut conversations = rule("Resolve review", "branch", "~DEFAULT_BRANCH");
    conversations["require_conversation_resolution"] = json!(true);
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":0,"rulesets":[conversations]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let response = h
        .write(
            Method::POST,
            &endpoint,
            &alice,
            request(merge_id, &detail["version"]),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "unresolved_conversations"
    );
    let detail = h.json(&pull, &alice).await;
    assert!(detail["merge_pending"].is_null());
    let current = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(current, format!("{base}\trefs/heads/main"));

    h.write(
        Method::PATCH,
        &format!("{threads}/1"),
        &alice,
        json!({"version":1,"resolved":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let detail = h.json(&pull, &alice).await;
    assert_eq!(detail["merge_requirements"]["unresolved_conversations"], 0);
    assert_eq!(detail["can_merge"], true);
    h.write(
        Method::POST,
        &endpoint,
        &alice,
        request(Uuid::now_v7(), &detail["version"]),
    )
    .await
    .error_for_status()
    .unwrap();
    let current = git::success(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await;
    assert_eq!(current, format!("{head}\trefs/heads/main"));
    h.close().await;
}

mod bypass;
mod linear_history;
