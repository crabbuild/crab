use crate::{cells::Address, test_support::http::Harness};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

fn registration(handle: &str) -> Value {
    json!({"request_id":uuid::Uuid::now_v7(),"handle":handle,"name":"Alice 🦀"})
}

#[tokio::test]
async fn profiles_require_preparation_session_and_csrf_and_expose_only_public_fields() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let response = h
        .http
        .get(format!("{}/api/profile", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "directory_unavailable"
    );
    h.server
        .cells()
        .unwrap()
        .initialize(Address::Directory)
        .await
        .unwrap();
    assert_eq!(
        h.json("/api/profile", &alice).await,
        json!({"profile":null})
    );
    let input = registration("alice");
    let csrf_missing = h
        .http
        .post(format!("{}/api/profile", h.origin))
        .header(header::COOKIE, &alice)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(csrf_missing.status(), StatusCode::FORBIDDEN);
    let created = h
        .write(Method::POST, "/api/profile", &alice, input.clone())
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let actor: Value = created.json().await.unwrap();
    let mut fields = actor
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "created_at_ms",
            "handle",
            "id",
            "name",
            "updated_at_ms",
            "version"
        ]
    );
    assert_eq!(
        h.write(Method::POST, "/api/profile", &alice, input)
            .await
            .json::<Value>()
            .await
            .unwrap(),
        actor
    );
    let id = actor["id"].as_str().unwrap();
    assert_eq!(h.json(&format!("/api/actors/{id}"), &alice).await, actor);
    for (body, expected) in [
        (
            json!({"request_id":uuid::Uuid::now_v7(),"handle":"Alice","name":"Alice"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"request_id":uuid::Uuid::now_v7(),"handle":"alice","name":"Alice","identity":{"issuer":"https://other.test","subject":"bob"}}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        assert_eq!(
            h.write(Method::POST, "/api/profile", &alice, body)
                .await
                .status(),
            expected
        );
    }
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    for path in ["/api/profile".to_string(), format!("/api/actors/{id}")] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .basic_auth("crab", token["token"].as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    h.close().await;
}

#[tokio::test]
async fn profiles_keep_actor_ids_on_rename_and_isolate_accounts_conflicts_and_revoked_sessions() {
    let h = Harness::new(false).await;
    h.server
        .cells()
        .unwrap()
        .initialize(Address::Directory)
        .await
        .unwrap();
    let alice = h.login().await;
    let input = registration("alice");
    let created = h
        .write(Method::POST, "/api/profile", &alice, input.clone())
        .await
        .json::<Value>()
        .await
        .unwrap();
    let edit = json!({"version":1,"handle":"alice-renamed","name":"Alice renamed"});
    let changed = h
        .write(Method::PATCH, "/api/profile", &alice, edit.clone())
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(changed["id"], created["id"]);
    assert_eq!(
        h.write(Method::PATCH, "/api/profile", &alice, edit)
            .await
            .json::<Value>()
            .await
            .unwrap(),
        changed
    );
    let newer = h
        .write(
            Method::PATCH,
            "/api/profile",
            &alice,
            json!({"version":2,"handle":"alice-renamed","name":"Newer name"}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        h.write(
            Method::PATCH,
            "/api/profile",
            &alice,
            json!({"version":1,"handle":"alice-renamed","name":"Old draft"})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.write(Method::POST, "/api/profile", &alice, input.clone())
            .await
            .json::<Value>()
            .await
            .unwrap(),
        newer
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let mut collision = registration("bob");
    collision["request_id"] = input["request_id"].clone();
    assert_eq!(
        h.write(Method::POST, "/api/profile", &bob, collision)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(h.json("/api/profile", &bob).await, json!({"profile":null}));
    let taken = h
        .write(
            Method::POST,
            "/api/profile",
            &bob,
            registration("alice-renamed"),
        )
        .await;
    assert_eq!(taken.status(), StatusCode::CONFLICT);
    assert_eq!(
        taken.json::<Value>().await.unwrap()["error"]["code"],
        "handle_taken"
    );
    let registered_bob = h
        .write(Method::POST, "/api/profile", &bob, registration("bob"))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_ne!(registered_bob["id"], created["id"]);
    assert_eq!(h.json("/api/profile", &alice).await["profile"], newer);
    assert_eq!(
        h.write(
            Method::PATCH,
            "/api/profile",
            &bob,
            json!({"id":created["id"],"version":1,"handle":"alice-renamed","name":"Impersonation"})
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let session = h.json("/api/session", &alice).await;
    assert_eq!(
        h.write(Method::POST, "/auth/logout", &alice, json!({}))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let response = h
        .http
        .patch(format!("{}/api/profile", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .json(&json!({"version":3,"handle":"alice-renamed","name":"After logout"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.json(
            &format!("/api/actors/{}", created["id"].as_str().unwrap()),
            &bob
        )
        .await,
        newer
    );
    h.close().await;
}
