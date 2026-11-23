use super::*;
use crate::cells::organization::{ChangeMember, ChangeOutcome, MemberChange, Role};
use crate::test_support::http::browser::Browser;

mod branch_update;
mod candidate;
mod issue_templates;
mod merge_queue;
mod retarget;
mod suggestions;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_schedules_auto_merge_and_checks_release_it() {
    use crate::test_support::git::success as git;

    let h = Harness::new(false).await;
    let writer = h.login().await;
    let root = "/api/repos/team/private";
    let settings = format!("{root}/settings/merge-methods");
    h.write(Method::PUT, &format!("{root}/settings/branch-protections"), &writer,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci"]}]}))
        .await.error_for_status().unwrap();
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
    let base = git(path, &["rev-parse", "main"]).await;
    h.write(Method::POST, &format!("{root}/pulls"), &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Merge after checks","body":"Fixes: #2. `Closes #3`. Resolves #999.","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    let issue = format!("{root}/issues/1");
    for title in [
        "Finish the checked change",
        "Close from the description",
        "Keep the code example open",
    ] {
        h.write(
            Method::POST,
            &format!("{root}/issues"),
            &writer,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let browser = Browser::new("auto-merge");
    browser
        .run(&["open", &format!("{}/team/private/settings", h.origin)])
        .await;
    let click = async |text: &str| {
        browser.run(&["wait", "--text", text]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", text, "--exact"])
            .await;
    };
    click("Continue to sign in").await;
    browser.run(&["wait", "--text", "Allow auto-merge"]).await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Allow auto-merge",
        ])
        .await;
    click("Save merge methods").await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('button')].some(button => button.disabled && button.textContent.trim() === 'Save merge methods')"]).await;
    assert_eq!(h.json(&settings, &writer).await["auto_merge"], true);
    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "From PR description"])
        .await;
    let related = h.json(&format!("{root}/issues/2/links"), &writer).await;
    assert_eq!(
        related["items"][0]["work"]["subject"],
        json!({"kind":"pull","number":1})
    );
    assert_eq!(related["items"][0]["from_description"], true);
    browser.run(&["wait", "--text", "Issue number"]).await;
    browser
        .run(&["find", "label", "Issue number", "fill", "1"])
        .await;
    browser
        .run(&["wait", "--fn", "[...document.querySelectorAll('button')].some(button => !button.disabled && button.textContent.trim() === 'Link issue')"])
        .await;
    click("Link issue").await;
    browser
        .run(&["wait", "--text", "#1 Finish the checked change"])
        .await;
    assert_eq!(h.json(&issue, &writer).await["state"], "open");
    browser.run(&["eval", "[...document.querySelectorAll('button')].find(button => button.textContent.trim() === 'Enable auto-merge')?.scrollIntoView({ block: 'center' })"]).await;
    click("Enable auto-merge").await;
    click("Confirm auto-merge").await;
    browser.run(&["wait", "--text", "Auto-merge enabled"]).await;
    git(path, &["fetch", remote.as_str(), "main"]).await;
    assert_eq!(git(path, &["rev-parse", "FETCH_HEAD"]).await, base);
    let worker = tokio::spawn(crate::api::auto_merge::run(std::sync::Arc::clone(
        &h.server,
    )));
    h.write(
        Method::POST,
        &format!("{root}/statuses/{head}"),
        &writer,
        json!({"request_id":Uuid::now_v7(),"context":"ci","state":"success"}),
    )
    .await
    .error_for_status()
    .unwrap();
    browser
        .run(&["wait", "--text", "Pull request merged"])
        .await;
    git(path, &["fetch", remote.as_str(), "main"]).await;
    let result = h.json(&format!("{root}/pulls/1"), &writer).await;
    assert_eq!(result["state"], "merged");
    assert_eq!(result["head_oid"], head);
    let commit = result["merge"]["commit_oid"].as_str().unwrap();
    assert_eq!(
        git(path, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{commit} {base} {head}")
    );
    let closed = h.json(&issue, &writer).await;
    assert_eq!(closed["state"], "closed");
    assert_eq!(closed["state_reason"], "completed");
    assert_eq!(
        h.json(&format!("{root}/issues/2"), &writer).await["state_reason"],
        "completed"
    );
    assert_eq!(
        h.json(&format!("{root}/issues/3"), &writer).await["state"],
        "open"
    );
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "#1 Finish the checked change",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "closed as completed"])
        .await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('.conversation-activity')].some(event => event.textContent.includes('closed as completed') && event.querySelector('a[href=\"/team/private/pull/1\"]')?.textContent === '#1')"]).await;
    drop(browser);
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_squashes_a_protected_private_fork_pull_request() {
    use crate::test_support::{git::success as git, http::github_workflow_delivery};
    let h = Harness::with_ci_key("browser-fork").await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let repo: Value = h
        .write(
            Method::POST,
            &format!("/api/organizations/{organization}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = repo["owner"].as_str().unwrap();
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/library.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let local = tempfile::tempdir().unwrap();
    let feature =
        crate::test_support::git::push_review_branches(local.path(), remote.as_str()).await;
    let ancestor = git(local.path(), &["rev-parse", "main"]).await;
    let browser = Browser::new("private-fork");
    browser
        .run(&["open", &format!("{}/{owner}/library", h.origin)])
        .await;
    for action in ["Continue to sign in", "Fork repository", "Create fork"] {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    }
    browser.run(&["wait", "--text", "Forked from"]).await;
    browser.run(&["wait", "--text", "README.md"]).await;
    let path = browser.run(&["get", "url"]).await;
    assert!(path.trim().ends_with(&format!("/{owner}/library-fork")));
    git(local.path(), &["checkout", "main"]).await;
    std::fs::write(local.path().join("UPSTREAM.txt"), "upstream change\n").unwrap();
    git(local.path(), &["add", "UPSTREAM.txt"]).await;
    git(local.path(), &["commit", "-m", "Advance upstream"]).await;
    git(local.path(), &["push", remote.as_str(), "main"]).await;
    let base_tip = git(local.path(), &["rev-parse", "main"]).await;
    let upstream_id = repo["id"].as_str().unwrap();
    let compare = format!(
        "/api/repos/{owner}/library-fork/changes?rev=refs/heads/feature&base=refs/heads/main&merge_base=true&upstream="
    );
    let comparison = h.json(&format!("{compare}{upstream_id}"), &alice).await;
    assert_eq!(comparison["base"], ancestor);
    assert_eq!(comparison["base_tip"], base_tip);
    assert_eq!(comparison["commit"], feature);
    let changes = comparison["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"], "README.md");
    let unrelated = h
        .http
        .get(format!("{}{compare}{}", h.origin, Uuid::now_v7()))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(unrelated.status(), StatusCode::NOT_FOUND);

    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Compare with upstream",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "This comparison has no changed files."])
        .await;
    browser
        .run(&["select", "#fork-compare-head", "refs/heads/feature"])
        .await;
    browser.run(&["wait", "--text", "1 changed file"]).await;
    browser
        .run(&["wait", "[data-change-path='README.md'] diffs-container"])
        .await;
    let diff = browser
        .run(&["get", "text", "[aria-label='Changed file diffs']"])
        .await;
    assert!(diff.contains("README.md"));
    assert!(!diff.contains("UPSTREAM.txt"));
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "← Back to fork",
            "--exact",
        ])
        .await;
    for action in ["Sync fork", "Update branch"] {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    }
    browser.run(&["wait", "--text", "Branch updated."]).await;
    browser.run(&["wait", "--text", "UPSTREAM.txt"]).await;
    let fork_token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"library-fork","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut fork_remote = remote.clone();
    fork_remote.set_path(&format!("/git/{owner}/library-fork.git"));
    fork_remote
        .set_password(fork_token["token"].as_str())
        .unwrap();
    std::fs::write(local.path().join("CONTRIBUTION.txt"), "from the fork\n").unwrap();
    let mut notes = String::from("draft notes\n");
    for line in 2..=19 {
        notes.push_str(&format!("Draft line {line}\n"));
    }
    std::fs::write(local.path().join("NOTES.txt"), notes).unwrap();
    git(local.path(), &["add", "CONTRIBUTION.txt", "NOTES.txt"]).await;
    git(local.path(), &["commit", "-m", "Contribute from the fork"]).await;
    git(local.path(), &["push", fork_remote.as_str(), "main"]).await;
    let head = git(local.path(), &["rev-parse", "HEAD"]).await;
    let base_tip = branch_update::advance_base(local.path(), remote.as_str(), &base_tip).await;
    let stable_tip = retarget::push_base(local.path(), remote.as_str(), &base_tip).await;
    let upstream = format!("/api/repos/{owner}/library");
    let id = Uuid::parse_str(upstream_id).unwrap();
    let connection = Uuid::now_v7();
    let credentials: Value = super::repositories::repository_write(
        &h, Method::POST, &format!("{upstream}/settings/integrations"), &alice, id,
        json!({"kind":"connect","id":connection,"name":"Build fork changes","github_repository":123,"workflow":456}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    let webhook = credentials["connection"]["url"].as_str().unwrap();
    let secret = credentials["secret"].as_str().unwrap();
    let mirror_branch = format!("crab/pulls/{id}/1");
    let workflow = |oid: &str, number: u64| {
        json!({
            "action":"completed","repository":{"id":123,"full_name":"example/mirror"},
            "workflow_run":{"id":789+number,"workflow_id":456,"run_number":number,"run_attempt":1,
                "head_sha":oid,"head_branch":mirror_branch,"head_repository":{"id":123},
                "event":"push","status":"completed","conclusion":"success"},
        })
    };
    let deliver = async |delivery: Uuid, body: &Value| {
        github_workflow_delivery(&h, webhook, secret, delivery, body, body).await
    };
    let fork_id = h
        .json(
            &format!("/api/repos?owner={owner}&name=library-fork"),
            &alice,
        )
        .await["repositories"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let settings = format!("{upstream}/settings/branch-protections");
    super::repositories::repository_write(&h, Method::PUT, &settings, &alice, id,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":["ci"],"required_integrations":[connection]}]})).await.error_for_status().unwrap();
    branch_update::enable_up_to_date_rule(&browser, &h.origin, owner).await;
    let saved = h
        .json(&format!("/api/repos?owner={owner}&name=library"), &alice)
        .await;
    assert_eq!(
        saved["repositories"][0]["protected_branches"][0]["require_up_to_date"],
        true
    );
    browser
        .run(&["open", &format!("{}/{owner}/library-fork", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Compare with upstream"])
        .await;
    let stale = super::repositories::repository_write(&h, Method::POST, &format!("{upstream}/pulls"), &alice, id,
        json!({"request_id":Uuid::now_v7(),"title":"Stale fork comparison","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/main","head_repository":fork_id,"expected":{"base_oid":base_tip,"head_oid":feature}})).await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<Value>().await.unwrap()["error"]["code"],
        "comparison_changed"
    );
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Compare with upstream",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#discussion-title"]).await;
    browser
        .run(&["fill", "#discussion-title", "Contribute from the fork"])
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
    browser.run(&["wait", "--text", "Merging is blocked"]).await;
    assert!(
        browser
            .run(&["get", "url"])
            .await
            .trim()
            .ends_with(&format!("/{owner}/library/pull/1"))
    );
    let pull = format!("{upstream}/pulls/1");
    let created = h.json(&pull, &alice).await;
    assert_eq!(created["original_base_oid"], base_tip);
    assert_eq!(created["original_head_oid"], head);
    assert_eq!(created["head_repository"]["name"], "library-fork");
    assert_eq!(created["head_ref"], "refs/heads/main");
    assert_eq!(created["base_ref"], "refs/heads/main");
    assert_eq!(created["merge_requirements"]["head_up_to_date"], false);
    browser
        .run(&[
            "wait",
            "--text",
            "The head branch is behind the base branch",
        ])
        .await;
    let base_revision =
        retarget::change_base(&h, &browser, &alice, &pull, (&base_tip, &stable_tip)).await;

    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Files changed",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "[data-change-path='CONTRIBUTION.txt'] diffs-container",
        ])
        .await;
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Commits", "--exact",
        ])
        .await;
    browser.run(&["wait", ".commit-subject"]).await;
    let commits = browser.run(&["get", "text", ".commit-list"]).await;
    assert!(commits.contains("Contribute from the fork"));
    assert!(!commits.contains("Advance upstream"));

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let head = suggestions::apply_to_fork(
        &h,
        &browser,
        (id, fork_id),
        (&alice, &bob),
        (local.path(), remote.as_str(), fork_remote.as_str()),
    )
    .await;
    let head = branch_update::update_fork(
        &h,
        &browser,
        (&alice, &bob),
        (id, fork_id),
        (local.path(), remote.as_str(), fork_remote.as_str()),
        (&base_tip, &head),
    )
    .await;
    let members = format!("{upstream}/members");
    assert_eq!(
        h.json(&pull, &alice).await["merge_requirements"]["head_up_to_date"],
        true
    );
    let reviewer_access = async |access: &str| {
        let acl = h.json(&members, &alice).await;
        super::repositories::repository_write(&h, Method::PUT, &members, &alice, id,
            json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":access}],"groups":[]})).await.error_for_status().unwrap();
    };
    reviewer_access("write").await;
    let approve = async |head: &str| {
        super::repositories::repository_write(&h, Method::POST, &format!("{pull}/reviews"), &bob, id,
            json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed the fork commit","commit_oid":head,"base_revision":base_revision})).await.error_for_status().unwrap();
        super::repositories::repository_write(
            &h,
            Method::POST,
            &format!("{upstream}/statuses/{head}?pull=1"),
            &bob,
            id,
            json!({"request_id":Uuid::now_v7(),"state":"success","context":"ci"}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    approve(&head).await;
    for path in ["reviewers", "conversation", "reviews"] {
        let page = h.json(&format!("{pull}/{path}"), &alice).await;
        assert_eq!(page["head_oid"], head, "{path}");
    }
    browser
        .run(&["open", &format!("{}/{owner}/library/pull/1", h.origin)])
        .await;
    browser.run(&["wait", "--text", "Eligible approval"]).await;
    assert_eq!(
        browser
            .run(&["get", "attr", ".required-checks-summary a", "href"])
            .await
            .trim(),
        format!("/{owner}/library-fork/commit/{head}?repository_id={fork_id}")
    );
    let refresh_checks = async || {
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
    };
    reviewer_access("read").await;
    refresh_checks().await;
    browser
        .run(&["wait", "--fn", "document.querySelector('.reviewers')?.textContent.includes('Write access required') && document.querySelector('[aria-label=\"Merge status\"]')?.textContent.includes('1 more approving review is required')"])
        .await;
    reviewer_access("write").await;
    refresh_checks().await;
    browser.run(&["wait", "--text", "Eligible approval"]).await;
    let completed = workflow(&head, 1);
    for (field, value) in [
        ("head_repository", json!({"id":999})),
        ("head_sha", json!(base_tip)),
        (
            "head_branch",
            json!(format!("crab/pulls/{}/1", Uuid::now_v7())),
        ),
        ("head_branch", json!(format!("crab/pulls/{id}/2"))),
        ("head_branch", json!("another-branch")),
    ] {
        let mut wrong = completed.clone();
        wrong["workflow_run"][field] = value;
        assert_eq!(deliver(Uuid::now_v7(), &wrong).await, StatusCode::NOT_FOUND);
    }
    assert_eq!(h.json(&pull, &alice).await["can_merge"], false);

    browser.run(&["reload"]).await;
    browser
        .run(&["wait", ".pull-tabs a[href$='/checks']"])
        .await;
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Checks", "--exact",
        ])
        .await;
    browser.run(&["wait", "#github-actions-refspec"]).await;
    let refspec = browser
        .run(&[
            "eval",
            "document.getElementById('github-actions-refspec').value",
        ])
        .await;
    let refspec = refspec.trim().trim_matches('"');
    assert_eq!(refspec, format!("{head}:refs/heads/{mirror_branch}"));
    let mirror = tempfile::tempdir().unwrap();
    git(mirror.path(), &["init", "--bare"]).await;
    git(
        local.path(),
        &["push", mirror.path().to_str().unwrap(), refspec],
    )
    .await;
    assert_eq!(
        git(
            mirror.path(),
            &["rev-parse", &format!("refs/heads/{mirror_branch}")]
        )
        .await,
        head
    );
    let candidate_report = candidate::verify(
        &h,
        &browser,
        (&alice, &pull),
        (local.path(), remote.as_str(), mirror.path()),
        (&base_tip, &head),
        (webhook, secret),
        workflow(&head, 20),
    )
    .await;
    assert_eq!(h.json(&pull, &alice).await["can_merge"], false);
    let delivery = Uuid::now_v7();
    assert_eq!(deliver(delivery, &completed).await, StatusCode::NO_CONTENT);
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Provider · GitHub Actions"])
        .await;
    assert_eq!(
        browser
            .run(&["get", "attr", "a[href*='/actions/runs/']", "href"])
            .await
            .trim(),
        "https://github.com/example/mirror/actions/runs/790"
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
    browser.run(&["reload"]).await;
    let click_merge = async |action: &str| {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    };
    let choose_squash = async || {
        browser
            .run(&["wait", "button[aria-label='Select merge method']"])
            .await;
        browser
            .run(&[
                "find",
                "role",
                "button",
                "click",
                "--name",
                "Select merge method",
                "--exact",
            ])
            .await;
        browser
            .run(&[
                "find",
                "role",
                "menuitemradio",
                "click",
                "--name",
                "Squash and merge",
            ])
            .await;
        click_merge("Squash and merge").await;
    };
    choose_squash().await;
    git(
        local.path(),
        &["commit", "--allow-empty", "-m", "Update the reviewed head"],
    )
    .await;
    git(local.path(), &["push", fork_remote.as_str(), "main"]).await;
    let new_head = git(local.path(), &["rev-parse", "HEAD"]).await;
    assert_eq!(
        deliver(Uuid::now_v7(), &candidate_report).await,
        StatusCode::NOT_FOUND
    );
    click_merge("Confirm merge").await;
    browser
        .run(&["wait", "--text", "The branch tips changed"])
        .await;
    browser.run(&["wait", "--text", "Merging is blocked"]).await;
    browser
        .run(&["wait", "--fn", "document.querySelector('.reviewers')?.textContent.includes('Outdated review') && document.querySelector('.review-event')?.textContent.includes('Outdated')"])
        .await;
    git(local.path(), &["fetch", remote.as_str(), "main"]).await;
    assert_eq!(
        git(local.path(), &["rev-parse", "FETCH_HEAD"]).await,
        base_tip
    );

    approve(&new_head).await;
    assert_eq!(deliver(delivery, &completed).await, StatusCode::NO_CONTENT);
    assert_eq!(
        deliver(Uuid::now_v7(), &completed).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.json(&pull, &alice).await["can_merge"], false);
    assert_eq!(
        deliver(Uuid::now_v7(), &workflow(&new_head, 2)).await,
        StatusCode::NO_CONTENT
    );
    browser.run(&["reload"]).await;
    choose_squash().await;
    h.drop_next_merge_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    click_merge("Confirm merge").await;
    browser
        .run(&["wait", "--text", "Pull request merged"])
        .await;
    assert!(
        !h.drop_next_merge_reply
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    git(local.path(), &["fetch", remote.as_str(), "main"]).await;
    let landed = git(local.path(), &["rev-parse", "FETCH_HEAD"]).await;
    let merged = h.json(&pull, &alice).await;
    for path in ["reviewers", "conversation", "reviews"] {
        assert_eq!(
            h.json(&format!("{pull}/{path}"), &alice).await["head_oid"],
            new_head,
            "{path}"
        );
    }
    assert_eq!(merged["merge"]["method"], "squash");
    assert_eq!(merged["merge"]["commit_oid"], landed);
    assert_ne!(landed, new_head);
    assert_eq!(
        git(local.path(), &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{landed} {base_tip}")
    );
    assert_eq!(
        git(local.path(), &["show", "FETCH_HEAD:CONTRIBUTION.txt"]).await,
        "reviewed fork contribution"
    );
    branch_update::resolve_fork(
        &h,
        &browser,
        (&alice, &bob),
        (id, fork_id),
        (local.path(), remote.as_str(), fork_remote.as_str()),
        &landed,
    )
    .await;
    branch_update::rebase_fork(
        &h,
        &browser,
        &alice,
        (id, fork_id),
        (local.path(), remote.as_str(), fork_remote.as_str()),
        &landed,
    )
    .await;
    drop(browser);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_saves_repository_team_access_and_review_requests() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "browser-team-access").await;
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
    *h.provider.mode.lock().await = "valid".into();
    let org = format!("/api/organizations/{organization}");
    let team = Uuid::now_v7();
    h.write(Method::POST, &format!("{org}/teams/commands"), &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Engineering","description":"","visibility":"secret"}}))
        .await.error_for_status().unwrap();
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
    h.write(Method::POST, &format!("{org}/teams/commands"), &alice,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":bob_id,"version":0,"role":"member"}}))
        .await.error_for_status().unwrap();
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let routing = format!("{org}/teams/{team}/review-routing");
    assert_eq!(
        h.http
            .get(format!("{}{routing}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let repository: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = repository["owner"].as_str().unwrap();
    let url = format!("{}/{owner}/library/settings/team-access", h.origin);
    let browser = Browser::new("team-access");
    browser.run(&["open", &url]).await;
    let actions: [(&[&str], &str); 3] = [
        (
            &["wait", "--text", "Continue to sign in"],
            "Continue to sign in",
        ),
        (&["wait", "--text", "Add team"], "Add team"),
        (
            &["wait", "[aria-label='Add Engineering']"],
            "Add Engineering",
        ),
    ];
    for (wait, label) in actions {
        browser.run(wait).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", label, "--exact",
            ])
            .await;
    }
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save team access",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Team access saved."]).await;
    assert_eq!(
        h.json(&format!("/api/repos/{owner}/library/team-access"), &alice)
            .await["organization"]["teams"],
        json!([{"team":team,"access":"read"}])
    );
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "[aria-label='Role for Engineering']"])
        .await;

    let properties = format!("{}/-/organizations/{organization}?tab=properties", h.origin);
    browser.run(&["open", &properties]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "New property",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "fill",
            "#repository-property-definition input",
            "native-policy",
        ])
        .await;
    browser
        .run(&[
            "select",
            "#repository-property-definition select",
            "boolean",
        ])
        .await;
    browser
        .run(&[
            "click",
            "#repository-property-definition input[type='checkbox']",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save property",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "[aria-label='Edit native-policy']"])
        .await;
    browser
        .run(&[
            "find", "role", "checkbox", "click", "--name", "library", "--exact",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Edit properties",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "#repository-property-values select"])
        .await;
    browser
        .run(&["select", "#repository-property-values select", "set"])
        .await;
    browser
        .run(&[
            "select",
            "#repository-property-values select:has(option[value='false'])",
            "false",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save repository values",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Repository values saved for 1 repository.",
        ])
        .await;
    let saved = h
        .json(&format!("/api/repos/{owner}/library/properties"), &alice)
        .await;
    let property = saved["schema"]["definitions"][0]["id"].as_str().unwrap();
    assert_eq!(saved["values"]["overrides"][property], false);

    let repository_list = format!(
        "{}/-/organizations/{organization}?tab=repositories",
        h.origin
    );
    browser.run(&["open", &repository_list]).await;
    browser.run(&["wait", "--text", "Filter by property"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Filter by property",
            "--exact",
        ])
        .await;
    let chooser = "form[aria-label='Repository property filter'] select";
    browser.run(&["wait", chooser]).await;
    browser.run(&["select", chooser, property]).await;
    browser
        .run(&[
            "select",
            "form[aria-label='Repository property filter'] select:has(option[value='false'])",
            "false",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Add filter",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**q=props.native-policy%3Afalse"])
        .await;
    browser
        .run(&["wait", "[aria-label='Repository results'] a"])
        .await;
    browser
        .run(&["fill", "input[name=q]", "props.native-policy:true"])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No repositories match this search."])
        .await;

    browser
        .run(&[
            "open",
            &format!("{}/-/organizations/{organization}?tab=rulesets", h.origin),
        ])
        .await;
    browser.run(&["wait", "--text", "New ruleset"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "New ruleset",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Name",
            "fill",
            "Browser fork policy",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "select",
            "select:has(option[value='properties'])",
            "properties",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Add condition",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "select",
            "select:has(option[value='system:fork'])",
            "system:fork",
        ])
        .await;
    browser
        .run(&[
            "select",
            "select:has(option[value='true']):has(option[value='false'])",
            "true",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save ruleset",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Repository type matches Fork"])
        .await;
    assert_eq!(
        h.json(&format!("{org}/rulesets"), &alice).await["rulesets"][0]["repositories"]["conditions"],
        json!([{"system":"fork","value":true}])
    );

    let team_settings = format!(
        "{}/-/organizations/{organization}?tab=teams&team={team}&view=settings",
        h.origin
    );
    browser.run(&["open", &team_settings]).await;
    browser.run(&["wait", "#review-routing-algorithm"]).await;
    browser
        .run(&["select", "#review-routing-algorithm", "fewest_open"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save review assignment",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Update review assignment completed."])
        .await;
    assert_eq!(
        h.json(&routing, &alice).await,
        json!({"version":1,"algorithm":"fewest_open","reviewers":1,"excluded":[],"options":{"count_existing":false,"keep_team":false}})
    );
    browser.run(&["reload"]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('#review-routing-algorithm')?.selectedOptions[0]?.textContent?.trim() === 'Load balance'",
        ])
        .await;

    let repo_id = Uuid::parse_str(repository["id"].as_str().unwrap()).unwrap();
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/library.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    crate::test_support::git::push_review_branches(source.path(), remote.as_str()).await;
    let pulls = format!("/api/repos/{owner}/library/pulls");
    super::repositories::repository_write(&h, Method::POST, &pulls, &alice, repo_id,
        json!({"request_id":Uuid::now_v7(),"title":"Review this change","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    browser
        .run(&["open", &format!("{}/{owner}/library/pull/1", h.origin)])
        .await;
    for name in ["Request reviewers", "Request review from team Engineering"] {
        let selector = format!("[aria-label='{name}']");
        browser.run(&["wait", &selector]).await;
        browser.run(&["click", &selector]).await;
    }
    browser
        .run(&["wait", "--text", "Team review requested"])
        .await;
    browser
        .run(&["open", &format!("{}/{owner}/library/pulls", h.origin)])
        .await;
    browser
        .run(&["wait", "[aria-label='Filter by reviews']"])
        .await;
    browser
        .run(&["click", "[aria-label='Filter by reviews']"])
        .await;
    browser
        .run(&["wait", "--text", "Awaiting review from you or your team"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "menuitemradio",
            "click",
            "--name",
            "Awaiting review from you or your team",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--fn",
            "new URL(location.href).searchParams.get('q')?.includes('review-requested:@me')",
        ])
        .await;
    browser.run(&["wait", "--text", "Review this change"]).await;
    browser
        .run(&[
            "find",
            "role",
            "combobox",
            "fill",
            "team-review-requested:eng",
        ])
        .await;
    browser.run(&["wait", "[role='option']"]).await;
    browser.run(&["press", "ArrowDown"]).await;
    browser.run(&["press", "Enter"]).await;
    browser.run(&["press", "Enter"]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "new URL(location.href).searchParams.get('q')?.includes('Engineering')",
        ])
        .await;
    browser.run(&["wait", "--text", "Review this change"]).await;
    browser
        .run(&["open", &format!("{}/{owner}/library/pull/1", h.origin)])
        .await;
    let cancel = "[aria-label='Cancel review request for team Engineering']";
    browser.run(&["wait", cancel]).await;
    let inbox = format!("/api/notifications?repository={repo_id}");
    h.deliver_notifications(owner, "library").await;
    assert_eq!(h.json(&inbox, &bob).await["items"], json!([]));
    browser.run(&["click", cancel]).await;
    browser.run(&["wait", "--text", "No reviews yet."]).await;
    let reviewers = h.json(&format!("{pulls}/1/reviewers"), &alice).await;
    assert_eq!(reviewers["teams"][0]["state"], Value::Null);
    // Give Bob Write so that only the team exclusion can prevent an assignment.
    let endpoint = format!("/api/repos/{owner}/library/team-access");
    let revision = h.json(&endpoint, &alice).await["revision"].clone();
    super::repositories::repository_write(
        &h,
        Method::PUT,
        &endpoint,
        &alice,
        repo_id,
        json!({"expected_revision":revision,"teams":[{"team":team,"access":"write"}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    browser.run(&["open", &team_settings]).await;
    let excluded = format!("#review-routing-exclude-{bob_id}");
    let save_routing = [
        "find",
        "role",
        "button",
        "click",
        "--name",
        "Save review assignment",
        "--exact",
    ];
    browser.run(&["wait", &excluded]).await;
    browser.run(&["click", &excluded]).await;
    browser.run(&save_routing).await;
    browser
        .run(&["wait", "--text", "Update review assignment completed."])
        .await;
    assert_eq!(
        h.json(&routing, &alice).await,
        json!({"version":2,"algorithm":"fewest_open","reviewers":1,"excluded":[bob_id],"options":{"count_existing":false,"keep_team":false}})
    );
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", &format!("{excluded}[aria-checked='true']")])
        .await;

    let pull_url = format!("{}/{owner}/library/pull/1", h.origin);
    browser.run(&["open", &pull_url]).await;
    let chooser = "[aria-label='Request reviewers']";
    let request_team = "[aria-label='Request review from team Engineering']";
    for selector in [chooser, request_team] {
        browser.run(&["wait", selector]).await;
        browser.run(&["click", selector]).await;
    }
    browser.run(&["wait", cancel]).await;
    let reviewers = h.json(&format!("{pulls}/1/reviewers"), &alice).await;
    assert_eq!(reviewers["teams"][0]["state"]["kind"], "requested");
    assert_eq!(reviewers["items"], json!([]));
    h.deliver_notifications(owner, "library").await;
    assert_eq!(h.json(&inbox, &bob).await["items"], json!([]));
    browser.run(&["click", chooser]).await;
    browser
        .run(&["wait", "[aria-label='Request review from bob']"])
        .await;
    browser.run(&["press", "Escape"]).await;
    browser.run(&["click", cancel]).await;
    browser.run(&["wait", "--text", "No reviews yet."]).await;

    browser.run(&["open", &team_settings]).await;
    browser
        .run(&["wait", &format!("{excluded}[aria-checked='true']")])
        .await;
    browser.run(&["click", &excluded]).await;
    for option in ["#review-routing-existing", "#review-routing-keep-team"] {
        browser.run(&["click", option]).await;
    }
    browser.run(&save_routing).await;
    browser
        .run(&["wait", "--text", "Update review assignment completed."])
        .await;
    let updated = h.json(&routing, &alice).await;
    assert_eq!(updated["excluded"], json!([]));
    assert_eq!(
        updated["options"],
        json!({"count_existing":true,"keep_team":true})
    );

    // Availability is self-managed in a separate signed-in browser.
    let bob_browser = Browser::new("review-availability");
    *h.provider.mode.lock().await = "member".into();
    bob_browser
        .run(&[
            "open",
            &format!("{}/-/settings/review-availability", h.origin),
        ])
        .await;
    bob_browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    bob_browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    let status = "#review-availability-choice";
    let save = [
        "find",
        "role",
        "button",
        "click",
        "--name",
        "Save availability",
        "--exact",
    ];
    bob_browser.run(&["wait", status]).await;
    *h.provider.mode.lock().await = "valid".into();
    bob_browser.run(&["select", status, "one_hour"]).await;
    bob_browser.run(&save).await;
    bob_browser
        .run(&["wait", "--text", "Review availability saved."])
        .await;
    let preference = "/api/profile/review-availability";
    let busy = h.json(preference, &bob).await;
    assert_eq!(busy["busy"], true);
    assert!(busy["expires_at_ms"].is_number());
    assert_eq!(h.json(preference, &alice).await["busy"], false);
    bob_browser.run(&["reload"]).await;
    bob_browser
        .run(&["wait", "--text", "Current status Busy"])
        .await;

    browser.run(&["open", &pull_url]).await;
    for selector in [chooser, request_team] {
        browser.run(&["wait", selector]).await;
        browser.run(&["click", selector]).await;
    }
    browser.run(&["wait", cancel]).await;
    let waiting = h.json(&format!("{pulls}/1/reviewers"), &alice).await;
    assert_eq!(waiting["items"], json!([]));
    h.deliver_notifications(owner, "library").await;
    assert_eq!(h.json(&inbox, &bob).await["items"], json!([]));
    browser.run(&["click", chooser]).await;
    browser
        .run(&["wait", "[aria-label='Request review from bob']"])
        .await;
    browser.run(&["press", "Escape"]).await;

    bob_browser.run(&["select", status, "available"]).await;
    bob_browser.run(&save).await;
    bob_browser
        .run(&["wait", "--text", "Review availability saved."])
        .await;
    assert_eq!(h.json(preference, &bob).await["busy"], false);
    let repository_rulesets = format!("{}/{owner}/library/rulesets", h.origin);
    let property_values = format!("/api/repos/{owner}/library/properties");
    let change_team_access = async |access: &str| {
        let revision = h.json(&endpoint, &alice).await["revision"].clone();
        super::repositories::repository_write(
            &h,
            Method::PUT,
            &endpoint,
            &alice,
            repo_id,
            json!({"expected_revision":revision,"teams":[{"team":team,"access":access}]}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    assert_eq!(h.json(&property_values, &bob).await["editable"], json!([]));
    change_team_access("admin").await;
    assert_eq!(
        h.json(&property_values, &bob).await["editable"],
        json!([property])
    );
    bob_browser.run(&["open", &repository_rulesets]).await;
    bob_browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Repository properties",
            "--exact",
        ])
        .await;
    bob_browser.run(&["wait", "--text", "native-policy"]).await;
    bob_browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Edit properties",
            "--exact",
        ])
        .await;
    bob_browser
        .run(&["select", "form select:has(option[value='true'])", "true"])
        .await;
    bob_browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save property values",
            "--exact",
        ])
        .await;
    bob_browser
        .run(&["wait", "--text", "Repository property values saved."])
        .await;
    assert_eq!(
        h.json(&property_values, &alice).await["values"]["overrides"][property],
        true
    );
    change_team_access("write").await;
    bob_browser.run(&["reload"]).await;
    bob_browser.run(&["wait", "--text", "native-policy"]).await;
    let after_revocation = bob_browser.run(&["snapshot"]).await;
    assert!(!after_revocation.contains("Edit properties"));
    let current = h.json(&property_values, &bob).await;
    assert_eq!(
        super::repositories::repository_write(
            &h, Method::PUT, &property_values, &bob, repo_id,
            json!({"schema_version":current["schema"]["version"],"expected_version":current["values"]["version"],"overrides":{(property):false}}),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
    );
    drop(bob_browser);
    browser.run(&["click", cancel]).await;
    browser.run(&["wait", "--text", "No reviews yet."]).await;
    for selector in [chooser, request_team] {
        browser.run(&["wait", selector]).await;
        browser.run(&["click", selector]).await;
    }
    browser
        .run(&["wait", "[aria-label='Cancel review request for bob']"])
        .await;
    let routed = h.json(&format!("{pulls}/1/reviewers"), &alice).await;
    assert_eq!(routed["teams"][0]["state"]["kind"], "requested");
    assert_eq!(routed["items"][0]["request"]["state"]["kind"], "requested");
    h.deliver_notifications(owner, "library").await;
    let inbox = h.json(&inbox, &bob).await;
    assert_eq!(inbox["items"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["items"][0]["reason"], "review_requested");

    // Add another eligible reviewer. Re-requesting the team should reuse
    // Bob's pending individual request and leave the new member unassigned.
    let alternate = h
        .server
        .cells()
        .unwrap()
        .command::<crate::cells::directory::RegisterAccounts>(
            crate::cells::Address::Directory,
            crate::app::mutation_identity().unwrap(),
            vec![crate::cells::directory::IdentityKey {
                issuer: h.provider.issuer.clone(),
                subject: "outsider-id".into(),
            }],
        )
        .await
        .unwrap()
        .output[0];
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    *h.provider.mode.lock().await = "valid".into();
    h.write(
        Method::PUT,
        &format!("{org}/invitations/{alternate}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{org}/invitation"),
        &outsider,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::POST, &format!("{org}/teams/commands"), &alice,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":alternate,"version":0,"role":"member"}}))
        .await.error_for_status().unwrap();
    let visible = h
        .json(
            &format!("/api/repos?organization={organization}"),
            &outsider,
        )
        .await;
    assert_eq!(visible["repositories"][0]["id"], json!(repo_id));
    assert_eq!(visible["repositories"][0]["access"], "write");
    browser.run(&["click", cancel]).await;
    let ready = format!(
        "!document.querySelector({cancel:?}) && !!document.querySelector(\"{chooser}:not([disabled])\")"
    );
    browser.run(&["wait", "--fn", &ready]).await;
    for selector in [chooser, request_team] {
        browser.run(&["wait", selector]).await;
        browser.run(&["click", selector]).await;
    }
    browser.run(&["wait", cancel]).await;
    let counted = h.json(&format!("{pulls}/1/reviewers"), &alice).await;
    assert_eq!(counted["items"], routed["items"]);
    assert_eq!(counted["teams"][0]["state"]["kind"], "requested");

    browser
        .run(&[
            "open",
            &format!("{}/{owner}/library/settings/rulesets", h.origin),
        ])
        .await;
    browser.run(&["wait", "--text", "New ruleset"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "New ruleset",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Name",
            "fill",
            "Release approval",
            "--exact",
        ])
        .await;
    browser
        .run(&["find", "label", "Restrict updates", "click", "--exact"])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Require linear history",
            "click",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Required approving reviews",
            "fill",
            "1",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "select",
            "select:has(option[value='organization_owner'])",
            "organization_owner",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save ruleset",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "1 bypass grant"]).await;

    let rulesets = h
        .json(&format!("/api/repos/{owner}/library/rulesets"), &alice)
        .await;
    let rule = &rulesets["rulesets"][0];
    assert_eq!(rule["required_approvals"], 1);
    assert_eq!(rule["restrict_updates"], true);
    assert_eq!(rule["require_linear_history"], true);
    assert_eq!(
        rule["bypass_actors"],
        json!([{"actor":{"kind":"organization_owner"},"mode":"pull_request"}])
    );
    browser
        .run(&["open", &format!("{}/{owner}/library/pull/1", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "A ruleset restricts updates to main."])
        .await;
    browser
        .run(&["wait", "--text", "main requires linear history."])
        .await;
    for label in ["Merge with ruleset bypass", "Confirm bypass and merge"] {
        browser.run(&["wait", "--text", label]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", label, "--exact",
            ])
            .await;
    }
    browser
        .run(&["wait", "--text", "Ruleset bypass used."])
        .await;
    let completed = h.json(&format!("{pulls}/1"), &alice).await;
    assert_eq!(completed["merge"]["bypass_rulesets"], json!([rule["id"]]));
    let advertised = crate::test_support::git::success(
        source.path(),
        &["ls-remote", remote.as_str(), "refs/heads/main"],
    )
    .await;
    assert_eq!(
        advertised,
        format!(
            "{}\trefs/heads/main",
            completed["merge"]["commit_oid"].as_str().unwrap()
        )
    );
    drop(browser);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cell fixtures"]
async fn native_browser_invitation_consent_and_removed_member_privacy() {
    let h = Harness::new(false).await;
    let (id, alice, bob) = crate::test_support::http::organizations::bootstrap(&h).await;
    let owner = Browser::new("owner");
    let invitee = Browser::new("invitee");
    let root = format!("{}/-/organizations/{id}", h.origin);
    owner.run(&["open", &root]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    owner.run(&["wait", "--text", "Organization members"]).await;
    *h.provider.mode.lock().await = "member".into();
    invitee
        .run(&["open", &format!("{}/-/profile", h.origin)])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    invitee.run(&["wait", "--text", "Public profile"]).await;
    invitee
        .run(&["find", "label", "Username", "fill", "bob-qualification"])
        .await;
    invitee
        .run(&["find", "label", "Display name", "fill", "Bob via browser"])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create profile",
            "--exact",
        ])
        .await;
    invitee.run(&["wait", "--text", "Profile created."]).await;
    owner
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Invitations",
            "--exact",
        ])
        .await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Invite member",
            "--exact",
        ])
        .await;
    owner
        .run(&["find", "label", "Username", "fill", "bob-qualification"])
        .await;
    owner.run(&["wait", "--text", "Bob via browser"]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Bob via browser @bob-qualification",
            "--exact",
        ])
        .await;
    owner.run(&["wait", "button[type='submit']:enabled"]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create invitation",
            "--exact",
        ])
        .await;
    owner.run(&["wait", "--text", "Invitation created."]).await;
    let cells = h.server.cells().unwrap();
    let before = cells
        .query::<ListMembers>(
            Address::Organization(id),
            None,
            PageQuery {
                actor: alice,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output;
    let Page::Page { items, .. } = before else {
        panic!("page changed")
    };
    assert_eq!(items.len(), 1);
    invitee.run(&["open", &format!("{root}/invitation")]).await;
    invitee
        .run(&["wait", "--text", "Invitation to Crab engineering"])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Join organization",
            "--exact",
        ])
        .await;
    invitee
        .run(&["wait", "--text", "You accepted this invitation."])
        .await;
    invitee
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "View organization",
            "--exact",
        ])
        .await;
    invitee
        .run(&["wait", "--text", "Organization members"])
        .await;
    invitee.run(&["wait", "--text", "Bob via browser"]).await;
    let after = cells
        .query::<ListMembers>(
            Address::Organization(id),
            None,
            PageQuery {
                actor: alice,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap()
        .output;
    let Page::Page { items, .. } = after else {
        panic!("page changed")
    };
    let member = items.iter().find(|member| member.actor == bob).unwrap();
    assert_eq!((items.len(), member.role), (2, Role::Member));
    let controls = invitee.run(&["snapshot", "-i"]).await;
    assert!(!controls.contains("Remove"));
    let removed = cells
        .command::<ChangeMember>(
            Address::Organization(id),
            app::mutation_identity().unwrap(),
            MemberChange {
                actor: alice,
                member: bob,
                version: member.version,
                role: None,
            },
        )
        .await
        .unwrap()
        .output;
    assert!(matches!(removed, ChangeOutcome::Applied(_)));
    invitee.run(&["open", &format!("{root}/invitation")]).await;
    invitee
        .run(&["wait", "--text", "Organization invitation"])
        .await;
    let history = invitee.run(&["snapshot"]).await;
    assert!(
        history.contains("You accepted this invitation.")
            && !history.contains("Crab engineering")
            && !history.contains("View organization")
    );
    drop(invitee);
    drop(owner);
    h.close().await;
}
