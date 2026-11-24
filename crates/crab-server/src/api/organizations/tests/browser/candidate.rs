use std::path::Path;

use super::{Browser, Harness, StatusCode, Uuid, Value, json};
use crate::test_support::{
    git::{git as run_git, success as git},
    http::github_workflow_delivery,
};

pub(super) async fn verify(
    h: &Harness,
    browser: &Browser,
    (writer, pull): (&str, &str),
    (checkout, crab, mirror): (&Path, &str, &Path),
    (base, head): (&str, &str),
    (webhook, secret): (&str, &str),
    mut report: Value,
) -> Value {
    browser
        .run(&["wait", "--text", "Prepare merge candidate"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Prepare merge candidate",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Candidate checks"]).await;
    let candidate = h.json(&format!("{pull}/merge-candidate"), writer).await["candidate"].clone();
    assert_eq!(candidate["current"], true);
    let commit = candidate["commit"].as_str().unwrap();
    let name = candidate["ref_name"].as_str().unwrap();
    git(checkout, &["fetch", crab, name]).await;
    assert_eq!(
        git(checkout, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{commit} {base} {head}")
    );
    assert_eq!(
        git(checkout, &["show", "FETCH_HEAD:CONTRIBUTION.txt"]).await,
        "reviewed fork contribution"
    );
    assert!(
        !run_git(checkout, &["push", crab, &format!(":{name}")])
            .await
            .status
            .success()
    );
    git(checkout, &["fetch", crab, "main"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, base);
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Candidate checks",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#github-actions-refspec"]).await;
    let refspec = browser
        .run(&[
            "eval",
            "document.getElementById('github-actions-refspec').value",
        ])
        .await;
    let mirror_ref = candidate["mirror_ref"].as_str().unwrap();
    assert_eq!(
        refspec.trim().trim_matches('"'),
        format!("{commit}:{mirror_ref}")
    );
    git(
        checkout,
        &[
            "push",
            mirror.to_str().unwrap(),
            refspec.trim().trim_matches('"'),
        ],
    )
    .await;
    assert_eq!(git(mirror, &["rev-parse", mirror_ref]).await, commit);

    report["workflow_run"]["head_sha"] = json!(commit);
    let mut wrong = report.clone();
    wrong["workflow_run"]["head_branch"] = json!("another-branch");
    assert_eq!(
        github_workflow_delivery(h, webhook, secret, Uuid::now_v7(), &wrong, &wrong).await,
        StatusCode::NOT_FOUND
    );
    let mirror_branch = mirror_ref.strip_prefix("refs/heads/").unwrap();
    report["workflow_run"]["head_branch"] = json!(mirror_branch);
    let mut wrong_revision = report.clone();
    wrong_revision["workflow_run"]["head_branch"] = json!(format!(
        "{}/{}",
        mirror_branch.rsplit_once('/').unwrap().0,
        "f".repeat(40)
    ));
    assert_eq!(
        github_workflow_delivery(
            h,
            webhook,
            secret,
            Uuid::now_v7(),
            &wrong_revision,
            &wrong_revision
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        github_workflow_delivery(h, webhook, secret, Uuid::now_v7(), &report, &report).await,
        StatusCode::NO_CONTENT
    );
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Refresh checks",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Provider · GitHub Actions"])
        .await;
    assert!(
        browser
            .run(&["get", "text", ".check-run-detail"])
            .await
            .contains(&commit[..7])
    );
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Head checks",
            "--exact",
        ])
        .await;
    report
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_publishes_the_tested_merge_candidate_after_a_lost_reply() {
    use reqwest::Method;
    use std::sync::atomic::Ordering;

    let h = Harness::new(false).await;
    let writer = h.login().await;
    let root = "/api/repos/team/private";
    let pull = format!("{root}/pulls/1");
    let candidate = format!("{pull}/merge-candidate");
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &writer,
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
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let head = crate::test_support::git::push_review_branches(path, remote.as_str()).await;

    // Diverge the base before protecting it. Candidate promotion must publish
    // the tested two-parent commit, even though the head does not contain base.
    git(path, &["checkout", "-b", "integration", "main"]).await;
    std::fs::write(path.join("BASE.txt"), "base update\n").unwrap();
    git(path, &["add", "BASE.txt"]).await;
    git(path, &["commit", "-m", "Update base"]).await;
    git(path, &["push", remote.as_str(), "integration"]).await;
    let base = git(path, &["rev-parse", "integration"]).await;
    h.write(Method::PUT, &format!("{root}/settings/branch-protections"), &writer,
        json!({"expected_version":0,"rules":[{"branch":"integration","required_approvals":0,"required_checks":["ci"]}]}))
        .await.error_for_status().unwrap();
    h.write(Method::POST, &format!("{root}/pulls"), &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Publish the tested composition","body":"Fixes #1","base_ref":"refs/heads/integration","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    h.write(
        Method::POST,
        &format!("{root}/issues"),
        &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Finish on the default branch","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{pull}/linked-issues/1"),
        &writer,
        json!({"active":true,"version":0,"closes_on_merge":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(
        h.json(&format!("{pull}/links"), &writer).await["items"][0]["from_description"].is_null()
    );
    let report = async |oid: &str, state: &str| {
        h.write(
            Method::POST,
            &format!("{root}/statuses/{oid}"),
            &writer,
            json!({"request_id":Uuid::now_v7(),"context":"ci","state":state}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    report(&head, "success").await;

    let browser = Browser::new("promote-candidate");
    let click = async |action: &str| {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    };
    browser
        .run(&["open", &format!("{}/team/private/pull/1/checks", h.origin)])
        .await;
    browser.sign_in().await;
    click("Prepare merge candidate").await;
    browser.run(&["wait", "--text", "Candidate checks"]).await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Candidate checks",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Required checks must pass on the candidate.",
        ])
        .await;
    let proposal = h.json(&candidate, &writer).await;
    let commit = proposal["candidate"]["commit"].as_str().unwrap();
    let input = json!({
        "request_id":Uuid::now_v7(), "version":proposal["preview"]["version"],
        "base_oid":base, "head_oid":head, "method":"merge_commit", "tested_candidate_oid":commit,
    });
    let merge = format!("{pull}/merge");
    let submit = || h.write(Method::POST, &merge, &writer, input.clone());
    let response = submit().await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "merge_candidate_checks"
    );
    report(commit, "success").await;
    report(&head, "failure").await;
    assert_eq!(
        h.json(&candidate, &writer).await["candidate"]["promotion"],
        "pull_blocked"
    );
    assert_eq!(submit().await.status(), StatusCode::CONFLICT);
    report(&head, "success").await;

    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let locks = crab_lfs::LfsLockManager::lfs(repo.store.clone(), &repo.config.prefix);
    let lock = locks.lock("README.md", "another-writer").await.unwrap();
    let locked = submit().await;
    assert_eq!(locked.status(), StatusCode::CONFLICT);
    assert_eq!(
        locked.json::<Value>().await.unwrap()["error"]["code"],
        "path_locked"
    );
    locks
        .unlock_with_id("README.md", "another-writer", Some(&lock.id))
        .await
        .unwrap();
    assert_eq!(
        h.json(&pull, &writer).await["merge_pending"]["tested_candidate_oid"],
        commit
    );

    // Re-enter publication for a previously admitted request and exercise a
    // check that changed after the reservation but before the ref edit.
    report(commit, "failure").await;
    let rejected = submit().await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "merge_candidate_checks"
    );
    assert!(h.json(&pull, &writer).await["merge_pending"].is_null());
    git(path, &["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(git(path, &["rev-parse", "FETCH_HEAD"]).await, base);

    report(commit, "success").await;
    browser.run(&["reload"]).await;
    click("Merge tested candidate").await;
    h.drop_next_merge_reply.store(true, Ordering::SeqCst);
    click("Confirm tested merge").await;
    click("Retry tested merge").await;
    browser
        .run(&["wait", "--text", "Published the tested merge candidate."])
        .await;
    assert!(!h.drop_next_merge_reply.load(Ordering::SeqCst));
    let merged = h.json(&pull, &writer).await;
    assert_eq!(merged["merge"]["commit_oid"], commit);
    assert_eq!(merged["merge"]["tested_candidate_oid"], commit);
    assert_eq!(
        h.json(&format!("{root}/issues/1"), &writer).await["state"],
        "open"
    );
    git(path, &["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(
        git(path, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{commit} {base} {head}")
    );
    assert_eq!(
        git(path, &["show", "FETCH_HEAD:README.md"]).await,
        "base\nfeature"
    );
    assert_eq!(
        git(path, &["show", "FETCH_HEAD:BASE.txt"]).await,
        "base update"
    );
    drop(browser);
    h.close().await;
}
