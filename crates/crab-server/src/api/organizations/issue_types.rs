use super::*;
use crate::{
    cells::{
        organization::issue_types::{
            Active, Definition, Get, Outcome, Replace, ReplaceInput, Schema,
        },
        repository::{
            issues::issue_type::{self, Selection},
            projects::IssueTypeField,
        },
    },
    repository::Repository,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/organizations/{id}/issue-types",
        get(schema).put(replace),
    )
}

async fn schema(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Schema>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let reply = cells
        .query::<Get>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?;
    app::account(&principal)?;
    Ok(Json(reply.output))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRequest {
    expected_version: u64,
    definitions: Vec<Definition>,
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<ReplaceRequest>,
) -> Result<Json<Schema>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    require_owner(&cells, target(id)?, actor).await?;
    let identity = app::mutation_identity()?;
    let output = properties::policy_write(server, id, move |server, address, cancel| async move {
        app::account(&principal)?;
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        let reply = cells
            .command::<Replace>(
                address,
                identity,
                ReplaceInput {
                    actor,
                    expected_version: input.expected_version,
                    definitions: input.definitions,
                },
            )
            .await
            .map_err(read_error)?;
        Ok(reply.output)
    })
    .await?;
    match output {
        Outcome::Saved(value) => Ok(Json(value)),
        Outcome::Conflict => Err(Error::Conflict),
        Outcome::Invalid => Err(Error::Invalid(
            "Choose up to 25 issue types with unique names and valid descriptions",
        )),
    }
}

// Only callers with an authorized repository boundary use these definitions.
pub(in crate::api) async fn active(server: &Server, organization: Option<Uuid>) -> Result<Schema> {
    let Some(organization) = organization else {
        return Ok(Schema::default());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    Ok(cells
        .query::<Active>(target(organization)?, None, ())
        .await
        .map_err(read_error)?
        .output)
}

pub(in crate::api) fn visible<'a>(
    repo: &Repository,
    selection: Option<Selection>,
    definitions: &'a [Definition],
) -> Option<&'a Definition> {
    let selection = selection?;
    if Some(selection.organization) != repo.organization
        || selection.ownership_epoch != repo.ownership_epoch
    {
        return None;
    }
    definitions
        .iter()
        .find(|definition| definition.id == selection.id)
}

pub(in crate::api) fn project_field<'a>(
    repo: &Repository,
    field: Option<IssueTypeField>,
    definitions: &'a [Definition],
    can_triage: bool,
) -> (Option<&'a Definition>, Option<u64>) {
    let Some(field) = field.filter(|_| repo.organization.is_some()) else {
        return (None, None);
    };
    (
        visible(repo, field.selection, definitions),
        field.editable_version.filter(|_| can_triage),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct AssignmentRequest {
    expected_version: u64,
    type_id: Option<Uuid>,
}

pub(in crate::api) async fn assign(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(input): Json<AssignmentRequest>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let organization = repo.organization.ok_or(Error::NotFound)?;
    let number = app::number(number)?;
    app::account(&principal)?;
    if !principal.can_triage(repo.id) {
        return Err(Error::NotFound);
    }
    let identity = app::mutation_identity()?;
    // Transfers and definition edits hold this lease as well. The repository
    // transaction can then check issue existence, archive state and version.
    let output =
        properties::policy_write(server, organization, move |server, _, cancel| async move {
            let membership = app::membership(&server, &principal, &repo).await?;
            if !membership.principal.can_triage(repo.id) {
                return Err(Error::NotFound);
            }
            if let Some(id) = input.type_id
                && !active(&server, Some(organization))
                    .await?
                    .definitions
                    .iter()
                    .any(|definition| definition.id == id)
            {
                return Err(Error::Invalid("Choose an enabled issue type"));
            }
            app::account(&membership.principal)?;
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            let routed = app::repository_cell(&server, &repo)?;
            app::command_output(
                routed
                    .client
                    .command::<issue_type::Set>(
                        &routed.target,
                        identity,
                        issue_type::SetInput {
                            number,
                            expected_version: input.expected_version,
                            selection: input.type_id.map(|id| Selection {
                                id,
                                organization,
                                ownership_epoch: repo.ownership_epoch,
                            }),
                        },
                    )
                    .await,
            )
        })
        .await?;
    match output {
        issue_type::Outcome::Saved(value) => Ok(Json(json!({"version": value.version}))),
        issue_type::Outcome::Conflict => Err(Error::Conflict),
        issue_type::Outcome::Archived => Err(Error::Archived),
        issue_type::Outcome::NotFound => Err(Error::NotFound),
    }
}
