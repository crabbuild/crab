use super::*;

async fn principal(h: &Harness, token: &str) -> auth::Principal {
    let mut headers = axum::http::HeaderMap::new();
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("crab:{token}"),
    );
    headers.insert(
        header::AUTHORIZATION,
        format!("Basic {encoded}").parse().unwrap(),
    );
    h.server
        .auth
        .as_ref()
        .unwrap()
        .git_principal(&headers)
        .await
}

#[tokio::test]
async fn token_permissions_intersect_repository_membership_and_requested_scope() {
    let h = Harness::new(false).await;
    let repo = &h.server.repositories[&("team".into(), "private".into())];
    crab_metadata::manifest_store::create_manifest(
        &repo.store,
        &repo.layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    for (identity, access, permitted) in [
        ("valid", "read", true),
        ("valid", "write", true),
        ("member", "read", true),
        ("member", "write", false),
        ("outsider", "read", false),
        ("outsider", "write", false),
    ] {
        *h.provider.mode.lock().await = identity.into();
        let cookie = h.login().await;
        let session = h.json("/api/session", &cookie).await;
        let response = h
            .http
            .post(format!("{}/api/git-token", h.origin))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"owner":"team","repository":"private","access":access}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if permitted {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            },
            "{identity}/{access}"
        );
        if !permitted {
            continue;
        }
        let issued: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            (&issued["owner"], &issued["repository"], &issued["access"]),
            (&json!("team"), &json!("private"), &json!(access))
        );
        let token = issued["token"].as_str().unwrap();
        for (method, route, body) in [
            (
                reqwest::Method::GET,
                "info/refs?service=git-receive-pack",
                Vec::new(),
            ),
            (reqwest::Method::POST, "git-receive-pack", b"0000".to_vec()),
        ] {
            let response = h
                .http
                .request(method, format!("{}/git/team/private/{route}", h.origin))
                .basic_auth("crab", Some(token))
                .header(
                    header::CONTENT_TYPE,
                    "application/x-git-receive-pack-request",
                )
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if access == "write" {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                }
            );
        }
        let principal = principal(&h, token).await;
        let mut config = h.server.repositories[&("team".into(), "private".into())]
            .config
            .clone();
        assert_eq!(
            (principal.can_read(&config), principal.can_write(&config)),
            (true, access == "write")
        );
        for member in &mut config.members {
            member.access = crate::RepositoryAccess::Read;
        }
        assert_eq!(
            (principal.can_read(&config), principal.can_write(&config)),
            (true, false)
        );
        config.name = "another".into();
        assert!(!principal.can_read(&config));
        config.name = "private".into();
        config.owner = "another".into();
        assert!(!principal.can_read(&config));
        config.owner = "team".into();
        config.members.clear();
        assert!(!principal.can_read(&config));
    }
    h.close().await;
}

#[tokio::test]
async fn token_issuance_requires_explicit_bounded_scope_and_authorized_target() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    for (body, status) in [
        (json!({}), StatusCode::UNPROCESSABLE_ENTITY),
        (
            json!({"owner":"team","repository":"private","access":"admin"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"owner":"team","repository":"absent","access":"read"}),
            StatusCode::FORBIDDEN,
        ),
        (
            json!({"owner":"team","repository":"private","access":"read","extra":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"owner":"team","repository":"a".repeat(3000),"access":"read"}),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = h
            .http
            .post(format!("{}/api/git-token", h.origin))
            .header(header::COOKIE, &cookie)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    h.close().await;
}

#[tokio::test]
async fn git_tokens_are_read_scoped_and_revoked_with_the_browser_session() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let csrf = session["csrf"].as_str().unwrap();
    let git_url = format!(
        "{}/git/team/private/info/refs?service=git-upload-pack",
        h.origin
    );
    let response = h
        .http
        .get(&git_url)
        .header(header::COOKIE, &cookie)
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
    let response = h
        .http
        .post(format!("{}/api/git-token", h.origin))
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"owner":"team","repository":"private","access":"read"}).to_string())
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let issued: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let token = issued["token"].as_str().unwrap();
    let retained = principal(&h, token).await;
    for authenticated in [false, true] {
        let request = h
            .http
            .post(format!("{}/git/team/private/git-upload-pack", h.origin))
            .header(
                header::CONTENT_TYPE,
                "application/x-git-upload-pack-request",
            )
            .body(b"0000".to_vec());
        let response = if authenticated {
            request.basic_auth("crab", Some(token))
        } else {
            request
        }
        .send()
        .await
        .unwrap();
        assert_eq!(
            response.status(),
            if authenticated {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
    }
    {
        use tower::ServiceExt as _;
        let _busy = h.server.git_admission.acquire_many(4).await.unwrap();
        let authorization = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("crab:{token}"),
        );
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/git/team/private/git-upload-pack")
            .header(header::HOST, h.origin.strip_prefix("http://").unwrap())
            .header(header::AUTHORIZATION, format!("Basic {authorization}"))
            .header(
                header::CONTENT_TYPE,
                "application/x-git-upload-pack-request",
            )
            .header("git-protocol", "version=2")
            .body(axum::body::Body::from_stream(
                futures_util::stream::pending::<
                    std::result::Result<axum::body::Bytes, std::io::Error>,
                >(),
            ))
            .unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            router(Arc::clone(&h.server)).oneshot(request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
    let response = h
        .http
        .get(&git_url)
        .basic_auth("crab", Some(token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .bytes()
            .await
            .unwrap()
            .starts_with(b"000eversion 2\n")
    );
    let response = h
        .http
        .get(format!("{}/api/repos", h.origin))
        .basic_auth("crab", Some(token))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = b"0014command=ls-refs\n00010000trailing";
    let response = h
        .http
        .post(format!("{}/git/team/private/git-upload-pack", h.origin))
        .basic_auth("crab", Some(token))
        .header("git-protocol", "version=2")
        .header(
            header::CONTENT_TYPE,
            "application/x-git-upload-pack-request",
        )
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = h
        .http
        .delete(format!("{}/api/git-token", h.origin))
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!retained.authenticated());
    assert!(!retained.can_read(&h.server.repositories[&("team".into(), "private".into())].config));
    let response = h
        .http
        .get(&git_url)
        .basic_auth("crab", Some(token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = h
        .http
        .post(format!("{}/api/git-token", h.origin))
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"owner":"team","repository":"private","access":"read"}).to_string())
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    let issued: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let token = issued["token"].as_str().unwrap();
    let response = h
        .http
        .post(format!("{}/auth/logout", h.origin))
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = h
        .http
        .get(&git_url)
        .basic_auth("crab", Some(token))
        .header("git-protocol", "version=2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    h.close().await;
}
