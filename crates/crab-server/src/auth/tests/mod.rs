use super::*;
use bytes::Bytes;
use openidconnect::{
    ClientId,
    core::{CoreClient, CoreProviderMetadata},
};
use serde_json::json;
use url::Url;

#[tokio::test]
async fn expired_sessions_are_rejected_and_https_cookies_cannot_be_shadowed() {
    let auth = shared_auth(Store::new(Arc::new(object_store::memory::InMemory::new())));
    let cookie = auth.cookie(false, "test-token", SESSION_LIFETIME).unwrap();
    assert_eq!(
        cookie,
        "__Host-crab_session=test-token; Path=/; HttpOnly; SameSite=Lax; Max-Age=28800; Secure"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("__Host-crab_session=test-token"),
    );
    let session = Arc::new(Session {
        identity: Identity {
            issuer: "https://identity.example".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        csrf: "test-csrf".into(),
        expires_at: now_epoch().unwrap() - 1,
        session_key: key("test-token"),
        revoked: AtomicBool::new(false),
    });
    auth.store_session(Arc::clone(&session)).await.unwrap();
    auth.sessions
        .lock()
        .await
        .insert(key("test-token"), Arc::clone(&session));
    let principal = Principal::git(Arc::new(GitToken {
        repository_id: uuid::Uuid::from_u128(1),
        session,
        owner: "team".into(),
        repository: "private".into(),
        access: RepositoryAccess::Write,
        revoked: AtomicBool::new(false),
    }));
    assert!(!principal.authenticated());
    assert!(!auth.principal(&headers).await.authenticated());
    assert!(auth.sessions.lock().await.is_empty());
    headers.append(
        header::COOKIE,
        HeaderValue::from_static("__Host-crab_session=shadow"),
    );
    assert!(cookie_value(&headers, "__Host-crab_session").is_none());
}

fn shared_auth(store: Store) -> Authentication {
    let config = OidcConfig {
        provider: AuthProvider::Oidc,
        issuer: openidconnect::IssuerUrl::new("https://id.example".into()).unwrap(),
        public_url: Url::parse("https://git.example").unwrap(),
        client_id: "crab".into(),
        client_secret_file: None,
        state_key_file: None,
        github: None,
    };
    let metadata: CoreProviderMetadata = serde_json::from_value(json!({
        "issuer":"https://id.example",
        "authorization_endpoint":"https://id.example/auth",
        "jwks_uri":"https://id.example/keys",
        "response_types_supported":["code"],
        "subject_types_supported":["public"],
        "id_token_signing_alg_values_supported":["RS256"]
    }))
    .unwrap();
    Authentication {
        config,
        secret: None,
        http: reqwest::Client::new(),
        client: Some(CoreClient::from_provider_metadata(
            metadata,
            ClientId::new("crab".into()),
            None,
        )),
        sessions: Mutex::new(HashMap::new()),
        git_tokens: Mutex::new(HashMap::new()),
        state: AuthState {
            store,
            prefix: "root/.crab/http-server/v1/auth".into(),
        },
        cursor_key: [7; 32],
        admission: Semaphore::new(1),
        logout_admission: Semaphore::new(1),
    }
}

#[tokio::test]
async fn shared_state_crosses_replica_boundaries_and_consumes_flows_once() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let first = shared_auth(store.clone());
    let second = shared_auth(store);
    let session_key = key("browser-token");
    let session = Arc::new(Session {
        identity: Identity {
            issuer: "https://id.example".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        csrf: "csrf".into(),
        expires_at: now_epoch().unwrap() + 600,
        session_key,
        revoked: AtomicBool::new(false),
    });
    first.store_session(Arc::clone(&session)).await.unwrap();
    let second_session = second.load_session(session_key).await.unwrap();
    assert!(second_session.active());

    let git_key = key("git-token");
    first
        .store_git_token(
            git_key,
            Arc::new(GitToken {
                repository_id: uuid::Uuid::from_u128(1),
                session,
                owner: "team".into(),
                repository: "private".into(),
                access: RepositoryAccess::Write,
                revoked: AtomicBool::new(false),
            }),
        )
        .await
        .unwrap();
    assert!(second.load_git_token(git_key).await.unwrap().active());
    second.remove_git_tokens(&second_session).await.unwrap();
    assert!(first.load_git_token(git_key).await.is_none());

    let flow_key = key("login-state");
    first
        .store_flow(
            flow_key,
            Flow {
                provider: AuthProvider::Oidc,
                nonce: Nonce::new("nonce".into()),
                verifier: PkceCodeVerifier::new("v".repeat(43)),
                return_to: "/team/private".into(),
                expires_at: now_epoch().unwrap() + 60,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        second.take_flow(flow_key).await.unwrap().return_to,
        "/team/private"
    );
    assert!(matches!(
        first.take_flow(flow_key).await,
        Err(AuthError::Invalid)
    ));
}

#[tokio::test]
async fn identity_revocation_crosses_replicas_and_deletes_derived_tokens() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let first = shared_auth(store.clone());
    let second = shared_auth(store.clone());
    for (name, issuer) in [
        ("one", "https://id.example"),
        ("two", "https://id.example"),
        ("other", "https://other.example"),
    ] {
        let session = Arc::new(Session {
            identity: Identity {
                issuer: issuer.into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            csrf: "csrf".into(),
            expires_at: now_epoch().unwrap() + 600,
            session_key: key(name),
            revoked: AtomicBool::new(false),
        });
        first.store_session(Arc::clone(&session)).await.unwrap();
        first
            .store_git_token(
                key(&format!("git-{name}")),
                Arc::new(GitToken {
                    repository_id: uuid::Uuid::from_u128(1),
                    session,
                    owner: "team".into(),
                    repository: "private".into(),
                    access: RepositoryAccess::Read,
                    revoked: AtomicBool::new(false),
                }),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        second
            .revoke_identity("https://id.example", "alice")
            .await
            .unwrap(),
        2
    );
    assert!(first.load_session(key("one")).await.is_none());
    assert!(first.load_session(key("two")).await.is_none());
    assert!(first.load_session(key("other")).await.is_some());
    for name in ["one", "two"] {
        let path = first.state.path("git-tokens", &key(&format!("git-{name}")));
        assert!(matches!(
            store.get_with_etag_bounded(&path, 8192).await,
            Err(StorageError::NotFound { .. })
        ));
    }
    assert_eq!(
        second
            .revoke_identity("https://id.example", "alice")
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn session_without_its_identity_marker_is_rejected() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let auth = shared_auth(store);
    let session = Arc::new(Session {
        identity: Identity {
            issuer: "https://id.example".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        csrf: "csrf".into(),
        expires_at: now_epoch().unwrap() + 600,
        session_key: key("unindexed"),
        revoked: AtomicBool::new(false),
    });
    auth.store_session(Arc::clone(&session)).await.unwrap();
    let state = &auth.state;
    state
        .delete_if_present(&state.identity_session_path(
            &session.identity.issuer,
            &session.identity.subject,
            &session.session_key,
        ))
        .await
        .unwrap();

    assert!(auth.load_session(session.session_key).await.is_none());
}

#[tokio::test]
async fn failed_session_creation_removes_its_marker_and_stale_markers_are_collected() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let auth = shared_auth(store.clone());
    let state = &auth.state;
    let session = Arc::new(Session {
        identity: Identity {
            issuer: "https://id.example".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        csrf: "csrf".into(),
        expires_at: now_epoch().unwrap() + 600,
        session_key: key("duplicate"),
        revoked: AtomicBool::new(false),
    });
    let record = StoredSession {
        identity: session.identity.clone(),
        csrf: session.csrf.clone(),
        expires_at: session.expires_at,
        git_tokens: vec![],
    };
    store
        .create_strict(
            &state.path("sessions", &session.session_key),
            Bytes::from(serde_json::to_vec(&record).unwrap()),
        )
        .await
        .unwrap();
    assert!(auth.store_session(Arc::clone(&session)).await.is_err());
    let marker_path = state.identity_session_path(
        &session.identity.issuer,
        &session.identity.subject,
        &session.session_key,
    );
    assert!(matches!(
        store.get_with_etag_bounded(&marker_path, 8192).await,
        Err(StorageError::NotFound { .. })
    ));
    store
        .delete(&state.path("sessions", &session.session_key))
        .await
        .unwrap();
    auth.store_session(Arc::clone(&session)).await.unwrap();
    store
        .delete(&state.path("sessions", &session.session_key))
        .await
        .unwrap();
    auth.revoke_identity(&session.identity.issuer, &session.identity.subject)
        .await
        .unwrap();
    assert!(matches!(
        store.get_with_etag_bounded(&marker_path, 8192).await,
        Err(StorageError::NotFound { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn token_attachment_racing_revocation_cannot_leave_a_usable_credential() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let auth = shared_auth(store.clone());
    for index in 0..8 {
        let session = Arc::new(Session {
            identity: Identity {
                issuer: "https://id.example".into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            csrf: "csrf".into(),
            expires_at: now_epoch().unwrap() + 600,
            session_key: key(&format!("race-{index}")),
            revoked: AtomicBool::new(false),
        });
        auth.store_session(Arc::clone(&session)).await.unwrap();
        let token_key = key(&format!("race-token-{index}"));
        let token = Arc::new(GitToken {
            repository_id: uuid::Uuid::from_u128(1),
            session: Arc::clone(&session),
            owner: "team".into(),
            repository: "private".into(),
            access: RepositoryAccess::Read,
            revoked: AtomicBool::new(false),
        });
        let (_, removed) = tokio::join!(
            auth.store_git_token(token_key, token),
            auth.remove_session(session.session_key)
        );
        removed.unwrap();
        assert!(auth.load_git_token(token_key).await.is_none());
        assert!(matches!(
            store
                .get_with_etag_bounded(&auth.state.path("git-tokens", &token_key), 8192)
                .await,
            Err(StorageError::NotFound { .. })
        ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn session_creation_racing_logout_never_leaves_an_unindexed_parent() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let auth = shared_auth(store.clone());
    let state = &auth.state;
    for index in 0..16 {
        let session = Arc::new(Session {
            identity: Identity {
                issuer: "https://id.example".into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            csrf: "csrf".into(),
            expires_at: now_epoch().unwrap() + 600,
            session_key: key(&format!("login-race-{index}")),
            revoked: AtomicBool::new(false),
        });
        let (_created, _revoked) = tokio::join!(
            auth.store_session(Arc::clone(&session)),
            auth.revoke_identity("https://id.example", "alice")
        );
        let parent = state
            .store
            .get_with_etag_bounded(&state.path("sessions", &session.session_key), 64 * 1024)
            .await;
        if parent.is_ok() {
            let marker_path =
                state.identity_session_path("https://id.example", "alice", &session.session_key);
            let (body, _) = store
                .get_with_etag_bounded(&marker_path, 8192)
                .await
                .unwrap();
            let marker: IdentitySessionMarker = serde_json::from_slice(&body).unwrap();
            assert_eq!(marker.session_key, session.session_key);
        }
        auth.remove_session(session.session_key).await.unwrap();
    }
}
