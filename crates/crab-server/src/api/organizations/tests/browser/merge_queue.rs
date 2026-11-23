use super::{Browser, Harness, Uuid, Value, json};
use crate::test_support::git::{push_review_branches, success as git};
use reqwest::Method;
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_queue_publishes_tested_commits_in_admission_order() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let root = "/api/repos/team/private";
    let members = format!("{root}/members");
    let revision = h.json(&members, &writer).await["revision"].clone();
    h.write(Method::PUT, &members, &writer, json!({"expected_revision":revision,"members":[
        {"subject":"alice-id","name":"Alice","access":"admin"}, {"subject":"bob-id","name":"Bob","access":"read"}],"groups":[]}))
        .await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    *h.provider.mode.lock().await = "valid".into();
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
    let first_head = push_review_branches(path, remote.as_str()).await;
    git(path, &["checkout", "-b", "feature-two", "main"]).await;
    std::fs::write(path.join("second.txt"), "second feature\n").unwrap();
    git(path, &["add", "second.txt"]).await;
    git(path, &["commit", "-m", "Second feature"]).await;
    let second_head = git(path, &["rev-parse", "HEAD"]).await;
    git(path, &["push", remote.as_str(), "feature-two"]).await;
    git(path, &["checkout", "-b", "integration", "main"]).await;
    std::fs::write(path.join("base.txt"), "independent base\n").unwrap();
    git(path, &["add", "base.txt"]).await;
    git(path, &["commit", "-m", "Advance integration"]).await;
    let base = git(path, &["rev-parse", "HEAD"]).await;
    git(path, &["push", remote.as_str(), "integration"]).await;

    h.write(
        Method::PUT,
        &format!("{root}/settings/branch-protections"),
        &writer,
        json!({
            "expected_version":0,"rules":[{
                "branch":"integration","required_approvals":0,"required_checks":["ci"],
                "require_up_to_date":true
            }]
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let report = async |commit: &str, state: &str| {
        h.write(
            Method::POST,
            &format!("{root}/statuses/{commit}"),
            &writer,
            json!({"request_id":Uuid::now_v7(),"context":"ci","state":state}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    for (branch, head) in [("feature", &first_head), ("feature-two", &second_head)] {
        h.write(
            Method::POST,
            &format!("{root}/pulls"),
            &writer,
            json!({
                "request_id":Uuid::now_v7(),"title":branch,"body":"",
                "base_ref":"refs/heads/integration","head_ref":format!("refs/heads/{branch}"),
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        report(head, "success").await;
    }
    let browser = Browser::new("merge-queue");
    let click = async |action: &str| {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    };
    browser
        .run(&["open", &format!("{}/team/private/settings", h.origin)])
        .await;
    click("Continue to sign in").await;
    browser.run(&["wait", "--text", "Allow merge queue"]).await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Allow merge queue",
        ])
        .await;
    click("Save merge methods").await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('button')].some(button => button.disabled && button.textContent.trim() === 'Save merge methods')"]).await;
    let first = format!("{root}/pulls/1");
    let second = format!("{root}/pulls/2");
    for (number, queued) in [
        (1, "Preparing merge candidate"),
        (2, "Waiting in the merge queue"),
    ] {
        browser
            .run(&["open", &format!("{}/team/private/pull/{number}", h.origin)])
            .await;
        click("Add to merge queue").await;
        click("Confirm merge queue entry").await;
        browser.run(&["wait", "--text", queued]).await;
    }
    assert_eq!(
        h.json(&format!("{first}/auto-merge"), &writer).await["request"]["first"],
        true
    );
    assert_eq!(
        h.json(&format!("{second}/auto-merge"), &writer).await["request"]["first"],
        false
    );

    let queue_endpoint = format!("{root}/pulls/queue?branch=refs%2Fheads%2Fintegration");
    let visible = h.json(&queue_endpoint, &reader).await;
    assert_eq!(visible["items"][0]["title"], "feature");
    assert_eq!(visible["items"][0]["can_remove"], false);
    assert_eq!(
        h.write(
            Method::DELETE,
            &format!("{first}/auto-merge"),
            &reader,
            json!({"request_id":visible["items"][0]["request_id"]})
        )
        .await
        .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "View merge queue",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "#2 feature-two"]).await;
    browser
        .run(&["wait", "--fn", "Array.from(document.querySelectorAll('ol[aria-label=\"Queued pull requests\"] li a:first-child'), a => a.textContent.trim()).join(',') === '#1 feature,#2 feature-two'"])
        .await;
    browser
        .run(&["select", "#merge-queue-branch", "refs/heads/main"])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "No pull requests are queued for this branch.",
        ])
        .await;
    browser
        .run(&["select", "#merge-queue-branch", "refs/heads/integration"])
        .await;
    browser
        .run(&["wait", "[aria-label='Remove pull request #2 from queue']"])
        .await;
    browser
        .run(&["click", "[aria-label='Remove pull request #2 from queue']"])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Removed pull request #2 from the merge queue.",
        ])
        .await;
    assert_eq!(
        h.json(&queue_endpoint, &writer).await["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["pull"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1]
    );
    browser
        .run(&["open", &format!("{}/team/private/pull/2", h.origin)])
        .await;
    click("Add to merge queue").await;
    click("Confirm merge queue entry").await;

    // Start the worker after the enqueues to exercise durable discovery.
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    let candidate = async |pull: &str| {
        let route = format!("{pull}/merge-candidate");
        let ready = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let value = h.json(&route, &writer).await["candidate"].clone();
                if value["current"] == true {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(125)).await;
            }
        })
        .await;
        match ready {
            Ok(value) => value,
            Err(_) => {
                let state = h.json(&format!("{pull}/auto-merge"), &writer).await;
                panic!("Candidate for {pull} did not become current: {state}");
            }
        }
    };
    let one = candidate(&first).await;
    let first_commit = one["commit"].as_str().unwrap();
    assert_eq!(one["base_oid"], base);
    assert!(h.json(&format!("{second}/merge-candidate"), &writer).await["candidate"].is_null());

    let second_pull = h.json(&second, &writer).await;
    let direct = h
        .write(
            Method::POST,
            &format!("{second}/merge"),
            &writer,
            json!({
                "request_id":Uuid::now_v7(), "version":second_pull["version"],
                "method":"merge_commit","base_oid":base,"head_oid":second_head,
            }),
        )
        .await;
    assert_eq!(
        direct.json::<Value>().await.unwrap()["error"]["code"],
        "merge_queued"
    );

    report(first_commit, "success").await;
    let two = candidate(&second).await;
    let second_commit = two["commit"].as_str().unwrap();
    assert_eq!(two["base_oid"], first_commit);
    assert_eq!(
        h.json(&first, &writer).await["merge"]["tested_candidate_oid"],
        first_commit
    );
    git(path, &["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(
        git(path, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{first_commit} {base} {first_head}")
    );

    report(second_commit, "failure").await;
    browser
        .run(&[
            "wait",
            "--text",
            "A required check failed on the merge candidate.",
        ])
        .await;
    assert_eq!(h.json(&second, &writer).await["state"], "open");
    git(path, &["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(git(path, &["rev-parse", "FETCH_HEAD"]).await, first_commit);

    // A new admission produces a new SHA, so an earlier failed run does not
    // poison retry. The unchanged PR head is still behind the protected base.
    click("Add to merge queue").await;
    click("Confirm merge queue entry").await;
    let retry = candidate(&second).await;
    let final_commit = retry["commit"].as_str().unwrap();
    assert_ne!(final_commit, second_commit);
    assert_eq!(retry["base_oid"], first_commit);
    report(final_commit, "success").await;
    browser
        .run(&["wait", "--text", "Published the tested merge candidate."])
        .await;
    git(path, &["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(
        git(path, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{final_commit} {first_commit} {second_head}")
    );
    assert_eq!(
        git(path, &["show", "FETCH_HEAD:second.txt"]).await,
        "second feature"
    );
    assert_eq!(
        git(path, &["show", "FETCH_HEAD:README.md"]).await,
        "base\nfeature"
    );

    drop(browser);
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_requires_the_queue_only_on_the_configured_branch() {
    use axum::http::StatusCode;

    let h = Harness::new(false).await;
    let writer = h.login().await;
    let root = "/api/repos/team/private";
    let protections = format!("{root}/settings/branch-protections");
    let methods = format!("{root}/settings/merge-methods");
    let pull = format!("{root}/pulls/1");
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
    let head = push_review_branches(path, remote.as_str()).await;
    let base = git(path, &["rev-parse", "main"]).await;
    git(path, &["push", remote.as_str(), "main:refs/heads/stable"]).await;
    h.write(
        Method::PUT,
        &protections,
        &writer,
        json!({"expected_version":0,"rules":[{"branch":"main","required_checks":["ci"],"required_approvals":0}]}),
    ).await.error_for_status().unwrap();
    for target in ["main", "stable"] {
        h.write(
            Method::POST,
            &format!("{root}/pulls"),
            &writer,
            json!({
                "request_id":Uuid::now_v7(),"title":target,"body":"",
                "base_ref":format!("refs/heads/{target}"),"head_ref":"refs/heads/feature",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let success = async |oid: &str| {
        h.write(
            Method::POST,
            &format!("{root}/statuses/{oid}"),
            &writer,
            json!({"request_id":Uuid::now_v7(),"context":"ci","state":"success"}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    success(&head).await;
    let candidate = format!("{pull}/merge-candidate");
    let preview = h.json(&candidate, &writer).await["preview"].clone();
    let prepared: Value = h
        .write(Method::POST, &candidate, &writer, preview.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let unchecked = prepared["commit"].as_str().unwrap();
    success(unchecked).await;
    let comparison = async |pull: &str| {
        let detail = h.json(pull, &writer).await;
        json!({
            "request_id":Uuid::now_v7(), "version":detail["version"],
            "method":"merge_commit", "base_oid":detail["base_oid"], "head_oid":detail["head_oid"],
        })
    };
    let mut pending = comparison(&pull).await;
    pending["tested_candidate_oid"] = json!(unchecked);
    let repository = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let locks = crab_lfs::LfsLockManager::lfs(repository.store.clone(), &repository.config.prefix);
    let lock = locks.lock("README.md", "another-writer").await.unwrap();
    let post_merge = async |input: Value| {
        h.write(Method::POST, &format!("{pull}/merge"), &writer, input)
            .await
    };
    let locked = post_merge(pending.clone()).await;
    assert_eq!(locked.status(), StatusCode::CONFLICT);
    assert_eq!(
        locked.json::<Value>().await.unwrap()["error"]["code"],
        "path_locked"
    );

    let browser = Browser::new("required-queue");
    let click = async |action: &str| {
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    };
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/branches", h.origin),
        ])
        .await;
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    click("Continue to sign in").await;
    browser
        .run(&["wait", "button[aria-label='Edit main']"])
        .await;
    click("Edit main").await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Require merge queue",
            "--exact",
        ])
        .await;
    click("Save changes").await;
    browser
        .run(&["wait", "--text", "Merge queue required"])
        .await;
    let metadata = h
        .json(&format!("{root}/settings/merge-methods"), &writer)
        .await;
    assert_ne!(metadata["merge_queue"], true);
    let remove_method = h
        .write(
            Method::PUT,
            &methods,
            &writer,
            json!({"expected_version":metadata["version"],"enabled":["squash"]}),
        )
        .await;
    let methods: Value = remove_method
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(methods["enabled"], json!(["squash"]));
    assert_eq!(
        h.json(&format!("{pull}/auto-merge"), &writer).await["queue_method"],
        "merge_commit"
    );

    locks
        .unlock_with_id("README.md", "another-writer", Some(&lock.id))
        .await
        .unwrap();
    let assert_queue_required = async |response: reqwest::Response| {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "merge_queue_required"
        );
    };
    assert_queue_required(post_merge(pending).await).await;
    assert!(h.json(&pull, &writer).await["merge_pending"].is_null());
    let direct = comparison(&pull).await;
    assert_queue_required(post_merge(direct.clone()).await).await;
    let mut tested = direct.clone();
    tested["tested_candidate_oid"] = json!(unchecked);
    assert_queue_required(post_merge(tested).await).await;
    assert_queue_required(
        h.write(Method::PUT, &format!("{pull}/auto-merge"), &writer, direct)
            .await,
    )
    .await;
    let replayed: Value = h
        .write(Method::POST, &candidate, &writer, preview)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed, prepared);
    let fresh_candidate = json!({
        "request_id":Uuid::now_v7(), "version":h.json(&pull, &writer).await["version"],
        "base_oid":base, "head_oid":head, "previous_oid":unchecked,
    });
    assert_queue_required(
        h.write(Method::POST, &candidate, &writer, fresh_candidate)
            .await,
    )
    .await;
    let rejected_push =
        crate::test_support::git::git(path, &["push", remote.as_str(), "feature:main"]).await;
    assert!(!rejected_push.status.success());
    assert!(String::from_utf8_lossy(&rejected_push.stderr).contains("requires the merge queue"));
    assert_eq!(
        git(path, &["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );

    let sibling = format!("{root}/pulls/2");
    let mut sibling_merge = comparison(&sibling).await;
    sibling_merge["method"] = json!("squash");
    h.write(
        Method::POST,
        &format!("{sibling}/merge"),
        &writer,
        sibling_merge,
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&sibling, &writer).await["state"], "merged");

    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Merge queue required"])
        .await;
    browser.run(&["wait", "--text", "Add to merge queue"]).await;
    click("Add to merge queue").await;
    browser
        .run(&["wait", "--text", "Confirm merge queue entry"])
        .await;
    click("Confirm merge queue entry").await;
    browser
        .run(&["wait", "--text", "Preparing merge candidate"])
        .await;
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    let worker_commit = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let current = h.json(&candidate, &writer).await["candidate"].clone();
            if current["current"] == true {
                break current["commit"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    assert_ne!(worker_commit, unchecked);
    success(&worker_commit).await;
    browser
        .run(&["wait", "--text", "Published the tested merge candidate."])
        .await;
    git(path, &["fetch", remote.as_str(), "main"]).await;
    assert_eq!(
        git(path, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{worker_commit} {base} {head}")
    );

    drop(browser);
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
}
