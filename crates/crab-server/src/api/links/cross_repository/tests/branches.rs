use super::*;
use crate::{
    cells::repository::{
        CreatePullWithLinks,
        collaboration::links::branches::{Branch, Decision, Edit, Set, promotion::cross},
    },
    repository::development_branches::promotion,
};

async fn linked(h: &Harness, actor: &str, issue: &Repo, pull: &Repo) {
    let subject = h
        .json(&issue.api("/issues/1/links?include_branches=true"), actor)
        .await;
    assert_eq!(subject["branches"]["items"], json!([]));
    assert_eq!(
        subject["cross_repository"][0]["repository"]["id"],
        json!(pull.id)
    );
    assert_eq!(subject["cross_repository"][0]["from_branch"], true);
    let reverse = h.json(&pull.api("/pulls/1/links"), actor).await;
    assert_eq!(
        reverse["cross_repository"][0]["repository"]["id"],
        json!(issue.id)
    );
    assert_eq!(reverse["cross_repository"][0]["from_branch"], true);
    assert_eq!(reverse["cross_repository"][0]["closes_on_merge"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_repository_branch_pr_replaces_the_branch_and_closes_only_after_an_authorized_merge()
{
    let (h, alice, _, issue, pull) = setup().await;
    let checkout = tempfile::tempdir().unwrap();
    let remote = git_remote(&h, &alice, &pull).await;
    git::push_review_branches(checkout.path(), remote.as_str()).await;
    let base = git::success(checkout.path(), &["rev-parse", "main"]).await;
    issue.write(&h, Method::POST, "/issues/1/development-branches", &alice, json!({
        "request_id":Uuid::now_v7(), "repository":pull.id, "name":"rollout", "source_oid":base
    })).await.error_for_status().unwrap();
    let head = git::success(checkout.path(), &["rev-parse", "feature"]).await;
    git::success(
        checkout.path(),
        &["push", remote.as_str(), "HEAD:refs/heads/rollout"],
    )
    .await;
    let body = json!({
        "request_id":Uuid::now_v7(), "title":"Publish the rollout", "body":"",
        "base_ref":"refs/heads/main", "head_ref":"refs/heads/rollout",
        "expected":{"base_oid":base,"head_oid":head}
    });
    let mut version = Value::Null;
    for _ in 0..2 {
        let opened: Value = pull
            .write(&h, Method::POST, "/pulls", &alice, body.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(opened["number"], 1);
        version = opened["version"].clone();
    }
    linked(&h, &alice, &issue, &pull).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    pull.set_bob_access(&h, Some(RepositoryAccess::Read)).await;
    assert_eq!(
        h.json(&pull.api("/pulls/1/links"), &bob).await["cross_repository"],
        json!([]),
    );
    let path = issue.api("/issues/1");
    assert_eq!(h.json(&path, &alice).await["state"], "open");
    pull.write(
        &h,
        Method::POST,
        "/pulls/1/merge",
        &alice,
        json!({
            "request_id":Uuid::now_v7(), "method":"squash", "version":version,
            "base_oid":base, "head_oid":head
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&path, &alice).await["state_reason"], "completed");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgraded_branch_projection_and_committed_pull_resume_across_cells() {
    let (h, alice, organization, issue, pull) = setup().await;
    let issue_repo = h.server.repositories.by_id(issue.id).unwrap();
    let pull_repo = h.server.repositories.by_id(pull.id).unwrap();
    let issue_cell = app::repository_cell(&h.server, &issue_repo).unwrap();
    let pull_cell = app::repository_cell(&h.server, &pull_repo).unwrap();
    let branch = Branch {
        repository: pull.id,
        reference: "refs/heads/feature".into(),
    };
    assert!(matches!(
        issue_cell
            .client
            .command::<Set>(
                issue_cell.target,
                app::mutation_identity().unwrap(),
                Edit {
                    issue: 1,
                    branch: branch.clone(),
                    active: true,
                    version: 0
                }
            )
            .await
            .unwrap()
            .output,
        Decision::Applied(1)
    ));
    promotion::recover(&h.server, &issue_repo).await.unwrap();
    let batch = promotion::capture(
        &h.server,
        &pull_repo,
        &crate::auth::Principal::local(),
        branch,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(batch.organization, organization);
    assert_eq!(batch.issues.len(), 1);
    assert_eq!(batch.issues[0].version, 1);

    let checkout = tempfile::tempdir().unwrap();
    let remote = git_remote(&h, &alice, &pull).await;
    let head_oid = git::push_review_branches(checkout.path(), remote.as_str()).await;
    let base_oid = git::success(checkout.path(), &["rev-parse", "main"]).await;
    let input = CreatePullInput {
        code_owner_requests: Default::default(),
        metadata: Default::default(),
        draft: false,
        submission_id: *Uuid::now_v7().as_bytes(),
        author: RepositoryAuthor {
            issuer: h.provider.issuer.clone(),
            subject: "alice-id".into(),
            name: "Alice".into(),
        },
        title: "Ship the older branch".into(),
        body: "".into(),
        base_ref: "refs/heads/main".into(),
        head_ref: "refs/heads/feature".into(),
        head_repository: None,
        base_oid,
        head_oid,
    };
    // Stop after the native PR commits, before delivering its saved cross-cell work.
    pull_cell
        .client
        .command::<CreatePullWithLinks>(
            pull_cell.target,
            app::mutation_identity().unwrap(),
            (input, batch),
        )
        .await
        .unwrap();
    assert!(
        pull_cell
            .client
            .query::<cross::Get>(pull_cell.target, None, 1)
            .await
            .unwrap()
            .output
            .is_some()
    );
    for _ in 0..2 {
        promotion::recover(&h.server, &pull_repo).await.unwrap();
        linked(&h, &alice, &issue, &pull).await;
    }
    assert!(
        pull_cell
            .client
            .query::<cross::Get>(pull_cell.target, None, 1)
            .await
            .unwrap()
            .output
            .is_none()
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and a built embedded UI"]
async fn browser_opens_a_cross_repository_issue_link_from_its_pr() {
    let (h, alice, _, issue, pull) = setup().await;
    let checkout = tempfile::tempdir().unwrap();
    let remote = git_remote(&h, &alice, &pull).await;
    git::push_review_branches(checkout.path(), remote.as_str()).await;
    issue.write(&h, Method::PUT, "/issues/1/development-branches", &alice, json!({
        "branch":{"repository":pull.id,"reference":"refs/heads/feature"},"version":0,"active":true
    })).await.error_for_status().unwrap();
    let browser = Browser::new("cross-branch");
    browser
        .run(&[
            "open",
            &format!("{}{}/compare/main...feature", h.origin, pull.path()),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "#discussion-title"]).await;
    browser
        .run(&["fill", "#discussion-title", "Release the service"])
        .await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('button')].some(button => !button.disabled && button.textContent.trim() === 'Create pull request')"]).await;
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
        .run(&["wait", "--url", &format!("**{}/pull/1*", pull.path())])
        .await;
    browser.run(&["wait", "--text", "From linked branch"]).await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            &format!("{}/{}#1 Coordinate rollout", issue.owner, issue.name),
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", &format!("**{}/issues/1*", issue.path())])
        .await;
    browser.run(&["wait", "--text", "From linked branch"]).await;
    linked(&h, &alice, &issue, &pull).await;
    drop(browser);
    h.close().await;
}
