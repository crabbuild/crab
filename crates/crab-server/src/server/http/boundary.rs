//! Enforce request authentication, authority, mutation, and response policy.

use crate::{app, auth::Principal, server::Server};
use axum::{
    Json, RequestExt,
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{net::IpAddr, sync::Arc, time::Instant};
use tracing::Instrument;
use uuid::Uuid;

use super::moved;

pub(super) async fn protect(
    State(server): State<Arc<Server>>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = Uuid::now_v7().to_string();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let observation = server.metrics.start_request(&method);
    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
    );
    async move {
        let mut response = boundary_request(server, request, next).await;
        if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
            response.headers_mut().insert("x-request-id", value);
        }
        tracing::info!(
            event = "http_response_ready",
            status = response.status().as_u16(),
            elapsed_us = started.elapsed().as_micros(),
            "request completed"
        );
        let status = response.status();
        let (parts, body) = response.into_parts();
        Response::from_parts(
            parts,
            axum::body::Body::new(crate::metrics::ObservedBody::new(
                body,
                observation.response(status),
            )),
        )
    }
    .instrument(span)
    .await
}

async fn boundary_request(server: Arc<Server>, mut request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok());
    let local_host = is_local_host(host);
    let internal_import = server
        .git_import
        .as_ref()
        .and_then(|context| context.authorizes_git(&request));
    let management_probe = matches!(request.uri().path(), "/healthz" | "/readyz");
    let load_balancer_probe = request.uri().path() == "/livez";
    let valid_host = load_balancer_probe
        || (management_probe && local_host)
        || internal_import.is_some()
        || server
            .auth
            .as_ref()
            .map(|auth| auth.allows_host(host))
            .unwrap_or(local_host);
    if !valid_host {
        return StatusCode::FORBIDDEN.into_response();
    }
    let git_request = request.uri().path().starts_with("/git/");
    let signed_webhook = crate::api::webhook_path(request.method(), request.uri().path())
        || crate::api::ci_git_path(request.method(), request.uri().path());
    let integration_request = integration_api_path(request.uri().path());
    let token_request = integration_request && request.headers().contains_key("authorization");
    let authentication_started = Instant::now();
    let mut principal = if signed_webhook {
        Principal::anonymous()
    } else if let Some(grant) = internal_import {
        grant.principal(&server).await
    } else {
        match &server.auth {
            Some(auth) if git_request || token_request => {
                auth.git_principal(request.headers()).await
            }
            Some(auth) => auth.principal(request.headers()).await,
            None => Principal::local(),
        }
    };
    tracing::debug!(
        target: "crab_server::action",
        event = "http_authentication_completed",
        elapsed_us = authentication_started.elapsed().as_micros(),
    );
    let protected =
        request.uri().path().starts_with("/api/") && request.uri().path() != "/api/session";
    // Membership's handler hides absent and non-admin repositories uniformly.
    // Only anonymous callers bypass generic login/CSRF responses; signed-in
    // mutations retain the normal Origin and session-CSRF requirements.
    let anonymous_membership = principal.is_anonymous()
        && matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::PUT
        )
        && request
            .uri()
            .path()
            .strip_prefix("/api/repos/")
            .is_some_and(|path| {
                let mut segments = path.split('/');
                matches!(
                    (segments.next(), segments.next(), segments.next(), segments.next()),
                    (Some(owner), Some(name), Some("members"), None)
                        if !owner.is_empty() && !name.is_empty()
                )
            });
    let denied = (protected || git_request) && !principal.authenticated() && !anonymous_membership;
    let unsafe_method = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let backchannel_logout = request.uri().path() == "/auth/backchannel-logout";
    let rejected_mutation = !backchannel_logout
        && !signed_webhook
        && !anonymous_membership
        && !git_request
        && unsafe_method
        && !principal.is_git()
        && server
            .auth
            .as_ref()
            .is_some_and(|auth| !auth.accepts_mutation(&principal, request.headers()));
    // Resolve decoded route parameters using Axum's extractor. Credentials alone
    // never authorize repository access; routing config supplies no grant.
    let params = request
        .extract_parts::<Path<std::collections::HashMap<String, String>>>()
        .await
        .ok()
        .map(|Path(params)| params);
    let browser_repository = protected
        && params
            .as_ref()
            .is_some_and(|params| params.contains_key("owner") && params.contains_key("name"))
        && !principal.is_git();
    let requested_repository = repository_identity(request.headers());
    let mut authorization_error = None;
    let mut redirect = None;
    let redirect_kind = if signed_webhook || principal.import_grant().is_some() {
        None
    } else if git_request {
        Some(moved::Kind::Git)
    } else if browser_repository
        && !unsafe_method
        && (requested_repository.is_some()
            || !request.headers().contains_key("x-crab-repository-id"))
    {
        Some(moved::Kind::Api)
    } else {
        None
    };
    if !denied && !rejected_mutation && principal.authenticated() {
        let address = params.as_ref().and_then(|params| {
            let owner = params.get("owner")?.as_str();
            let name = params.get("name")?.as_str();
            let name = if git_request {
                name.strip_suffix(".git")?
            } else {
                name
            };
            Some((owner, name))
        });
        if let Some((owner, name)) = address {
            let key = (owner.to_owned(), name.to_owned());
            let mut repository = server.repositories.get(&key);
            let needs_lookup = repository.is_none() || principal.is_local();
            if needs_lookup && let Some(kind) = redirect_kind {
                match moved::response(
                    &server,
                    &principal,
                    request.uri(),
                    kind,
                    (owner, name),
                    requested_repository,
                )
                .await
                {
                    Ok(response) => redirect = response,
                    Err(error) => authorization_error = Some(error.into_response()),
                }
                repository = server.repositories.get(&key);
            }
            if redirect.is_none()
                && authorization_error.is_none()
                && let Some(repository) = repository
            {
                let current = if principal.is_local() {
                    true
                } else {
                    match app::membership(&server, &principal, &repository).await {
                        Ok(current) => {
                            principal = current.principal;
                            true
                        }
                        Err(app::Error::NotFound) => false,
                        Err(error) => {
                            authorization_error = Some(error.into_response());
                            false
                        }
                    }
                };
                if current {
                    principal = principal.scoped_to_repository(Some(repository.id));
                } else if !needs_lookup
                    && authorization_error.is_none()
                    && let Some(kind) = redirect_kind
                {
                    match moved::response(
                        &server,
                        &principal,
                        request.uri(),
                        kind,
                        (owner, name),
                        requested_repository,
                    )
                    .await
                    {
                        Ok(response) => redirect = response,
                        Err(error) => authorization_error = Some(error.into_response()),
                    }
                }
                // A reused browser address must not retarget a scoped request.
                if current
                    && unsafe_method
                    && protected
                    && !principal.is_git()
                    && requested_repository != Some(repository.id)
                {
                    authorization_error = Some(app::Error::RepositoryChanged.into_response());
                }
                if browser_repository
                    && !unsafe_method
                    && request.headers().contains_key("x-crab-repository-id")
                    && requested_repository != Some(repository.id)
                {
                    authorization_error = Some(app::Error::NotFound.into_response());
                }
            }
        } else if !protected
            && !git_request
            && !unsafe_method
            && let Some((owner, name)) = moved::web_address(request.uri())
            && (principal.is_local()
                || server
                    .repositories
                    .get(&(owner.to_owned(), name.to_owned()))
                    .is_none())
        {
            match moved::response(
                &server,
                &principal,
                request.uri(),
                moved::Kind::Web,
                (owner, name),
                None,
            )
            .await
            {
                Ok(response) => redirect = response,
                Err(error) => authorization_error = Some(error.into_response()),
            }
        }
    }
    if browser_repository && unsafe_method {
        // Membership can resolve a case-insensitive catalog name independently
        // of the live routing key. Its refreshed grant must still match the
        // captured UUID; absent/invalid identity denies repository access.
        principal = principal.scoped_to_repository(requested_repository);
    }
    let check_archive = !denied
        && !rejected_mutation
        && authorization_error.is_none()
        && unsafe_method
        && !git_request;
    let deferred_archive = (check_archive && command_checks_archive(request.uri().path()))
        .then(|| request.uri().clone());
    let archive_check_started = Instant::now();
    let archived_response = if check_archive && deferred_archive.is_none() {
        archived_mutation_response(&server, &principal, request.uri().path()).await
    } else {
        None
    };
    tracing::debug!(
        target: "crab_server::action",
        event = "http_archive_check_completed",
        command_checks_archive = deferred_archive.is_some(),
        elapsed_us = archive_check_started.elapsed().as_micros(),
    );
    request.extensions_mut().insert(principal.clone());
    let mut response = if denied && git_request {
        (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                "Basic realm=\"Crab Git\", charset=\"UTF-8\"",
            )],
            "Use a Git access token from your signed-in Crab account",
        )
            .into_response()
    } else if denied {
        (StatusCode::UNAUTHORIZED, Json(json!({"error":{"code":"sign_in_required","message":"Sign in to access repositories"}}))).into_response()
    } else if rejected_mutation {
        (StatusCode::FORBIDDEN, Json(json!({"error":{"code":"csrf_rejected","message":"Reload the page before trying again"}}))).into_response()
    } else if let Some(response) = authorization_error {
        response
    } else if let Some(response) = redirect {
        response
    } else if let Some(response) = archived_response {
        response
    } else {
        next.run(request).await
    };
    // The shipped archive error takes precedence over invalid mutation input.
    // Guarded commands need the HTTP lookup only on client errors; never replace
    // a server error that may represent an ambiguous committed mutation.
    if let Some(uri) = deferred_archive
        && response.status().is_client_error()
        && let Some(mut archived) =
            archived_mutation_response(&server, &principal, uri.path()).await
    {
        // Changing the error presentation must retain a native proof of no
        // effects. Unmarked context or submission conflicts remain uncertain.
        if response
            .headers()
            .get("x-crab-write-outcome")
            .is_some_and(|value| value == "rejected")
        {
            archived.headers_mut().insert(
                "x-crab-write-outcome",
                axum::http::HeaderValue::from_static("rejected"),
            );
        }
        response = archived;
    }
    response
        .headers_mut()
        .entry("cache-control")
        .or_insert(axum::http::HeaderValue::from_static("no-store"));
    (
        [
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "same-origin"),
            ("content-security-policy", "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self' https://extensions.duckdb.org; style-src 'self' 'unsafe-inline'; worker-src 'self' blob:; img-src 'self' data: blob:; media-src 'self' blob:; frame-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"),
        ], response,
    ).into_response()
}

