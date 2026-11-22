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
    let mut repositories = Vec::new();
    for repository in server.repositories.values() {
        if selection.owner.as_ref().is_some_and(|owner| {
            owner != &repository.config.owner
                || selection.name.as_ref() != Some(&repository.config.name)
        }) {
            continue;
        }
        let record = grants
            .as_ref()
            .and_then(|grants| grants.get(&repository.id))
            .filter(|record| {
                record.owner == repository.config.owner && record.name == repository.config.name
            });
        let current = if let Some(record) = record {
            let mut config = repository.config.clone();
            config.members = record.members.clone();
            config.groups = record.groups.clone();
            app::resolve_repository_membership(
                &server,
                &principal,
                repository.id,
                &config,
                record.organization.as_ref(),
            )
            .await?
        } else if principal.is_local() {
            principal.clone()
        } else {
            continue;
        };
        if !current.can_read(repository.id) {
            continue;
        }
        let protections = crate::repository::settings::load(&server, &repository).await?;
        let lifecycle = crate::repository::settings::load_lifecycle(&server, &repository).await?;
        let mut view = json!({
            "id": repository.id,
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if current.can_write(repository.id) { "write" } else { "read" },
            "can_admin": current.can_admin(repository.id),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        });
        if let Some(organization) = record.and_then(|record| record.organization.as_ref()) {
            view["organization_id"] = json!(organization.id);
        }
        repositories.push(view);
    }
    Ok(Json(json!({"repositories":repositories})))
}
