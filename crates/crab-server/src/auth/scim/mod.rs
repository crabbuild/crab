use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{auth::Authentication, server::Server};

mod discovery;
mod profile;
mod store;
use profile::{Patch, Profile, UserInput, valid_subject};
use store::{Directory, User};

const ROOT: &str = "/scim/v2";
const USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
type Result<T> = std::result::Result<T, Error>;

pub(crate) fn machine_path(path: &str) -> bool {
    path == ROOT
        || path
            .strip_prefix(ROOT)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    let scim = Router::new()
        .route("/Users", post(create).get(list))
        .route(
            "/Users/{id}",
            get(get_user).put(replace).patch(patch).delete(delete),
        )
        .merge(discovery::routes())
        .fallback(|| async { Error::not_found() })
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn_with_state(server, authenticate));
    Router::new().nest(ROOT, scim)
}

async fn authenticate(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let Some(auth) = server.auth.as_ref() else {
        return Error::not_found().into_response();
    };
    let Some(key) = auth.scim_key else {
        return Error::not_found().into_response();
    };
    let mut authorization = request.headers().get_all(header::AUTHORIZATION).iter();
    let token = authorization
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        });
    if authorization.next().is_some()
        || token.is_none_or(|token| blake3::hash(token.as_bytes()) != key)
    {
        return (
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Error(
                StatusCode::UNAUTHORIZED,
                None,
                "Valid SCIM credentials are required",
            ),
        )
            .into_response();
    }
    let Ok(_permit) = auth.scim_admission.try_acquire() else {
        return Error::busy().into_response();
    };
    next.run(request).await
}

#[derive(Debug)]
struct Error(StatusCode, Option<&'static str>, &'static str);

impl Error {
    fn not_found() -> Self {
        Self(StatusCode::NOT_FOUND, None, "SCIM resource not found")
    }

    fn invalid(kind: &'static str, detail: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, Some(kind), detail)
    }

    fn conflict(detail: &'static str) -> Self {
        Self(StatusCode::CONFLICT, Some("uniqueness"), detail)
    }

    fn busy() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            None,
            "SCIM directory unavailable. Retry this request.",
        )
    }

    fn unavailable(error: impl std::fmt::Display) -> Self {
        tracing::warn!(%error, "SCIM persistence operation failed");
        Self::busy()
    }

    fn capacity() -> Self {
        Self(
            StatusCode::INSUFFICIENT_STORAGE,
            None,
            "SCIM directory capacity reached",
        )
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let Self(status, kind, detail) = self;
        let mut value = json!({"schemas": [ERROR], "status": status.as_str(), "detail": detail});
        if let Some(kind) = kind {
            value["scimType"] = json!(kind);
        }
        let mut response = response(status, value);
        if status == StatusCode::SERVICE_UNAVAILABLE {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response
    }
}

fn response(status: StatusCode, value: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/scim+json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(value),
    )
        .into_response()
}

fn list_response(resources: Vec<Value>, total: usize, start: usize) -> Response {
    response(
        StatusCode::OK,
        json!({
            "schemas": [LIST],
            "totalResults": total,
            "startIndex": start,
            "itemsPerPage": resources.len(),
            "Resources": resources,
        }),
    )
}

fn document(auth: &Authentication, user: &User) -> Value {
    let mut value = json!({
        "schemas": [USER],
        "id": user.id,
        "externalId": user.external_id,
        "userName": user.profile.user_name,
        "active": user.profile.active,
        "meta": {
            "resourceType": "User",
            "created": user.created,
            "lastModified": user.updated,
            "location": location(auth, user.id),
            "version": user.tag(),
        },
    });
    if let Some(name) = &user.profile.display_name {
        value["displayName"] = json!(name);
    }
    if let Some(name) = &user.profile.name {
        value["name"] = json!(name);
    }
    if !user.profile.emails.is_empty() {
        value["emails"] = json!(user.profile.emails);
    }
    value
}

