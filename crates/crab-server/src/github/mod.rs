//! GitHub-compatible REST (`/api/v3`) and GraphQL (`/api/graphql`) surfaces.
//!
//! GitHub clients such as `gh` address this server as a GitHub Enterprise
//! host. Every operation is translated into native API requests that pass
//! the shared security boundary in process (see `native`), so repository
//! policy stays with its native owners. Personal access tokens authenticate
//! this surface; cookies never do, so it needs no CSRF proof.

mod graphql;
mod ids;
mod native;
mod objects;
mod rest;

#[cfg(test)]
mod tests;

use std::sync::{Arc, LazyLock};

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub(crate) use native::Dispatch;
use native::{Failure, Native, Outcome};

use crate::{
    auth::Principal,
    cells::{Address, directory::IdentityKey},
    server::Server,
};

/// The GitHub Enterprise Server release whose API this surface follows.
/// GitHub CLI gates features on it: Projects (classic) is absent from 3.17,
/// and 3.18 starts the advanced issue-search syntax, which Crab does not parse.
pub(crate) const COMPATIBLE_VERSION: &str = "3.17.0";

const DOCUMENTATION: &str = "https://docs.github.com/rest";
const MAX_GRAPHQL_BODY_BYTES: usize = 512 * 1024;

static SCHEMA: LazyLock<Result<graphql::Schema, String>> = LazyLock::new(|| {
    graphql::Schema::parse(include_str!("schema.graphql")).map_err(|error| error.to_string())
});

/// Whether `path` belongs to the GitHub-compatible API surface.
pub(crate) fn is_api_path(path: &str) -> bool {
    path == "/api/graphql" || path == "/api/v3" || path.starts_with("/api/v3/")
}

/// A principal authenticated by an outer GitHub API request, carried into the
/// native router in process. Extensions never cross the network, so a client
/// cannot present one.
#[derive(Clone)]
pub(crate) struct Delegated(pub(crate) Principal);

/// GitHub's error body for a failed operation. Every 404 reads the same,
/// so a response never confirms what its caller may not see.
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let message = if self.status == StatusCode::NOT_FOUND {
            "Not Found"
        } else {
            self.message.as_str()
        };
        error(self.status, message)
    }
}

pub(crate) fn bad_credentials() -> Response {
    error(StatusCode::UNAUTHORIZED, "Bad credentials")
}

pub(crate) fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "message": message,
            "documentation_url": DOCUMENTATION,
            "status": status.as_u16().to_string(),
        })),
    )
        .into_response()
}

pub(crate) fn routes(dispatch: Dispatch) -> Router<Arc<Server>> {
    rest::routes()
        .route(
            "/api/graphql",
            post(graphql).layer(DefaultBodyLimit::max(MAX_GRAPHQL_BODY_BYTES)),
        )
        .layer(middleware::from_fn(github_headers))
        .layer(Extension(dispatch))
}

