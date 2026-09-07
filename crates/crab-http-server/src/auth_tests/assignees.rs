use super::*;

const ASSIGNEES: &str = "/api/repos/team/private/assignees";
const ISSUES: &str = "/api/repos/team/private/issues";

async fn write(
    h: &Harness,
    cookie: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let session = h.json("/api/session", cookie).await;
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn repository_members_are_assignable_by_writers() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let catalog = h.json(ASSIGNEES, &alice).await;
    assert_eq!(
        catalog,
        json!({
            "items":[
                {"subject":"alice-id","name":"Alice"},
                {"subject":"bob-id","name":"Bob"}
            ],
            "can_manage":true
        })
    );
    let issue = write(
        &h,
        &alice,
        reqwest::Method::POST,
        ISSUES,
        json!({
            "request_id":"00000000-0000-4000-8002-000000000001",
            "title":"Assign this report",
            "body":"Details"
        }),
    )
    .await
    .1;
    let issue_path = format!("{ISSUES}/{}", issue["number"]);
    let assigned = write(
        &h,
        &alice,
        reqwest::Method::PATCH,
        &issue_path,
        json!({"version":1,"assignees":["bob-id","alice-id"]}),
    )
    .await;
    assert_eq!(assigned.0, StatusCode::OK);
    assert_eq!(
        assigned.1["assignees"],
        json!([
            {"subject":"alice-id","name":"Alice"},
            {"subject":"bob-id","name":"Bob"}
        ])
    );
    assert_eq!(
        h.json(ISSUES, &alice).await["items"][0]["assignees"],
        assigned.1["assignees"]
    );
    for selection in [json!(["bob-id", "bob-id"]), json!(["outsider-id"])] {
        assert_eq!(
            write(
                &h,
                &alice,
                reqwest::Method::PATCH,
                &issue_path,
                json!({"version":2,"assignees":selection}),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(ASSIGNEES, &bob).await["can_manage"], false);
    assert_eq!(
        write(
            &h,
            &bob,
            reqwest::Method::PATCH,
            &issue_path,
            json!({"version":2,"assignees":[]}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    h.close().await;
}
