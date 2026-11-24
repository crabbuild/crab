use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const PULL: &str = "/api/repos/team/private/pulls/1";
const RESTORE: &str = "/api/repos/team/private/pulls/1/restore-branch";

async fn open_pull(
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
    assert!(h.json(RESTORE, writer).await["branch"].is_null());
    h.write(
        Method::PATCH,
        PULL,
        writer,
        json!({"version":opened["version"],"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    git::success(path, &["push", remote, "--delete", "feature"]).await;
    let mut input = h.json(RESTORE, writer).await["branch"].clone();
    assert_eq!(input["head_oid"], head);
    input["request_id"] = json!(Uuid::now_v7());
    (head, input)
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
    git::success(path, &["push", remote.as_str(), "--delete", "feature"]).await;
    let mut input = h.json(RESTORE, &writer).await["branch"].clone();
    assert_eq!(input["head_oid"], head);
    input["request_id"] = json!(Uuid::now_v7());

    h.write(Method::PUT, protection, &writer,
        json!({"expected_version":1,"rules":[{"branch":"feature","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    assert_eq!(
        h.write(Method::POST, RESTORE, &writer, input.clone())
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
    h.write(Method::POST, RESTORE, &writer, input)
        .await
        .error_for_status()
        .unwrap();
    git::success(path, &["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git::success(path, &["rev-parse", "FETCH_HEAD"]).await, head);
    h.close().await;
}

async fn remote(h: &Harness, writer: &str) -> url::Url {
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
async fn completed_pull_restores_the_saved_head_once_without_overwriting_a_branch() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let (head, input) = closed_pull(&h, &writer, remote.as_str(), checkout.path()).await;
    let git = async |args: &[&str]| git::success(checkout.path(), args).await;
    let restore = async |body: Value| h.write(Method::POST, RESTORE, &writer, body).await;

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
    assert!(h.json(RESTORE, &writer).await["branch"].is_null());

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
    assert_eq!(h.json(RESTORE, &writer).await["branch"]["head_oid"], head);
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

    h.write(Method::POST, RESTORE, &writer, restore)
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
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_restores_the_closed_pull_branch_and_stock_git_fetches_it() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let remote = remote(&h, &writer).await;
    let checkout = tempfile::tempdir().unwrap();
    let (head, _) = closed_pull(&h, &writer, remote.as_str(), checkout.path()).await;
    let browser = Browser::new("restore-branch");
    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser.sign_in().await;
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
