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
mod assignees;
mod authors;
mod people;
mod reviews;
pub(super) async fn resolve_people(
    server: &Server,
    principal: &Principal,
    input: &mut crate::cells::repository::SearchInput,
) -> Result<()> {
    people::resolve(server, principal, input).await?;
    assignees::resolve(server, principal, input).await
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/discussion-search/labels",
            get(labels),
        )
        .route(
            "/api/repos/{owner}/{name}/discussion-search/milestones",
            get(milestones),
        )
        .route(
            "/api/repos/{owner}/{name}/discussion-search/authors",
            get(authors::options),
        )
        .route(
            "/api/repos/{owner}/{name}/discussion-search/assignees",
            get(assignees::options),
        )
        .route(
            "/api/repos/{owner}/{name}/discussion-search/reviewers",
            get(reviews::options),
        )
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
    let prefix = crate::cells::repository::search::completion::person_prefix(&input)
        .map_err(app::Error::Invalid)?;
    let mut result = app::query_output(
        routed
            .client
            .query::<CompleteSearch>(
                &routed.target,
                None,
                CompletionInput {
                    query: input.query.clone(),
                    cursor: input.cursor,
                },
            )
            .await,
    )?;
    if let Some((qualifier, prefix)) =
        prefix.filter(|(_, prefix)| crate::cells::directory::validate_handle_prefix(prefix).is_ok())
    {
        let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
        let handles = app::query_output(
            cells
                .query::<crate::cells::directory::CompleteHandles>(
                    crate::cells::Address::Directory,
                    None,
                    prefix,
                )
                .await,
        )?;
        crate::cells::repository::search::completion::append_people(
            &input,
            &mut result,
            &qualifier,
            handles
                .items
                .into_iter()
                .map(|actor| (actor.handle, actor.name)),
        )
        .map_err(app::Error::Invalid)?;
    }
    // Recheck the durable grant after the awaited read: the materialized
    // repository may still contain a member whose access has been revoked.
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(result))
}

async fn milestones(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<crate::cells::repository::search::milestones::OptionInput>,
) -> Result<Json<crate::cells::repository::search::milestones::Options>> {
    use crate::cells::repository::search::milestones::MilestoneOptions;
    let repo = app::repository(&server, &principal, &key)?;
    input.validate().map_err(app::Error::Invalid)?;
    let cell = app::repository_cell(&server, &repo)?;
    let result = app::query_output(
        cell.client
            .query::<MilestoneOptions>(&cell.target, None, input)
            .await,
    )?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(result))
}

async fn labels(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<crate::cells::repository::search::labels::OptionInput>,
) -> Result<Json<crate::cells::repository::search::labels::Options>> {
    use crate::cells::repository::search::labels::LabelOptions;
    let repo = app::repository(&server, &principal, &key)?;
    input.validate().map_err(app::Error::Invalid)?;
    let cell = app::repository_cell(&server, &repo)?;
    let result = app::query_output(
        cell.client
            .query::<LabelOptions>(cell.target, None, input)
            .await,
    )?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests;

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ListParameters {
    pub before: Option<String>,
    pub limit: Option<u8>,
    pub q: Option<String>,
    pub sort: Option<crate::cells::repository::search::index::Sort>,
    pub direction: Option<crate::cells::repository::search::index::Direction>,
}
impl ListParameters {
    pub(super) fn input(
        &self,
        default_query: &str,
    ) -> Result<crate::cells::repository::SearchInput> {
        let input = crate::cells::repository::SearchInput {
            before: self.before.clone(),
            limit: self.limit.unwrap_or(30),
            query: app::search_query(self.q.as_deref().or(Some(default_query)))?,
            sort: self.sort.unwrap_or_default(),
            direction: self.direction.unwrap_or_default(),
            people: Vec::new(),
            assignees: Vec::new(),
        };
        input.validate().map_err(app::Error::Invalid)?;
        Ok(input)
    }
}
