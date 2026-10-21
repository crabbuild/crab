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
        .query(&[
            ("query", query),
            ("cursor", cursor.as_str()),
            ("kind", "issue"),
        ])
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
            .query(&[("query", query), ("cursor", cursor), ("kind", "issue")])
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
        .query(&[("query", "label:"), ("cursor", "6"), ("kind", "issue")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
