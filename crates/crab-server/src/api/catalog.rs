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
    let grants = if principal.is_local() {
        None
    } else {
        let catalog = server.catalog().ok_or(app::Error::NotFound)?;
        Some(
            catalog
                .load()
                .await
                .map_err(|error| app::Error::Membership(Box::new(error)))?
                .0
                .repositories
                .into_iter()
                .map(|record| (record.id, record))
                .collect::<std::collections::HashMap<_, _>>(),
        )
    };
    let issuer = server
        .auth
        .as_ref()
        .map_or("urn:crab:local", |auth| auth.issuer());
    let mut repositories = Vec::new();
    for repository in server.repositories.values() {
        if selection.owner.as_ref().is_some_and(|owner| {
            owner != &repository.config.owner
                || selection.name.as_ref() != Some(&repository.config.name)
        }) {
            continue;
        }
        let current = if let Some(grants) = &grants {
            let Some(record) = grants.get(&repository.id).filter(|record| {
                record.owner == repository.config.owner && record.name == repository.config.name
            }) else {
                continue;
            };
            let mut config = repository.config.clone();
            config.members = record.members.clone();
            principal.with_membership(repository.id, &config, issuer)
        } else {
            principal.clone()
        };
        if !current.can_read(repository.id) {
            continue;
        }
        let protections = crate::repository::settings::load(&server, &repository).await?;
        let lifecycle = crate::repository::settings::load_lifecycle(&server, &repository).await?;
        repositories.push(json!({
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if current.can_write(repository.id) { "write" } else { "read" },
            "can_admin": current.can_admin(repository.id),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        }));
    }
    Ok(Json(json!({"repositories":repositories})))
}
