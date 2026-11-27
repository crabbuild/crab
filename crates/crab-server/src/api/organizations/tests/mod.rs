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

mod audit;
mod automation;
mod codeowners;
mod creation;
mod issue_types;
mod offboarding;
mod repositories;
mod repository_access;
mod review_workload;
mod team_hierarchy;

#[tokio::test]
async fn people_search_filters_current_profiles_roles_teams_and_invitation_status() {
    let h = Harness::new(false).await;
    let (id, alice_actor, bob_actor) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let owned = h.json("/api/organizations", &alice).await;
    assert_eq!(owned["items"][0]["organization"]["id"], json!(id));
    assert_eq!(owned["items"][0]["actor"], json!(alice_actor));
    assert_eq!(owned["items"][0]["role"], "owner");
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob Engineer"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let root = format!("/api/organizations/{id}");
    h.write(
        Method::PUT,
        &format!("{root}/invitations/{bob_actor}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let pending = format!("{root}/invitations?status=pending&role=member&q=engineer");
    assert_eq!(
        h.json(&pending, &alice).await["items"][0]["invitation"]["recipient"],
        json!(bob_actor)
    );
    assert_eq!(
        h.json("/api/organizations", &bob).await["items"][0]["kind"],
        "invitation"
    );
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&pending, &alice).await["items"], json!([]));
    let accepted = format!("{root}/invitations?status=accepted&q=bob");
    assert_eq!(
        h.json(&accepted, &alice).await["items"][0]["invitation"]["state"],
        "accepted"
    );

    let team = Uuid::now_v7();
    let commands = format!("{root}/teams/commands");
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Release","description":"","visibility":"secret"}}))
        .await.error_for_status().unwrap();
    let filtered = format!("{root}/members?role=member&team={team}&q=%40bob");
    assert_eq!(h.json(&filtered, &alice).await["items"], json!([]));
    assert_eq!(
        h.http
            .get(format!("{}{filtered}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.write(Method::POST, &commands, &alice,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":bob_actor,"version":0,"role":"member"}}))
        .await.error_for_status().unwrap();
    assert_eq!(
        h.json(&filtered, &bob).await["items"][0]["profile"]["handle"],
        "bob"
    );
    assert_eq!(
        h.json("/api/organizations", &bob).await["items"][0]["kind"],
        "member"
    );
    h.close().await;
}

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
    let existing_handle = h.json(&root, &alice).await["organization"]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        h.write(
            Method::PATCH,
            &root,
            &bob,
            json!({"version":3,"handle":existing_handle,"name":"Takeover","description":""})
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
                handle: crate::catalog::derive_organization_handle("other-realm", other),
                name: "Other realm".into(),
                description: String::new(),
                website: String::new(),
                email: String::new(),
                location: String::new(),
                avatar: None,
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
    let current = h.json(&root, &alice).await["organization"].clone();
    let revision = current["version"].clone();
    assert_eq!(
        h.write(
            Method::PATCH,
            &root,
            &alice,
            json!({
                "version": revision,
                "handle": current["handle"],
                "name": "Organization renamed",
                "description": "Native metadata",
                "website": "https://crab.build",
                "email": "hello@crab.build",
                "location": "San Francisco"
            })
        )
        .await
        .status(),
        StatusCode::OK
    );
    let updated = h.json(&root, &alice).await["organization"].clone();
    assert_eq!(updated["name"], "Organization renamed");
    assert_eq!(updated["website"], "https://crab.build");
    assert_eq!(updated["email"], "hello@crab.build");
    assert_eq!(updated["location"], "San Francisco");
    h.close().await;
}

#[cfg(unix)]
mod browser;

mod rulesets;
