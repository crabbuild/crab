use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::{
    app::Result,
    auth::Principal,
    config::valid_repository_component,
    server::{Server, repository_route},
};

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Web,
    Api,
    Git,
}

pub(super) fn web_address(uri: &Uri) -> Option<(&str, &str)> {
    let mut segments = uri.path().strip_prefix('/')?.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    (valid_repository_component(owner)
        && valid_repository_component(name)
        && !crate::config::reserved_owner(owner))
    .then_some((owner, name))
}

pub(super) async fn response(
    server: &Server,
    principal: &Principal,
    uri: &Uri,
    kind: Kind,
    (owner, name): (&str, &str),
    expected: Option<Uuid>,
) -> Result<Option<Response>> {
    if !valid_repository_component(owner) || !valid_repository_component(name) {
        return Ok(None);
    }
    let Some(destination) =
        repository_route::renamed(server, principal, owner, name, expected).await?
    else {
        return Ok(None);
    };
    if (owner, name) == (destination.owner.as_str(), destination.name.as_str()) {
        // A stale peer may have installed the canonical route while resolving.
        return Ok(None);
    }
    let (prefix, extension) = match kind {
        Kind::Web => ("", ""),
        Kind::Api => ("/api/repos", ""),
        Kind::Git => ("/git", ".git"),
    };
    let Some(rest) = uri
        .path()
        .strip_prefix(prefix)
        .and_then(|path| path.strip_prefix('/'))
    else {
        return Ok(None);
    };
    let suffix = rest
        .splitn(3, '/')
        .nth(2)
        .map(|tail| format!("/{tail}"))
        .unwrap_or_default();
    let query = uri
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let location = format!(
        "{prefix}/{}/{}{extension}{suffix}{query}",
        destination.owner, destination.name
    );
    Ok(Some(
        (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response(),
    ))
}
