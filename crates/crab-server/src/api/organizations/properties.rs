use std::collections::{BTreeMap, HashMap, HashSet};

use super::*;
use crate::{
    cells::{
        MutationIdentity,
        application::Mutation,
        organization::properties::{
            Definition, Editor, GetSchema, GetSelection, GetValues, MAX_BATCH, Outcome,
            PropertyValue, ReadValues, Repository as Binding, Schema, Selection, SelectionReceipt,
            SelectionView, SetSchema, SetSchemaInput, SetSelection, SetValues, SetValuesInput,
            UpdateSelection, ValueError, VersionedRepository, View, search::Fields,
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
        .route(
            "/api/organizations/{id}/repository-properties/values",
            get(read_selection).put(update_selection),
        )
        .route(
            "/api/organizations/{id}/repository-properties/filters",
            get(filter_fields),
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

async fn filter_fields(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
) -> Result<Json<Schema>> {
    let visible =
        crate::api::catalog::visible(&server, &principal, None, Some(organization)).await?;
    let Some(repository) = visible.first().map(|item| &item.repository) else {
        return Ok(Json(Schema::default()));
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let schema = cells
        .query::<Fields>(target(organization)?, None, ())
        .await
        .map_err(read_error)?
        .output;
    // A definition is visible through repository access. Revalidate that
    // access after the Organization Cell read before returning definitions.
    let current = crate::api::catalog::visible_matching(&server, &principal, None, |repo| {
        repo.id == repository.id
            && repo.organization == Some(organization)
            && repo.ownership_epoch == repository.ownership_epoch
    })
    .await?;
    if current.is_empty() {
        return Err(Error::PageChanged);
    }
    Ok(Json(schema))
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
    saved(
        policy_write(server, id, |server, address, _| async move {
            submit::<SetSchema>(&server, address, app::mutation_identity()?, command).await
        })
        .await?,
    )
}

fn binding(repo: &Repository) -> Result<(Uuid, Binding)> {
    Ok((
        repo.organization.ok_or(Error::NotFound)?,
        Binding {
            id: repo.id,
            ownership_epoch: repo.ownership_epoch,
            property_seed: repo.property_seed,
        },
    ))
}

#[derive(Serialize)]
struct PublicProperty {
    name: String,
    value: PropertyValue,
}

#[derive(Serialize)]
#[serde(untagged)]
enum PropertyRead {
    Repository(View),
    Public { properties: Vec<PublicProperty> },
}

async fn read_values(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<(String, String)>,
) -> Result<Json<PropertyRead>> {
    let repo = app::repository(&server, &principal, &path)?;
    let (organization, binding) = binding(&repo)?;
    let editor = match app::account(&principal) {
        Ok(_) if principal.is_public_reader(repo.id) => None,
        Ok(identity) => crate::repository::access::actor_id(&server, identity)
            .await?
            .map(|actor| Editor {
                actor,
                repository_admin: principal.can_admin(repo.id),
            }),
        Err(_) => None,
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let mut view = cells
        .query::<GetValues>(
            target(organization)?,
            None,
            ReadValues {
                repository: binding,
                editor,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    let current = app::membership(&server, &principal, &repo).await?;
    if current.principal.is_public_reader(repo.id) {
        let properties = view
            .schema
            .definitions
            .iter()
            .filter_map(|definition| {
                view.values
                    .effective(definition)
                    .map(|value| PublicProperty {
                        name: definition.name.clone(),
                        value: value.clone(),
                    })
            })
            .collect();
        return Ok(Json(PropertyRead::Public { properties }));
    }
    // A stale render hint cannot keep delegated controls after loss of Admin.
    // The write path always checks the live grant, independently of the hint.
    if !current.principal.can_admin(repo.id) && editor.is_some_and(|editor| editor.repository_admin)
    {
        view.editable.clear();
    }
    Ok(Json(PropertyRead::Repository(view)))
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
    saved(
        policy_write(
            server,
            organization,
            move |server, address, cancel| async move {
                // A current catalog binding protects both Admin and ownership epoch
                // under the same lease as transfers and inherited policy publication.
                let current = app::membership(&server, &principal, &repo).await?;
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                let command = SetValuesInput {
                    editor: Editor {
                        actor,
                        repository_admin: current.principal.can_admin(repo.id),
                    },
                    repository,
                    expected_version: input.expected_version,
                    schema_version: input.schema_version,
                    overrides: input.overrides,
                };
                submit::<SetValues>(&server, address, app::mutation_identity()?, command).await
            },
        )
        .await?,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionQuery {
    repositories: String,
}

async fn read_selection(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Query(input): Query<SelectionQuery>,
) -> Result<Json<SelectionView>> {
    let actor = selection_owner(&server, organization, &principal).await?;
    let ids = input
        .repositories
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<Vec<Uuid>, _>>()
        .map_err(|_| Error::Invalid("Choose valid repository IDs"))?;
    let selected = catalog_selection(&server, organization, ids).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = cells
        .query::<GetSelection>(
            target(organization)?,
            None,
            Selection {
                actor,
                repositories: selected,
            },
        )
        .await
        .map_err(read_error)?;
    Ok(Json(result.output))
}

async fn selection_owner(
    server: &Server,
    organization: Uuid,
    principal: &Principal,
) -> Result<Uuid> {
    let actor = actor(server, principal).await?;
    let role = crate::repository::access::organization_role(server, organization, actor).await?;
    if role != Some(Role::Owner) {
        return Err(Error::NotFound);
    }
    Ok(actor)
}

async fn catalog_selection(
    server: &Server,
    organization: Uuid,
    ids: Vec<Uuid>,
) -> Result<Vec<Binding>> {
    let mut unique = HashSet::new();
    if !(1..=MAX_BATCH).contains(&ids.len())
        || ids.iter().any(|id| id.is_nil() || !unique.insert(*id))
    {
        return Err(Error::Invalid("Select 1–50 distinct repositories"));
    }
    let (document, _) = server
        .catalog()
        .ok_or(Error::NotFound)?
        .load()
        .await
        .map_err(|error| Error::Membership(Box::new(error)))?;
    let present: HashMap<_, _> = document
        .repositories
        .into_iter()
        .filter_map(|record| {
            (record.organization_id() == Some(organization)
                && record.application == crate::catalog::RepositoryApplicationState::CellReady
                && unique.contains(&record.id))
            .then_some((
                record.id,
                Binding {
                    id: record.id,
                    ownership_epoch: record.ownership_epoch,
                    property_seed: record.property_seed,
                },
            ))
        })
        .collect();
    ids.into_iter()
        .map(|id| present.get(&id).copied().ok_or(Error::NotFound))
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionUpdate {
    request_id: Uuid,
    issued_at_ms: i64,
    schema_version: u64,
    repositories: Vec<VersionedRepository>,
    changes: BTreeMap<Uuid, Option<PropertyValue>>,
}

async fn update_selection(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Json(input): Json<SelectionUpdate>,
) -> Result<Json<SelectionReceipt>> {
    let actor = selection_owner(&server, organization, &principal).await?;
    let identity = bulk_identity(actor, input.request_id, input.issued_at_ms)?;
    let command = UpdateSelection {
        actor,
        schema_version: input.schema_version,
        repositories: input.repositories,
        changes: input.changes,
    };
    let result = policy_write(
        server,
        organization,
        move |server, address, cancel| async move {
            let ids = command
                .repositories
                .iter()
                .map(|row| row.repository.id)
                .collect();
            // The catalog ownership and Cell value versions are checked under the
            // same policy lease before any selected repository can change.
            let current = catalog_selection(&server, organization, ids)
                .await
                .map_err(|error| {
                    if matches!(error, Error::NotFound) {
                        Error::Conflict
                    } else {
                        error
                    }
                })?;
            if command
                .repositories
                .iter()
                .zip(current)
                .any(|(row, current)| row.repository != current)
            {
                return Err(Error::Conflict);
            }
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            submit::<SetSelection>(&server, address, identity, command).await
        },
    )
    .await?;
    saved(result)
}

fn bulk_identity(actor: Uuid, request_id: Uuid, issued_at_ms: i64) -> Result<MutationIdentity> {
    if request_id.is_nil() || issued_at_ms < 0 || issued_at_ms.unsigned_abs() > app::MAX_NUMBER {
        return Err(Error::Invalid("Repository property submission is invalid"));
    }
    let mut digest = blake3::Hasher::new_derive_key("crab repository property bulk submission v1");
    digest.update(actor.as_bytes());
    digest.update(request_id.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.finalize().as_bytes()[..16]);
    Ok(MutationIdentity {
        request_id: Uuid::from_bytes(bytes),
        issued_at_ms,
        expires_at_ms: issued_at_ms + 24 * 60 * 60 * 1000,
    })
}

async fn submit<C: Mutation>(
    server: &Server,
    address: Address,
    identity: MutationIdentity,
    input: C::Input,
) -> Result<C::Output> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    cells
        .command::<C>(address, identity, input)
        .await
        .map(|reply| reply.output)
        .map_err(read_error)
}

pub(in crate::api) async fn policy_write<T: Send + 'static, F, Fut>(
    server: Arc<Server>,
    organization: Uuid,
    write: F,
) -> Result<T>
where
    F: FnOnce(Arc<Server>, Address, tokio_util::sync::CancellationToken) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>> + Send,
{
    let address = target(organization)?;
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let result = with_ruleset_lease(
            &server,
            RulesetScope::Organization(organization),
            &server.cancellation,
            |cancel| {
                let worker = Arc::clone(&server);
                async move {
                    if cancel.is_cancelled() {
                        return Err(Error::MergeBusy);
                    }
                    write(worker, address, cancel).await
                }
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
        Outcome::Denied => Err(Error::NotFound),
        Outcome::Invalid => Err(Error::Invalid(
            "Choose valid property names, types, and values",
        )),
        Outcome::MissingRequiredDefault => Err(Error::Invalid(
            "Set a default value for each required property",
        )),
        Outcome::MissingExplicitValue => Err(Error::Invalid(ValueError::MissingExplicit.message())),
        Outcome::UsedByRuleset => Err(Error::Invalid("A ruleset uses this property or value")),
        Outcome::IncompatibleDefinition => Err(Error::Invalid(
            "Existing properties keep their type and allowed values",
        )),
    }
}
