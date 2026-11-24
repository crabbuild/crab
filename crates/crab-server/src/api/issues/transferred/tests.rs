use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{RepositoryAccess, RepositoryMember, test_support::http::Harness};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_issue_links_resolve_the_current_published_location_with_live_grants() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let session = h.json("/api/session", &alice).await;
    let csrf = session["csrf"].as_str().unwrap();
    let admin = RepositoryMember {
        subject: "alice-id".into(),
        name: "Alice".into(),
        access: RepositoryAccess::Admin,
    };
    let reader = RepositoryMember {
        subject: "bob-id".into(),
        name: "Bob".into(),
        access: RepositoryAccess::Read,
    };
    let catalog = h.server.catalog().unwrap();
    for repository in ["library", "platform"] {
        h.add_repository(repository).await;
        catalog
            .set_members(
                "team",
                repository,
                vec![admin.clone(), reader.clone()],
                true,
            )
            .await
            .unwrap();
    }
    let repo = |name: &str| {
        h.server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap()
    };
    let private = repo("private");
    let library = repo("library");
    let platform = repo("platform");
    let created: Value = h
        .write(
            Method::POST,
            "/api/repos/team/private/issues",
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Rollout plan","body":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let original = format!("/api/repos/team/private/issues/{}", created["number"]);
    let mut path = original.clone();
    for (source, target) in [(&private, &library), (&library, &platform)] {
        let issue = h.json(&path, &alice).await;
        let response = h
            .http
            .post(format!("{}{path}/transfer", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::ORIGIN, &h.origin)
            .header("x-crab-repository-id", source.id.to_string())
            .header("x-csrf-token", csrf)
            .json(&json!({
                "request_id":Uuid::now_v7(), "version":issue["version"], "destination":target.id,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let transfer = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let response = h.json(&path, &alice).await;
                if response["transfer"]["state"] == "ready" {
                    break response["transfer"].clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(transfer["repository"]["id"], json!(target.id));
        path = format!(
            "/api/repos/team/{}/issues/{}",
            target.config.name, transfer["number"]
        );
    }
    let expected = h.json("/api/repos/team/library/issues/1", &bob).await;
    assert_eq!(expected["transfer"]["repository"]["id"], json!(platform.id));
    assert_eq!(h.json(&original, &bob).await, expected);
    assert_eq!(h.json(&path, &bob).await["title"], "Rollout plan");

    for restricted in ["library", "platform"] {
        catalog
            .set_members("team", restricted, vec![admin.clone()], true)
            .await
            .unwrap();
        assert_eq!(
            h.json(&original, &bob).await,
            json!({"transfer":{"state":"unavailable"}})
        );
        catalog
            .set_members(
                "team",
                restricted,
                vec![admin.clone(), reader.clone()],
                true,
            )
            .await
            .unwrap();
        assert_eq!(h.json(&original, &bob).await, expected);
    }
    h.close().await;
}
