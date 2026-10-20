use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Query, State},
};
use serde_json::json;

use crate::{app, auth::Principal, server::Server};

#[derive(Default, serde::Deserialize)]
pub(super) struct RepositorySelection {
    owner: Option<String>,
    name: Option<String>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(selection): Query<RepositorySelection>,
) -> app::Result<Json<serde_json::Value>> {
    if selection.owner.is_some() != selection.name.is_some() {
        return Err(app::Error::Invalid(
            "owner and name must be provided together",
        ));
    }
    app::actor(&principal)?;
    let mut repositories = Vec::new();
    for repository in server
        .repositories
        .values()
        .into_iter()
        .filter(|repository| {
            principal.can_read(&repository.config)
                && selection.owner.as_ref().is_none_or(|owner| {
                    owner == &repository.config.owner
                        && selection.name.as_ref() == Some(&repository.config.name)
                })
        })
    {
        let protections = crate::repository::settings::load(&server, &repository).await?;
        let lifecycle = crate::repository::settings::load_lifecycle(&server, &repository).await?;
        repositories.push(json!({
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if principal.can_write(&repository.config) { "write" } else { "read" },
            "can_admin": principal.can_admin(&repository.config),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        }));
    }
    Ok(Json(json!({"repositories":repositories})))
}
