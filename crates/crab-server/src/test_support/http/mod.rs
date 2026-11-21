pub(crate) mod browser;
pub(crate) mod organizations;
use crate::{
    RepositoryConfig,
    auth::Authentication,
    catalog::CatalogStore,
    repository::Repository,
    server::{Server, http::router},
};
use axum::http::{StatusCode, header};
use crab_remote_git::RepositoryIdentity;
use crab_storage::{Store, StoreLayout};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use url::Url;
use uuid::Uuid;

mod provider;
pub(crate) use provider::{KEY_ONE, KEY_TWO, Provider, start_provider};

pub(crate) const TEST_SCIM_TOKEN: &str = "crab-fixture-user-provisioning-token-only";

pub(crate) fn rustfs_store(fixture: &str) -> Store {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let endpoint = required("CRAB_HTTP_CELL_TEST_ENDPOINT");
    assert_eq!(Url::parse(&endpoint).unwrap().host_str(), Some("127.0.0.1"));
    let store = crab_storage::build_explicit_store(
        &required("CRAB_HTTP_CELL_TEST_BUCKET"),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        true,
    )
    .unwrap();
    let prefix = format!(
        "{}/{fixture}-{}",
        required("CRAB_HTTP_CELL_TEST_PREFIX"),
        Uuid::now_v7()
    );
    Store::new(Arc::new(object_store::prefix::PrefixStore::new(
        Arc::clone(store.inner()),
        prefix,
    )))
}

#[derive(Default)]
struct TestAuthFiles {
    state_key: Option<std::path::PathBuf>,
    scim_token: Option<std::path::PathBuf>,
}

type TestGitHubActions = Box<dyn FnOnce(Uuid) -> crate::api::GitHubActions + Send>;

pub(crate) struct Harness {
    pub(crate) origin: String,
    pub(crate) http: reqwest::Client,
    pub(crate) provider: Arc<Provider>,
    pub(crate) server: Arc<Server>,
    pub(crate) drop_next_import_reply: Arc<AtomicBool>,
    pub(crate) drop_next_fork_sync_reply: Arc<AtomicBool>,
    pub(crate) drop_next_merge_reply: Arc<AtomicBool>,
    pub(crate) drop_next_branch_update_reply: Arc<AtomicBool>,
    pub(crate) drop_next_suggestion_reply: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub(crate) async fn github_workflow_delivery(
    h: &Harness,
    url: &str,
    secret: &str,
    delivery: Uuid,
    signed: &Value,
    body: &Value,
) -> StatusCode {
    github_workflow_request(&h.http, url, secret, delivery, signed, body)
        .send()
        .await
        .unwrap()
        .status()
}

pub(crate) fn github_workflow_request(
    client: &reqwest::Client,
    url: &str,
    secret: &str,
    delivery: Uuid,
    signed: &Value,
    body: &Value,
) -> reqwest::RequestBuilder {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(&serde_json::to_vec(signed).unwrap());
    let signature = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
    client
        .post(url)
        .header("x-github-event", "workflow_run")
        .header("x-github-delivery", delivery.to_string())
        .header("x-hub-signature-256", format!("sha256={signature}"))
        .json(body)
}

impl Harness {
    pub(crate) async fn deliver_notifications(&self, owner: &str, name: &str) {
        let repo = self
            .server
            .repositories
            .get(&(owner.into(), name.into()))
            .unwrap();
        crate::server::mentions::drain_repository(&self.server, &repo)
            .await
            .unwrap();
    }

