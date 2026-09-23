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

struct UnavailablePeer;

impl crab_cell_runtime::peer::PeerRoundTrip for UnavailablePeer {
    fn send(
        &self,
        _target: crab_cell_runtime::CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
    }
}

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
    if provider.mode.lock().await.as_str() == "wrong-logout-key-operation" {
        let mut key = serde_json::to_value(provider.signing_key().as_verification_key()).unwrap();
        key["key_ops"] = json!(["encrypt"]);
        return Json(json!({"keys":[key]}));
    }
    if provider.mode.lock().await.as_str() == "ambiguous-logout-key" {
        return Json(
            json!({"keys":[provider.signing_key().as_verification_key(),provider.signing_key().as_verification_key()]}),
        );
    }
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

async fn github_authorize(
    State(provider): State<Arc<Provider>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Redirect {
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "crab-browser");
    assert_eq!(params["scope"], "read:user");
    assert_eq!(params["code_challenge_method"], "S256");
    let code = openidconnect::CsrfToken::new_random().secret().clone();
    let mut target = Url::parse(&params["redirect_uri"]).unwrap();
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    provider.codes.lock().await.insert(code, params);
    Redirect::to(target.as_str())
}

async fn github_token(State(provider): State<Arc<Provider>>, body: String) -> Json<Value> {
    let params: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let flow = provider.codes.lock().await.remove(&params["code"]).unwrap();
    assert_eq!(params["client_id"], "crab-browser");
    assert_eq!(params["redirect_uri"], flow["redirect_uri"]);
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
        params["code_verifier"].clone(),
    ));
    assert_eq!(challenge.as_str(), flow["code_challenge"]);
    Json(json!({"access_token":"github-access-token","token_type":"bearer"}))
}

async fn github_user(headers: axum::http::HeaderMap) -> Json<Value> {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer github-access-token");
    Json(json!({"id":4242,"login":"octocat","name":"Octo Cat"}))
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
        .route("/github/authorize", get(github_authorize))
        .route("/github/token", post(github_token))
        .route("/github/user", get(github_user))
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
    cell_dir: tempfile::TempDir,
}

impl Harness {
    async fn new(confidential: bool) -> Self {
        Self::new_with_auth(confidential, false).await
    }

    async fn new_github() -> Self {
        Self::new_with_auth(true, true).await
    }

