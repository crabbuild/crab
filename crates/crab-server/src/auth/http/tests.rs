use crate::test_support::http::Harness;
use crate::test_support::http::start_provider;
use axum::http::StatusCode;
use axum::http::header;
use serde_json::json;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn browser_sign_in_enforces_membership_csrf_logout_and_rotated_signing_keys() {
    let h = Harness::new(false).await;
    assert_eq!(h.json("/api/session", "").await["authenticated"], false);
    let response = h
        .http
        .get(format!("{}/api/repos", h.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Rotation happens after startup discovery; callbacks must obtain the new key.
    h.provider.rotated.store(true, Ordering::SeqCst);
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    assert_eq!(session["user"]["subject"], "alice-id");
    assert_eq!(
        h.json("/api/repos", &cookie).await["repositories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for (origin, csrf) in [
        ("http://evil.invalid", session["csrf"].as_str().unwrap()),
        (h.origin.as_str(), "wrong"),
    ] {
        let response = h
            .http
            .post(format!("{}/auth/logout", h.origin))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, origin)
            .header("x-csrf-token", csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = h
        .http
        .post(format!("{}/auth/logout", h.origin))
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        h.json("/api/session", &cookie).await["authenticated"],
        false
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert!(
        h.json("/api/repos", &outsider).await["repositories"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for action in [
        "refs", "commit", "commits", "tree", "search", "file", "blob", "asset", "changes", "diff",
        "blame",
    ] {
        let response = h
            .http
            .get(format!("{}/api/repos/team/private/{action}", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{action}");
    }
    h.close().await;
}

#[tokio::test]
async fn github_sign_in_uses_pkce_and_a_stable_provider_subject() {
    let h = Harness::new_github().await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    assert_eq!(session["authenticated"], true);
    assert_eq!(session["mode"], "github");
    assert_eq!(session["user"]["issuer"], h.provider.issuer.as_str());
    assert_eq!(session["user"]["subject"], "4242");
    assert_eq!(session["user"]["name"], "Octo Cat");
    h.close().await;
}

#[tokio::test]
async fn non_members_cannot_trigger_repository_publication() {
    let h = Harness::new(false).await;
    *h.provider.mode.lock().await = "outsider".into();
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let response = h
        .http
        .post(format!("{}/api/git-token", h.origin))
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"owner":"team","repository":"private","access":"read"}).to_string())
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    for (method, body) in [
        (
            reqwest::Method::POST,
            json!({
                "branch": "refs/heads/main",
                "expected_head": "1111111111111111111111111111111111111111",
                "path_hex": "524541444d452e6d64",
                "content": "created",
                "message": "Create README"
            }),
        ),
        (
            reqwest::Method::PATCH,
            json!({
                "branch": "refs/heads/main",
                "expected_head": "1111111111111111111111111111111111111111",
                "expected_blob": "2222222222222222222222222222222222222222",
                "path_hex": "524541444d452e6d64",
                "content": "updated",
                "message": "Update README"
            }),
        ),
        (
            reqwest::Method::DELETE,
            json!({
                "branch": "refs/heads/main",
                "expected_head": "1111111111111111111111111111111111111111",
                "expected_blob": "2222222222222222222222222222222222222222",
                "path_hex": "524541444d452e6d64",
                "message": "Delete README"
            }),
        ),
    ] {
        let response = h
            .http
            .request(
                method.clone(),
                format!("{}/api/repos/team/private/contents", h.origin),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
    }
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let before = crab_metadata::capsule_protocol::load_root(&repo.layout)
        .await
        .unwrap();
    let api = h
        .http
        .get(format!("{}/api/repos/team/private/refs", h.origin))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    let git = h
        .http
        .post(format!("{}/git/team/private.git/git-upload-pack", h.origin))
        .basic_auth("crab", Some("denied-token"))
        .header("git-protocol", "version=2")
        .header(
            header::CONTENT_TYPE,
            "application/x-git-upload-pack-request",
        )
        .body("0014command=ls-refs\n00010000")
        .send()
        .await
        .unwrap();
    assert_eq!(
        (api.status(), git.status()),
        (StatusCode::NOT_FOUND, StatusCode::UNAUTHORIZED)
    );
    assert_eq!(
        before.record().digest(),
        crab_metadata::capsule_protocol::load_root(&repo.layout)
            .await
            .unwrap()
            .record()
            .digest()
    );
    assert_eq!(
        repo.store
            .list_prefix(&repo.layout.capsule_ref_heads_prefix())
            .await
            .unwrap()
            .len(),
        0
    );
    h.close().await;
}

#[tokio::test]
async fn callback_rejects_unbound_browser_replays_and_invalid_signed_claims() {
    let h = Harness::new(false).await;
    let (callback, cookie) = h.start_login().await;
    let response = h.http.get(&callback).send().await.unwrap();
    assert_eq!(
        response.headers()[header::LOCATION],
        "/?auth_error=sign_in_failed"
    );
    // An unbound request cannot consume the original browser's transaction.
    let response = h
        .http
        .get(&callback)
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()[header::LOCATION], "/team/private");
    let response = h
        .http
        .get(&callback)
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::LOCATION],
        "/?auth_error=sign_in_failed"
    );
    for mode in [
        "nonce",
        "issuer",
        "audience",
        "authorized_party",
        "expired",
        "future",
        "signature",
        "access_hash",
    ] {
        *h.provider.mode.lock().await = mode.into();
        let (callback, cookie) = h.start_login().await;
        let response = h
            .http
            .get(&callback)
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::LOCATION],
            "/?auth_error=sign_in_failed",
            "{mode}"
        );
        assert!(
            response.headers().get(header::SET_COOKIE).is_none(),
            "{mode}"
        );
    }
    for target in [
        "//evil.invalid",
        "/\\evil.invalid",
        "https://evil.invalid",
        "/auth/logout",
    ] {
        let response = h
            .http
            .get(format!("{}/auth/login", h.origin))
            .query(&[("return_to", target)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{target}");
    }
    h.close().await;
}

#[tokio::test]
#[ignore = "manual browser qualification identity provider; never part of the production binary"]
async fn browser_identity_fixture() {
    let (provider, task) = start_provider(8790).await;
    println!(
        "Local test identity issuer: {} (public test subject alice-id)",
        provider.issuer
    );
    tokio::signal::ctrl_c().await.unwrap();
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn confidential_client_uses_secret_file_and_authenticated_token_exchange() {
    let h = Harness::new(true).await;
    let cookie = h.login().await;
    assert_eq!(
        h.json("/api/session", &cookie).await["user"]["subject"],
        "alice-id"
    );
    h.close().await;
}
