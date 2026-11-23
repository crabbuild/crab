use std::{io::Read, time::Duration};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use url::Url;
use uuid::Uuid;

use super::stored::{Connection, dispatches};
use crate::GitHubActionsBinding;

const API_VERSION: &str = "2026-03-10";
const MAX_RESPONSE: usize = 2 * 1024 * 1024;

pub(crate) struct GitHubActions {
    app_id: u64,
    key: EncodingKey,
    webhook_secret: Vec<u8>,
    http: reqwest::Client,
    api: Url,
    actor: tokio::sync::OnceCell<String>,
    bindings: Vec<GitHubActionsBinding>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("GitHub Actions credential was rejected")]
    Rejected,
    #[error("GitHub Actions request was not confirmed")]
    Unconfirmed,
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

#[derive(Deserialize)]
struct GitHubApp {
    id: u64,
    slug: String,
}

#[derive(Deserialize)]
struct GitHubRepository {
    id: u64,
}

#[derive(Deserialize)]
struct Run {
    workflow_run_id: u64,
}

#[derive(Deserialize)]
struct Runs {
    total_count: usize,
    workflow_runs: Vec<ExistingRun>,
}

#[derive(Deserialize)]
struct ExistingRun {
    id: u64,
    display_title: String,
    workflow_id: u64,
    head_branch: Option<String>,
    event: String,
}

