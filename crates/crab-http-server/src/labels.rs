use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::{Identity, Principal},
    server::{Repository, Server},
};

pub(crate) const ROOT: &str = "app/v1/labels";
const CATALOG: &str = "app/v1/labels/catalog.json";
const MAX_LABELS: u64 = 500;
const MAX_SELECTION: usize = 20;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Label {
    pub number: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LabelReservation {
    request_id: String,
    author: Identity,
    label: Label,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeletedLabel {
    number: u64,
    version: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    labels: Vec<Label>,
    #[serde(default)]
    deleted: Vec<DeletedLabel>,
}

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

fn reservation(request_id: &str) -> String {
    format!("{ROOT}/requests/{request_id}.json")
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

fn same_name(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

fn same_request(
    saved: &LabelReservation,
    request_id: &str,
    author: &Identity,
    name: &str,
    color: &str,
    description: &Option<String>,
) -> bool {
    saved.request_id == request_id
        && app_storage::same_author(&saved.author, author)
        && saved.label.name == name
        && saved.label.color == color
        && &saved.label.description == description
}

fn view(label: &Label) -> Value {
    json!({
        "id": label.number,
        "name": label.name,
        "color": label.color,
        "description": label.description,
        "version": label.version,
        "created_at": label.created_at,
        "updated_at": label.updated_at,
    })
}

pub(crate) async fn catalog(repo: &Repository) -> Result<Vec<Label>> {
    let Some((catalog, _)) = app_storage::read::<Catalog>(repo, CATALOG).await? else {
        return Ok(vec![]);
    };
    Ok(catalog.labels)
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

async fn publish(repo: &Repository, proposed: &Label) -> Result<Label> {
    for _ in 0..10 {
        let Some((mut catalog, etag)) = app_storage::read::<Catalog>(repo, CATALOG).await? else {
            let created = app_storage::create_or_read(
                repo,
                CATALOG,
                Catalog {
                    labels: vec![proposed.clone()],
                    deleted: vec![],
                },
            )
            .await?;
            if created
                .deleted
                .iter()
                .any(|label| label.number == proposed.number)
            {
                return Err(Error::LabelNotFound);
            }
            if let Some(label) = created
                .labels
                .iter()
                .find(|label| label.number == proposed.number)
            {
                return Ok(label.clone());
            }
            continue;
        };
        if catalog
            .deleted
            .iter()
            .any(|label| label.number == proposed.number)
        {
            return Err(Error::LabelNotFound);
        }
        if let Some(label) = catalog
            .labels
            .iter()
            .find(|label| label.number == proposed.number)
        {
            return Ok(label.clone());
        }
        if catalog
            .labels
            .iter()
            .any(|label| same_name(&label.name, &proposed.name))
        {
            return Err(Error::LabelConflict);
        }
        catalog.labels.push(proposed.clone());
        catalog
            .labels
            .sort_by_cached_key(|label| label.name.to_lowercase());
        match app_storage::update(repo, CATALOG, &catalog, etag).await {
            Ok(()) => return Ok(proposed.clone()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let labels = catalog(repo).await?;
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
    let request_id = app::submission(&input.request_id)?;
    let actor = app::actor(&principal)?;
    let name = name(&input.name)?;
    let color = color(&input.color)?;
    let description = description(input.description)?;
    let path = reservation(&request_id);
    let saved = match app_storage::read::<LabelReservation>(repo, &path).await? {
        Some((saved, _)) => saved,
        None => {
            let number = app_storage::reserve_number(repo, ROOT).await?;
            if number > MAX_LABELS {
                return Err(Error::Invalid(
                    "A repository supports at most 500 labels over its lifetime",
                ));
            }
            let timestamp = app_storage::now()?;
            app_storage::create_or_read(
                repo,
                &path,
                LabelReservation {
                    request_id: request_id.clone(),
                    author: actor.clone(),
                    label: Label {
                        number,
                        name: name.clone(),
                        color: color.clone(),
                        description: description.clone(),
                        version: 1,
                        created_at: timestamp,
                        updated_at: timestamp,
                    },
                },
            )
            .await?
        }
    };
    if !same_request(&saved, &request_id, &actor, &name, &color, &description) {
        return Err(Error::RequestConflict);
    }
    if !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    let label = publish(repo, &saved.label).await?;
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
    let number = app::number(number)?;
    let name = name(&input.name)?;
    let color = color(&input.color)?;
    let description = description(input.description)?;
    let (mut catalog, etag) = app_storage::read::<Catalog>(repo, CATALOG)
        .await?
        .ok_or(Error::LabelNotFound)?;
    let index = catalog
        .labels
        .iter()
        .position(|label| label.number == number)
        .ok_or(Error::LabelNotFound)?;
    if catalog.labels[index].version != input.version {
        return Err(Error::Conflict);
    }
    if catalog
        .labels
        .iter()
        .enumerate()
        .any(|(other, label)| other != index && same_name(&label.name, &name))
    {
        return Err(Error::LabelConflict);
    }
    let label = &mut catalog.labels[index];
    label.name = name;
    label.color = color;
    label.description = description;
    label.version = label
        .version
        .checked_add(1)
        .filter(|version| *version < app_storage::MAX_NUMBER)
        .ok_or(Error::Conflict)?;
    label.updated_at = app_storage::now()?;
    let label = label.clone();
    catalog
        .labels
        .sort_by_cached_key(|label| label.name.to_lowercase());
    if !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    app_storage::update(repo, CATALOG, &catalog, etag).await?;
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
    let number = app::number(number)?;
    let (mut catalog, etag) = app_storage::read::<Catalog>(repo, CATALOG)
        .await?
        .ok_or(Error::LabelNotFound)?;
    if let Some(deleted) = catalog.deleted.iter().find(|label| label.number == number) {
        return if deleted.version == input.version {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(Error::LabelNotFound)
        };
    }
    let index = catalog
        .labels
        .iter()
        .position(|label| label.number == number)
        .ok_or(Error::LabelNotFound)?;
    if catalog.labels[index].version != input.version {
        return Err(Error::Conflict);
    }
    catalog.labels.remove(index);
    catalog.deleted.push(DeletedLabel {
        number,
        version: input.version,
    });
    if !principal.can_write(&repo.config) {
        return Err(Error::LabelPermission);
    }
    app_storage::update(repo, CATALOG, &catalog, etag).await?;
    Ok(StatusCode::NO_CONTENT)
}
