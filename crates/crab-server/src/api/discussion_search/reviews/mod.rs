use crate::{
    app::{self, Result},
    auth::Principal,
    cells::repository::search::Query,
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{Path, Query as QueryParameters, State},
};
use std::sync::Arc;
#[cfg(test)]
mod tests;

pub(super) async fn bind_source(
    server: &Server,
    repo: &crate::repository::Repository,
    input: &mut crate::cells::repository::SearchInput,
) -> Result<()> {
    use crate::cells::repository::search::reviews::status::{Context, GetSourceState};
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    if !query.needs_review_source() {
        return Ok(());
    }
    let cell = app::repository_cell(server, repo)?;
    let state = app::query_output(
        cell.client
            .query::<GetSourceState>(&cell.target, None, ())
            .await,
    )?;
    let source_token = if let Some(state) = state {
        let source =
            crate::repository::projection::current_source(repo, server, &server.cancellation)
                .await?;
        if state.ready_epoch.is_none()
            || state.source_token.as_deref() != Some(source.source_token.as_str())
        {
            repo.schedule_maintenance(server, crate::repository::MaintenanceTrigger::Poll(&source))
                .await?;
            return Err(app::Error::ReviewIndexPending);
        }
        Some(source.source_token)
    } else {
        None
    };
    input.review_context = Some(Context {
        source_token,
        configured_rules: repo
            .config
            .protected_branches
            .iter()
            .cloned()
            .map(crate::repository::settings::protection_record)
            .collect(),
    });
    Ok(())
}

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct MenuInput {
    query: Option<String>,
}
pub(super) async fn menu(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    QueryParameters(input): QueryParameters<MenuInput>,
) -> Result<Json<crate::cells::repository::search::reviews::status::Menu>> {
    let repo = app::repository(&server, &principal, &key)?;
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    let menu = query.review_menu();
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(menu))
}
