use axum::http::{StatusCode, header};
use reqwest::{Method, RequestBuilder};
use serde_json::{Value, json};

use super::{ERROR, ROOT, USER};
use crate::test_support::http::{Harness, TEST_SCIM_TOKEN, organizations};

fn request(h: &Harness, method: Method, path: &str) -> RequestBuilder {
    h.http
        .request(method, format!("{}{ROOT}{path}", h.origin))
        .bearer_auth(TEST_SCIM_TOKEN)
        .header(header::CONTENT_TYPE, "application/scim+json")
}

async fn send(h: &Harness, method: Method, path: &str, body: Value) -> reqwest::Response {
    request(h, method, path).json(&body).send().await.unwrap()
}

async fn create(h: &Harness, external_id: &str, username: &str) -> reqwest::Response {
    send(
        h,
        Method::POST,
        "/Users",
        json!({
            "schemas": [USER], "externalId": external_id, "userName": username,
            "password": "discarded fixture field",
        }),
    )
    .await
}

async fn patch(h: &Harness, path: &str, value: Value) -> reqwest::Response {
    send(
        h,
        Method::PATCH,
        path,
        json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [value],
        }),
    )
    .await
}

async fn assert_sign_in_blocked(h: &Harness) {
    let (callback, cookie) = h.start_login().await;
    let rejected = h
        .http
        .get(callback)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        rejected.headers()[header::LOCATION],
        "/?auth_error=sign_in_failed"
    );
}

async fn git_token(h: &Harness, cookie: &str) -> String {
    h.write(
        Method::POST,
        "/api/git-token",
        cookie,
        json!({
            "owner": "team", "repository": "private", "access": "read"
        }),
    )
    .await
    .error_for_status()
    .unwrap()
    .json::<Value>()
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .into()
}

