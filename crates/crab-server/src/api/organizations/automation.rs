use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{actor, read_error, require_owner, target};
use crate::{
    app::{self, Error, Result},
    auth::{Principal, automation},
    catalog::CatalogDocument,
    cells::organization::{Caller, automation as stored},
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/organizations/{id}/automation",
        get(list).post(change).layer(DefaultBodyLimit::max(8192)),
    )
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AutomationSnapshot {
    version: u64,
    identities: Vec<AutomationIdentity>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AutomationIdentity {
    id: Uuid,
    name: String,
    enabled: bool,
    grants: Vec<AutomationGrant>,
    tokens: Vec<stored::TokenSummary>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct AutomationGrant {
    repository: Uuid,
    name: Option<String>,
    access: stored::Access,
    current: bool,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<AutomationSnapshot>> {
    // Automation credentials are managed only from an interactive session.
    if !principal.has_browser_session() {
        return Err(Error::SessionRequired);
    }
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let address = target(id)?;
    let snapshot = cells
        .query::<stored::List>(address, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let identities = snapshot
        .identities
        .into_iter()
        .map(|identity| {
            let grants = identity
                .grants
                .into_iter()
                .map(|grant| {
                    let record = document
                        .repository_by_id(grant.repository)
                        .filter(|record| record.organization_id() == Some(id));
                    AutomationGrant {
                        repository: grant.repository,
                        name: record.map(|record| record.name.clone()),
                        access: grant.access,
                        current: record
                            .is_some_and(|record| record.ownership_epoch == grant.ownership_epoch),
                    }
                })
                .collect();
            AutomationIdentity {
                id: identity.id,
                name: identity.name,
                enabled: identity.enabled,
                grants,
                tokens: identity.tokens,
            }
        })
        .collect();
    require_owner(&cells, address, actor).await?;
    app::account(&principal)?;
    Ok(Json(AutomationSnapshot {
        version: snapshot.version,
        identities,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    expected_version: u64,
    edit: Edit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedGrant {
    repository: Uuid,
    access: stored::Access,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Edit {
    Create {
        name: String,
        grants: Vec<SelectedGrant>,
    },
    Configure {
        identity: Uuid,
        name: String,
        grants: Vec<SelectedGrant>,
    },
    Enabled {
        identity: Uuid,
        enabled: bool,
    },
    Delete {
        identity: Uuid,
    },
    Issue {
        identity: Uuid,
        hours: u16,
    },
    Revoke {
        identity: Uuid,
        token: Uuid,
    },
}

#[derive(Serialize)]
struct ChangeResult {
    identity: Uuid,
    token: Option<String>,
}

async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<Input>,
) -> Result<Json<ChangeResult>> {
    // Automation credentials are managed only from an interactive session.
    if !principal.has_browser_session() {
        return Err(Error::SessionRequired);
    }
    let actor = actor(&server, &principal).await?;
    let address = target(id)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    require_owner(&cells, address, actor).await?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let read_grants = |grants| async {
        let (document, _) = catalog
            .load_snapshot()
            .await
            .map_err(app::repository_access_error)?;
        current_grants(&document, id, grants)
    };
    let (identity, edit, token) = match input.edit {
        Edit::Create { name, grants } => (
            Uuid::now_v7(),
            stored::Edit::Create {
                name: name.trim().to_owned(),
                grants: read_grants(grants).await?,
            },
            None,
        ),
        Edit::Configure {
            identity,
            name,
            grants,
        } => (
            identity,
            stored::Edit::Configure {
                name: name.trim().to_owned(),
                grants: read_grants(grants).await?,
            },
            None,
        ),
        Edit::Enabled { identity, enabled } => (identity, stored::Edit::Enabled { enabled }, None),
        Edit::Delete { identity } => (identity, stored::Edit::Delete, None),
        Edit::Issue { identity, hours } => {
            let snapshot = cells
                .query::<stored::List>(address, None, Caller { actor })
                .await
                .map_err(read_error)?
                .output;
            if snapshot.version != input.expected_version {
                return Err(Error::PageChanged);
            }
            let selected = snapshot
                .identities
                .into_iter()
                .find(|value| value.id == identity)
                .ok_or(Error::NotFound)?;
            let (document, _) = catalog
                .load_snapshot()
                .await
                .map_err(app::repository_access_error)?;
            let grants = selected
                .grants
                .into_iter()
                .filter(|grant| {
                    document
                        .repository_by_id(grant.repository)
                        .is_some_and(|record| {
                            record.organization_id() == Some(id)
                                && record.ownership_epoch == grant.ownership_epoch
                        })
                })
                .collect();
            let token = Uuid::now_v7();
            let (secret, digest) = automation::issue(id, identity, token);
            (
                identity,
                stored::Edit::Issue {
                    token,
                    digest,
                    hours,
                    grants,
                },
                Some(secret),
            )
        }
        Edit::Revoke { identity, token } => (identity, stored::Edit::Revoke { token }, None),
    };
    app::account(&principal)?;
    let result = app::command_output(
        cells
            .command::<stored::Change>(
                address,
                app::mutation_identity()?,
                stored::Input {
                    actor,
                    expected_version: input.expected_version,
                    identity,
                    edit,
                },
            )
            .await,
    )?;
    match result {
        stored::Outcome::Saved => Ok(Json(ChangeResult { identity, token })),
        stored::Outcome::Stale => Err(Error::PageChanged),
        stored::Outcome::Missing => Err(Error::NotFound),
        stored::Outcome::Enabled => Err(Error::Invalid(
            "Disable the automation identity before deleting it",
        )),
        stored::Outcome::Disabled => Err(Error::Invalid(
            "Enable the automation identity before issuing a token",
        )),
        stored::Outcome::NoGrants => Err(Error::Invalid(
            "Grant access to at least one repository before issuing a token",
        )),
        stored::Outcome::Full => Err(Error::Invalid(
            "An organization can have 32 automation identities and each can have four active tokens",
        )),
    }
}

fn current_grants(
    document: &CatalogDocument,
    organization: Uuid,
    grants: Vec<SelectedGrant>,
) -> Result<Vec<stored::Grant>> {
    let mut grants = grants
        .into_iter()
        .map(|grant| {
            let record = document
                .repository_by_id(grant.repository)
                .filter(|record| record.organization_id() == Some(organization))
                .ok_or(Error::Invalid(
                    "Each grant must name a current repository in this organization",
                ))?;
            Ok(stored::Grant {
                repository: record.id,
                ownership_epoch: record.ownership_epoch,
                access: grant.access,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    grants.sort_by_key(|grant| grant.repository);
    Ok(grants)
}
