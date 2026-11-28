//! In-process dispatch of GitHub API operations to the native API.
//!
//! Each operation passes the shared security boundary exactly like a browser
//! request, so repository admission, current grants, archive state, and Cell
//! transactions keep one implementation. This adapter only translates.

use axum::{
    Router,
    body::Body,
    http::{HeaderValue, Method, Request, StatusCode, header},
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

use super::Delegated;
use crate::auth::Principal;

/// Native JSON responses stay far below this; it only bounds a faulty one.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// The native routes behind the security boundary, without the asset fallback.
#[derive(Clone)]
pub(crate) struct Dispatch(pub(crate) Router);

/// A native API failure, kept separate from transport errors so each
/// GitHub surface can report it in its own error format.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: message.into(),
        }
    }
}

pub(crate) type Outcome<T> = Result<T, Failure>;

#[derive(Clone)]
pub(crate) struct Native {
    router: Router,
    principal: Principal,
    host: Option<HeaderValue>,
}

impl Native {
    pub(crate) fn new(
        dispatch: &Dispatch,
        principal: Principal,
        host: Option<HeaderValue>,
    ) -> Self {
        Self {
            router: dispatch.0.clone(),
            principal,
            host,
        }
    }

    pub(crate) fn principal(&self) -> &Principal {
        &self.principal
    }

    pub(crate) async fn get(&self, path: &str) -> Outcome<Value> {
        self.send(Method::GET, path, None, None).await
    }

    /// Reads a raw native response body, such as file bytes.
    pub(crate) async fn bytes(&self, path: &str) -> Outcome<bytes::Bytes> {
        let (status, body) = self.exchange(Method::GET, path, None, None).await?;
        if status.is_success() {
            return Ok(body);
        }
        Err(failure(status, &body))
    }

    /// Sends one native request. Mutations bind to `repository`, the same
    /// UUID guard the browser sends, so a reused address cannot be retargeted.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        repository: Option<Uuid>,
        body: Option<&Value>,
    ) -> Outcome<Value> {
        let (status, bytes) = self.exchange(method, path, repository, body).await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        if status.is_success() {
            return Ok(value);
        }
        Err(failure(status, &bytes))
    }

    async fn exchange(
        &self,
        method: Method,
        path: &str,
        repository: Option<Uuid>,
        body: Option<&Value>,
    ) -> Outcome<(StatusCode, bytes::Bytes)> {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(host) = &self.host {
            request = request.header(header::HOST, host);
        }
        if let Some(repository) = repository {
            request = request.header("x-crab-repository-id", repository.to_string());
        }
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(
                    serde_json::to_vec(body)
                        .map_err(|_| Failure::invalid("Invalid request body"))?,
                )
            }
            None => Body::empty(),
        };
        let mut request = request
            .body(body)
            .map_err(|_| Failure::invalid("Invalid request"))?;
        request
            .extensions_mut()
            .insert(Delegated(self.principal.clone()));
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .unwrap_or_else(|never| match never {});
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
            .await
            .map_err(|_| Failure {
                status: StatusCode::BAD_GATEWAY,
                message: "The repository service response could not be read".into(),
            })?;
        Ok((status, bytes))
    }
}

fn failure(status: StatusCode, body: &[u8]) -> Failure {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    Failure {
        status,
        message: value["error"]["message"]
            .as_str()
            .map_or_else(|| default_message(status).to_owned(), str::to_owned),
    }
}

fn default_message(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "Not Found",
        StatusCode::UNAUTHORIZED => "Requires authentication",
        StatusCode::FORBIDDEN => "Resource not accessible by personal access token",
        StatusCode::CONFLICT => "Conflict",
        _ => "The request could not be completed",
    }
}

/// Encodes query parameters for a native path.
pub(crate) fn query(pairs: &[(&str, &str)]) -> String {
    let mut encoded = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        encoded.append_pair(name, value);
    }
    encoded.finish()
}
