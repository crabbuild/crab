use super::*;

const LABELS: &str = "/api/repos/team/private/labels";
const ISSUES: &str = "/api/repos/team/private/issues";

fn nonce(id: usize) -> String {
    format!("00000000-0000-4000-8001-{id:012}")
}

async fn write(
    h: &Harness,
    cookie: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Option<Value>) {
    let session = h.json("/api/session", cookie).await;
    let response = h
        .http
        .request(method.parse().unwrap(), format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let body = (!bytes.is_empty()).then(|| serde_json::from_slice(&bytes).unwrap());
    (status, body)
}

#[tokio::test]
async fn repository_labels_are_durable_assignable_and_tombstoned() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let request = json!({
        "request_id": nonce(1),
        "name": "bug",
        "color": "D73A4A",
        "description": "Something is not working"
    });
    let (status, body) = write(&h, &alice, "POST", LABELS, request.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let original = body.unwrap();
    let id = original["id"].as_u64().unwrap();
    assert_eq!(original["color"], "d73a4a");

    let path = format!("{LABELS}/{id}");
    let (status, body) = write(
        &h,
        &alice,
        "PATCH",
        &path,
        json!({"version":1,"name":"kind/bug","color":"b60205","description":"Confirmed defect"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let edited = body.unwrap();
    assert_eq!(edited["version"], 2);
    assert_eq!(
        write(&h, &alice, "POST", LABELS, request.clone()).await.1,
        Some(edited.clone())
    );
    assert_eq!(
        write(
            &h,
            &alice,
            "POST",
            LABELS,
            json!({"request_id":nonce(2),"name":"KIND/BUG","color":"ffffff","description":null})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    let issue = write(
        &h,
        &alice,
        "POST",
        ISSUES,
        json!({"request_id":nonce(3),"title":"Label this report","body":"Details"}),
    )
    .await
    .1
    .unwrap();
    let issue_path = format!("{ISSUES}/{}", issue["number"]);
    let (status, assigned) = write(
        &h,
        &alice,
        "PATCH",
        &issue_path,
        json!({"version":1,"label_ids":[id]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(assigned.unwrap()["labels"][0]["name"], "kind/bug");
    assert_eq!(
        write(
            &h,
            &alice,
            "PATCH",
            &issue_path,
            json!({"version":2,"label_ids":[id,id]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        write(&h, &alice, "DELETE", &path, json!({"version":1}))
            .await
            .0,
        StatusCode::CONFLICT
    );

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(LABELS, &bob).await["can_manage"], false);
    assert_eq!(
        write(
            &h,
            &bob,
            "PATCH",
            &issue_path,
            json!({"version":2,"label_ids":[]})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        write(
            &h,
            &bob,
            "POST",
            LABELS,
            json!({"request_id":nonce(4),"name":"denied","color":"ffffff","description":null})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    *h.provider.mode.lock().await = String::new();
    assert_eq!(
        write(&h, &alice, "DELETE", &path, json!({"version":2}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        write(&h, &alice, "DELETE", &path, json!({"version":2}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert!(
        h.json(&issue_path, &alice).await["labels"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        write(&h, &alice, "POST", LABELS, request).await.0,
        StatusCode::NOT_FOUND
    );
    assert!(
        h.json(LABELS, &alice).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    h.close().await;
}
