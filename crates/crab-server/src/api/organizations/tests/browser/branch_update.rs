use std::{path::Path, sync::atomic::Ordering};

use super::{Browser, Harness, Method, StatusCode, Uuid, Value, header, json};
use crate::api::organizations::tests::repositories::repository_write;
use crate::test_support::git::success as git;

pub(super) async fn rebase_fork(
    h: &Harness,
    browser: &Browser,
    writer: &str,
    (upstream_id, fork_id): (Uuid, Uuid),
    (checkout, upstream_git, fork_git): (&Path, &str, &str),
    ancestor: &str,
) {
    git(checkout, &["checkout", "-b", "rebase-base", ancestor]).await;
    std::fs::write(checkout.join("REBASE-BASE.txt"), "upstream only\n").unwrap();
    git(checkout, &["add", "REBASE-BASE.txt"]).await;
    git(checkout, &["commit", "-m", "Advance the upstream base"]).await;
    git(checkout, &["push", upstream_git, "rebase-base"]).await;
    let base = git(checkout, &["rev-parse", "HEAD"]).await;
    git(checkout, &["checkout", "-b", "rebase-head", ancestor]).await;
    std::fs::write(checkout.join("REBASE-HEAD.txt"), "fork only\n").unwrap();
    git(checkout, &["add", "REBASE-HEAD.txt"]).await;
    git(checkout, &["commit", "-m", "First fork commit"]).await;
    std::fs::write(
        checkout.join("REBASE-HEAD.txt"),
        "fork only\nsecond change\n",
    )
    .unwrap();
    git(checkout, &["commit", "-am", "Second fork commit"]).await;
    let head = git(checkout, &["rev-parse", "HEAD"]).await;
    git(checkout, &["push", fork_git, "rebase-head"]).await;

    let upstream = h.server.repositories.by_id(upstream_id).unwrap();
    let (owner, repo) = (&upstream.config.owner, &upstream.config.name);
    let pull: Value = repository_write(h, Method::POST, &format!("/api/repos/{owner}/{repo}/pulls"), writer, upstream_id,
        json!({"request_id":Uuid::now_v7(),"title":"Rebase a private fork","body":"","base_ref":"refs/heads/rebase-base",
        "head_ref":"refs/heads/rebase-head","head_repository":fork_id,"expected":{"base_oid":base,"head_oid":head}}))
        .await.error_for_status().unwrap().json().await.unwrap();
    let number = pull["number"].as_u64().unwrap();
    browser
        .run(&[
            "open",
            &format!("{}/{owner}/{repo}/pull/{number}", h.origin),
        ])
        .await;
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser
        .run(&["wait", "[aria-label='Branch update options']"])
        .await;
    click("button", "Branch update options").await;
    browser.run(&["wait", "--text", "Update with rebase"]).await;
    click("menuitem", "Update with rebase").await;
    browser
        .run(&["wait", "--text", "This rewrites the head branch history"])
        .await;
    h.drop_next_branch_update_reply
        .store(true, Ordering::SeqCst);
    click("button", "Rebase branch").await;
    browser
        .run(&["wait", "--text", "The response was lost"])
        .await;
    assert!(!h.drop_next_branch_update_reply.load(Ordering::SeqCst));

    git(checkout, &["fetch", fork_git, "rebase-head"]).await;
    let rebased = git(checkout, &["rev-parse", "FETCH_HEAD"]).await;
    assert_ne!(rebased, head);
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD~2"]).await, base);
    assert_eq!(
        git(
            checkout,
            &["log", "--format=%s", &format!("{base}..FETCH_HEAD")]
        )
        .await,
        "Second fork commit\nFirst fork commit"
    );
    assert_eq!(
        git(checkout, &["rev-parse", "FETCH_HEAD^{tree}"]).await,
        git(checkout, &["merge-tree", "--write-tree", &base, &head]).await
    );
    click("button", "Retry rebase").await;
    browser
        .run(&[
            "wait",
            "--fn",
            "!document.querySelector('[aria-label=\"Update pull request branch\"]')",
        ])
        .await;
    git(checkout, &["fetch", upstream_git, "rebase-base"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, base);
    git(checkout, &["fetch", fork_git, "rebase-head"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, rebased);
}

pub(super) async fn resolve_fork(
    h: &Harness,
    browser: &Browser,
    (writer, reviewer): (&str, &str),
    (upstream_id, fork_id): (Uuid, Uuid),
    (checkout, upstream_git, fork_git): (&Path, &str, &str),
    ancestor: &str,
) {
    let mut tips = Vec::new();
    for (branch, text, remote) in [
        ("conflict-base", "upstream version\n", upstream_git),
        ("conflict-head", "fork version\n", fork_git),
    ] {
        git(checkout, &["checkout", "-b", branch, ancestor]).await;
        std::fs::write(checkout.join("README.md"), text).unwrap();
        git(checkout, &["commit", "-am", "Edit the same line"]).await;
        git(checkout, &["push", remote, branch]).await;
        tips.push(git(checkout, &["rev-parse", "HEAD"]).await);
    }
    let upstream = h.server.repositories.by_id(upstream_id).unwrap();
    let (owner, repo) = (&upstream.config.owner, &upstream.config.name);
    let pull: Value = repository_write(h, Method::POST, &format!("/api/repos/{owner}/{repo}/pulls"), writer, upstream_id,
        json!({"request_id":Uuid::now_v7(),"title":"Resolve fork conflicts","body":"","base_ref":"refs/heads/conflict-base",
        "head_ref":"refs/heads/conflict-head","head_repository":fork_id,"expected":{"base_oid":tips[0],"head_oid":tips[1]}}))
        .await.error_for_status().unwrap().json().await.unwrap();
    let number = pull["number"].as_u64().unwrap();
    let pull_api = format!("/api/repos/{owner}/{repo}/pulls/{number}");
    repository_write(h, Method::POST, &format!("{pull_api}/reviews"), reviewer, upstream_id,
        json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed before the head changed","commit_oid":tips[1]}))
        .await.error_for_status().unwrap();
    let fork = h.server.repositories.by_id(fork_id).unwrap();
    let protection = format!(
        "/api/repos/{}/{}/settings/branch-protections",
        fork.config.owner, fork.config.name
    );
    let state = h
        .json(
            &format!(
                "/api/repos?owner={}&name={}",
                fork.config.owner, fork.config.name
            ),
            writer,
        )
        .await;
    repository_write(h, Method::PUT, &protection, writer, fork_id,
        json!({"expected_version":state["repositories"][0]["protection_version"],"rules":[{"branch":"conflict-head","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    browser
        .run(&[
            "open",
            &format!("{}/{owner}/{repo}/pull/{number}", h.origin),
        ])
        .await;
    let click = async |action: &str| {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    };
    click("Update branch").await;
    click("Resolve conflicts").await;
    let editor = "[role='dialog'] textarea";
    browser.run(&["wait", editor]).await;
    let marked = browser
        .run(&[
            "eval",
            "document.querySelector('[role=dialog] textarea').value",
        ])
        .await;
    assert!(marked.contains("<<<<<<< PR head") && marked.contains(">>>>>>> Base branch"));
    assert!(marked.contains("fork version") && marked.contains("upstream version"));
    browser
        .run(&["fill", editor, "resolved in the browser\n"])
        .await;
    click("Mark as resolved").await;
    click("Commit merge").await;
    browser
        .run(&["wait", "--text", "The head branch requires a pull request"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Create a branch and update this pull request",
            "--exact",
        ])
        .await;
    h.drop_next_branch_update_reply
        .store(true, Ordering::SeqCst);
    click("Create branch and update PR").await;
    browser.run(&["wait", "--text", "The reply was lost"]).await;
    assert!(!h.drop_next_branch_update_reply.load(Ordering::SeqCst));
    let resolution_branch = format!("resolve-pr-{number}");
    git(checkout, &["fetch", fork_git, &resolution_branch]).await;
    let resolved = git(checkout, &["rev-parse", "FETCH_HEAD"]).await;
    assert_eq!(
        git(checkout, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{resolved} {} {}", tips[1], tips[0])
    );
    assert_eq!(
        git(checkout, &["show", "FETCH_HEAD:README.md"]).await,
        "resolved in the browser"
    );
    click("Retry commit").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    git(checkout, &["fetch", upstream_git, "conflict-base"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, tips[0]);
    git(checkout, &["fetch", fork_git, "conflict-head"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, tips[1]);
    let updated = h.json(&pull_api, writer).await;
    assert_eq!(updated["number"], number);
    assert_eq!(updated["head_repository"]["id"], fork_id.to_string());
    assert_eq!(
        updated["head_ref"],
        format!("refs/heads/{resolution_branch}")
    );
    assert_eq!(updated["head_oid"], resolved);
    assert_eq!(updated["original_head_oid"], tips[1]);
    assert_eq!(updated["base_revision"], updated["version"]);
    let review = h.json(&format!("{pull_api}/reviews/1"), writer).await;
    assert_eq!(review["state"], "dismissed");
    assert_eq!(
        review["dismissal"]["body"],
        "The pull request head branch changed."
    );
    browser
        .run(&["wait", "--text", "changed the head from conflict-head"])
        .await;
}

pub(super) async fn enable_up_to_date_rule(browser: &Browser, origin: &str, owner: &str) {
    browser
        .run(&[
            "open",
            &format!("{origin}/{owner}/library/settings/branches"),
        ])
        .await;
    browser
        .run(&["wait", "button[aria-label='Edit main']"])
        .await;
    browser
        .run(&["click", "button[aria-label='Edit main']"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Require branches to be up to date before merging",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save changes",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Up-to-date head branch required"])
        .await;
}

pub(super) async fn advance_base(checkout: &Path, upstream: &str, base: &str) -> String {
    git(checkout, &["checkout", "-b", "update-base", base]).await;
    std::fs::write(checkout.join("LATEST.txt"), "latest upstream changes\n").unwrap();
    git(checkout, &["add", "LATEST.txt"]).await;
    git(checkout, &["commit", "-m", "Advance the comparison base"]).await;
    git(checkout, &["push", upstream, "HEAD:refs/heads/main"]).await;
    let base = git(checkout, &["rev-parse", "HEAD"]).await;
    git(checkout, &["checkout", "main"]).await;
    base
}

pub(super) async fn update_fork(
    h: &Harness,
    browser: &Browser,
    (writer, reviewer): (&str, &str),
    (upstream_id, fork_id): (Uuid, Uuid),
    (checkout, upstream_git, fork_git): (&Path, &str, &str),
    (base, head): (&str, &str),
) -> String {
    let api_path = |id| {
        let repo = h.server.repositories.by_id(id).unwrap();
        format!("/api/repos/{}/{}", repo.config.owner, repo.config.name)
    };
    let route = format!("{}/upstream-pulls/1/update-branch", api_path(fork_id));
    let comparison = h.json(&route, writer).await;
    assert_eq!(comparison["update"]["base_oid"], base);
    assert_eq!(comparison["update"]["head_oid"], head);
    assert_eq!(
        h.http
            .get(format!("{}{route}", h.origin))
            .header(header::COOKIE, reviewer)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN,
    );
    let wrong = format!("{}/pulls/1/update-branch", api_path(upstream_id));
    assert_eq!(
        repository_write(
            h,
            Method::POST,
            &wrong,
            writer,
            upstream_id,
            comparison["update"].clone()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
    );
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Conversation",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--fn",
        "[...document.querySelectorAll('[aria-label=\"Update pull request branch\"] button')].some(button => !button.disabled && button.textContent.trim() === 'Update branch')"
    ]).await;
    let update = [
        "find",
        "role",
        "button",
        "click",
        "--name",
        "Update branch",
        "--exact",
    ];
    h.drop_next_branch_update_reply
        .store(true, Ordering::SeqCst);
    browser.run(&update).await;
    browser
        .run(&["wait", "--text", "The response was lost"])
        .await;
    assert!(!h.drop_next_branch_update_reply.load(Ordering::SeqCst));
    git(checkout, &["fetch", fork_git, "main"]).await;
    let commit = git(checkout, &["rev-parse", "FETCH_HEAD"]).await;
    assert_eq!(
        git(checkout, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{commit} {head} {base}"),
    );
    assert_eq!(
        git(checkout, &["show", "FETCH_HEAD:LATEST.txt"]).await,
        "latest upstream changes"
    );
    assert_eq!(
        git(checkout, &["show", "FETCH_HEAD:CONTRIBUTION.txt"]).await,
        "reviewed fork contribution"
    );
    browser.run(&update).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "!document.querySelector('[aria-label=\"Update pull request branch\"]')",
        ])
        .await;
    git(checkout, &["fetch", upstream_git, "main"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, base);
    git(checkout, &["fetch", fork_git, "main"]).await;
    assert_eq!(git(checkout, &["rev-parse", "FETCH_HEAD"]).await, commit);
    git(checkout, &["merge", "--ff-only", "FETCH_HEAD"]).await;
    commit
}
