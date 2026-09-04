use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::header;
use axum::response::Redirect;
use openidconnect::{
    AccessToken, JsonWebKeyId, PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey,
    core::{CoreEdDsaPrivateSigningKey, CoreIdToken, CoreIdTokenClaims, CoreJwsSigningAlgorithm},
};
use serde_json::Value;
use url::Url;

// Public deterministic keys used only by the local test identity provider.
const KEY_ONE: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIBERERERERERERERERERERERERERERERERERERERERER\n-----END PRIVATE KEY-----";
const KEY_TWO: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEICIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIi\n-----END PRIVATE KEY-----";

struct Provider {
    issuer: String,
    rotated: AtomicBool,
    confidential: AtomicBool,
    mode: Mutex<String>,
    codes: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl Provider {
    fn signing_key(&self) -> CoreEdDsaPrivateSigningKey {
        let rotated = self.rotated.load(Ordering::SeqCst);
        CoreEdDsaPrivateSigningKey::from_ed25519_pem(
            if rotated { KEY_TWO } else { KEY_ONE },
            Some(JsonWebKeyId::new(
                if rotated { "two" } else { "one" }.into(),
            )),
        )
        .unwrap()
    }
}

async fn metadata(State(provider): State<Arc<Provider>>) -> Json<Value> {
    Json(
        json!({"issuer":provider.issuer,"authorization_endpoint":format!("{}/authorize",provider.issuer),"token_endpoint":format!("{}/token",provider.issuer),"jwks_uri":format!("{}/jwks",provider.issuer),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["EdDSA"]}),
    )
}

async fn keys(State(provider): State<Arc<Provider>>) -> Json<Value> {
    Json(json!({"keys":[provider.signing_key().as_verification_key()]}))
}

async fn authorize(
    State(provider): State<Arc<Provider>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Redirect {
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "crab-browser");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(params["scope"].split(' ').any(|scope| scope == "openid"));
    let code = openidconnect::CsrfToken::new_random().secret().clone();
    let mut target = Url::parse(&params["redirect_uri"]).unwrap();
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    provider.codes.lock().await.insert(code, params);
    Redirect::to(target.as_str())
}

async fn token(
    State(provider): State<Arc<Provider>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Json<Value> {
    let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let flow = provider.codes.lock().await.remove(&params["code"]).unwrap();
    assert_eq!(params["grant_type"], "authorization_code");
    assert_eq!(params["redirect_uri"], flow["redirect_uri"]);
    if provider.confidential.load(Ordering::SeqCst) {
        assert_eq!(
            headers[header::AUTHORIZATION],
            "Basic Y3JhYi1icm93c2VyOmZpeHR1cmUtb25seS1zZWNyZXQ="
        );
    } else {
        assert_eq!(params["client_id"], "crab-browser");
    }
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
        params["code_verifier"].clone(),
    ));
    assert_eq!(challenge.as_str(), flow["code_challenge"]);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mode = provider.mode.lock().await.clone();
    let mut claims = json!({"iss":provider.issuer,"aud":"crab-browser","exp":now+3600,"iat":now,"sub":"alice-id","preferred_username":"Alice","nonce":flow["nonce"]});
    match mode.as_str() {
        "member" => {
            claims["sub"] = json!("bob-id");
            claims["preferred_username"] = json!("Bob");
        }
        "nonce" => claims["nonce"] = json!("another-nonce"),
        "issuer" => claims["iss"] = json!("https://other.invalid"),
        "audience" => claims["aud"] = json!("other-client"),
        "authorized_party" => claims["azp"] = json!("other-client"),
        "expired" => claims["exp"] = json!(now - 1),
        "future" => claims["iat"] = json!(now + 600),
        "outsider" => claims["sub"] = json!("outsider-id"),
        _ => {}
    }
    let claims: CoreIdTokenClaims = serde_json::from_value(claims).unwrap();
    let key = if mode == "signature" {
        CoreEdDsaPrivateSigningKey::from_ed25519_pem(KEY_TWO, Some(JsonWebKeyId::new("one".into())))
            .unwrap()
    } else {
        provider.signing_key()
    };
    let signed = CoreIdToken::new(
        claims,
        &key,
        CoreJwsSigningAlgorithm::EdDsa,
        Some(&AccessToken::new("test-access".into())),
        None,
    )
    .unwrap();
    Json(
        json!({"access_token":if mode == "access_hash" {"substituted"} else {"test-access"},"token_type":"Bearer","id_token":signed}),
    )
}

