use super::people;
use crate::{
    app::{self, Result},
    auth::Principal,
    cells::repository::search::{Query, reviews::Kind},
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{Path, Query as QueryParameters, State},
};
use std::sync::Arc;
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OptionInput {
    query: Option<String>,
    search: String,
    relation: Kind,
}
pub(super) async fn options(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    QueryParameters(input): QueryParameters<OptionInput>,
) -> Result<Json<people::Options>> {
    let repo = app::repository(&server, &principal, &key)?;
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    let choose = |query: &Query, name: Option<&str>| query.reviewer_choice(input.relation, name);
    people::options(&server, &principal, &repo, &query, &input.search, choose).await
}
#[cfg(test)]
mod tests;
