use super::*;

async fn replace(h: &Harness, cookie: &str, csrf: &str, body: Value) -> reqwest::Response {
    h.http
        .put(format!("{}/api/repos/team/private/members", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn membership_cas_audit_and_current_authorization() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let csrf = session["csrf"].as_str().unwrap();
    let before = h.json("/api/repos/TEAM/PRIVATE/members", &cookie).await;
    let no_op = replace(
        &h,
        &cookie,
        csrf,
        json!({"expected_revision":before["revision"], "members":before["members"]}),
    )
    .await;
    assert_eq!(no_op.status(), StatusCode::OK);
    let no_op: Value = serde_json::from_slice(&no_op.bytes().await.unwrap()).unwrap();
    assert_eq!(no_op, before);
    let final_admin = replace(
        &h,
        &cookie,
        csrf,
        json!({"expected_revision":before["revision"], "members":[]}),
    )
    .await;
    assert_eq!(final_admin.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let mut members = before["members"].clone();
    members[1]["access"] = json!("admin");
    let changed = replace(
        &h,
        &cookie,
        csrf,
        json!({"expected_revision":before["revision"], "members":members}),
    )
    .await;
    assert_eq!(changed.status(), StatusCode::OK);
    let changed: Value = serde_json::from_slice(&changed.bytes().await.unwrap()).unwrap();
    assert_eq!(
        changed["revision"].as_u64(),
        Some(before["revision"].as_u64().unwrap() + 1)
    );
    assert_eq!(
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"], "members":members})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let catalog = h.server.catalog.as_ref().unwrap();
    let (document, _) = catalog.load().await.unwrap();
    let path = object_store::path::Path::from(document.membership_audit_head.unwrap());
    let (event, _) = catalog
        .root()
        .store
        .get_with_etag_bounded(&path, 8192)
        .await
        .unwrap();
    let event: Value = serde_json::from_slice(&event).unwrap();
    assert_eq!(
        event["actor"],
        json!({"issuer":h.provider.issuer,"subject":"alice-id"})
    );
    members[0]["access"] = json!("read");
    assert_eq!(
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":changed["revision"], "members":members})
        )
        .await
        .status(),
        StatusCode::OK
    );
    // The routing snapshot still grants Alice admin; the catalog decision must not.
    assert_eq!(
        h.http
            .get(format!("{}/api/repos/team/private/members", h.origin))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn membership_hides_non_admins_and_enforces_browser_mutation_protection() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let before = h.json("/api/repos/team/private/members", &cookie).await;
    let body = json!({"expected_revision":before["revision"],"members":before["members"]});
    for (origin, csrf) in [
        (None, None),
        (
            Some("https://wrong.invalid"),
            Some(session["csrf"].as_str().unwrap()),
        ),
        (Some(h.origin.as_str()), None),
        (Some(h.origin.as_str()), Some("wrong")),
    ] {
        let mut request = h
            .http
            .put(format!("{}/api/repos/team/private/members", h.origin))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    for identity in ["member", "outsider"] {
        *h.provider.mode.lock().await = identity.into();
        let cookie = h.login().await;
        for name in ["private", "missing"] {
            assert_eq!(
                h.http
                    .get(format!("{}/api/repos/team/{name}/members", h.origin))
                    .header(header::COOKIE, &cookie)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
    h.close().await;
}

#[tokio::test]
async fn anonymous_membership_requests_are_hidden_without_changing_other_api_authentication() {
    let h = Harness::new(false).await;
    for name in ["private", "missing"] {
        for method in [reqwest::Method::GET, reqwest::Method::PUT] {
            let response = h
                .http
                .request(
                    method,
                    format!("{}/api/repos/team/{name}/members", h.origin),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(
                body,
                json!({"error":{"code":"repository_not_found","message":"Repository not found."}})
            );
        }
    }
    assert_eq!(
        h.http
            .get(format!("{}/api/repos", h.origin))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    h.close().await;
}

#[tokio::test]
async fn malformed_membership_is_rejected_and_two_writers_cannot_overwrite_each_other() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let csrf = session["csrf"].as_str().unwrap();
    let before = h.json("/api/repos/team/private/members", &cookie).await;
    for mutation in [
        "empty-subject",
        "duplicate-subject",
        "duplicate-name",
        "unknown-access",
        "unknown-field",
        "oversized-subject",
    ] {
        let mut members = before["members"].clone();
        match mutation {
            "empty-subject" => members[0]["subject"] = json!(""),
            "duplicate-subject" => members[1]["subject"] = members[0]["subject"].clone(),
            "duplicate-name" => members[1]["name"] = json!("alice"),
            "unknown-access" => members[1]["access"] = json!("owner"),
            "unknown-field" => members[1]["extra"] = json!(true),
            _ => members[0]["subject"] = json!("s".repeat(513)),
        }
        let response = replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"],"members":members}),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{mutation}"
        );
        let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "invalid_membership");
    }
    let mut first = before["members"].clone();
    first[1]["name"] = json!("Bob First");
    let mut second = before["members"].clone();
    second[1]["name"] = json!("Bob Second");
    let (first, second) = tokio::join!(
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"],"members":first})
        ),
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"],"members":second})
        )
    );
    let mut statuses = [first.status(), second.status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    h.close().await;
}
