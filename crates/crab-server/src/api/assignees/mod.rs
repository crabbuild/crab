use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    middleware,
    routing::get,
};
use serde_json::{Value, json};

use crate::{
    app::{self, Result},
    auth::Principal,
    server::Server,
};

mod resolution;
pub(crate) use resolution::Roster;
pub(crate) use resolution::Selection;

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/assignees", get(list))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let roster = Roster::load(&server, &principal, repo).await?;
    Ok(Json(json!({
        "items":roster.items,
        "can_manage":roster.principal.can_triage(repo.id),
    })))
}

#[cfg(test)]
mod tests;
