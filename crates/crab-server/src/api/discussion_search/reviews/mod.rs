use crate::{
    app::{self, Result},
    auth::Principal,
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{Path, Query as QueryParameters, State},
};
use std::sync::Arc;
#[cfg(test)]
mod tests;

pub(super) async fn bind_policy(
    server: &Server,
    principal: &Principal,
    repo: &crate::repository::Repository,
    input: &mut crate::cells::repository::SearchInput,
    default_ref: String,
) -> Result<()> {
    use crate::cells::repository::search::reviews::status::Context;
    input.review_context = Some(Context {
        default_ref,
        inherited_rulesets: crate::repository::settings::inherited_rulesets(server, repo).await?,
        reviewers: app::membership(server, principal, repo)
            .await?
            .merge_reviewers(server)
            .await?,
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
    let query = crate::cells::repository::search::Query::parse(input.query.as_deref())
        .map_err(app::Error::Invalid)?;
    let menu = query.review_menu();
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(menu))
}