fn repository_identity(headers: &axum::http::HeaderMap) -> Option<Uuid> {
    let mut values = headers.get_all("x-crab-repository-id").iter();
    let identity = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uuid>().ok());
    if values.next().is_some() {
        return None;
    }
    identity
}

fn is_local_host(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let hostname = authority.host();
    let ip_literal = hostname
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(hostname);
    hostname.eq_ignore_ascii_case("localhost")
        || ip_literal
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn command_checks_archive(path: &str) -> bool {
    // These commands enforce lifecycle transactionally. Merge recovery may
    // finish an admitted Git publication after archive; preflight must not strand it.
    path.strip_prefix("/api/repos/").is_some_and(|path| {
        matches!(
            path.split('/').nth(2),
            Some("issues" | "labels" | "pulls" | "discussions" | "fork-sync")
        )
    })
}

async fn archived_mutation_response(
    server: &Server,
    principal: &Principal,
    path: &str,
) -> Option<Response> {
    let mut segments = path.strip_prefix("/api/repos/")?.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    let action = segments.next()?;
    let discussion_preference = matches!(action, "issues" | "pulls")
        && matches!(
            (segments.next(), segments.next(), segments.next()),
            (Some(_), Some("subscription"), None)
        );
    // Membership belongs to the catalog, including archived repositories. Its
    // handler must authorize against that revision before any Cell response.
    if (matches!(action, "members" | "team-access" | "subscription") && segments.next().is_none())
        || discussion_preference
        || path == format!("/api/repos/{owner}/{name}/settings/integrations")
        || path == format!("/api/repos/{owner}/{name}/settings/webhooks")
    {
        return None;
    }
    if owner.is_empty()
        || name.is_empty()
        || path == format!("/api/repos/{owner}/{name}/settings/archive")
    {
        return None;
    }
    let repository = server
        .repositories
        .get(&(owner.to_owned(), name.to_owned()))
        .filter(|repository| principal.can_read(repository.id))?;
    app::actor(principal).ok()?;
    match crate::repository::settings::load_lifecycle(server, &repository).await {
        Ok(lifecycle) if lifecycle.archived => Some(app::Error::Archived.into_response()),
        Ok(_) => None,
        Err(error) => Some(error.into_response()),
    }
}

fn integration_api_path(path: &str) -> bool {
    let mut segments = path.split('/');
    segments.next() == Some("")
        && segments.next() == Some("api")
        && segments.next() == Some("repos")
        && segments.next().is_some_and(|value| !value.is_empty())
        && segments.next().is_some_and(|value| !value.is_empty())
        && match segments.next() {
            Some("check-runs") => segments.next().is_none(),
            Some("statuses") => {
                segments.next().is_some_and(|value| !value.is_empty()) && segments.next().is_none()
            }
            Some("commits") => {
                segments.next().is_some_and(|value| !value.is_empty())
                    && match segments.next() {
                        Some("status") => segments.next().is_none(),
                        Some("check-runs") => segments
                            .next()
                            .is_none_or(|value| !value.is_empty() && segments.next().is_none()),
                        _ => false,
                    }
            }
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_tokens_are_only_considered_on_exact_integration_routes() {
        assert!(integration_api_path(
            "/api/repos/team/repo/statuses/0123456789012345678901234567890123456789"
        ));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/status"
        ));
        assert!(integration_api_path("/api/repos/team/repo/check-runs"));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/check-runs"
        ));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/check-runs/1"
        ));
        for path in [
            "/api/repos/team/repo/pulls/1",
            "/api/repos/team/repo/check-runs/1",
            "/api/repos/team/repo/statuses/oid/extra",
            "/api/repos/team/repo/commits/oid/statuses",
            "/api/repos/team/repo/commits/oid/check-runs/1/extra",
            "/api/repos/team/repo/commits/oid/check-runs/",
            "/api/repos//repo/commits/oid/statuses",
            "/api/repos/team/repo/commits//statuses",
        ] {
            assert!(!integration_api_path(path), "{path}");
        }
    }
}