async fn git_status(h: &Harness, token: &str) -> StatusCode {
    h.http
        .get(format!(
            "{}/git/team/private.git/info/refs?service=git-upload-pack",
            h.origin
        ))
        .header("git-protocol", "version=2")
        .basic_auth("crab", Some(token))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn only_managed_active_subjects_can_sign_in_and_deactivation_retires_credentials() {
    let h = Harness::scim().await;
    assert_sign_in_blocked(&h).await;
    let created = create(&h, "alice-id", "alice@example.test").await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let initial_tag = created.headers()[header::ETAG].clone();
    let user: Value = created.json().await.unwrap();
    let path = format!("/Users/{}", user["id"].as_str().unwrap());
    let cookie = h.login().await;
    let token = git_token(&h, &cookie).await;
    let token_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(token_file.path(), TEST_SCIM_TOKEN).unwrap();
    let mut config = h.server.auth.as_ref().unwrap().config.clone();
    config.scim_token_file = Some(token_file.path().to_owned());
    let replica =
        crate::auth::Authentication::new(config, h.server.catalog.as_ref().unwrap().root())
            .await
            .unwrap();
    let session_headers =
        axum::http::HeaderMap::from_iter([(header::COOKIE, cookie.parse().unwrap())]);
    let key = crate::auth::key(&token);
    assert!(replica.principal(&session_headers).await.authenticated());
    assert!(replica.load_git_token(key).await.is_some());
    assert_eq!(git_status(&h, &token).await, StatusCode::OK);

    let edited = patch(
        &h,
        &path,
        json!({"op":"Replace","path":"displayName","value":"Alice"}),
    )
    .await;
    assert_eq!(edited.status(), StatusCode::OK);
    assert_eq!(git_status(&h, &token).await, StatusCode::OK);
    let stale = request(&h, Method::DELETE, &path)
        .header(header::IF_MATCH, initial_tag)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

    let disabled = patch(&h, &path, json!({"op":"replace","value":{"active":false}})).await;
    assert_eq!(disabled.status(), StatusCode::OK);
    assert_eq!(
        h.json("/api/session", &cookie).await["authenticated"],
        false
    );
    assert_eq!(git_status(&h, &token).await, StatusCode::UNAUTHORIZED);
    assert!(!replica.principal(&session_headers).await.authenticated());
    assert!(replica.load_git_token(key).await.is_none());
    assert_sign_in_blocked(&h).await;

    let enabled = patch(
        &h,
        &path,
        json!({"op":"replace","path":"active","value":true}),
    )
    .await;
    assert_eq!(enabled.status(), StatusCode::OK);
    assert_eq!(git_status(&h, &token).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.json("/api/session", &cookie).await["authenticated"],
        false
    );
    let new_cookie = h.login().await;
    assert_eq!(
        git_status(&h, &git_token(&h, &new_cookie).await).await,
        StatusCode::OK
    );

    let auth = h.server.auth.as_ref().unwrap();
    let revision = auth.account_state("alice-id").await.unwrap().revision;
    auth.set_account_suspended("alice-id", revision, true)
        .await
        .unwrap();
    for active in [false, true] {
        assert_eq!(
            patch(
                &h,
                &path,
                json!({"op":"replace","path":"active","value":active})
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    assert_sign_in_blocked(&h).await;
    h.close().await;
}

#[tokio::test]
async fn directory_authentication_and_identity_round_trip_bind_exact_subject() {
    let h = Harness::scim().await;
    let resource = format!("{}{ROOT}/Users", h.origin);
    let denied = h.http.get(&resource).send().await.unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(denied.headers()[header::WWW_AUTHENTICATE], "Bearer");
    assert_eq!(
        denied.json::<Value>().await.unwrap()["schemas"],
        json!([ERROR])
    );
    let wrong_url = format!("{}/api/repos", h.origin);
    assert_eq!(
        h.http
            .get(wrong_url)
            .bearer_auth(TEST_SCIM_TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let discovery = request(&h, Method::GET, "/ServiceProviderConfig")
        .send()
        .await
        .unwrap();
    assert_eq!(discovery.status(), StatusCode::OK);
    assert_eq!(
        discovery.headers()[header::CONTENT_TYPE],
        "application/scim+json"
    );

    let wrong_case = create(&h, "Alice-ID", "alice@example.test").await;
    assert_eq!(wrong_case.status(), StatusCode::CREATED);
    assert_sign_in_blocked(&h).await;
    let created = create(&h, "alice-id", "Alice@example.test").await;
    assert_eq!(created.status(), StatusCode::CONFLICT);
    let created = create(&h, "alice-id", "alice2@example.test").await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let user: Value = created.json().await.unwrap();
    assert!(user.get("password").is_none());
    let id = user["id"].as_str().unwrap();
    let path = format!("/Users/{id}");
    let filter = request(&h, Method::GET, "/Users")
        .query(&[
            ("filter", "userName eq \"Alice2@example.test\""),
            ("startIndex", "1"),
            ("count", "1"),
        ])
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(filter["totalResults"], 1);
    assert_eq!(filter["Resources"][0]["id"], id);
    let cookie = h.login().await;
    let (org, actor, _) = organizations::bootstrap(&h).await;
    let member = h
        .json(
            &format!("/api/organizations/{org}/members/{actor}/repository-access"),
            &cookie,
        )
        .await;
    assert_eq!(
        (member["managed"].clone(), member["account"].clone()),
        (json!(true), json!("active"))
    );

    let input = json!({"schemas":[USER],"userName":"renamed@example.test","externalId":"other","active":false});
    assert_eq!(
        send(&h, Method::PUT, &path, input).await.status(),
        StatusCode::BAD_REQUEST
    );
    let input = json!({
        "schemas":[USER],"userName":"renamed@example.test","active":false,
        "name":{"givenName":"Alice", "familyName":"Example"},"emails":[{"value":"alice@example.test","type":"work","primary":true}],
    });
    let replaced = send(&h, Method::PUT, &path, input)
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(replaced["userName"], "renamed@example.test");
    assert_eq!(replaced["externalId"], "alice-id");
    assert_eq!(replaced["active"], false);
    assert_eq!(replaced["emails"][0]["primary"], true);

    let deleted = request(&h, Method::DELETE, &path).send().await.unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let read = request(&h, Method::GET, &path).send().await.unwrap();
    assert_eq!(read.status(), StatusCode::NOT_FOUND);
    let rejoined = create(&h, "alice-id", "alice2@example.test")
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(rejoined["id"], id);
    assert_eq!(
        h.json("/api/session", &cookie).await["authenticated"],
        false
    );
    h.close().await;
}
