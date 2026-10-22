use crate::test_support::{git, http::Harness};
use reqwest::Method;
use serde_json::{Value, json};
const ROOT: &str = "/api/repos/team/private";

#[tokio::test]
async fn list_rollups_follow_live_heads_and_do_not_expose_qualified_authentication_claims() {
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
    let mut url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), url.as_str()).await;
    h.write(Method::POST,&format!("{ROOT}/pulls"),&alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Truthful list summaries","body":"Current source head","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    h.write(
        Method::POST,
        &format!("{ROOT}/statuses/{head}"),
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"context":"ci/status","state":"success"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::POST,&format!("{ROOT}/check-runs"),&alice,json!({"request_id":uuid::Uuid::now_v7(),"head_sha":head,"name":"ci/run","status":"completed","conclusion":"skipped","details_url":null,"output":{"title":"Skipped","summary":"No change"}})).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let id = h.assignee_id(&alice, "Bob").await;
    h.write(
        Method::PUT,
        &format!("{ROOT}/pulls/1/review-requests"),
        &alice,
        json!({"version":1,"change":{"kind":"request","actor_id":id}}),
    )
    .await
    .error_for_status()
    .unwrap();
    let pending = h.json(&format!("{ROOT}/pulls"), &bob).await;
    assert_eq!(pending["items"][0]["rollup"]["reviews"]["requested"], 1);
    h.write(Method::POST,&format!("{ROOT}/pulls/1/reviews"),&bob,json!({"request_id":uuid::Uuid::now_v7(),"state":"approved","body":"Looks good","commit_oid":head})).await.error_for_status().unwrap();
    for path in [format!("{ROOT}/pulls"), format!("{ROOT}/issues?q=is%3Apr")] {
        let value = h.json(&path, &bob).await;
        let rollup = &value["items"][0]["rollup"];
        assert_eq!(rollup["head_oid"], head);
        assert_eq!(
            rollup["checks"],
            json!({"success":1,"neutral":0,"skipped":1,"pending":0,"failed":0})
        );
        assert_eq!(
            rollup["reviews"],
            json!({"approved":1,"changes_requested":0,"requested":0,"outdated":0})
        );
        for private in ["alice-id", "bob-id", h.provider.issuer.as_str()] {
            assert!(!value.to_string().contains(private));
        }
    }
    std::fs::write(source.path().join("README.md"), "Advanced source head\n").unwrap();
    git::success(source.path(), &["commit", "-am", "Advance source"]).await;
    let advanced = git::success(source.path(), &["rev-parse", "HEAD"]).await;
    git::success(source.path(), &["push", url.as_str(), "feature"]).await;
    let changed = h.json(&format!("{ROOT}/pulls"), &bob).await;
    assert_eq!(changed["items"][0]["rollup"]["head_oid"], advanced);
    assert_eq!(changed["items"][0]["rollup"]["reviews"]["approved"], 0);
    assert_eq!(changed["items"][0]["rollup"]["reviews"]["outdated"], 1);
    assert_eq!(changed["items"][0]["rollup"]["checks"]["success"], 0);
    git::success(
        source.path(),
        &["push", url.as_str(), "--delete", "feature"],
    )
    .await;
    let missing = h.json(&format!("{ROOT}/pulls"), &bob).await;
    assert_eq!(
        missing["items"][0]["rollup"],
        json!({"number":1,"head_oid":null,"reviews":null,"checks":null})
    );
    h.close().await;
}
