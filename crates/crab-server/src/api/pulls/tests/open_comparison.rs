use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::{git, http::Harness};

use super::branch_restore::remote;

const ROOT: &str = "/api/repos/team/private/pulls";

#[tokio::test(flavor = "multi_thread")]
async fn one_open_pull_wins_and_each_submission_keeps_its_original_decision() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let remote = remote(&h, &alice).await;
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    let head = git::push_review_branches(path, remote.as_str()).await;
    let base = git::success(path, &["rev-parse", "main"]).await;
    git::success(path, &["push", remote.as_str(), "main:refs/heads/stable"]).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let payload = |target| {
        json!({
            "request_id":Uuid::now_v7(),"title":"Review the topic","body":"",
            "base_ref":target,"head_ref":"refs/heads/feature",
            "expected":{"base_oid":base,"head_oid":head},
        })
    };
    let first = payload("refs/heads/main");
    let second = payload("refs/heads/main");
    let create = async |user: &str, input: Value| h.write(Method::POST, ROOT, user, input).await;
    let find = format!(
        "{ROOT}/open-comparison?base_ref=refs%2Fheads%2Fmain&head_ref=refs%2Fheads%2Ffeature"
    );
    assert!(h.json(&find, &bob).await["pull_number"].is_null());
    let (a, b) = tokio::join!(create(&alice, first.clone()), create(&bob, second.clone()));
    let (winner, receipt, attempt, rejected_actor, rejected_input) = match (a.status(), b.status())
    {
        (StatusCode::CREATED, _) => (a, &alice, first, &bob, second),
        (_, StatusCode::CREATED) => (b, &bob, second, &alice, first),
        (left, right) => panic!("neither author opened a pull: {left}, {right}"),
    };
    let opened: Value = winner.json().await.unwrap();
    assert_eq!(opened["number"], 1);
    let loser = create(rejected_actor, rejected_input.clone()).await;
    assert_eq!(loser.status(), StatusCode::CONFLICT);
    assert_eq!(loser.headers()["x-crab-write-outcome"], "rejected");
    let error = loser.json::<Value>().await.unwrap()["error"].clone();
    assert_eq!(error["code"], "open_pull_exists");
    assert_eq!(error["pull_number"], 1);
    assert_eq!(h.json(&find, &bob).await["pull_number"], 1);

    let other = create(&alice, payload("refs/heads/stable"))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(other["number"], 2);
    h.write(
        Method::PATCH,
        &format!("{ROOT}/1"),
        &alice,
        json!({"version":opened["version"],"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(h.json(&find, &bob).await["pull_number"].is_null());
    let replacement = create(&alice, payload("refs/heads/main"))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(replacement["number"], 3);
    assert_eq!(h.json(&find, &bob).await["pull_number"], 3);

    let original = create(receipt, attempt)
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(original["number"], 1);
    let rejected = create(rejected_actor, rejected_input).await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    assert_eq!(rejected.json::<Value>().await.unwrap()["error"], error);
    h.close().await;
}
