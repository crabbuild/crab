use crate::test_support::{git, http::Harness};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const REPO: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";

#[tokio::test(flavor = "multi_thread")]
async fn retargeting_binds_tips_and_expires_approvals_without_removing_changes_requested() {
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
    let head = git::push_review_branches(work.path(), remote.as_str()).await;
    let git = async |args: &[&str]| git::success(work.path(), args).await;
    let original = git(&["rev-parse", "main"]).await;
    git(&["switch", "-c", "stable", "main"]).await;
    std::fs::write(work.path().join("STABLE.txt"), "included on stable\n").unwrap();
    git(&["add", "STABLE.txt"]).await;
    git(&["commit", "-m", "Give stable its own history"]).await;
    let stale = git(&["rev-parse", "HEAD"]).await;
    git(&["push", remote.as_str(), "stable"]).await;
    let opened = json!({"request_id":Uuid::now_v7(),"title":"Feature","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature","expected":{"base_oid":original,"head_oid":head}});
    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        opened.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let endpoint = format!("{PULL}/base");
    let original_detail = h.json(PULL, &alice).await;
    let input = |version: &Value, branch: &str, oid: &str| json!({"request_id":Uuid::now_v7(),"version":version,"base_ref":branch,"base_oid":oid,"head_oid":head});
    assert_eq!(
        h.write(
            Method::POST,
            &endpoint,
            &bob,
            input(&original_detail["version"], "refs/heads/stable", &stale)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let acl = h.json(&format!("{REPO}/members"), &alice).await;
    h.write(Method::PUT, &format!("{REPO}/members"), &alice, json!({"expected_revision":acl["revision"],"groups":[],"members":[{"subject":"alice-id","name":"Alice","access":"admin"},{"subject":"bob-id","name":"Bob","access":"write"}]})).await.error_for_status().unwrap();

    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance the previewed base",
    ])
    .await;
    git(&["push", remote.as_str(), "stable"]).await;
    let stable = git(&["rev-parse", "HEAD"]).await;
    h.write(Method::PUT, &format!("{REPO}/settings/branch-protections"), &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1},{"branch":"stable","required_approvals":1}]})).await.error_for_status().unwrap();
    let reviews = format!("{PULL}/reviews");
    let old_review = json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed against main","commit_oid":head});
    h.write(Method::POST, &reviews, &bob, old_review.clone())
        .await
        .error_for_status()
        .unwrap();
    let approved = h.json(PULL, &alice).await;
    assert_eq!(approved["can_merge"], true);
    let stale_change = h
        .write(
            Method::POST,
            &endpoint,
            &alice,
            input(&approved["version"], "refs/heads/stable", &stale),
        )
        .await;
    assert_eq!(stale_change.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale_change.json::<Value>().await.unwrap()["error"]["code"],
        "comparison_changed"
    );
    assert_eq!(h.json(PULL, &alice).await["base_ref"], "refs/heads/main");

    let change = input(&approved["version"], "refs/heads/stable", &stable);
    for _ in 0..2 {
        h.write(Method::POST, &endpoint, &alice, change.clone())
            .await
            .error_for_status()
            .unwrap();
    }
    let mut modified = change.clone();
    modified["base_ref"] = json!("refs/heads/main");
    assert_eq!(
        h.write(Method::POST, &endpoint, &alice, modified)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let updated = h.json(PULL, &alice).await;
    assert_eq!(updated["base_ref"], "refs/heads/stable");
    assert_eq!(updated["base_oid"], stable);
    assert_eq!(updated["head_oid"], head);
    assert_eq!(updated["original_base_oid"], original);
    assert_eq!(updated["merge_requirements"]["approvals"], 0);
    assert_eq!(updated["can_merge"], false);
    assert!(updated["base_revision"].is_number());
    assert_eq!(
        h.json(&reviews, &alice).await["items"][0]["state"],
        "dismissed"
    );
    h.write(Method::POST, &format!("{REPO}/pulls"), &alice, opened)
        .await
        .error_for_status()
        .unwrap();
    h.write(Method::POST, &reviews, &bob, old_review.clone())
        .await
        .error_for_status()
        .unwrap();
    let mut outdated = old_review;
    outdated["request_id"] = json!(Uuid::now_v7());
    assert_eq!(
        h.write(Method::POST, &reviews, &bob, outdated)
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let new_review = |state: &str| json!({"request_id":Uuid::now_v7(),"state":state,"body":"Reviewed against stable","commit_oid":head,"base_revision":updated["base_revision"]});
    h.write(Method::POST, &reviews, &bob, new_review("approved"))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(h.json(PULL, &alice).await["can_merge"], true);
    h.write(
        Method::POST,
        &reviews,
        &bob,
        new_review("changes_requested"),
    )
    .await
    .error_for_status()
    .unwrap();
    let current = h.json(PULL, &alice).await;
    h.write(
        Method::POST,
        &endpoint,
        &alice,
        input(&current["version"], "refs/heads/main", &original),
    )
    .await
    .error_for_status()
    .unwrap();
    let returned = h.json(PULL, &alice).await;
    assert_eq!(returned["merge_requirements"]["changes_requested"], 1);
    assert_eq!(returned["can_merge"], false);

    h.write(
        Method::POST,
        &endpoint,
        &alice,
        input(&returned["version"], "refs/heads/stable", &stable),
    )
    .await
    .error_for_status()
    .unwrap();
    let destination = h.json(PULL, &alice).await;
    h.write(Method::POST, &reviews, &bob, json!({
        "request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed updated comparison","commit_oid":head,
        "base_revision":destination["base_revision"],
    })).await.error_for_status().unwrap();
    let ready = h.json(PULL, &alice).await;
    let merged: Value = h.write(Method::POST, &format!("{PULL}/merge"), &alice,
        json!({"request_id":Uuid::now_v7(),"version": ready["version"],"method":"squash","base_oid":stable,"head_oid":head}))
        .await.error_for_status().unwrap().json().await.unwrap();
    git(&["fetch", remote.as_str(), "stable"]).await;
    assert_eq!(
        git(&["rev-parse", "FETCH_HEAD"]).await,
        merged["merge"]["commit_oid"].as_str().unwrap()
    );
    assert_eq!(
        git(&["show", "FETCH_HEAD:STABLE.txt"]).await,
        "included on stable"
    );
    git(&["fetch", remote.as_str(), "main"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, original);
    h.close().await;
}