    pub(crate) async fn add_repository(&self, name: &str) {
        let primary = self
            .server
            .repositories
            .get(&("team".into(), "private".into()))
            .unwrap();
        let mut config = primary.config.clone();
        config.name = name.into();
        config.prefix = format!("test-{name}");
        let layout = StoreLayout::new(primary.store.clone(), config.prefix.clone());
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let catalog = self.server.catalog.as_ref().unwrap();
        let record = catalog
            .adopt_repository(
                config.owner.clone(),
                config.name.clone(),
                config.prefix.clone(),
                config.description.clone(),
                config.members.clone(),
            )
            .await
            .unwrap();
        self.server
            .cells()
            .unwrap()
            .repository(record.id)
            .unwrap()
            .client
            .initialize(record.id)
            .await
            .unwrap();
        catalog.mark_cell_ready(primary.id).await.unwrap();
        catalog.mark_cell_ready(record.id).await.unwrap();
        let repository = Repository::new(
            record.id,
            config.clone(),
            layout,
            RepositoryIdentity::new("test", config.prefix.as_str(), 1).unwrap(),
        );
        let mut repositories: BTreeMap<_, _> = self
            .server
            .repositories
            .values()
            .into_iter()
            .map(|repository| {
                (
                    (
                        repository.config.owner.clone(),
                        repository.config.name.clone(),
                    ),
                    repository,
                )
            })
            .collect();
        repositories.insert((config.owner, config.name), Arc::new(repository));
        let (document, _) = catalog.load().await.unwrap();
        self.server
            .repositories
            .replace(crate::repository::catalog::RepositoryIndex::new(
                document.version,
                repositories,
            ));
    }

    pub(crate) async fn initialize_directory(&self) {
        self.server
            .cells()
            .unwrap()
            .initialize(crate::cells::Address::Directory)
            .await
            .unwrap();
    }