impl GitHubActions {
    pub(crate) fn load(config: &crate::GitHubActionsConfig) -> crate::Result<Self> {
        let file = std::fs::File::open(&config.private_key_file)?;
        let mut pem = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut pem)?;
        if pem.len() > 16 * 1024 {
            return Err(crate::Error::Config(
                "GitHub App key exceeds its size limit",
            ));
        }
        let key = EncodingKey::from_rsa_pem(&pem).map_err(|source| crate::Error::Identity {
            source: Box::new(source),
        })?;
        let api =
            Url::parse("https://api.github.com/").map_err(|source| crate::Error::Identity {
                source: Box::new(source),
            })?;
        // Bound secret loading and accept the line ending used by mounted files.
        let mut webhook_secret = Vec::new();
        std::fs::File::open(&config.webhook_secret_file)?
            .take(515)
            .read_to_end(&mut webhook_secret)?;
        if webhook_secret.ends_with(b"\r\n") {
            webhook_secret.truncate(webhook_secret.len() - 2);
        } else if webhook_secret.ends_with(b"\n") {
            webhook_secret.pop();
        }
        Self::new(
            config.app_id,
            key,
            webhook_secret,
            api,
            config.bindings.clone(),
        )
    }

    pub(crate) fn new(
        app_id: u64,
        key: EncodingKey,
        webhook_secret: Vec<u8>,
        api: Url,
        bindings: Vec<GitHubActionsBinding>,
    ) -> crate::Result<Self> {
        if !(32..=512).contains(&webhook_secret.len())
            || !webhook_secret.iter().all(u8::is_ascii_graphic)
        {
            return Err(crate::Error::Config(
                "GitHub App webhook secret must contain 32–512 visible ASCII characters",
            ));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .user_agent("crab-server")
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|source| crate::Error::Identity {
                source: Box::new(source),
            })?;
        Ok(Self {
            app_id,
            key,
            webhook_secret,
            http,
            api,
            actor: tokio::sync::OnceCell::new(),
            bindings,
        })
    }

    pub(super) fn webhook_secret(&self) -> &[u8] {
        &self.webhook_secret
    }

    pub(super) fn webhook_target(
        &self,
        installation: u64,
        repository: u64,
        workflow: u64,
    ) -> Option<Uuid> {
        self.bindings
            .iter()
            .find(|binding| {
                binding.installation == installation
                    && binding.github_repository == repository
                    && binding.workflow == workflow
            })
            .map(|binding| binding.crab_repository)
    }

    pub(super) fn binds(&self, repository: Uuid, github_repository: u64, workflow: u64) -> bool {
        self.bindings.iter().any(|binding| {
            binding.crab_repository == repository
                && binding.github_repository == github_repository
                && binding.workflow == workflow
        })
    }

    pub(super) fn permits(
        &self,
        repository: Uuid,
        connection: &Connection,
        config: &dispatches::Configuration,
    ) -> bool {
        self.bindings.iter().any(|binding| {
            binding.crab_repository == repository
                && binding.github_repository == connection.github_repository
                && binding.workflow == connection.workflow
                && binding.installation == config.installation
                && binding.branch == config.branch
        })
    }

    fn jwt(&self) -> Result<String, Error> {
        let millis = crate::cells::unix_now_ms().map_err(|_| Error::Unconfirmed)?;
        let now = u64::try_from(millis / 1000).map_err(|_| Error::Unconfirmed)?;
        jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iss: self.app_id.to_string(),
                iat: now.saturating_sub(60),
                exp: now.saturating_add(480),
            },
            &self.key,
        )
        .map_err(|_| Error::Rejected)
    }

    pub(super) async fn actor(&self) -> Result<&str, Error> {
        self.actor
            .get_or_try_init(|| async {
                let app: GitHubApp = self
                    .send(self.request(reqwest::Method::GET, "app", &self.jwt()?)?)
                    .await?;
                if app.id != self.app_id
                    || app.slug.is_empty()
                    || !app
                        .slug
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
                {
                    return Err(Error::Rejected);
                }
                Ok(format!("{}[bot]", app.slug))
            })
            .await
            .map(String::as_str)
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
    ) -> Result<reqwest::RequestBuilder, Error> {
        let url = self.api.join(path).map_err(|_| Error::Rejected)?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION))
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, Error> {
        let mut response = request.send().await.map_err(|_| Error::Unconfirmed)?;
        let status = response.status();
        if status.is_client_error()
            && status != reqwest::StatusCode::TOO_MANY_REQUESTS
            && status != reqwest::StatusCode::REQUEST_TIMEOUT
            && !(status == reqwest::StatusCode::FORBIDDEN
                && (response.headers().contains_key("retry-after")
                    || response
                        .headers()
                        .get("x-ratelimit-remaining")
                        .is_some_and(|value| value == "0")))
        {
            return Err(Error::Rejected);
        }
        if !status.is_success() {
            return Err(Error::Unconfirmed);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unconfirmed)? {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Error::Unconfirmed);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| Error::Unconfirmed)
    }

    pub(super) async fn launch(
        &self,
        connection: &Connection,
        config: &dispatches::Configuration,
        dispatch: &dispatches::Dispatch,
        oid: &str,
        source: &str,
    ) -> Result<u64, Error> {
        let token: Token = self
            .send(
                self.request(
                    reqwest::Method::POST,
                    &format!("app/installations/{}/access_tokens", config.installation),
                    &self.jwt()?,
                )?
                .json(&json!({
                    "repository_ids": [connection.github_repository],
                    "permissions": {"actions": "write"},
                })),
            )
            .await?;
        let slug = config.repository.to_ascii_lowercase();
        // A GitHub repository name can change or be reused. The connection's
        // numeric provider ID must still identify the destination of the POST.
        let repository: GitHubRepository = self
            .send(self.request(reqwest::Method::GET, &format!("repos/{slug}"), &token.token)?)
            .await?;
        if repository.id != connection.github_repository {
            return Err(Error::Rejected);
        }
        let workflow = format!("repos/{slug}/actions/workflows/{}", connection.workflow);
        let actor = self.actor().await?;
        let expected_title = format!("Crab queue {oid} {}", dispatch.id);
        if dispatch.attempts > 1 {
            let runs: Runs = self
                .send(
                    self.request(
                        reqwest::Method::GET,
                        &format!("{workflow}/runs"),
                        &token.token,
                    )?
                    .query(&[
                        ("event", "workflow_dispatch"),
                        ("branch", config.branch.as_str()),
                        ("actor", actor),
                        ("per_page", "100"),
                    ]),
                )
                .await?;
            if let Some(run) = runs
                .workflow_runs
                .iter()
                .filter(|run| {
                    run.display_title == expected_title
                        && run.event == "workflow_dispatch"
                        && run.workflow_id == connection.workflow
                        && run.head_branch.as_deref() == Some(config.branch.as_str())
                })
                .map(|run| run.id)
                .min()
            {
                return validate_run(run);
            }
            if runs.total_count > runs.workflow_runs.len() {
                return Err(Error::Unconfirmed);
            }
        }
        let run: Run = self
            .send(self.request(
                reqwest::Method::POST, &format!("{workflow}/dispatches"), &token.token,
            )?.json(&json!({
                "ref": config.branch,
                "inputs": {"crab_commit": oid, "crab_dispatch": dispatch.id, "crab_source": source},
            })))
            .await?;
        validate_run(run.workflow_run_id)
    }
}

fn validate_run(id: u64) -> Result<u64, Error> {
    crate::app::number(id).map_err(|_| Error::Unconfirmed)
}
