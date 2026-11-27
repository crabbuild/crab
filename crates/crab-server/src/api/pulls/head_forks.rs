use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    server::Server,
};

const LIMIT: usize = 24;

#[derive(Deserialize)]
pub(super) struct Search {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
struct Fork {
    id: Uuid,
    owner: String,
    name: String,
    owned: bool,
}

#[derive(Default, Serialize)]
pub(super) struct Page {
    items: Vec<Fork>,
    truncated: bool,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(search): Query<Search>,
) -> Result<Json<Page>> {
    let upstream = app::repository(&server, &principal, &key)?;
    if search.q.chars().count() > 128 {
        return Err(Error::Invalid("Fork search accepts at most 128 characters"));
    }
    if upstream.organization.is_none() || upstream.forked_from.is_some() {
        return Ok(Json(Page::default()));
    }
    let grant = app::membership(&server, &principal, &upstream).await?;
    let reader = grant
        .principal
        .related_catalog(upstream.id)
        .filter(|_| !grant.principal.is_public_reader(upstream.id))
        .ok_or(Error::Forbidden)?;
    let actor = app::account(&principal)?;
    let namespace = crate::catalog::personal_namespace(&actor.issuer, &actor.subject);
    let query = search.q.trim().to_lowercase();
    // Resolve the fork network against one catalog snapshot and batched
    // organization grants. The composer independently rechecks on submission.
    let visible = crate::api::catalog::visible_matching(&server, &reader, None, |repo| {
        repo.forked_from == Some(upstream.id)
            && (query.is_empty()
                || format!("{}/{}", repo.config.owner, repo.config.name)
                    .to_lowercase()
                    .contains(&query))
    })
    .await?;
    app::membership(&server, &principal, &upstream).await?;
    let mut items: Vec<_> = visible
        .into_iter()
        .filter(|row| row.principal.can_write(row.repository.id))
        .map(|row| Fork {
            id: row.repository.id,
            owned: row.repository.personal_namespace.as_deref() == Some(namespace.as_str()),
            owner: row.repository.config.owner.clone(),
            name: row.repository.config.name.clone(),
        })
        .collect();
    items.sort_by_key(|fork| !fork.owned);
    let truncated = items.len() > LIMIT;
    items.truncate(LIMIT);
    Ok(Json(Page { items, truncated }))
}