    pub(crate) async fn assignee_id(&self, cookie: &str, name: &str) -> Value {
        let catalog = self.json("/api/repos/team/private/assignees", cookie).await;
        catalog["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == name)
            .unwrap()["id"]
            .clone()
    }

    pub(crate) async fn write(
        &self,
        method: reqwest::Method,
        path: &str,
        cookie: &str,
        body: Value,
    ) -> reqwest::Response {
        let session = self.json("/api/session", cookie).await;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.origin))
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, &self.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap());
        if let Some(repository) = self
            .server
            .repositories
            .get(&("team".into(), "private".into()))
        {
            request = request.header("x-crab-repository-id", repository.id.to_string());
        }
        request.json(&body).send().await.unwrap()
    }

    pub(crate) async fn new(confidential: bool) -> Self {
        let harness = Self::new_unprepared(confidential).await;
        harness.initialize_directory().await;
        harness
    }

    pub(crate) async fn new_unprepared(confidential: bool) -> Self {
        Self::new_with_auth(confidential, false).await
    }

    pub(crate) async fn new_github() -> Self {
        Self::new_with_auth(true, true).await
    }

    async fn new_with_auth(confidential: bool, github: bool) -> Self {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        Self::new_with_store(confidential, github, store).await
    }

    pub(crate) async fn new_with_store(confidential: bool, github: bool, store: Store) -> Self {
        Self::new_with_store_and_prefix(confidential, github, store, "").await
    }

    pub(crate) async fn new_with_store_and_prefix(
        confidential: bool,
        github: bool,
        store: Store,
        catalog_prefix: &str,
    ) -> Self {
        Self::configured(
            confidential,
            github,
            store,
            catalog_prefix,
            TestAuthFiles::default(),
            None,
            None,
            None,
        )
        .await
    }

    pub(crate) async fn with_ci_key(catalog_prefix: &str) -> Self {
        Self::with_ci_dispatch(catalog_prefix, None).await
    }

    pub(crate) async fn with_ci_dispatch(
        catalog_prefix: &str,
        github_actions: Option<TestGitHubActions>,
    ) -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), [42_u8; 32]).unwrap();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let harness = Self::configured(
            false,
            false,
            store,
            catalog_prefix,
            TestAuthFiles {
                state_key: Some(file.path().to_owned()),
                ..Default::default()
            },
            github_actions,
            None,
            None,
        )
        .await;
        harness.initialize_directory().await;
        harness
    }

    pub(crate) async fn with_notification_email(github: bool, endpoint: Url) -> Self {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let harness = Self::configured(
            github,
            github,
            store,
            "",
            TestAuthFiles::default(),
            None,
            Some(endpoint),
            None,
        )
        .await;
        harness.initialize_directory().await;
        harness
    }

    pub(crate) async fn with_webhook_receivers(
        endpoints: impl IntoIterator<Item = Url>,
        repository: Option<Uuid>,
    ) -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), [42_u8; 32]).unwrap();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let catalog_prefix = if repository.is_some() {
            "webhook-fork"
        } else {
            ""
        };
        let harness = Self::configured(
            false,
            false,
            store,
            catalog_prefix,
            TestAuthFiles {
                state_key: Some(file.path().to_owned()),
                ..Default::default()
            },
            None,
            None,
            Some((endpoints.into_iter().collect(), repository)),
        )
        .await;
        harness.initialize_directory().await;
        harness
    }

    pub(crate) async fn scim() -> Self {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), format!("{TEST_SCIM_TOKEN}\n")).unwrap();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let harness = Self::configured(
            false,
            false,
            store,
            "",
            TestAuthFiles {
                scim_token: Some(file.path().to_owned()),
                ..Default::default()
            },
            None,
            None,
            None,
        )
        .await;
        harness.initialize_directory().await;
        harness
    }

    async fn configured(
        confidential: bool,
        github: bool,
        store: Store,
        catalog_prefix: &str,
        auth_files: TestAuthFiles,
        github_actions: Option<TestGitHubActions>,
        email_endpoint: Option<Url>,
        webhook_endpoints: Option<(Vec<Url>, Option<Uuid>)>,
    ) -> Self {
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
        let root = crate::storage_root::StorageRoot::memory(store.clone(), catalog_prefix);
        let auth_config = if github {
            crate::OidcConfig {
                provider: crate::AuthProvider::GitHub,
                issuer: openidconnect::IssuerUrl::new(provider.issuer.clone()).unwrap(),
                public_url: Url::parse(&origin).unwrap(),
                client_id: "crab-browser".into(),
                client_secret_file: secret_file.as_ref().map(|file| file.path().to_owned()),
                state_key_file: auth_files.state_key,
                scim_token_file: None,
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
                state_key_file: auth_files.state_key,
                scim_token_file: auth_files.scim_token,
                github: None,
            }
        };
        let auth = Authentication::new(auth_config, &root).await.unwrap();
        let protected_branches = vec![crate::BranchProtection {
            branch: "main".into(),
            required_approvals: 1,
            required_code_owner_reviews: false,
            require_conversation_resolution: false,
            require_up_to_date: false,
            require_merge_queue: false,
            require_linear_history: false,
            required_checks: vec!["ci/test".into()],
            required_integrations: vec![],
        }];
        let admission_store = store.clone();
        // Most fixtures place their seed repo at the bucket root. Exercise
        // catalog materialization and native Git with a deployed-style root.
        let layout = if catalog_prefix.is_empty() {
            StoreLayout::new(store, "test".into())
        } else {
            root.repository_layout(root.repository_prefix("test").unwrap())
        };
        let prefix = layout.repo_prefix().to_owned();
        let mut repository = Repository::new(
            uuid::Uuid::from_bytes([1; 16]),
            RepositoryConfig {
                groups: Vec::new(),
                owner: "team".into(),
                name: "private".into(),
                bucket: "test".into(),
                prefix: prefix.clone(),
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
            layout,
            RepositoryIdentity::new("test", prefix.as_str(), 1).unwrap(),
        );
        crab_write::capsule_protocol::initialize(
            &repository.layout,
            &"1".repeat(64),
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
        let cells = crate::cells::runtime::TestRuntime::start(address).await;
        cells
            .client
            .repository(repository.id)
            .unwrap()
            .client
            .initialize(repository.id)
            .await
            .unwrap();
        let server = Arc::new({
            let mut server = Server::for_test(
                BTreeMap::from([(("team".into(), "private".into()), repository)]).into(),
                crate::transfer_admission::TransferAdmission::new(
                    admission_store,
                    "test/.crab/http-server/v1/admission".into(),
                    4,
                ),
            );
            server.cells = Some(cells.client.clone());
            server.cell_test_runtime = Some(cells);
            server.cursor_key = [7; 32];
            server.auth = Some(auth);
            server.github_actions = github_actions.map(|configure| configure(record.id));
            server.webhook_sender = webhook_endpoints.map(|(urls, repository)| {
                let destinations: Vec<_> = urls
                    .into_iter()
                    .map(|url| crate::WebhookDestination {
                        repository: repository.unwrap_or(record.id),
                        url,
                    })
                    .collect();
                crate::server::webhooks::Sender::new(&destinations).unwrap()
            });
            server.email = email_endpoint.map(|endpoint| {
                crate::server::email::Sender::with_endpoint(
                    "crab@example.test".into(),
                    "fixture-mail-key".into(),
                    &Url::parse(&origin).unwrap(),
                    endpoint,
                )
                .unwrap()
            });
            server.catalog = Some(catalog);
            server.git_import = Some(crate::git_import::ImportContext::for_test(address));
            server
        });
        let drop_next_import_reply = Arc::new(AtomicBool::new(false));
        let drop_next_fork_sync_reply = Arc::new(AtomicBool::new(false));
        let drop_next_merge_reply = Arc::new(AtomicBool::new(false));
        let drop_next_branch_update_reply = Arc::new(AtomicBool::new(false));
        let drop_next_suggestion_reply = Arc::new(AtomicBool::new(false));
        let app = router(Arc::clone(&server)).layer(axum::middleware::from_fn({
            let drop_import = Arc::clone(&drop_next_import_reply);
            let drop_sync = Arc::clone(&drop_next_fork_sync_reply);
            let drop_merge = Arc::clone(&drop_next_merge_reply);
            let drop_branch_update = Arc::clone(&drop_next_branch_update_reply);
            let drop_suggestion = Arc::clone(&drop_next_suggestion_reply);
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let drop_import = Arc::clone(&drop_import);
                let drop_sync = Arc::clone(&drop_sync);
                let drop_merge = Arc::clone(&drop_merge);
                let drop_branch_update = Arc::clone(&drop_branch_update);
                let drop_suggestion = Arc::clone(&drop_suggestion);
                async move {
                    let is_import = request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/git-receive-pack")
                        && request.headers().contains_key("x-crab-internal-import");
                    let is_sync = request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/fork-sync");
                    let is_merge = request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/merge");
                    let is_branch_update = request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/update-branch");
                    let is_suggestion = request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/suggestions");
                    let mut response = next.run(request).await;
                    if response.status().is_success()
                        && (is_branch_update && drop_branch_update.swap(false, Ordering::SeqCst)
                            || is_merge && drop_merge.swap(false, Ordering::SeqCst)
                            || is_suggestion && drop_suggestion.swap(false, Ordering::SeqCst))
                    {
                        // Deliver the headers so Chromium cannot retry an
                        // interrupted connection before the app sees the loss.
                        response.headers_mut().remove(header::CONTENT_LENGTH);
                        *response.body_mut() = axum::body::Body::empty();
                    }
                    if response.status().is_success()
                        && (is_import && drop_import.swap(false, Ordering::SeqCst)
                            || is_sync && drop_sync.swap(false, Ordering::SeqCst))
                    {
                        response.headers_mut().remove(header::CONTENT_LENGTH);
                        *response.body_mut() =
                            axum::body::Body::from_stream(futures_util::stream::once(async {
                                Err::<bytes::Bytes, _>(std::io::Error::other(
                                    "test interrupted Git response",
                                ))
                            }));
                    }
                    response
                }
            }
        }));
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
            drop_next_import_reply,
            drop_next_fork_sync_reply,
            drop_next_merge_reply,
            drop_next_branch_update_reply,
            drop_next_suggestion_reply,
            tasks: vec![task, provider_task],
        }
    }

    pub(crate) async fn start_login(&self) -> (String, String) {
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

    pub(crate) async fn login(&self) -> String {
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

    pub(crate) async fn json(&self, path: &str, cookie: &str) -> Value {
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

    pub(crate) async fn close(self) {
        self.server.cancellation.cancel();
        for task in self.tasks {
            task.abort();
            let _ = task.await;
        }
        self.server.finish_maintenance().await.unwrap();
        self.server.shutdown_runtimes().await.unwrap();
    }
}

pub(crate) fn cookie_pair(response: &reqwest::Response, name: &str) -> String {
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