fn location(auth: &Authentication, id: Uuid) -> String {
    format!(
        "{}{ROOT}/Users/{id}",
        auth.config.public_url.as_str().trim_end_matches('/')
    )
}

fn user_response(status: StatusCode, auth: &Authentication, user: &User) -> Response {
    let mut response = response(status, document(auth, user));
    if let Ok(tag) = user.tag().parse() {
        response.headers_mut().insert(header::ETAG, tag);
    }
    if status == StatusCode::CREATED
        && let Ok(location) = location(auth, user.id).parse()
    {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListParameters {
    filter: Option<String>,
    count: Option<i64>,
    start_index: Option<i64>,
    #[serde(default)]
    sort_by: Option<String>,
}

enum Filter {
    UserName(String),
    ExternalId(String),
}

impl Filter {
    fn parse(expression: &str) -> Result<Self> {
        let invalid = || {
            Error::invalid(
                "invalidFilter",
                "Use userName eq or externalId eq with a quoted value",
            )
        };
        let expression = expression.trim();
        let (attribute, expression) = expression
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;
        let (operator, argument) = expression
            .trim_start()
            .split_once(char::is_whitespace)
            .ok_or_else(invalid)?;
        if !operator.eq_ignore_ascii_case("eq") {
            return Err(invalid());
        }
        let value: String = serde_json::from_str(argument.trim()).map_err(|_| invalid())?;
        if attribute.eq_ignore_ascii_case("userName") {
            Ok(Self::UserName(value.to_lowercase()))
        } else if attribute.eq_ignore_ascii_case("externalId") {
            Ok(Self::ExternalId(value))
        } else {
            Err(invalid())
        }
    }

    fn matches(&self, user: &User) -> bool {
        match self {
            Self::UserName(value) => user.profile.user_name.to_lowercase() == *value,
            Self::ExternalId(value) => user.external_id == *value,
        }
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    params: std::result::Result<Query<ListParameters>, QueryRejection>,
) -> Result<Response> {
    let Query(params) =
        params.map_err(|_| Error::invalid("invalidValue", "Invalid query parameters"))?;
    if params.sort_by.is_some() {
        return Err(Error::invalid(
            "invalidValue",
            "SCIM sorting is unavailable",
        ));
    }
    let filter = params.filter.as_deref().map(Filter::parse).transpose()?;
    let start = params.start_index.unwrap_or(1).max(1) as usize;
    let count = params.count.unwrap_or(100).clamp(0, 100) as usize;
    let directory = Directory::new(&server)?;
    let users = directory.users().await?;
    let eligible: Vec<_> = users
        .values()
        .filter(|user| !user.deleted && filter.as_ref().is_none_or(|filter| filter.matches(user)))
        .collect();
    let total = eligible.len();
    let resources = eligible
        .into_iter()
        .skip(start - 1)
        .take(count)
        .map(|user| document(directory.auth, user))
        .collect();
    Ok(list_response(resources, total, start))
}

async fn get_user(State(server): State<Arc<Server>>, Path(id): Path<String>) -> Result<Response> {
    let id = Uuid::parse_str(&id).map_err(|_| Error::not_found())?;
    let directory = Directory::new(&server)?;
    let users = directory.users().await?;
    let user = users
        .get(&id)
        .filter(|user| !user.deleted)
        .ok_or_else(Error::not_found)?;
    Ok(user_response(StatusCode::OK, directory.auth, user))
}

fn body<T>(input: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    input.map(|Json(value)| value).map_err(|error| {
        let status = match error.status() {
            StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNSUPPORTED_MEDIA_TYPE => error.status(),
            _ => StatusCode::BAD_REQUEST,
        };
        Error(status, Some("invalidSyntax"), "Invalid SCIM request body")
    })
}

fn timestamp() -> Result<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(Error::unavailable)
}

fn unique_name(
    users: &std::collections::BTreeMap<Uuid, User>,
    profile: &Profile,
    id: Uuid,
) -> Result<()> {
    let candidate = profile.user_name.to_lowercase();
    if users.values().any(|user| {
        !user.deleted && user.id != id && user.profile.user_name.to_lowercase() == candidate
    }) {
        return Err(Error::conflict("userName is already assigned"));
    }
    Ok(())
}

async fn create(
    State(server): State<Arc<Server>>,
    input: std::result::Result<Json<UserInput>, JsonRejection>,
) -> Result<Response> {
    let input = body(input)?;
    let external_id = input
        .external_id
        .as_deref()
        .filter(|id| valid_subject(id))
        .ok_or_else(|| {
            Error::invalid(
                "invalidValue",
                "externalId must contain the exact OIDC subject",
            )
        })?;
    let profile = input.profile(true)?;
    let directory = Directory::new(&server)?;
    let user = directory
        .change(|users| {
            let previous = users.values().find(|user| user.external_id == external_id);
            if previous.is_some_and(|user| !user.deleted) {
                return Err(Error::conflict("externalId is already assigned"));
            }
            let id = previous.map_or_else(Uuid::now_v7, |user| user.id);
            unique_name(users, &profile, id)?;
            let updated = timestamp()?;
            let (version, created) = match previous {
                Some(user) => (next_version(user)?, user.created.clone()),
                None => (1, updated.clone()),
            };
            users.insert(
                id,
                User {
                    id,
                    external_id: external_id.into(),
                    profile: profile.clone(),
                    version,
                    created,
                    updated,
                    deleted: false,
                },
            );
            Ok(id)
        })
        .await?;
    Ok(user_response(StatusCode::CREATED, directory.auth, &user))
}

fn next_version(user: &User) -> Result<u64> {
    user.version.checked_add(1).ok_or_else(Error::capacity)
}

async fn update(
    directory: &Directory<'_>,
    id: &str,
    headers: &HeaderMap,
    edit: impl Fn(&User) -> Result<Option<Profile>>,
) -> Result<User> {
    let id = Uuid::parse_str(id).map_err(|_| Error::not_found())?;
    directory
        .change(|users| {
            let user = users
                .get(&id)
                .filter(|user| !user.deleted)
                .ok_or_else(Error::not_found)?;
            if headers
                .get(header::IF_MATCH)
                .is_some_and(|header| header != "*" && header != user.tag().as_str())
            {
                return Err(Error(
                    StatusCode::PRECONDITION_FAILED,
                    None,
                    "SCIM resource version changed",
                ));
            }
            let profile = edit(user)?;
            if let Some(profile) = &profile {
                unique_name(users, profile, id)?;
            }
            let user = users.get_mut(&id).ok_or_else(Error::not_found)?;
            if let Some(profile) = profile {
                user.profile = profile;
            } else {
                user.deleted = true;
            }
            user.version = next_version(user)?;
            user.updated = timestamp()?;
            Ok(id)
        })
        .await
}

async fn replace(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    input: std::result::Result<Json<UserInput>, JsonRejection>,
) -> Result<Response> {
    let input = body(input)?;
    let directory = Directory::new(&server)?;
    let user = update(&directory, &id, &headers, |user| {
        if input
            .external_id
            .as_ref()
            .is_some_and(|value| *value != user.external_id)
        {
            return Err(Error::invalid("mutability", "externalId is immutable"));
        }
        input.profile(user.profile.active).map(Some)
    })
    .await?;
    Ok(user_response(StatusCode::OK, directory.auth, &user))
}

async fn patch(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    input: std::result::Result<Json<Patch>, JsonRejection>,
) -> Result<Response> {
    let input = body(input)?;
    let directory = Directory::new(&server)?;
    let user = update(&directory, &id, &headers, |user| {
        input.apply(user).map(Some)
    })
    .await?;
    Ok(user_response(StatusCode::OK, directory.auth, &user))
}

async fn delete(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    update(&Directory::new(&server)?, &id, &headers, |_| Ok(None)).await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::CACHE_CONTROL, "no-store")],
    )
        .into_response())
}

#[cfg(test)]
mod tests;
