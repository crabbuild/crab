use crate::{
    api::pulls::{git::PullGit, merge},
    auth::Identity,
    cells::repository::{
        MergeMethod, PullMerge, PullMergeTransition, RepositoryAuthor, TransitionPullMerge,
        TransitionPullMergeInput, TransitionPullMergeOutcome,
    },
    test_support::{git, http::Harness},
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const REPO: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";

#[tokio::test(flavor = "multi_thread")]
async fn up_to_date_policy_rechecks_pending_merges_and_requires_checks_after_an_update() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let work = tempfile::tempdir().unwrap();
    let path = work.path();
    let head = git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    git(&["switch", "-c", "integration", "main"]).await;
    std::fs::write(path.join("UPSTREAM.txt"), "upstream changes\n").unwrap();
    git(&["add", "UPSTREAM.txt"]).await;
    git(&["commit", "-m", "Advance integration"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    let base = git(&["rev-parse", "HEAD"]).await;
    h.write(Method::POST, &format!("{REPO}/pulls"), &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Review the feature","body":"","base_ref":"refs/heads/integration","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    let report = async |head: &str| {
        h.write(
            Method::POST,
            &format!("{REPO}/statuses/{head}"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"state":"success","context":"ci/strict"}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    report(&head).await;
    let detail = h.json(PULL, &alice).await;
    assert_eq!(detail["can_merge"], true);

    // Reserve a valid merge-commit plan before enabling the rule. Its retry
    // goes through the receive worker and must evaluate today's policy.
    let repository = &h.server.repositories.values()[0];
    let actor = Identity {
        issuer: h.provider.issuer.clone(),
        subject: "alice-id".into(),
        name: "Alice".into(),
    };
    let comparison = PullGit::open(&h.server, repository, None).await.unwrap();
    let message = "Merge reviewed feature";
    let candidate = merge::build_merge_plan(
        &comparison,
        &h.server.cancellation,
        gix_hash::ObjectId::from_hex(base.as_bytes()).unwrap(),
        gix_hash::ObjectId::from_hex(head.as_bytes()).unwrap(),
        merge::tree::Strategy {
            method: MergeMethod::MergeCommit,
            actor: &actor,
            message,
            seconds: 1,
            allow_empty_rebase: false,
        },
        None,
    )
    .await
    .unwrap();
    let request_id = Uuid::now_v7();
    let version = detail["version"].as_u64().unwrap();
    let cell = h.server.cells().unwrap().repository(repository.id).unwrap();
    let pending = cell
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
                        issuer: actor.issuer,
                        subject: actor.subject,
                        name: actor.name,
                    },
                    method: MergeMethod::MergeCommit,
                    pull_version: version,
                    base_oid: base.clone(),
                    head_oid: head.clone(),
                    commit_oid: candidate.oid.to_string(),
                    message: message.into(),
                    created_at_ms: 1000,
                    publication_plan: Some("4".repeat(64)),
                    bypass_rulesets: vec![],
                    tested_candidate: false,
                },
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        pending.output,
        TransitionPullMergeOutcome::Applied(_)
    ));

    let ruleset = |checks: Value| {
        json!({"expected_version":0,"rulesets":[{
            "id":Uuid::now_v7(),"name":"Require current integration","target":"branch","enforcement":"active",
            "include":["integration"],"exclude":[],"restrict_creations":false,"restrict_deletions":false,
            "require_pull_request":false,"required_approvals":0,"required_code_owner_reviews":false,
            "require_up_to_date":true,"required_checks":checks,"required_integrations":[],
        }]})
    };
    let rules = format!("{REPO}/rulesets");
    assert_eq!(
        h.write(Method::PUT, &rules, &alice, ruleset(json!([])))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    h.write(Method::PUT, &rules, &alice, ruleset(json!(["ci/strict"])))
        .await
        .error_for_status()
        .unwrap();
    let endpoint = format!("{PULL}/merge");
    let detail = h.json(PULL, &alice).await;
    assert_eq!(detail["merge_requirements"]["head_up_to_date"], false);
    assert_eq!(detail["merge_requirements"]["checks_satisfied"], true);
    let mut version = version;
    for request_id in [request_id, Uuid::now_v7()] {
        let response = h.write(Method::POST, &endpoint, &alice,
            json!({"request_id":request_id,"version":version,"method":"merge_commit","base_oid":base,"head_oid":head,"message":message}))
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "head_branch_behind"
        );
        let detail = h.json(PULL, &alice).await;
        assert_eq!(detail["base_oid"], base);
        assert!(detail["merge_pending"].is_null());
        assert_eq!(detail["can_merge"], false);
        version = detail["version"].as_u64().unwrap();
    }

    let update_endpoint = format!("{PULL}/update-branch");
    let comparison = h.json(&update_endpoint, &alice).await;
    h.write(
        Method::POST,
        &update_endpoint,
        &alice,
        comparison["update"].clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    let updated = h.json(PULL, &alice).await;
    let tested = updated["head_oid"].as_str().unwrap();
    assert_eq!(updated["merge_requirements"]["head_up_to_date"], true);
    assert_eq!(updated["merge_requirements"]["checks_satisfied"], false);
    assert_eq!(updated["can_merge"], false);
    report(tested).await;
    h.write(Method::POST, &endpoint, &alice,
        json!({"request_id":Uuid::now_v7(),"version":updated["version"],"method":"fast_forward","base_oid":base,"head_oid":tested}))
        .await.error_for_status().unwrap();
    git(&["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, tested);
    h.close().await;
}
