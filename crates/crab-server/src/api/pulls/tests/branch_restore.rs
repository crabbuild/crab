use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const PULL: &str = "/api/repos/team/private/pulls/1";
const HEAD_BRANCH: &str = "/api/repos/team/private/pulls/1/head-branch";
const MERGE_SETTINGS: &str = "/api/repos/team/private/settings/merge-methods";

pub(super) async fn open_pull(
    h: &Harness,
    writer: &str,
    remote: &str,
    path: &std::path::Path,
) -> (String, Value) {
    let head = git::push_review_branches(path, remote).await;
    let opened: Value = h
        .write(
            Method::POST,
            "/api/repos/team/private/pulls",
            writer,
            json!({
                "request_id":Uuid::now_v7(),"title":"Retain the reviewed feature","body":"",
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (head, opened)
}

async fn closed_pull(
    h: &Harness,
    writer: &str,
    remote: &str,
    path: &std::path::Path,
) -> (String, Value) {
    let (head, opened) = open_pull(h, writer, remote, path).await;
    assert!(h.json(HEAD_BRANCH, writer).await["branch"].is_null());
    close(h, writer, PULL, &opened["version"]).await;
    git::success(path, &["push", remote, "--delete", "feature"]).await;
    let mut input = h.json(HEAD_BRANCH, writer).await["branch"].clone();
    assert_eq!(input["head_oid"], head);
    assert_eq!(input["action"], "restore");
    input["request_id"] = json!(Uuid::now_v7());
    (head, input)
}

async fn close(h: &Harness, writer: &str, pull: &str, version: &Value) {
    h.write(
        Method::PATCH,
        pull,
        writer,
        json!({"version":version,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn squash_merge_restores_the_original_head_under_current_branch_policy() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (head, opened) = open_pull(&h, &writer, remote.as_str(), path).await;
    let protection = "/api/repos/team/private/settings/branch-protections";
    h.write(
        Method::PUT,
        protection,
        &writer,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/merge-methods",
        &writer,
        json!({"expected_version":0,"enabled":["squash"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let merged: Value = h
        .write(
            Method::POST,
            &format!("{PULL}/merge"),
            &writer,
            json!({"request_id":Uuid::now_v7(),"version":opened["version"],"method":"squash",
            "base_oid":opened["base_oid"],"head_oid":head}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(merged["state"], "merged");
    assert_ne!(merged["merge"]["commit_oid"], head);
    let mut deletion = h.json(HEAD_BRANCH, &writer).await["branch"].clone();
    assert_eq!(deletion["action"], "delete");
    deletion["request_id"] = json!(Uuid::now_v7());
    h.write(Method::POST, HEAD_BRANCH, &writer, deletion)
        .await
        .error_for_status()
        .unwrap();
    let searched = h
        .json(
            "/api/repos/team/private/pulls?q=is%3Amerged+base%3Amain+head%3Afeat",
            &writer,
        )
        .await;
    assert_eq!(searched["matching"], 1);
    assert_eq!(searched["items"][0]["number"], merged["number"]);
    let mut input = h.json(HEAD_BRANCH, &writer).await["branch"].clone();
    assert_eq!(input["head_oid"], head);
    input["request_id"] = json!(Uuid::now_v7());

    h.write(Method::PUT, protection, &writer,
        json!({"expected_version":1,"rules":[{"branch":"feature","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    assert_eq!(
        h.write(Method::POST, HEAD_BRANCH, &writer, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    h.write(
        Method::PUT,
        protection,
        &writer,
        json!({"expected_version":2,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::POST, HEAD_BRANCH, &writer, input)
        .await
        .error_for_status()
        .unwrap();
    git::success(path, &["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git::success(path, &["rev-parse", "FETCH_HEAD"]).await, head);
    h.close().await;
}

pub(super) async fn remote(h: &Harness, writer: &str) -> url::Url {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            writer,
            json!({
                "owner":"team","repository":"private","access":"write",
            }),
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
    remote
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_cleanup_recovers_once_and_skips_a_recreated_ref_or_stale_setting() {
    use super::super::head_branch::automatic;

    for (recreate, toggle) in [(false, false), (true, false), (false, true)] {
        let h = Harness::new(false).await;
        let writer = h.login().await;
        let remote = remote(&h, &writer).await;
        let checkout = tempfile::tempdir().unwrap();
        let path = checkout.path();
        let (head, opened) = open_pull(&h, &writer, remote.as_str(), path).await;
        let repo = &h.server.repositories.values()[0];
        let save =
            async |version, enabled| {
                h.write(Method::PUT, MERGE_SETTINGS, &writer, json!({
                "expected_version":version,"enabled":["squash"],"delete_head_branches":enabled,
            })).await.error_for_status().unwrap();
            };
        save(0, true).await;
        h.write(
            Method::PUT,
            "/api/repos/team/private/settings/branch-protections",
            &writer,
            json!({"expected_version":0,"rules":[]}),
        )
        .await
        .error_for_status()
        .unwrap();
        h.write(
            Method::POST,
            &format!("{PULL}/merge"),
            &writer,
            json!({
                "request_id":Uuid::now_v7(),"version":opened["version"],"method":"squash",
                "base_oid":opened["base_oid"],"head_oid":head,
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        assert_eq!(h.json(HEAD_BRANCH, &writer).await["cleanup_pending"], true);
        let pending = async || {
            automatic::pending(&h.server, repo, None)
                .await
                .unwrap()
                .pop()
                .unwrap()
        };
        let (first, replay) = (pending().await, pending().await);
        let restore_ref = async || {
            // After a squash the original head is unreachable through live refs.
            let mut restore = h.json(HEAD_BRANCH, &writer).await["branch"].clone();
            assert_eq!(restore["action"], "restore");
            restore["request_id"] = json!(Uuid::now_v7());
            h.write(Method::POST, HEAD_BRANCH, &writer, restore)
                .await
                .error_for_status()
                .unwrap();
        };
        if recreate {
            git::success(path, &["push", remote.as_str(), "--delete", "feature"]).await;
            restore_ref().await;
        }
        if toggle {
            save(1, false).await;
            save(2, true).await;
        }
        automatic::reconcile(&h.server, repo, first).await.unwrap();
        assert!(
            automatic::pending(&h.server, repo, None)
                .await
                .unwrap()
                .is_empty()
        );
        if !recreate && !toggle {
            restore_ref().await;
            automatic::reconcile(&h.server, repo, replay).await.unwrap();
        }
        assert_eq!(
            git::success(path, &["ls-remote", remote.as_str(), "refs/heads/feature"]).await,
            format!("{head}\trefs/heads/feature")
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_enables_automatic_cleanup_and_restores_the_merged_branch() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (head, _) = open_pull(&h, &writer, remote.as_str(), path).await;
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/branch-protections",
        &writer,
        json!({"expected_version":0,"rules":[{"branch":"feature","required_checks":[]}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let browser = Browser::new("auto-cleanup");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/branches", h.origin),
        ])
        .await;
    browser.sign_in().await;
    let click = async |role, name| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser
        .run(&["wait", "--text", "Branch protection rules"])
        .await;
    click("button", "Edit feature").await;
    click("checkbox", "Allow deletions").await;
    click("button", "Save changes").await;
    browser.run(&["wait", "--text", "Deletions allowed"]).await;
    assert_eq!(
        h.json("/api/repos?owner=team&name=private", &writer).await["repositories"][0]["protected_branches"]
            [0]["allow_deletions"],
        true
    );
    browser
        .run(&["open", &format!("{}/team/private/settings", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Automatically delete head branches"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Automatically delete head branches",
        ])
        .await;
    click("button", "Save merge methods").await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('button')].some(button => button.disabled && button.textContent.trim() === 'Save merge methods')"]).await;
    assert_eq!(
        h.json(MERGE_SETTINGS, &writer).await["delete_head_branches"],
        true
    );
    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser.run(&["wait", "--text", "Merge pull request"]).await;
    click("button", "Merge pull request").await;
    click("button", "Confirm merge").await;
    browser
        .run(&["wait", "--text", "Automatic deletion of feature is pending"])
        .await;
    let worker = tokio::spawn(crate::api::auto_merge::run(h.server.clone()));
    browser.run(&["wait", "--text", "Restore branch"]).await;
    assert!(
        git::success(path, &["ls-remote", remote.as_str(), "refs/heads/feature"])
            .await
            .is_empty()
    );
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/branch-protections",
        &writer,
        json!({"expected_version":2,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    click("button", "Restore branch").await;
    browser.run(&["wait", "--text", "Restored feature"]).await;
    assert_eq!(
        git::success(path, &["ls-remote", remote.as_str(), "refs/heads/feature"]).await,
        format!("{head}\trefs/heads/feature")
    );
    worker.abort();
    drop(browser);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_pull_restores_the_saved_head_once_without_overwriting_a_branch() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let (head, input) = closed_pull(&h, &writer, remote.as_str(), checkout.path()).await;
    let git = async |args: &[&str]| git::success(checkout.path(), args).await;
    let restore = async |body: Value| h.write(Method::POST, HEAD_BRANCH, &writer, body).await;

    let mut stale = input.clone();
    stale["version"] = json!(0);
    assert_eq!(restore(stale).await.status(), StatusCode::CONFLICT);

    git(&["push", remote.as_str(), "main:refs/heads/feature"]).await;
    let changed = restore(input.clone()).await;
    assert_eq!(changed.status(), StatusCode::CONFLICT);
    assert_ne!(
        git(&["ls-remote", remote.as_str(), "refs/heads/feature"]).await,
        format!("{head}\trefs/heads/feature")
    );
    git(&["push", remote.as_str(), "--delete", "feature"]).await;

    let restored: Value = restore(input.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        restored,
        json!({"branch":"refs/heads/feature","commit":head})
    );
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, head);
    assert_eq!(
        h.json(HEAD_BRANCH, &writer).await["branch"]["action"],
        "delete"
    );

    // The browser issues a new ref mutation even when the previous native Git
    // delete had the same ref name and object ID on the wire.
    h.write(
        Method::DELETE,
        "/api/repos/team/private/branches",
        &writer,
        json!({"name":"feature","expected_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(HEAD_BRANCH, &writer).await["branch"]["head_oid"],
        head
    );
    let replayed: Value = restore(input)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed, restored);
    assert!(
        git(&["ls-remote", remote.as_str(), "refs/heads/feature"])
            .await
            .is_empty()
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reopen_checks_live_refs_and_recovers_its_original_decision() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let (head, restore) = closed_pull(&h, &writer, remote.as_str(), checkout.path()).await;
    let version = restore["version"].as_u64().unwrap();
    let state = format!("{PULL}/state");
    let reopen = json!({"request_id":Uuid::now_v7(),"version":version,"state":"open","body":"Resume the review"});
    let rejected = |response: reqwest::Response| async move {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "pull_branches_unavailable"
        );
    };
    rejected(
        h.write(
            Method::PATCH,
            PULL,
            &writer,
            json!({"version":version,"state":"open"}),
        )
        .await,
    )
    .await;
    rejected(h.write(Method::POST, &state, &writer, reopen.clone()).await).await;
    assert_eq!(
        h.json(&format!("{PULL}/comments"), &writer).await["items"],
        json!([])
    );

    let root = "/api/repos/team/private";
    let issue = h.write(Method::POST, &format!("{root}/issues"), &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Keep this issue closed","body":"Closed for the bulk test"})).await.error_for_status().unwrap().json::<Value>().await.unwrap();
    h.write(
        Method::POST,
        &format!("{root}/issues/1/state"),
        &writer,
        json!({"request_id":Uuid::now_v7(),"version":issue["version"],"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let closed = h.json(&format!("{root}/issues/1"), &writer).await;
    let repo = &h.server.repositories.values()[0];
    let selected = h
        .json(
            &format!(
                "{root}/discussions/selection?repository_id={}&items=issue:1:{},pull:1:{version}",
                repo.id, closed["version"]
            ),
            &writer,
        )
        .await;
    let batch = json!({"request_id":Uuid::now_v7(),"repository_id":repo.id,"items":selected["items"],"action":{"kind":"state","state":"open"}});
    rejected(
        h.write(
            Method::POST,
            &format!("{root}/discussions/bulk"),
            &writer,
            batch,
        )
        .await,
    )
    .await;
    assert_eq!(
        h.json(&format!("{root}/issues/1"), &writer).await["state"],
        "closed"
    );

    h.write(Method::POST, HEAD_BRANCH, &writer, restore)
        .await
        .error_for_status()
        .unwrap();
    rejected(h.write(Method::POST, &state, &writer, reopen.clone()).await).await;
    let mut fresh = reopen;
    fresh["request_id"] = json!(Uuid::now_v7());
    let busy = crab_remote::publication::with_leases::<_, crab_remote::publication::Error, _, _>(
        &repo.store,
        &repo.layout,
        ["refs/heads/feature".to_owned()],
        std::time::Duration::from_secs(30),
        &h.server.cancellation,
        |_, _| async {
            Ok(h.write(Method::POST, &state, &writer, fresh.clone())
                .await
                .status())
        },
    )
    .await
    .unwrap();
    assert_eq!(busy, StatusCode::TOO_MANY_REQUESTS);
    let applied: Value = h
        .write(Method::POST, &state, &writer, fresh.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(applied["comment"]["body"], "Resume the review");
    assert_eq!(h.json(PULL, &writer).await["state"], "open");
    h.write(
        Method::DELETE,
        &format!("{root}/branches"),
        &writer,
        json!({"name":"feature","expected_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();
    let replayed: Value = h
        .write(Method::POST, &state, &writer, fresh)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(applied["comment"]["number"], replayed["comment"]["number"]);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_branch_deletion_rechecks_both_dependency_roles_and_never_replays_the_edit() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let (head, opened) = open_pull(&h, &writer, remote.as_str(), checkout.path()).await;
    close(&h, &writer, PULL, &opened["version"]).await;
    let git = async |args: &[&str]| git::success(checkout.path(), args).await;
    let edit = async |body: Value| h.write(Method::POST, HEAD_BRANCH, &writer, body).await;
    let mut deletion = h.json(HEAD_BRANCH, &writer).await["branch"].clone();
    assert_eq!(deletion["action"], "delete");
    assert_eq!(deletion["head_oid"], head);
    deletion["request_id"] = json!(Uuid::now_v7());

    git(&["switch", "-c", "dependent"]).await;
    git(&["commit", "--allow-empty", "-m", "Stacked change"]).await;
    git(&[
        "push",
        remote.as_str(),
        "dependent",
        "main:refs/heads/other-base",
    ])
    .await;
    for (base, source) in [("feature", "dependent"), ("other-base", "feature")] {
        let pull: Value = h
            .write(
                Method::POST,
                "/api/repos/team/private/pulls",
                &writer,
                json!({"request_id":Uuid::now_v7(),"title":"Uses the source branch","body":"",
                "base_ref":format!("refs/heads/{base}"),"head_ref":format!("refs/heads/{source}")}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(h.json(HEAD_BRANCH, &writer).await["branch"].is_null());
        let blocked = edit(deletion.clone()).await;
        assert_eq!(blocked.status(), StatusCode::CONFLICT);
        assert_eq!(
            blocked.json::<Value>().await.unwrap()["error"]["code"],
            "pull_branch_in_use"
        );
        let endpoint = format!("/api/repos/team/private/pulls/{}", pull["number"]);
        close(&h, &writer, &endpoint, &pull["version"]).await;
    }

    let protection = "/api/repos/team/private/settings/branch-protections";
    h.write(
        Method::PUT,
        protection,
        &writer,
        json!({"expected_version":0,"rules":[{"branch":"feature","required_approvals":1}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(h.json(HEAD_BRANCH, &writer).await["branch"].is_null());
    assert_eq!(edit(deletion.clone()).await.status(), StatusCode::FORBIDDEN);
    h.write(
        Method::PUT,
        protection,
        &writer,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();

    git(&["push", remote.as_str(), "dependent:refs/heads/feature"]).await;
    assert!(h.json(HEAD_BRANCH, &writer).await["branch"].is_null());
    assert_eq!(edit(deletion.clone()).await.status(), StatusCode::CONFLICT);
    let advanced = git(&["rev-parse", "dependent"]).await;
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/feature"]).await,
        format!("{advanced}\trefs/heads/feature")
    );
    let branches = "/api/repos/team/private/branches";
    h.write(
        Method::DELETE,
        branches,
        &writer,
        json!({"name":"feature","expected_oid":advanced}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        branches,
        &writer,
        json!({"name":"feature","source_oid":head}),
    )
    .await
    .error_for_status()
    .unwrap();

    let deleted: Value = edit(deletion.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["commit"], head);
    assert!(
        git(&["ls-remote", remote.as_str(), "refs/heads/feature"])
            .await
            .is_empty()
    );
    let mut restore = h.json(HEAD_BRANCH, &writer).await["branch"].clone();
    assert_eq!(restore["action"], "restore");
    restore["request_id"] = json!(Uuid::now_v7());
    edit(restore).await.error_for_status().unwrap();
    let replay: Value = edit(deletion)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay, deleted);
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/feature"]).await,
        format!("{head}\trefs/heads/feature")
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_deletes_restores_and_reopens_the_pull_branch() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let (head, opened) = open_pull(&h, &writer, remote.as_str(), checkout.path()).await;
    close(&h, &writer, PULL, &opened["version"]).await;
    let browser = Browser::new("restore-branch");
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
    browser.run(&["wait", "--text", "Delete branch?"]).await;
    browser.run(&delete).await;
    browser.run(&["wait", "--text", "Deleted feature"]).await;
    assert!(
        git::success(
            checkout.path(),
            &["ls-remote", remote.as_str(), "refs/heads/feature"]
        )
        .await
        .is_empty()
    );
    browser.run(&["wait", "--text", "Restore branch"]).await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('button')).some(button => button.textContent.trim() === 'Reopen pull request' && button.disabled)"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Restore branch",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Restored feature"]).await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('button')).some(button => button.textContent.trim() === 'Reopen pull request' && !button.disabled)"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Reopen pull request",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('.pull-state.open') !== null",
        ])
        .await;
    let fresh = tempfile::tempdir().unwrap();
    git::success(fresh.path(), &["init", "--object-format=sha1", "."]).await;
    git::success(fresh.path(), &["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(
        git::success(fresh.path(), &["rev-parse", "FETCH_HEAD"]).await,
        head
    );
    assert_eq!(
        git::success(fresh.path(), &["show", "FETCH_HEAD:README.md"]).await,
        "base\nfeature"
    );
    drop(browser);
    h.close().await;
}