    async fn new_with_auth(confidential: bool, github: bool) -> Self {
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
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let root = crate::storage_root::StorageRoot::memory(store.clone(), "");
        let auth_config = if github {
            crate::OidcConfig {
                provider: crate::AuthProvider::GitHub,
                issuer: openidconnect::IssuerUrl::new(provider.issuer.clone()).unwrap(),
                public_url: Url::parse(&origin).unwrap(),
                client_id: "crab-browser".into(),
                client_secret_file: secret_file.as_ref().map(|file| file.path().to_owned()),
                state_key_file: None,
                github: Some(crate::GitHubConfig {
                    authorize_url: format!("{}/github/authorize", provider.issuer),
                    token_url: format!("{}/github/token", provider.issuer),
                    api_url: format!("{}/github/", provider.issuer),
                }),
            }
        } else {
            crate::OidcConfig {
                provider: crate::AuthProvider::Oidc,
                issuer: openidconnect::IssuerUrl::new(provider.issuer.clone()).unwrap(),
                public_url: Url::parse(&origin).unwrap(),
                client_id: "crab-browser".into(),
                client_secret_file: secret_file.as_ref().map(|file| file.path().to_owned()),
                state_key_file: None,
                github: None,
            }
        };
        let auth = Authentication::new_durable(auth_config, &root)
            .await
            .unwrap();
        let protected_branches = vec![crate::BranchProtection {
            branch: "main".into(),
            required_approvals: 1,
            required_checks: vec!["ci/test".into()],
        }];
        let admission_store = store.clone();
        let mut repository = Repository {
            id: uuid::Uuid::from_bytes([1; 16]),
            config: RepositoryConfig {
                owner: "team".into(),
                name: "private".into(),
                bucket: "test".into(),
                prefix: "test".into(),
                default_branch: "main".into(),
                description: "Private project".into(),
                members: vec![
                    crate::RepositoryMember {
                        subject: "alice-id".into(),
                        name: "Alice".into(),
                        access: crate::RepositoryAccess::Admin,
                    },
                    crate::RepositoryMember {
                        subject: "bob-id".into(),
                        name: "Bob".into(),
                        access: crate::RepositoryAccess::Read,
                    },
                ],
                protected_branches: protected_branches.clone(),
            },
            store: store.clone(),
            layout: StoreLayout::new(store, "test".into()),
            identity: RepositoryIdentity::new("test", "test", 1).unwrap(),
            pinned: Mutex::new(None),
            maintenance: Mutex::new(None),
        };
        crab_write::initialize::initialize_repository(
            &repository.store,
            &repository.layout,
            "refs/heads/main",
        )
        .await
        .unwrap();
        let catalog = CatalogStore::new(root);
        let record = catalog
            .adopt_repository(
                "team".into(),
                "private".into(),
                "test".into(),
                "Private project".into(),
                repository.config.members.clone(),
            )
            .await
            .unwrap();
        repository.id = record.id;
        let cell_identity = crab_cell_runtime::cell::application::ApplicationIdentity::new(
            crab_cell_runtime::TenantId::from_bytes([2; 16]),
            crab_cell_runtime::ApplicationId::from_bytes([3; 16]),
        );
        let cell_layout = crab_cell_runtime::ltx::CellStorageLayout::new(
            admission_store.clone(),
            object_store::path::Path::from("test-cells"),
            *cell_identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        crate::cells::bootstrap_release_at(
            &cell_layout,
            cell_identity,
            &registry,
            &format!("sha256:{}", "a".repeat(64)),
        )
        .await
        .unwrap();
        let cell_dir = tempfile::TempDir::new().unwrap();
        let application = crate::cells::compiled_application().unwrap();
        crate::cells::initialize_repository_at(
            &cell_layout,
            cell_identity,
            &registry,
            &application,
            cell_dir.path(),
            32 * 1024 * 1024 * 1024,
            "https://initializer.test:8081".into(),
            repository.id,
        )
        .await
        .unwrap();
        let cell_session = crab_cell_runtime::SessionId::from_bytes([4; 16]);
        let cell_runtime = crab_cell_runtime::CellRuntime::new(
            crab_cell_runtime::SqlWorkerPool::new(1, 16).unwrap(),
            16 * 1024 * 1024,
            cell_session,
        )
        .unwrap();
        let repository_cells = crate::cells::RepositoryCellRouter::new(
            cell_identity,
            cell_layout.clone(),
            Arc::clone(&registry),
            cell_runtime.clone(),
            crate::cells::RepositoryCellPeer::new(
                crab_cell_runtime::node::NodeDirectory::new(
                    cell_layout,
                    crab_cell_runtime::Digest::from_bytes([6; 32]),
                    crab_cell_runtime::Digest::from_bytes([7; 32]),
                    registry.release_digest(),
                ),
                Arc::new(crab_cell_runtime::peer::PeerSigner::new(
                    cell_session,
                    registry.release_digest(),
                    ed25519_dalek::SigningKey::from_bytes(&[5; 32]),
                )),
                Arc::new(UnavailablePeer),
                crab_cell_runtime::control::Owner {
                    session: cell_session,
                    endpoint: "https://server.test:8081".into(),
                },
            ),
            cell_dir.path().to_path_buf(),
        )
        .unwrap();
        let server = Arc::new(Server {
            repositories: BTreeMap::from([(("team".into(), "private".into()), repository)]).into(),
            runtime: Arc::new(RemoteGitRuntime::default()),
            cell_runtime,
            cell_node: None,
            repository_cells: Some(repository_cells),
            peer_receiver: None,
            follower_store: None,
            node_log_transport: None,
            options: RepositoryOptions::default(),
            cursor_key: [7; 32],
            admission: Semaphore::new(16),
            transfer_admission: crate::transfer_admission::TransferAdmission::new(
                admission_store,
                "test/.crab/http-server/v1/admission".into(),
                4,
            ),
            local_staging: crate::local_disk::LocalStaging::for_test(),
            app_admission: Semaphore::new(8),
            maintenance_admission: Arc::new(Semaphore::new(2)),
            cancellation: CancellationToken::new(),
            receives: tokio_util::task::TaskTracker::new(),
            auth: Some(auth),
            catalog: Some(catalog),
            git_import: None,
            catalog_healthy: AtomicBool::new(false),
            node_healthy: AtomicBool::new(false),
            scheduler_status: crate::cells::SchedulerStatus::new(
                crate::cells::unix_now_ms().unwrap(),
            )
            .unwrap(),
            cell_capacity: super::test_cell_capacity_report(),
            metrics: crate::metrics::Metrics::new().unwrap(),
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
            cell_dir,
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
        self.server.shutdown_runtimes().await.unwrap();
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
    crab_write::initialize::initialize_repository(&repo.store, &repo.layout, "refs/heads/main")
        .await
        .unwrap();
    let lease = super::maintenance_tests::commit_without_proof(&repo).await;
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

#[path = "auth_tests/labels.rs"]
mod labels;

#[path = "auth_tests/assignees.rs"]
mod assignees;

#[path = "auth_tests/branches.rs"]
mod branches;

#[path = "auth_tests/git_tokens.rs"]
mod git_tokens;

#[path = "auth_tests/members.rs"]
mod members;

#[path = "auth_tests/backchannel_logout.rs"]
mod backchannel_logout;

#[path = "auth_tests/pulls.rs"]
mod pulls;

#[path = "auth_tests/releases.rs"]
mod releases;
