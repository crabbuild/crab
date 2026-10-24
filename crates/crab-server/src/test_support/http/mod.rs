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
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};
use url::Url;

mod provider;
pub(crate) use provider::{KEY_ONE, KEY_TWO, Provider, start_provider};

pub(crate) struct Harness {
    pub(crate) origin: String,
    pub(crate) http: reqwest::Client,
    pub(crate) provider: Arc<Provider>,
    pub(crate) server: Arc<Server>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
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
        let auth = Authentication::new(auth_config, &root).await.unwrap();
        let protected_branches = vec![crate::BranchProtection {
            branch: "main".into(),
            required_approvals: 1,
            required_checks: vec!["ci/test".into()],
        }];
        let admission_store = store.clone();
        let mut repository = Repository::new(
            uuid::Uuid::from_bytes([1; 16]),
            RepositoryConfig {
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
            StoreLayout::new(store, "test".into()),
            RepositoryIdentity::new("test", "test", 1).unwrap(),
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
            server.catalog = Some(catalog);
            server
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
