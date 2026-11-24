use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    catalog::{CatalogRecord, MembershipActor, OrganizationAccess},
    cells::{
        Address,
        directory::organizations::{Candidate, RecordCandidate},
        directory::{IdentityKey, RegisterAccounts},
        organization::{self, Bootstrap, BootstrapInput, Caller, ChangeOutcome, teams},
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/internal/repositories/{owner}/{name}/organization",
            put(set_repository_organization),
        )
        .route("/internal/organizations/{id}", post(bootstrap))
        .route(
            "/internal/organizations/{id}/teams",
            get(team_state).post(prepare_teams),
        )
        .route(
            "/internal/organizations/{id}/projects",
            get(project_state).post(prepare_projects),
        )
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryOrganization {
    repository_id: Uuid,
    expected_revision: u64,
    organization: OrganizationAccess,
}

async fn set_repository_organization(
    State(server): State<Arc<Server>>,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<RepositoryOrganization>,
) -> Result<Json<CatalogRecord>> {
    let _permit = super::admit(&server)?;
    input
        .organization
        .validate()
        .map_err(|_| Error::Invalid("Invalid organization or team grants"))?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let snapshot = catalog
        .membership(&owner, &name)
        .await
        .map_err(app::repository_access_error)?;
    if snapshot.repository_id != input.repository_id || snapshot.revision != input.expected_revision
    {
        return Err(Error::Conflict);
    }
    let teams = app::repository_team_access(&server, &input.organization, None, false).await?;
    if teams.teams.len() != input.organization.teams.len() {
        return Err(Error::Invalid(
            "Every grant must name a current team in the owning organization",
        ));
    }
    let actor = MembershipActor {
        issuer: "urn:crab:local".into(),
        subject: "operator".into(),
    };
    let record = catalog
        .set_organization_access(
            &owner,
            &name,
            input.repository_id,
            input.expected_revision,
            input.organization,
            actor,
        )
        .await
        .map_err(app::repository_access_error)?;
    let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
    // Publish the new owner with the repository route before returning. Until
    // another peer refreshes, it must fail closed on a different owner binding.
    server
        .install_catalog(document)
        .await
        .map_err(|source| Error::RepositorySetup(Box::new(source)))?;
    Ok(Json(record))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    owner: IdentityKey,
    name: String,
    #[serde(default)]
    description: String,
}
async fn bootstrap(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    if id.is_nil() {
        return Err(Error::Invalid("Organization ID is invalid"));
    }
    input.owner.validate().map_err(Error::Cell)?;
    if server
        .auth
        .as_ref()
        .is_some_and(|auth| auth.issuer() != input.owner.issuer)
    {
        return Err(Error::Invalid(
            "First owner must use the server's configured identity issuer",
        ));
    }
    organization::details(&input.name, &input.description).map_err(Error::Cell)?;
    let _permit = super::admit(&server)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    // Bootstrap is explicit private operator work. The qualified account is
    // registered without a profile; no catalog owner string acquires authority.
    cells
        .initialize(Address::Directory)
        .await
        .map_err(Error::Cell)?;
    let registered = app::command_output(
        cells
            .command::<RegisterAccounts>(
                Address::Directory,
                app::mutation_identity()?,
                vec![input.owner],
            )
            .await,
    )?;
    let [actor] = registered.as_slice() else {
        return Err(Error::CellContract(
            "Account registration returned an invalid result count",
        ));
    };
    app::command_output(
        cells
            .command::<RecordCandidate>(
                Address::Directory,
                app::mutation_identity()?,
                Candidate {
                    actor: *actor,
                    organization: id,
                },
            )
            .await,
    )?;
    let target = Address::Organization(id);
    cells.initialize(target).await.map_err(Error::Cell)?;
    let result = app::command_output(
        cells
            .command::<Bootstrap>(
                target,
                app::mutation_identity()?,
                BootstrapInput {
                    actor: *actor,
                    name: input.name,
                    description: input.description,
                },
            )
            .await,
    )?;
    match result {
        ChangeOutcome::Applied(organization) => Ok(Json(
            json!({"organization":organization,"owner":actor,"url":format!("/-/organizations/{id}")}),
        )),
        ChangeOutcome::Conflict => Err(Error::Conflict),
        _ => Err(Error::CellContract(
            "Organization bootstrap returned an invalid outcome",
        )),
    }
}

async fn prepare_projects(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Json(input): Json<organization::projects::PrepareInput>,
) -> Result<Json<crate::LifecycleStatus>> {
    let _permit = super::admit(&server)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::command_output(
        cells
            .command::<organization::projects::Prepare>(
                Address::Organization(id),
                app::mutation_identity()?,
                input,
            )
            .await,
    )?;
    Ok(Json(result))
}

async fn project_state(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Query(input): Query<Caller>,
) -> Result<Json<crate::LifecycleStatus>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    Ok(Json(app::query_output(
        cells
            .query::<organization::projects::GetPreparation>(Address::Organization(id), None, input)
            .await,
    )?))
}

async fn team_state(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Query(input): Query<Caller>,
) -> Result<Json<teams::OrganizationTeamState>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let state = app::query_output(
        cells
            .query::<teams::GetPreparation>(Address::Organization(id), None, input)
            .await,
    )?;
    Ok(Json(state))
}
async fn prepare_teams(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Json(input): Json<teams::PrepareInput>,
) -> Result<Json<teams::OrganizationTeamState>> {
    let _permit = super::admit(&server)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let state = app::command_output(
        cells
            .command::<teams::Prepare>(Address::Organization(id), app::mutation_identity()?, input)
            .await,
    )?;
    Ok(Json(state))
}