async fn start_provider(port: u16) -> (Arc<Provider>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let provider = Arc::new(Provider {
        issuer: format!("http://{}", listener.local_addr().unwrap()),
        rotated: AtomicBool::new(false),
        confidential: AtomicBool::new(false),
        mode: Mutex::new(String::new()),
        codes: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(metadata))
        .route("/jwks", get(keys))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(Arc::clone(&provider));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (provider, task)
}

struct Harness {
    origin: String,
    http: reqwest::Client,
    provider: Arc<Provider>,
    server: Arc<Server>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn new(confidential: bool) -> Self {
        let (provider, provider_task) = start_provider(0).await;
        provider.confidential.store(confidential, Ordering::SeqCst);
        let secret_file = confidential.then(|| {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), "fixture-only-secret\r\n").unwrap();
            file
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("http://{address}");
        let auth = Authentication::new(crate::OidcConfig {
            issuer: openidconnect::IssuerUrl::new(provider.issuer.clone()).unwrap(),
            public_url: Url::parse(&origin).unwrap(),
            client_id: "crab-browser".into(),
            client_secret_file: secret_file.as_ref().map(|file| file.path().to_owned()),
        })
        .await
        .unwrap();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let repository = Repository {
            config: RepositoryConfig {
                owner: "team".into(),
                name: "private".into(),
                bucket: "test".into(),
                prefix: "test".into(),
                description: "Private project".into(),
                members: vec![
                    crate::RepositoryMember {
                        subject: "alice-id".into(),
                        access: crate::RepositoryAccess::Write,
                    },
                    crate::RepositoryMember {
                        subject: "bob-id".into(),
                        access: crate::RepositoryAccess::Read,
                    },
                ],
                protected_branches: vec![],
            },
            store: store.clone(),
            layout: StoreLayout::new(store, "test".into()),
            identity: RepositoryIdentity::new("test", "test", 1).unwrap(),
            pinned: Mutex::new(None),
            maintenance: Mutex::new(None),
        };
        let server = Arc::new(Server {
            repositories: BTreeMap::from([(("team".into(), "private".into()), repository)]),
            runtime: Arc::new(RemoteGitRuntime::default()),
            options: RepositoryOptions::default(),
            cursor_key: [7; 32],
            admission: Semaphore::new(16),
            git_admission: Arc::new(Semaphore::new(4)),
            app_admission: Semaphore::new(8),
            maintenance_admission: Arc::new(Semaphore::new(2)),
            cancellation: CancellationToken::new(),
            receives: tokio_util::task::TaskTracker::new(),
            port: address.port(),
            auth: Some(auth),
        });
        let app = router(Arc::clone(&server));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            provider,
            server,
            tasks: vec![task, provider_task],
        }
    }

    async fn start_login(&self) -> (String, String) {
        let response = self
            .http
            .get(format!(
                "{}/auth/login?return_to=%2Fteam%2Fprivate",
                self.origin
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = cookie_pair(&response, "crab_login");
        let response = self
            .http
            .get(response.headers()[header::LOCATION].to_str().unwrap())
            .send()
            .await
            .unwrap();
        (
            response.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .to_owned(),
            cookie,
        )
    }

    async fn login(&self) -> String {
        let (callback, cookie) = self.start_login().await;
        let response = self
            .http
            .get(callback)
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()[header::LOCATION], "/team/private");
        let full = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find(|value| value.to_str().unwrap().starts_with("crab_session="))
            .unwrap()
            .to_str()
            .unwrap();
        assert!(full.contains("HttpOnly; SameSite=Lax"));
        cookie_pair(&response, "crab_session")
    }

    async fn json(&self, path: &str, cookie: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.origin))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
    }

    async fn close(self) {
        self.server.cancellation.cancel();
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
        self.server.finish_maintenance().await.unwrap();
        self.server.runtime.shutdown().await;
    }
}

fn cookie_pair(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

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
        "refs", "commit", "commits", "tree", "file", "blob", "changes", "diff", "blame",
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
    let repo = &h.server.repositories[&("team".into(), "private".into())];
    crab_metadata::manifest_store::create_manifest(
        &repo.store,
        &repo.layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    let lease = super::maintenance_tests::commit_without_proof(repo).await;
    let before = crab_metadata::manifest_store::read_manifest(&repo.store, &repo.layout)
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
    assert!(repo.maintenance.lock().await.is_none());
    assert_eq!(
        before,
        crab_metadata::manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap()
    );
    assert_eq!(
        crab_metadata::ref_journal::list_active_transactions(&repo.store, &repo.layout)
            .await
            .unwrap()
            .len(),
        1
    );
    lease.release().await.unwrap();
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

#[path = "auth_tests/issues.rs"]
mod issues;

#[path = "auth_tests/git_tokens.rs"]
mod git_tokens;

#[path = "auth_tests/pulls.rs"]
mod pulls;
