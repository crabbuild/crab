use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

#[tokio::test]
async fn list_authors_use_current_public_profiles_without_exposing_private_keys_or_changing_source()
{
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let root = "/api/repos/team/private/issues";
    h.write(
        Method::POST,
        root,
        &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Attributed work","body":"Original text"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let unprepared = h
        .http
        .get(format!("{}{root}", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(unprepared.status(), StatusCode::SERVICE_UNAVAILABLE);
    h.initialize_directory().await;
    let source = h.json(&format!("{root}/1"), &alice).await;
    assert_eq!(
        h.json(root, &alice).await["items"][0]["author"],
        json!({"name":"Alice","profile":null})
    );
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &alice,
            json!({
                "request_id": uuid::Uuid::now_v7(),
                "handle": "public-alice",
                "name": "Public Alice",
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let author = h.json(root, &alice).await["items"][0]["author"].clone();
    assert_eq!(
        author,
        json!({"name":"Alice","profile":{"id":profile["id"],"handle":"public-alice","name":"Public Alice"}})
    );
    for private in ["issuer", "subject", "alice-id"] {
        assert!(!author.to_string().contains(private));
    }
    h.write(
        Method::PATCH,
        "/api/profile",
        &alice,
        json!({"version":1,"handle":"renamed-alice","name":"Renamed Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let renamed = h.json(root, &alice).await["items"][0]["author"].clone();
    assert_eq!(
        renamed,
        json!({"name":"Alice","profile":{"id":profile["id"],"handle":"renamed-alice","name":"Renamed Alice"}})
    );
    assert_eq!(h.json(&format!("{root}/1"), &alice).await, source);
    use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};
    let repo = h.server.repositories.values()[0].id;
    for issuer in [
        h.provider.issuer.clone(),
        format!("{}/other", h.provider.issuer),
    ] {
        h.server
            .cells()
            .unwrap()
            .command::<CreatePull>(
                repo,
                crate::app::mutation_identity().unwrap(),
                CreatePullInput {
                    submission_id: *uuid::Uuid::now_v7().as_bytes(),
                    author: RepositoryAuthor {
                        issuer,
                        subject: "alice-id".into(),
                        name: "Alice".into(),
                    },
                    draft: false,
                    title: "Attributed pull".into(),
                    body: String::new(),
                    base_ref: "refs/heads/main".into(),
                    head_ref: "refs/heads/feature".into(),
                    base_oid: "a".repeat(40),
                    head_oid: "b".repeat(40),
                },
            )
            .await
            .unwrap();
    }
    let pulls = h.json("/api/repos/team/private/pulls", &alice).await;
    assert_eq!(
        pulls["items"][0]["author"],
        json!({"name":"Alice","profile":null})
    );
    assert_eq!(pulls["items"][1]["author"], renamed);
    let mixed = h
        .json(&format!("{root}?q=is%3Aissue%20OR%20is%3Apr"), &alice)
        .await;
    assert_eq!(mixed["matching"], 3);
    assert_eq!(mixed["items"][1]["author"], renamed);
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"read"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let scoped = h
        .http
        .get(format!("{}{root}", h.origin))
        .basic_auth("crab", token["token"].as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(scoped.status(), StatusCode::UNAUTHORIZED);
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let members = h.server.repositories.values()[0]
        .config
        .members
        .iter()
        .filter(|member| member.subject != "bob-id")
        .cloned()
        .collect();
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
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
    h.close().await;
}