pub(crate) fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Headers GitHub clients read on every response. Crab enforces no request
/// quota for tokens, so the rate-limit headers report an unused allowance.
async fn github_headers(request: Request, next: Next) -> Response {
    let scopes = request
        .extensions()
        .get::<Principal>()
        .and_then(Principal::personal_token)
        .map(|token| token.scope_header());
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let reset = now_seconds() + 3600;
    for (name, value) in [
        ("x-github-media-type", "github.v3; format=json".to_owned()),
        ("x-github-enterprise-version", COMPATIBLE_VERSION.to_owned()),
        ("x-github-api-version-selected", "2022-11-28".to_owned()),
        ("x-ratelimit-limit", "5000".to_owned()),
        ("x-ratelimit-remaining", "5000".to_owned()),
        ("x-ratelimit-used", "0".to_owned()),
        ("x-ratelimit-reset", reset.to_string()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    if let Some(scopes) = scopes.and_then(|scopes| HeaderValue::from_str(&scopes).ok()) {
        headers.insert("x-oauth-scopes", scopes);
    }
    response
}

/// The viewer of one GitHub API request.
#[derive(Clone)]
pub(crate) struct Viewer {
    pub(crate) login: String,
    pub(crate) name: String,
    pub(crate) actor: Option<Uuid>,
}

/// Per-request state shared by REST handlers and GraphQL resolvers.
pub(crate) struct Context {
    pub(crate) server: Arc<Server>,
    pub(crate) native: Native,
    pub(crate) origin: String,
    viewer: tokio::sync::OnceCell<Viewer>,
}

impl Context {
    pub(crate) fn new(
        server: Arc<Server>,
        principal: Principal,
        dispatch: &Dispatch,
        headers: &HeaderMap,
    ) -> Self {
        let host = headers.get(header::HOST).cloned();
        let origin = match &server.auth {
            Some(auth) => auth.origin(),
            None => format!(
                "http://{}",
                host.as_ref()
                    .and_then(|host| host.to_str().ok())
                    .unwrap_or("localhost")
            ),
        };
        Self {
            native: Native::new(dispatch, principal, host),
            server,
            origin,
            viewer: tokio::sync::OnceCell::new(),
        }
    }

    pub(crate) fn principal(&self) -> &Principal {
        self.native.principal()
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}/{}", self.origin, path.trim_start_matches('/'))
    }

    /// The signed-in account behind this request. Logins follow the personal
    /// repository namespace: the Directory username, else the derived one.
    pub(crate) async fn viewer(&self) -> Outcome<&Viewer> {
        self.viewer
            .get_or_try_init(|| async {
                let principal = self.principal();
                if principal.is_local() {
                    return Ok(Viewer {
                        login: crate::catalog::LOCAL_NAMESPACE.into(),
                        name: "Local operator".into(),
                        actor: None,
                    });
                }
                let identity = principal
                    .identity()
                    .filter(|_| principal.is_user())
                    .ok_or_else(unauthenticated)?;
                let key = IdentityKey {
                    issuer: identity.issuer.clone(),
                    subject: identity.subject.clone(),
                };
                let cells = self.server.cells().ok_or_else(unavailable)?;
                let profile = cells
                    .query::<crate::cells::directory::GetOwnProfile>(
                        Address::Directory,
                        None,
                        key.clone(),
                    )
                    .await
                    .map_err(|_| unavailable())?
                    .output;
                let actor = match &profile {
                    Some(profile) => Some(profile.id),
                    None => crate::repository::access::actor_id(&self.server, key.clone())
                        .await
                        .map_err(|_| unavailable())?,
                };
                let login = profile
                    .as_ref()
                    .map(|profile| profile.handle.clone())
                    .filter(|handle| crate::catalog::valid_account_namespace(handle))
                    .unwrap_or_else(|| {
                        crate::catalog::personal_namespace(&key.issuer, &key.subject)
                    });
                Ok(Viewer {
                    login,
                    name: profile.map_or(identity.name, |profile| profile.name),
                    actor,
                })
            })
            .await
    }
}

pub(crate) fn unauthenticated() -> Failure {
    Failure {
        status: StatusCode::UNAUTHORIZED,
        message: "Requires authentication".into(),
    }
}

pub(crate) fn unavailable() -> Failure {
    Failure {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "The repository service is unavailable. Retry shortly.".into(),
    }
}

#[derive(serde::Deserialize)]
struct GraphqlBody {
    query: String,
    #[serde(default, rename = "operationName")]
    operation_name: Option<String>,
    #[serde(default)]
    variables: Option<Map<String, Value>>,
}

async fn graphql(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(dispatch): Extension<Dispatch>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // GitHub's GraphQL API serves authenticated requests only.
    if !principal.authenticated() {
        return error(
            StatusCode::UNAUTHORIZED,
            "This endpoint requires you to be authenticated.",
        );
    }
    let schema = match &*SCHEMA {
        Ok(schema) => schema,
        Err(message) => {
            tracing::error!(error = %message, "GitHub GraphQL schema failed to parse");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "GraphQL is unavailable");
        }
    };
    let Ok(body) = serde_json::from_slice::<GraphqlBody>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"errors":[{"message":"Problems parsing JSON"}]})),
        )
            .into_response();
    };
    let context = Context::new(server, principal, &dispatch, &headers);
    let request = graphql::Request {
        query: body.query,
        operation_name: body.operation_name.filter(|name| !name.is_empty()),
        variables: body.variables.unwrap_or_default(),
    };
    Json(
        graphql::execute(
            schema,
            &context,
            &objects::Query,
            &objects::Mutation,
            &request,
        )
        .await,
    )
    .into_response()
}
