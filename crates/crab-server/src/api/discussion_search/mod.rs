use crate::cells::repository::search::completion::{CompleteSearch, CompletionInput, Suggestions};
use crate::{
    app::{self, Result},
    auth::Principal,
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use std::sync::Arc;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/discussion-search", get(complete))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn complete(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<CompletionInput>,
) -> Result<Json<Suggestions>> {
    let repo = app::repository(&server, &principal, &key)?;
    // Public input errors should be 400s; native ingress validates again
    // because peer callers cross a separate trust boundary.
    input.validate().map_err(app::Error::Invalid)?;
    let routed = app::repository_cell(&server, &repo)?;
    let result = app::query_output(
        routed
            .client
            .query::<CompleteSearch>(&routed.target, None, input)
            .await,
    )?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests;
