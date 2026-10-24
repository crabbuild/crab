use crate::{RepositoryAccess, test_support::http::Harness};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::json;

#[tokio::test]
async fn durable_downgrade_cannot_keep_write_rights_from_an_old_routing_snapshot() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let issued = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await;
    assert_eq!(issued.status(), StatusCode::OK);
    let issued: serde_json::Value = issued.json().await.unwrap();
    let token = issued["token"].as_str().unwrap();
    let repo = h.server.repositories.values()[0].clone();
    let mut members = repo.config.members.clone();
    members[0].access = RepositoryAccess::Triage;
    members[1].access = RepositoryAccess::Admin;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(repo.config.members[0].access, RepositoryAccess::Admin);
    for (path, body) in [
        (
            "/api/git-token",
            json!({"owner":"team","repository":"private","access":"write"}),
        ),
        (
            "/api/repos/team/private/labels",
            json!({"request_id":uuid::Uuid::now_v7(),"name":"Must not be created","color":"abcdef"}),
        ),
    ] {
        assert_eq!(
            h.write(Method::POST, path, &alice, body).await.status(),
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    for (method, route, body) in [
        (Method::GET, "info/refs?service=git-receive-pack", None),
        (
            Method::POST,
            "info/lfs/objects/batch",
            Some(json!({"operation":"upload","objects":[]})),
        ),
    ] {
        let mut request = h
            .http
            .request(method, format!("{}/git/team/private.git/{route}", h.origin))
            .basic_auth("crab", Some(token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "{route}"
        );
    }
    let catalog = h.json("/api/repos", &alice).await;
    assert_eq!(catalog["repositories"][0]["access"], "read");
    assert_eq!(catalog["repositories"][0]["can_admin"], false);
    h.close().await;
}

#[tokio::test]
async fn current_grants_apply_before_routing_refresh_and_removal_hides_all_reads() {
    let h = Harness::new(false).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let repo = h.server.repositories.values()[0].clone();
    let mut members = repo.config.members.clone();
    members[1].access = RepositoryAccess::Write;
    let catalog = h.server.catalog().unwrap();
    catalog
        .set_members("team", "private", members.clone(), true)
        .await
        .unwrap();
    assert_eq!(repo.config.members[1].access, RepositoryAccess::Read);
    let created = h
        .write(
            Method::POST,
            "/api/repos/%74eam/%70rivate/labels",
            &bob,
            json!({"request_id":uuid::Uuid::now_v7(),"name":"Current grant","color":"abcdef"}),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let visible = h.json("/api/repos", &bob).await;
    assert_eq!(visible["repositories"][0]["access"], "write");
    members.retain(|member| member.subject != "bob-id");
    catalog
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    let denied = h
        .http
        .get(format!("{}/api/repos/%74eam/%70rivate/labels", h.origin))
        .header(axum::http::header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json("/api/repos", &bob).await["repositories"], json!([]));
    h.close().await;
}

#[tokio::test]
async fn unreadable_authority_never_falls_back_to_cached_membership() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let root = h.server.catalog().unwrap().root().clone();
    let path = root.path(".crab/http-server/v1/catalog.json");
    let (_, etag) = root
        .store
        .get_with_etag_bounded(&path, 256 * 1024)
        .await
        .unwrap();
    root.store
        .update(&path, bytes::Bytes::from_static(b"invalid catalog"), etag)
        .await
        .unwrap();
    for path in [
        "/api/repos",
        "/api/repos/team/private/labels",
        "/api/repos/team/private/members",
    ] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .header(axum::http::header::COOKIE, &alice)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
    }
    let response = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    h.close().await;
}

#[tokio::test]
async fn a_reused_route_cannot_authorize_the_previous_repository_identity() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let catalog = h.server.catalog().unwrap();
    let (mut document, etag) = catalog.load().await.unwrap();
    document.repositories[0].id = uuid::Uuid::now_v7();
    document.version += 1;
    let root = catalog.root();
    root.store
        .update(
            &root.path(".crab/http-server/v1/catalog.json"),
            serde_json::to_vec(&document).unwrap().into(),
            etag.unwrap(),
        )
        .await
        .unwrap();
    for path in [
        "/api/repos/team/private/labels",
        "/api/repos/team/private/members",
    ] {
        let response = h
            .http
            .get(format!("{}{path}", h.origin))
            .header(axum::http::header::COOKIE, &alice)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        h.json("/api/repos", &alice).await["repositories"],
        json!([])
    );
    h.close().await;
}
