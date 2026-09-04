use super::*;

const ROOT: &str = "/api/repos/team/private/pulls";

#[tokio::test]
async fn pull_routes_require_membership_and_csrf_before_repository_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let input = json!({
        "request_id":"00000000-0000-4000-8000-000000000001",
        "title":"Proposed change",
        "body":"Please review",
        "base_ref":"refs/heads/main",
        "head_ref":"refs/heads/feature"
    });
    assert_eq!(
        h.http
            .post(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        let session = h.json("/api/session", &outsider).await;
        let response = h
            .http
            .request(method, format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &outsider)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    h.close().await;
}
