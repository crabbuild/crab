use super::*;
use bytes::Bytes;
use openidconnect::{ClientId, core::CoreProviderMetadata};
use serde_json::json;
use url::Url;

mod session_reads;

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
        groups: Vec::new(),
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
        close_related_issues: false,
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
        scim_token_file: None,
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
        oidc: Some(DiscoveredOidc {
            client: Client::from_provider_metadata(metadata, ClientId::new("crab".into()), None),
            groups_scope: false,
            can_sign_up: false,
        }),
        sessions: Mutex::new(HashMap::new()),
        git_tokens: Mutex::new(HashMap::new()),
        state: AuthState {
            store,
            prefix: "root/.crab/http-server/v1/auth".into(),
            account_prefix: "root/.crab/http-server/v1/accounts".into(),
            require_provisioning: false,
            pending_checks: Arc::default(),
        },
        cursor_key: [7; 32],
        webhook_key: None,
        mirror_key: None,
        scim_key: None,
        scim_admission: Semaphore::new(4),
        admission: Semaphore::new(1),
        logout_admission: Semaphore::new(1),
    }
}

fn active_session(session_key: Key) -> Arc<Session> {
    Arc::new(Session {
        identity: Identity {
            issuer: "https://id.example".into(),
            subject: "alice".into(),
            name: "Alice".into(),
        },
        groups: Vec::new(),
        csrf: "csrf".into(),
        expires_at: now_epoch().unwrap() + 600,
        session_key,
        revoked: AtomicBool::new(false),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn cached_session_validation_reads_all_durable_authorities_concurrently() {
    struct Gate {
        entered: Semaphore,
        release: Semaphore,
        cancellation: tokio_util::sync::CancellationToken,
    }

    #[async_trait::async_trait]
    impl crab_storage::ReadAdmission for Gate {
        fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
            &self.cancellation
        }

        async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.entered.add_permits(1);
            self.release.acquire().await?.forget();
            Ok(())
        }

        async fn bytes(&self, _: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
    }

    let mut auth = shared_auth(Store::new(Arc::new(object_store::memory::InMemory::new())));
    let session = active_session(key("parallel"));
    auth.store_session(session.clone()).await.unwrap();
    auth.load_session(session.session_key).await.unwrap();
    let gate = Arc::new(Gate {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        cancellation: tokio_util::sync::CancellationToken::new(),
    });
    auth.state.store = auth.state.store.clone().with_read_admission(gate.clone());
    let auth = Arc::new(auth);
    let pending = tokio::spawn(async move { auth.load_session(session.session_key).await });
    tokio::time::timeout(Duration::from_secs(5), gate.entered.acquire_many(3))
        .await
        .unwrap()
        .unwrap()
        .forget();
    gate.release.add_permits(3);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn cached_session_never_bypasses_missing_or_expired_durable_state() {
    for (kind, expired) in [
        ("sessions", false),
        ("marker", false),
        ("sessions", true),
        ("marker", true),
    ] {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let auth = shared_auth(store.clone());
        let session = active_session(key("fresh-durable-state"));
        auth.store_session(session.clone()).await.unwrap();
        auth.load_session(session.session_key).await.unwrap();
        let path = if kind == "sessions" {
            auth.state.path(kind, &session.session_key)
        } else {
            auth.state.identity_session_path(
                &session.identity.issuer,
                &session.identity.subject,
                &session.session_key,
            )
        };
        if expired {
            let (body, _) = store.get_with_etag_bounded(&path, 64 * 1024).await.unwrap();
            let mut value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            value["expires_at"] = json!(0);
            store
                .put_overwrite(&path, Bytes::from(serde_json::to_vec(&value).unwrap()))
                .await
                .unwrap();
        } else {
            store.delete(&path).await.unwrap();
        }
        assert!(
            auth.load_session(session.session_key).await.is_none(),
            "{kind}, expired={expired}"
        );
    }
}

#[tokio::test]
async fn stale_cached_identity_cannot_replace_the_current_marker_location() {
    let auth = shared_auth(Store::new(Arc::new(object_store::memory::InMemory::new())));
    let session = active_session(key("current-marker"));
    auth.store_session(session.clone()).await.unwrap();
    let mut stale = active_session(session.session_key);
    Arc::get_mut(&mut stale).unwrap().identity.subject = "outdated".into();
    auth.sessions
        .lock()
        .await
        .insert(session.session_key, stale.clone());
    let current = auth.load_session(session.session_key).await.unwrap();
    assert_eq!(
        (current.identity.subject.as_str(), stale.active()),
        ("alice", false)
    );
}

#[tokio::test]
async fn shared_state_crosses_replica_boundaries_and_consumes_flows_once() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let first = shared_auth(store.clone());
    let second = shared_auth(store);
    let session_key = key("browser-token");
    let session = Arc::new(Session {
        groups: Vec::new(),
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
                close_related_issues: false,
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
async fn detached_git_token_stays_revoked_across_replicas_when_cleanup_is_interrupted() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let issuer = shared_auth(store.clone());
    let replica = shared_auth(store.clone());
    let session = active_session(key("browser-session"));
    let session_key = session.session_key;
    let token_key = key("git-token");
    issuer.store_session(session.clone()).await.unwrap();
    issuer
        .store_git_token(
            token_key,
            Arc::new(GitToken {
                repository_id: uuid::Uuid::from_u128(1),
                session,
                owner: "team".into(),
                repository: "private".into(),
                access: RepositoryAccess::Read,
                close_related_issues: false,
                revoked: AtomicBool::new(false),
            }),
        )
        .await
        .unwrap();
    assert!(replica.load_git_token(token_key).await.is_some());

    // Capture the durable state after detachment and before token object cleanup.
    let path = issuer.state.path("sessions", &session_key);
    let (body, etag) = store.get_with_etag_bounded(&path, 64 * 1024).await.unwrap();
    let mut session: StoredSession = serde_json::from_slice(&body).unwrap();
    assert_eq!(session.git_tokens, vec![token_key]);
    session.git_tokens.clear();
    store
        .update(
            &path,
            Bytes::from(serde_json::to_vec(&session).unwrap()),
            etag,
        )
        .await
        .unwrap();

    let restarted = shared_auth(store);
    let session = restarted.load_session(session_key).await.unwrap();
    restarted.remove_git_tokens(&session).await.unwrap();
    assert!(session.active());
    assert!(replica.load_git_token(token_key).await.is_none());
    assert!(restarted.load_git_token(token_key).await.is_none());
}

#[tokio::test]
async fn reactivation_requires_new_sessions_on_every_replica() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let operator = shared_auth(store.clone());
    let replica = shared_auth(store.clone());
    let session_key = key("before-suspension");
    operator
        .store_session(active_session(session_key))
        .await
        .unwrap();
    assert!(replica.load_session(session_key).await.is_some());

    let suspended = operator
        .set_account_suspended("alice", 0, true)
        .await
        .unwrap();
    assert!(replica.load_session(session_key).await.is_none());
    assert!(matches!(
        replica
            .store_session(active_session(key("during-suspension")))
            .await,
        Err(AuthError::Suspended)
    ));
    assert!(matches!(
        replica.set_account_suspended("alice", 0, false).await,
        Err(AuthError::AccountChanged)
    ));

    replica
        .set_account_suspended("alice", suspended.revision, false)
        .await
        .unwrap();
    let restarted = shared_auth(store);
    assert!(restarted.load_session(session_key).await.is_none());
    let fresh_key = key("after-reactivation");
    operator
        .store_session(active_session(fresh_key))
        .await
        .unwrap();
    assert!(restarted.load_session(fresh_key).await.is_some());
}

#[tokio::test]
async fn suspended_accounts_lose_mention_group_evidence() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let operator = shared_auth(store.clone());
    let replica = shared_auth(store);
    let mut session = active_session(key("mention-group"));
    Arc::get_mut(&mut session)
        .unwrap()
        .groups
        .push("engineering".into());
    operator.store_session(session).await.unwrap();
    let grants = [crate::RepositoryGroup {
        group: "engineering".into(),
        access: RepositoryAccess::Read,
    }];
    assert!(
        replica
            .mention_group("alice", &grants)
            .await
            .unwrap()
            .is_some()
    );

    operator
        .set_account_suspended("alice", 0, true)
        .await
        .unwrap();
    assert!(
        replica
            .mention_group("alice", &grants)
            .await
            .unwrap()
            .is_none()
    );
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
            groups: Vec::new(),
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
                    close_related_issues: false,
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
        groups: Vec::new(),
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
        groups: Vec::new(),
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
        groups: Vec::new(),
        identity: session.identity.clone(),
        account_revision: 0,
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
            groups: Vec::new(),
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
            close_related_issues: false,
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
            groups: Vec::new(),
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
