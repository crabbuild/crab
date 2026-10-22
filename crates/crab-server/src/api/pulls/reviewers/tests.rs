use crate::test_support::{git, http::Harness};
use reqwest::Method;
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private/pulls/1";
#[tokio::test]
async fn unrequested_reviews_keep_decisions_and_use_the_live_head_without_private_claims() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let mut git_url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), git_url.as_str()).await;
    h.write(Method::POST,"/api/repos/team/private/pulls",&alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Participant status","body":"Review without an invitation","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    for state in ["approved", "commented"] {
        h.write(Method::POST,&format!("{ROOT}/reviews"),&bob,json!({"request_id":uuid::Uuid::now_v7(),"body":"Review feedback","state":state,"commit_oid":head})).await.error_for_status().unwrap();
    }
    let path = format!("{ROOT}/reviewers");
    let page = h.json(&path, &alice).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let item = &page["items"][0];
    assert_eq!(item["review"]["number"], 2);
    assert_eq!(item["decision"]["number"], 1);
    assert_eq!(item["decision"]["state"], "approved");
    assert_eq!(item["decision"]["current"], true);
    assert!(item["request"].is_null());
    assert!(item["candidate_id"].is_string());
    for private in ["alice-id", "bob-id", h.provider.issuer.as_str()] {
        assert!(!page.to_string().contains(private));
    }
    std::fs::write(source.path().join("README.md"), "Updated after review\n").unwrap();
    git::success(source.path(), &["commit", "-am", "Advance reviewed head"]).await;
    let advanced = git::success(source.path(), &["rev-parse", "HEAD"]).await;
    git::success(source.path(), &["push", git_url.as_str(), "feature"]).await;
    let changed = h.json(&path, &alice).await;
    assert_eq!(changed["head_oid"], advanced);
    assert_eq!(changed["items"][0]["decision"]["current"], false);
    assert_eq!(
        h.json(&format!("{ROOT}/reviews"), &alice).await["head_oid"],
        advanced
    );
    assert_eq!(h.json(ROOT, &alice).await["original_head_oid"], head);
    git::success(
        source.path(),
        &["push", git_url.as_str(), "--delete", "feature"],
    )
    .await;
    let missing = h.json(&path, &alice).await;
    assert!(missing["head_oid"].is_null());
    assert!(missing["items"][0]["decision"]["current"].is_null());
    assert_eq!(missing["items"][0]["decision"]["state"], "approved");
    let reviews = h.json(&format!("{ROOT}/reviews"), &alice).await;
    assert!(reviews.get("head_oid").is_some_and(Value::is_null));
    assert!(reviews["items"][0]["current"].is_null());
    for suffix in ["?limit=0", "?limit=51", "?after=invalid"] {
        assert_eq!(
            h.http
                .get(format!("{}{path}{suffix}", h.origin))
                .header("cookie", &alice)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }
    h.close().await;
}
