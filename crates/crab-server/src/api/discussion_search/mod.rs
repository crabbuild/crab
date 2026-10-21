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

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ListParameters {
    pub before: Option<String>,
    pub limit: Option<u8>,
    pub state: Option<String>,
    pub q: Option<String>,
    pub sort: Option<crate::cells::repository::search::index::Sort>,
    pub direction: Option<crate::cells::repository::search::index::Direction>,
}
impl ListParameters {
    pub(super) fn input(&self) -> Result<crate::cells::repository::SearchInput> {
        let state = match self.state.as_deref().unwrap_or("open") {
            "open" => 0,
            "closed" => 1,
            "all" => 2,
            _ => {
                return Err(app::Error::Invalid(
                    "Discussion state must be open, closed or all",
                ));
            }
        };
        let input = crate::cells::repository::SearchInput {
            before: self.before.clone(),
            limit: self.limit.unwrap_or(30),
            state,
            query: app::search_query(self.q.as_deref())?,
            sort: self.sort.unwrap_or_default(),
            direction: self.direction.unwrap_or_default(),
        };
        input.validate().map_err(app::Error::Invalid)?;
        Ok(input)
    }
}
