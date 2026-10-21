use crate::test_support::http::Harness;
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::json;

const ROOT: &str = "/api/repos/team/private";

#[tokio::test]
async fn member_activity_is_paginated_and_subject_scoped_without_identity_claims() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let input =
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Activity issue","body":"Private body"});
    for _ in 0..2 {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            input.clone(),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.write(
        Method::POST,
        &format!("{ROOT}/issues/1/comments"),
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"body":"Comment body"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PATCH,
        &format!("{ROOT}/issues/1"),
        &alice,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let page = h.json(&format!("{ROOT}/activity?limit=1"), &bob).await;
    assert_eq!(page["items"][0]["action"]["kind"], "closed");
    let next = page["next"].as_u64().unwrap();
    let older = h
        .json(
            &format!("{ROOT}/issues/1/activity?before={next}&limit=2"),
            &bob,
        )
        .await;
    assert_eq!(
        older["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["number"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(
        older["items"][0]["target"],
        json!({"kind":"issue_comment","issue":1,"number":1})
    );
    assert_eq!(older["items"][1]["actor"], "Alice");
    let encoded = older.to_string();
    for private in [
        "issuer",
        "subject",
        "alice-id",
        "Private body",
        "Comment body",
    ] {
        assert!(!encoded.contains(private), "{private}");
    }
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}/pulls/1/activity", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn activity_requires_membership_and_rejects_invalid_pages_before_native_dispatch() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let empty = h.json(&format!("{ROOT}/activity"), &cookie).await;
    assert_eq!(empty, json!({"items":[],"next":null}));
    for query in [
        "limit=0",
        "limit=51",
        "before=0",
        "before=9007199254740992",
        "limit=abc",
        "extra=true",
    ] {
        let response = h
            .http
            .get(format!("{}{ROOT}/activity?{query}", h.origin))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error(), "{query}");
    }
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let response = h
        .http
        .get(format!("{}{ROOT}/activity", h.origin))
        .header("cookie", &outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
