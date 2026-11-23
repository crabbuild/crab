use std::collections::BTreeMap;

use super::*;
use crate::{
    cells::{
        application::Mutation,
        organization::properties::{
            Definition, GetSchema, GetValues, Outcome, PropertyValue, Repository as Binding,
            Schema, SetSchema, SetSchemaInput, SetValues, SetValuesInput, View,
        },
    },
    repository::{
        Repository,
        settings::{RulesetScope, with_ruleset_lease},
    },
};

pub(super) fn organization_routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/organizations/{id}/repository-properties",
            get(read_schema).put(update_schema),
        )
        .layer(axum::extract::DefaultBodyLimit::max(128 * 1024))
}

pub(in crate::api) fn repository_routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/properties",
            get(read_values).put(update_values),
        )
        .layer(axum::extract::DefaultBodyLimit::max(128 * 1024))
}

async fn read_schema(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Schema>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = cells
        .query::<GetSchema>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?;
    Ok(Json(result.output))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaUpdate {
    expected_version: u64,
    definitions: Vec<Definition>,
}

async fn update_schema(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<SchemaUpdate>,
) -> Result<Json<Schema>> {
    let actor = actor(&server, &principal).await?;
    let command = SetSchemaInput {
        actor,
        expected_version: input.expected_version,
        definitions: input.definitions,
    };
    saved(save::<SetSchema>(server, id, command, None).await?)
}

fn binding(repo: &Repository) -> Result<(Uuid, Binding)> {
    Ok((
        repo.organization.ok_or(Error::NotFound)?,
        Binding {
            id: repo.id,
            ownership_epoch: repo.ownership_epoch,
        },
    ))
}

async fn read_values(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<(String, String)>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &path)?;
    let (organization, binding) = binding(&repo)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = cells
        .query::<GetValues>(target(organization)?, None, binding)
        .await
        .map_err(read_error)?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(result.output))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueUpdate {
    expected_version: u64,
    schema_version: u64,
    overrides: BTreeMap<Uuid, PropertyValue>,
}

async fn update_values(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<(String, String)>,
    Json(input): Json<ValueUpdate>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &path)?;
    let (organization, repository) = binding(&repo)?;
    let actor = actor(&server, &principal).await?;
    let command = SetValuesInput {
        actor,
        repository,
        expected_version: input.expected_version,
        schema_version: input.schema_version,
        overrides: input.overrides,
    };
    saved(save::<SetValues>(server, organization, command, Some((repo, principal))).await?)
}

async fn save<C: Mutation>(
    server: Arc<Server>,
    organization: Uuid,
    command: C::Input,
    repository: Option<(Arc<Repository>, Principal)>,
) -> Result<C::Output>
where
    C::Input: Send + 'static,
    C::Output: Send + 'static,
{
    let address = target(organization)?;
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let server = server.as_ref();
        let result = with_ruleset_lease(
            server,
            RulesetScope::Organization(organization),
            &server.cancellation,
            |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                if let Some((repo, principal)) = repository {
                    // A transfer changes the ownership epoch. Verify it under the
                    // same lease as inherited policy and the property update.
                    app::membership(server, &principal, &repo).await?;
                }
                let cells = server.cells().ok_or(Error::CellUnavailable)?;
                cells
                    .command::<C>(address, app::mutation_identity()?, command)
                    .await
                    .map(|result| result.output)
                    .map_err(read_error)
            },
        )
        .await;
        let _ = send.send(result);
    });
    reply.await.map_err(|_| Error::MergeBusy)?
}

fn saved<T>(outcome: Outcome<T>) -> Result<Json<T>> {
    match outcome {
        Outcome::Saved(value) => Ok(Json(value)),
        Outcome::Conflict => Err(Error::Conflict),
        Outcome::Invalid => Err(Error::Invalid(
            "Choose valid property names, types, and values",
        )),
        Outcome::UsedByRuleset => Err(Error::Invalid("A ruleset uses this property or value")),
        Outcome::IncompatibleDefinition => Err(Error::Invalid(
            "Existing properties keep their type and allowed values",
        )),
    }
}
