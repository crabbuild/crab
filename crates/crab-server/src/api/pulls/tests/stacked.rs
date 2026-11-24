use std::path::Path;
use std::time::Duration;

use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use super::branch_restore::{open_pull, remote};

const ROOT: &str = "/api/repos/team/private";
const BRANCH: &str = "/api/repos/team/private/pulls/1/head-branch";

async fn child(h: &Harness, writer: &str, path: &Path, remote: &str, name: &str) -> Value {
    git::success(path, &["switch", "-c", name, "feature"]).await;
    std::fs::write(path.join(format!("{name}.txt")), "A change above feature\n").unwrap();
    git::success(path, &["add", "."]).await;
    git::success(path, &["commit", "-m", "Add a stacked change"]).await;
    git::success(path, &["push", remote, name]).await;
    h.write(
        Method::POST,
        &format!("{ROOT}/pulls"),
        writer,
        json!({
            "request_id":Uuid::now_v7(), "title":name, "body":"", "base_ref":"refs/heads/feature",
            "head_ref":format!("refs/heads/{name}"),
        }),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn merge(h: &Harness, writer: &str, parent: Value, automatic: bool) {
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/branch-protections"),
        writer,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    if automatic {
        h.write(
            Method::PUT,
            &format!("{ROOT}/settings/merge-methods"),
            writer,
            json!({"expected_version":0,"enabled":["fast_forward"],"delete_head_branches":true}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::POST,
        &format!("{ROOT}/pulls/1/merge"),
        writer,
        json!({
            "request_id":Uuid::now_v7(),"version":parent["version"],"method":"fast_forward",
            "base_oid":parent["base_oid"],"head_oid":parent["head_oid"],
        }),
    )
    .await
    .error_for_status()
    .unwrap();
}

fn deletion(preview: Value) -> Value {
    json!({"action":"delete", "request_id":Uuid::now_v7(), "version":preview["version"],
      "head_oid":preview["head_oid"], "repository_id":preview["repository_id"], "retarget_token":preview["retarget"]["token"]})
}

#[tokio::test(flavor = "multi_thread")]
async fn stacked_branch_deletion_fences_dependents_and_replays_the_original_result() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (head, parent) = open_pull(&h, &writer, remote.as_str(), path).await;
    let first = child(&h, &writer, path, remote.as_str(), "second-layer").await;
    merge(&h, &writer, parent, false).await;
    let preview = h.json(BRANCH, &writer).await["branch"].clone();
    assert_eq!(preview["retarget"]["pulls"], json!([2]));
    assert_eq!(preview["retarget"]["base_ref"], "refs/heads/main");
    let second = child(&h, &writer, path, remote.as_str(), "another-layer").await;
    let stale = h
        .write(Method::POST, BRANCH, &writer, deletion(preview))
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    for number in [2, 3] {
        assert_eq!(
            h.json(&format!("{ROOT}/pulls/{number}"), &writer).await["base_ref"],
            "refs/heads/feature"
        );
    }
    let fresh = h.json(BRANCH, &writer).await["branch"].clone();
    assert_eq!(fresh["retarget"]["pulls"], json!([2, 3]));
    let input = deletion(fresh);
    let repo = &h.server.repositories.values()[0];
    let blocked =
        crab_remote::publication::with_leases::<_, crab_remote::publication::Error, _, _>(
            &repo.store,
            &repo.layout,
            ["refs/heads/main".into()],
            Duration::from_secs(30),
            &h.server.cancellation,
            |_, _| async {
                Ok(h.write(Method::POST, BRANCH, &writer, input.clone())
                    .await
                    .status())
            },
        )
        .await
        .unwrap();
    assert_eq!(blocked, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        repo.ref_oid("refs/heads/feature").await.unwrap().as_deref(),
        Some(head.as_str())
    );
    for _ in 0..2 {
        h.write(Method::POST, BRANCH, &writer, input.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    let mut changed_replay = input;
    changed_replay["retarget_token"] = json!("0".repeat(64));
    assert_eq!(
        h.write(Method::POST, BRANCH, &writer, changed_replay)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert!(
        git::success(path, &["ls-remote", remote.as_str(), "refs/heads/feature"])
            .await
            .is_empty()
    );
    for (number, old) in [(2, first), (3, second)] {
        let pull = h.json(&format!("{ROOT}/pulls/{number}"), &writer).await;
        assert_eq!(pull["state"], "open");
        assert_eq!(pull["base_ref"], "refs/heads/main");
        assert_eq!(pull["base_oid"], head);
        assert_eq!(pull["head_oid"], old["head_oid"]);
        assert_eq!(
            pull["version"].as_u64().unwrap(),
            old["version"].as_u64().unwrap() + 1
        );
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stacked_deletion_resumes_when_git_publication_fails_after_the_cell_commit() {
    let (store, fail_intent) = crate::git::receive::fault_plan_intent();
    let h = Harness::new_with_store(false, false, store).await;
    h.initialize_directory().await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (head, parent) = open_pull(&h, &writer, remote.as_str(), path).await;
    child(&h, &writer, path, remote.as_str(), "follow-up").await;
    merge(&h, &writer, parent, false).await;
    let selected = deletion(h.json(BRANCH, &writer).await["branch"].clone());
    fail_intent(true);
    let failed = h
        .write(Method::POST, BRANCH, &writer, selected.clone())
        .await;
    assert!(!failed.status().is_success());
    let pull_path = format!("{ROOT}/pulls/2");
    let moved = h.json(&pull_path, &writer).await;
    assert_eq!(moved["base_ref"], "refs/heads/main");
    let repo = &h.server.repositories.values()[0];
    assert_eq!(
        repo.ref_oid("refs/heads/feature").await.unwrap().as_deref(),
        Some(head.as_str())
    );
    fail_intent(false);
    h.write(Method::POST, BRANCH, &writer, selected)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(repo.ref_oid("refs/heads/feature").await.unwrap(), None);
    assert_eq!(
        h.json(&pull_path, &writer).await["version"],
        moved["version"]
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_cleanup_retargets_a_stack_before_deleting_its_base_ref() {
    use super::super::head_branch::automatic;

    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (_, parent) = open_pull(&h, &writer, remote.as_str(), path).await;
    child(&h, &writer, path, remote.as_str(), "follow-up").await;
    merge(&h, &writer, parent, true).await;
    let repo = &h.server.repositories.values()[0];
    let pending = automatic::pending(&h.server, repo, None)
        .await
        .unwrap()
        .pop()
        .unwrap();
    automatic::reconcile(&h.server, repo, pending)
        .await
        .unwrap();
    assert_eq!(h.json(BRANCH, &writer).await["branch"]["action"], "restore");
    assert_eq!(
        h.json(&format!("{ROOT}/pulls/2"), &writer).await["base_ref"],
        "refs/heads/main"
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_previews_and_retargets_a_stacked_pull_when_deleting_its_base() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (_, parent) = open_pull(&h, &writer, remote.as_str(), path).await;
    child(&h, &writer, path, remote.as_str(), "follow-up").await;
    merge(&h, &writer, parent, false).await;
    let browser = Browser::new("stacked-delete");
    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser.sign_in().await;
    let delete = [
        "find",
        "role",
        "button",
        "click",
        "--name",
        "Delete branch",
        "--exact",
    ];
    browser.run(&["wait", "--text", "Delete branch"]).await;
    browser.run(&delete).await;
    browser
        .run(&["wait", "--text", "Pull request #2 will move to main"])
        .await;
    browser.run(&delete).await;
    browser.run(&["wait", "--text", "Restore branch"]).await;
    browser
        .run(&["open", &format!("{}/team/private/pull/2", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "changed the base from feature to main"])
        .await;
    assert_eq!(
        h.json(&format!("{ROOT}/pulls/2"), &writer).await["base_ref"],
        "refs/heads/main"
    );
    drop(browser);
    h.close().await;
}
