use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use cellule_runtime::{Committed, InvocationError, MutationIdentity, Observed, RequestId};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::{
        RepositoryCell,
        repository::{
            CreateLabel, CreateLabelInput, CreateLabelOutcome, DeleteLabel, DeleteLabelInput,
            DeleteLabelOutcome, LabelRecord, ListLabels, RepositoryAuthor, UpdateLabel,
            UpdateLabelInput, UpdateLabelOutcome,
        },
    },
    server::{Repository, Server},
};

pub(crate) type Label = LabelRecord;
const MAX_SELECTION: usize = 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLabel {
    request_id: String,
    name: String,
    color: String,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelEdit {
    version: u64,
    name: String,
    color: String,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelDelete {
    version: u64,
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/labels", get(list).post(create))
        .route(
            "/api/repos/{owner}/{name}/labels/{number}",
            axum::routing::patch(edit).delete(remove),
        )
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn name(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Label name must contain 1–50 characters without controls",
        ));
    }
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 50 {
        return Err(Error::Invalid(
            "Label name must contain 1–50 characters without controls",
        ));
    }
    Ok(value.to_owned())
}

fn color(value: &str) -> Result<String> {
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Invalid(
            "Label color must be a six-digit hexadecimal value",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn description(value: Option<String>) -> Result<Option<String>> {
    if value
        .as_ref()
        .is_some_and(|value| value.chars().any(char::is_control))
    {
        return Err(Error::Invalid(
            "Label description must be at most 100 characters without controls",
        ));
    }
    let value = value.map(|value| value.trim().to_owned());
    if value
        .as_ref()
        .is_some_and(|value| value.chars().count() > 100)
    {
        return Err(Error::Invalid(
            "Label description must be at most 100 characters without controls",
        ));
    }
    Ok(value.filter(|value| !value.is_empty()))
}

fn view(label: &Label) -> Value {
    json!({
        "id": label.number,
        "name": label.name,
        "color": label.color,
        "description": label.description,
        "version": label.version,
        "created_at": label.created_at_ms,
        "updated_at": label.updated_at_ms,
    })
}

pub(crate) async fn catalog(
    server: &Server,
    repo: &Repository,
    principal: &Identity,
) -> Result<Vec<Label>> {
    let routed = route(server, repo, principal, "repository.read").await?;
    Ok(query_output(
        routed
            .client
            .query::<ListLabels>(&routed.target, None, ())
            .await,
    )?
    .labels)
}

pub(crate) fn selection_view(ids: &[u64], catalog: &[Label]) -> Vec<Value> {
    catalog
        .iter()
        .filter(|label| ids.contains(&label.number))
        .map(view)
        .collect()
}

pub(crate) fn validate_selection(mut ids: Vec<u64>, catalog: &[Label]) -> Result<Vec<u64>> {
    if ids.len() > MAX_SELECTION || ids.contains(&0) {
        return Err(Error::Invalid("An item supports at most 20 labels"));
    }
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1])
        || ids
            .iter()
            .any(|id| !catalog.iter().any(|label| label.number == *id))
    {
        return Err(Error::Invalid(
            "Label selection contains an unknown or duplicate label",
        ));
    }
    Ok(ids)
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let actor = app::actor(&principal)?;
    let labels = catalog(&server, &repo, &actor).await?;
    Ok(Json(json!({
        "items": labels.iter().map(view).collect::<Vec<_>>(),
        "can_manage": principal.can_write(&repo.config),
    })))
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewLabel>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &key)?;
    if !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let routed = route(&server, &repo, &actor, "repository.label.create").await?;
    let output = command_output(
        routed
            .client
            .command::<CreateLabel>(
                &routed.target,
                mutation_identity()?,
                CreateLabelInput {
                    submission_id: submission_id(&input.request_id)?,
                    author: repository_author(&actor),
                    name: name(&input.name)?,
                    color: color(&input.color)?,
                    description: description(input.description)?,
                },
            )
            .await,
    )?;
    let label = match output {
        CreateLabelOutcome::Created(label) => label,
        CreateLabelOutcome::RequestConflict => return Err(Error::RequestConflict),
        CreateLabelOutcome::NameConflict => return Err(Error::LabelConflict),
        CreateLabelOutcome::NotFound => return Err(Error::LabelNotFound),
        CreateLabelOutcome::LimitReached => {
            return Err(Error::Invalid(
                "A repository supports at most 500 labels over its lifetime",
            ));
        }
    };
    Ok((StatusCode::CREATED, Json(view(&label))))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name_key, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<LabelEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name_key))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let routed = route(&server, &repo, &actor, "repository.label.update").await?;
    let output = command_output(
        routed
            .client
            .command::<UpdateLabel>(
                &routed.target,
                mutation_identity()?,
                UpdateLabelInput {
                    number: app::number(number)?,
                    version: input.version,
                    name: name(&input.name)?,
                    color: color(&input.color)?,
                    description: description(input.description)?,
                },
            )
            .await,
    )?;
    let label = match output {
        UpdateLabelOutcome::Updated(label) => label,
        UpdateLabelOutcome::NotFound => return Err(Error::LabelNotFound),
        UpdateLabelOutcome::NameConflict => return Err(Error::LabelConflict),
        UpdateLabelOutcome::Conflict => return Err(Error::Conflict),
    };
    Ok(Json(view(&label)))
}

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<LabelDelete>, JsonRejection>,
) -> Result<StatusCode> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let routed = route(&server, &repo, &actor, "repository.label.delete").await?;
    match command_output(
        routed
            .client
            .command::<DeleteLabel>(
                &routed.target,
                mutation_identity()?,
                DeleteLabelInput {
                    number: app::number(number)?,
                    version: input.version,
                },
            )
            .await,
    )? {
        DeleteLabelOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteLabelOutcome::NotFound => Err(Error::LabelNotFound),
        DeleteLabelOutcome::Conflict => Err(Error::Conflict),
    }
}

async fn route(
    server: &Server,
    repository: &Repository,
    principal: &Identity,
    action: &'static str,
) -> Result<RepositoryCell> {
    let router = server.repository_cells().ok_or(Error::CellUnavailable)?;
    router
        .route(repository.id, principal, action)
        .await
        .map_err(|error| match error {
            crate::Error::Cell(source) => Error::Cell(source),
            source => Error::Repository(source),
        })
}

fn repository_author(identity: &Identity) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: identity.issuer.clone(),
        subject: identity.subject.clone(),
        name: identity.name.clone(),
    }
}

fn mutation_identity() -> Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Repository)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn submission_id(value: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(&app::submission(value)?)
        .map(Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(committed) => Ok(committed.output),
        Err(InvocationError::Rejected(committed)) => Ok(committed.output),
        Err(InvocationError::Pending(_)) => Err(Error::CellPending),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(observed) => Ok(observed.output),
        Err(InvocationError::Rejected(_)) => Err(Error::CellContract(
            "Cell query returned a durable rejection",
        )),
        Err(InvocationError::Pending(_)) => Err(Error::CellContract(
            "Cell query returned pending mutation evidence",
        )),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}
