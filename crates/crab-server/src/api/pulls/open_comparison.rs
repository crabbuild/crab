use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use serde::Serialize;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::pull_branch::{Comparison, FindOpenComparison},
    server::Server,
};

#[derive(Serialize)]
pub(super) struct Match {
    pull_number: Option<u64>,
}

pub(super) async fn find(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(comparison): Query<Comparison>,
) -> Result<Json<Match>> {
    let repo = app::repository(&server, &principal, &key)?;
    if [&comparison.base_ref, &comparison.head_ref]
        .into_iter()
        .any(|name| {
            !name.starts_with("refs/heads/")
                || name.len() > 1_024
                || crab_git::validate_push_refname(name).is_err()
        })
    {
        return Err(Error::Invalid("Pull requests require branch refs"));
    }
    if let Some(fork) = comparison.head_repository {
        super::git::fork_repository(&server, &repo, fork)?;
    }
    let cell = app::repository_cell(&server, &repo)?;
    let pull_number = app::query_output(
        cell.client
            .query::<FindOpenComparison>(&cell.target, None, comparison)
            .await,
    )?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(Match { pull_number }))
}
