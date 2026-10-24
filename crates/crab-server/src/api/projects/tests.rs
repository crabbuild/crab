use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;
const ROOT: &str = "/api/repos/team/private";
#[tokio::test]
async fn projects_are_native_versioned_and_repository_authorized() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let body = json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","id":id,"name":"Release","template":"kanban"}});
    let path = format!("{ROOT}/projects/commands");
    let first: Value = h
        .write(Method::POST, &path, &alice, body.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["outcome"], "applied");
    let retried: Value = h
        .write(Method::POST, &path, &alice, body)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, retried);
    assert_eq!(
        h.json(&format!("{ROOT}/projects/{id}/items"), &alice).await["page"]["project"]["definition"]
            ["name"],
        "Release"
    );
    h.write(Method::POST,&format!("{ROOT}/issues"),&alice,json!({"request_id":Uuid::now_v7(),"title":"Native linked issue","body":"Private discussion body"})).await.error_for_status().unwrap();
    h.write(Method::POST,&path,&alice,json!({"request_id":Uuid::now_v7(),"command":{"kind":"item","project":id,"version":1,"item":{"id":Uuid::now_v7(),"subject":{"kind":"issue","number":1},"title":"","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}}})).await.error_for_status().unwrap();
    let page = h.json(&format!("{ROOT}/projects/{id}/items"), &alice).await;
    assert_eq!(
        page["page"]["items"][0]["work"]["title"],
        "Native linked issue"
    );
    for private in ["alice-id", "Private discussion body", "issuer"] {
        assert!(!page.to_string().contains(private));
    }
    assert_eq!(
        h.http
            .post(format!("{}{path}", h.origin))
            .header(header::COOKIE, &alice)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for command in [
        json!({"kind":"unknown"}),
        json!({"kind":"create","id":Uuid::now_v7(),"template":"board","name":"x".repeat(100*1024)}),
    ] {
        let rejected = h
            .write(
                Method::POST,
                &path,
                &alice,
                json!({"request_id":Uuid::now_v7(),"command":command}),
            )
            .await;
        assert!(rejected.status().is_client_error());
        assert_eq!(rejected.headers()["x-crab-write-outcome"], "rejected");
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(&format!("{ROOT}/projects"), &bob).await["can_manage"],
        false
    );
    assert_eq!(h.write(Method::POST,&path,&bob,json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","id":Uuid::now_v7(),"name":"No","template":"board"}})).await.status(),StatusCode::FORBIDDEN);
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}/projects/{id}", h.origin))
            .header(header::COOKIE, outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
