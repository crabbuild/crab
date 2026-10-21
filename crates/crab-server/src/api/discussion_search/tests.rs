use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use serde_json::{Value, json};

#[tokio::test]
async fn completion_uses_authorized_repository_labels_and_bounds_cursor_input() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let root = "/api/repos/team/private";
    let response = h.write(reqwest::Method::POST, &format!("{root}/labels"), &cookie, json!({
        "request_id": uuid::Uuid::now_v7(), "name":"Help wanted", "color":"abcdef", "description":"First contribution",
    })).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let query = "🦀 label:he";
    let cursor = query.encode_utf16().count().to_string();
    let response = h
        .http
        .get(format!("{}{root}/discussion-search", h.origin))
        .header(header::COOKIE, &cookie)
        .query(&[("query", query), ("cursor", cursor.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["items"][0]["value"], "label:\"Help wanted\"");
    assert_eq!(
        (response["start"].as_u64(), response["end"].as_u64()),
        (Some(3), Some(11))
    );
    for (query, cursor) in [("🦀", "1"), ("a", "3"), ("a\n", "2")] {
        let response = h
            .http
            .get(format!("{}{root}/discussion-search", h.origin))
            .header(header::COOKIE, &cookie)
            .query(&[("query", query), ("cursor", cursor)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{root}/discussion-search", h.origin))
        .header(header::COOKIE, &outsider)
        .query(&[("query", "label:"), ("cursor", "6")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn issue_workspace_returns_tagged_pull_results_and_paginates_colliding_numbers() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let root = "/api/repos/team/private";
    let token: Value = h
        .write(
            reqwest::Method::POST,
            "/api/git-token",
            &cookie,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut git_url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    crate::test_support::git::push_review_branches(source.path(), git_url.as_str()).await;
    let issue: Value = h.write(reqwest::Method::POST, &format!("{root}/issues"), &cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Issue result","body":"Private issue body"})).await.error_for_status().unwrap().json().await.unwrap();
    let pull: Value = h.write(reqwest::Method::POST, &format!("{root}/pulls"), &cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Pull result","body":"Private pull body","base_ref":"refs/heads/main","head_ref":"refs/heads/feature","draft":true})).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(
        (issue["number"].as_u64(), pull["number"].as_u64()),
        (Some(1), Some(1))
    );
    let default = h.json(&format!("{root}/issues"), &cookie).await;
    assert_eq!(default["items"].as_array().unwrap().len(), 1);
    assert_eq!(default["items"][0]["kind"], "issue");
    let pulls = h.json(&format!("{root}/issues?q=is%3Apr"), &cookie).await;
    assert_eq!(pulls["items"][0]["kind"], "pull");
    assert_eq!(pulls["items"][0]["draft"], true);
    assert!(pulls["items"][0].get("body").is_none());
    assert!(pulls["items"][0]["author"].is_string());
    let query = format!("{root}/issues?q=is%3Aopen&sort=comments&direction=asc&limit=1");
    let first = h.json(&query, &cookie).await;
    assert_eq!(first["matching"], 2);
    assert_eq!(first["counts"]["open"], 2);
    assert_eq!(first["items"][0]["kind"], "issue");
    let cursor = first["next"].as_str().unwrap();
    let last = h.json(&format!("{query}&before={cursor}"), &cookie).await;
    assert_eq!(last["items"][0]["kind"], "pull");
    assert_eq!(last["items"][0]["number"], 1);
    assert!(last["next"].is_null());
    let response = h
        .http
        .get(format!(
            "{}{root}/pulls?q=is%3Aopen&sort=comments&direction=asc&limit=1&before={cursor}",
            h.origin
        ))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    h.write(
        reqwest::Method::PATCH,
        &format!("{root}/pulls/1"),
        &cookie,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let open = h.json(&format!("{root}/issues?q=is%3Aopen"), &cookie).await;
    assert_eq!(open["counts"], json!({"open":1,"closed":1,"merged":0}));
    assert_eq!(open["matching"], 1);
    let default_pulls = h.json(&format!("{root}/pulls"), &cookie).await;
    assert!(default_pulls["items"].as_array().unwrap().is_empty());
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{root}/issues?q=is%3Apr", h.origin))
        .header(header::COOKIE, &outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
