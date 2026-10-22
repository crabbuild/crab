use super::*;
use crate::{
    cells::{
        directory::{IdentityKey, RegisterAccounts},
        organization::{Bootstrap, BootstrapInput},
    },
    test_support::http::Harness,
};
use axum::http::{StatusCode, header};
use reqwest::Method;

#[tokio::test]
async fn invitations_require_consent_and_current_qualified_authority_without_repository_grants() {
    let h = Harness::new(false).await;
    let (id, alice_id, bob_id) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{id}");
    let own = h.json(&root, &alice).await;
    assert_eq!(
        (own["actor"].clone(), own["role"].clone()),
        (json!(alice_id), json!("owner"))
    );
    assert_eq!(
        h.http
            .get(format!("{}{root}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.write(
            Method::PUT,
            &format!("{root}/members/{bob_id}"),
            &alice,
            json!({"version":0,"role":"owner"})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let invite_path = format!("{root}/invitations/{bob_id}");
    let no_csrf = h
        .http
        .put(format!("{}{invite_path}", h.origin))
        .header(header::COOKIE, &alice)
        .json(&json!({"version":0,"role":"member"}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);
    let offer = h
        .write(
            Method::PUT,
            &invite_path,
            &alice,
            json!({"version":0,"role":"member"}),
        )
        .await;
    assert_eq!(offer.status(), StatusCode::OK);
    let offer: Value = offer.json().await.unwrap();
    assert_eq!(offer["state"], "pending");
    assert_eq!(
        h.json(&format!("{root}/members"), &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let recipient = h.json(&format!("{root}/invitation"), &bob).await;
    assert_eq!(recipient["invitation"], offer);
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{root}/invitation"),
            &alice,
            json!({"version":1,"accept":true,"actor":bob_id})
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let accepted = h
        .write(
            Method::POST,
            &format!("{root}/invitation"),
            &bob,
            json!({"version":1,"accept":true}),
        )
        .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(accepted.json::<Value>().await.unwrap()["state"], "accepted");
    let page = h.json(&format!("{root}/members?limit=1"), &alice).await;
    let cursor = page["next"].as_str().unwrap();
    assert_eq!(
        h.json(&format!("{root}/members?limit=1&after={cursor}"), &alice)
            .await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let cross = h
        .http
        .get(format!(
            "{}{root}/invitations?limit=1&after={cursor}",
            h.origin
        ))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), StatusCode::CONFLICT);
    assert_eq!(
        cross.json::<Value>().await.unwrap()["error"]["code"],
        "page_changed"
    );
    let members = h.json(&format!("{root}/members"), &bob).await;
    assert_eq!(
        (
            members["items"].as_array().unwrap().len(),
            members["can_manage"].clone()
        ),
        (2, json!(false))
    );
    assert!(
        members["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["profile"].is_null() && item.get("subject").is_none())
    );
    for recipient in [alice_id, Uuid::now_v7()] {
        assert_eq!(
            h.write(
                Method::PUT,
                &format!("{root}/invitations/{recipient}"),
                &bob,
                json!({"version":0,"role":"member"})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        h.write(
            Method::PATCH,
            &root,
            &bob,
            json!({"version":3,"name":"Takeover","description":""})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.write(
            Method::PUT,
            &format!("{root}/members/{bob_id}"),
            &alice,
            json!({"version":1,"role":"owner"})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(h.json(&root, &bob).await["role"], "owner");
    // Organization ownership does not widen the catalog's repository authority.
    assert_eq!(
        h.write(
            Method::PUT,
            "/api/repos/team/private/members",
            &bob,
            json!({"expected_revision":1,"members":[]})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.write(
            Method::DELETE,
            &format!("{root}/members/{bob_id}"),
            &alice,
            json!({"version":2})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        h.http
            .get(format!("{}{root}/members", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let history = h.json(&format!("{root}/invitation"), &bob).await;
    assert!(history["organization"].is_null());
    assert_eq!(history["invitation"]["state"], "accepted");
    // Historical acceptance cannot regrant a removed membership.
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{root}/invitation"),
            &bob,
            json!({"version":1,"accept":true})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn organization_accounts_are_issuer_qualified_and_git_tokens_cannot_manage_them() {
    let h = Harness::new(false).await;
    let (id, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{id}");
    assert_eq!(
        h.write(
            Method::PUT,
            &format!("{root}/invitations/{}", Uuid::now_v7()),
            &alice,
            json!({"version":0,"role":"member"})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let cells = h.server.cells().unwrap();
    let actors = cells
        .command::<RegisterAccounts>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer: "https://other-realm.invalid".into(),
                subject: "alice-id".into(),
            }],
        )
        .await
        .unwrap()
        .output;
    let other = Uuid::now_v7();
    cells
        .initialize(Address::Organization(other))
        .await
        .unwrap();
    cells
        .command::<Bootstrap>(
            Address::Organization(other),
            app::mutation_identity().unwrap(),
            BootstrapInput {
                actor: actors[0],
                name: "Other realm".into(),
                description: String::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        h.http
            .get(format!("{}/api/organizations/{other}", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
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
    for path in [
        root.clone(),
        format!("{root}/invitation"),
        format!("{root}/members"),
    ] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .basic_auth("crab", token["token"].as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let revision = h.json(&root, &alice).await["organization"]["version"].clone();
    assert_eq!(h.write(Method::PATCH,&root,&alice,json!({"version":revision,"name":"Organization renamed","description":"Native metadata"})).await.status(),StatusCode::OK);
    assert_eq!(
        h.json(&root, &alice).await["organization"]["name"],
        "Organization renamed"
    );
    h.close().await;
}

#[cfg(unix)]
mod browser;
