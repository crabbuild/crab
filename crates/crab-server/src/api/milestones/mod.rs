use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{
        Decision, Subject, SubjectKind,
        milestones::{
            AssignMilestone, AssignMilestoneInput, CreateMilestone, CreateMilestoneInput,
            EditMilestone, EditMilestoneInput, GetAssignment, GetMilestone, ListMilestoneItems,
            ListMilestoneItemsInput, ListMilestones, ListMilestonesInput, Milestone,
            MilestoneState, validate_fields,
        },
    },
    server::Server,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMilestone {
    request_id: String,
    title: String,
    #[serde(default)]
    description: String,
    due_on: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MilestoneEdit {
    version: u64,
    title: String,
    description: String,
    due_on: Option<String>,
    state: Option<MilestoneState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MilestoneSelection {
    version: u64,
    milestone: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<u8>,
    state: Option<String>,
}

impl ListParameters {
    fn limit(&self) -> Result<u8> {
        let limit = self.limit.unwrap_or(30);
        if !(1..=50).contains(&limit) {
            return Err(Error::Invalid("Page size must be 1–50"));
        }
        Ok(limit)
    }

    fn before(&self) -> Result<Option<u64>> {
        if self
            .before
            .is_some_and(|value| value == 0 || value > app::MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid page cursor"));
        }
        Ok(self.before)
    }
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/milestones",
            get(list).post(create),
        )
        .route(
            "/api/repos/{owner}/{name}/milestones/{number}",
            get(detail).patch(edit),
        )
        .route(
            "/api/repos/{owner}/{name}/milestones/{number}/items",
            get(items),
        )
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/milestone",
            get(selection)
                .put(assign)
                .layer(Extension(SubjectKind::Issue)),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/milestone",
            get(selection)
                .put(assign)
                .layer(Extension(SubjectKind::Pull)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn applied<T>(value: Decision<T>) -> Result<T> {
    match value {
        Decision::Applied(value) => Ok(value),
        Decision::Archived => Err(Error::Archived),
        Decision::NotFound => Err(Error::NotFound),
        Decision::Conflict => Err(Error::Conflict),
        Decision::Forbidden => Err(Error::MilestonePermission),
        Decision::NameConflict => Err(Error::MilestoneConflict),
        Decision::LimitReached => Err(Error::Invalid(
            "A repository supports at most 1,000 milestones",
        )),
    }
}

fn view(value: &Milestone) -> Value {
    // Repository membership grants content access, not a need for internal
    // identity-provider subjects. Public milestone authors are display names.
    json!({
        "number":value.number,"title":value.title,"description":value.description,
        "state":value.state,"due_on":value.due_on,"author":value.author.name,
        "open_count":value.open_count,"closed_count":value.closed_count,
        "version":value.version,"created_at":value.created_at,"updated_at":value.updated_at,
        "closed_at":value.closed_at,
    })
}

fn validate(title: &str, description: &str, due_on: Option<&str>) -> Result<()> {
    validate_fields(title, description, due_on).map_err(|error| match error {
        crate::cells::Error::Invalid(message) => Error::Invalid(message),
        other => Error::Cell(other),
    })
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    app::actor(&principal)?;
    let state = match params.state.as_deref().unwrap_or("open") {
        "open" => Some(MilestoneState::Open),
        "closed" => Some(MilestoneState::Closed),
        "all" => None,
        _ => {
            return Err(Error::Invalid(
                "Milestone state must be open, closed or all",
            ));
        }
    };
    let routed = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        routed
            .client
            .query::<ListMilestones>(
                &routed.target,
                None,
                ListMilestonesInput {
                    before: params.before()?,
                    limit: params.limit()?,
                    state,
                },
            )
            .await,
    )?;
    Ok(Json(
        json!({"items":page.items.iter().map(view).collect::<Vec<_>>(),"next":page.next,"can_manage":principal.can_write(&repo.config)}),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    app::actor(&principal)?;
    let routed = app::repository_cell(&server, &repo)?;
    let value = app::query_output(
        routed
            .client
            .query::<GetMilestone>(&routed.target, None, app::number(number)?)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    Ok(Json(
        json!({"milestone":view(&value),"can_manage":principal.can_write(&repo.config)}),
    ))
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewMilestone>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>)> {
    let repo = app::repository(&server, &principal, &key)?;
    if !principal.can_write(&repo.config) {
        return Err(Error::MilestonePermission);
    }
    let author = app::actor(&principal)?;
    let Json(input) = input?;
    let title = app::title(&input.title)?;
    validate(&title, &input.description, input.due_on.as_deref())?;
    let routed = app::repository_cell(&server, &repo)?;
    let value = applied(app::command_output(
        routed
            .client
            .command::<CreateMilestone>(
                &routed.target,
                app::mutation_identity()?,
                CreateMilestoneInput {
                    submission_id: app::submission_id(&input.request_id)?,
                    author: (&author).into(),
                    title,
                    description: input.description,
                    due_on: input.due_on,
                },
            )
            .await,
    )?)?;
    Ok((StatusCode::CREATED, Json(view(&value))))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<MilestoneEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::MilestonePermission);
    }
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    let title = app::title(&input.title)?;
    validate(&title, &input.description, input.due_on.as_deref())?;
    let routed = app::repository_cell(&server, &repo)?;
    let value = applied(app::command_output(
        routed
            .client
            .command::<EditMilestone>(
                &routed.target,
                app::mutation_identity()?,
                EditMilestoneInput {
                    number: app::number(number)?,
                    version: input.version,
                    actor: (&actor).into(),
                    title,
                    description: input.description,
                    due_on: input.due_on,
                    state: input.state,
                },
            )
            .await,
    )?)?;
    Ok(Json(view(&value)))
}

async fn selection(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<SubjectKind>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    app::actor(&principal)?;
    let subject = Subject {
        kind,
        number: app::number(number)?,
    };
    let routed = app::repository_cell(&server, &repo)?;
    let value = app::query_output(
        routed
            .client
            .query::<GetAssignment>(&routed.target, None, subject)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let permissions = app::membership(&server, &principal, &repo).await?;
    Ok(Json(
        json!({"milestone":value.milestone.as_ref().map(view),"version":value.version,"can_manage":permissions.can_triage}),
    ))
}

async fn assign(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<SubjectKind>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<MilestoneSelection>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !app::membership(&server, &principal, &repo)
        .await?
        .can_triage
    {
        return Err(Error::MilestonePermission);
    }
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    let routed = app::repository_cell(&server, &repo)?;
    let value = applied(app::command_output(
        routed
            .client
            .command::<AssignMilestone>(
                &routed.target,
                app::mutation_identity()?,
                AssignMilestoneInput {
                    subject: Subject {
                        kind,
                        number: app::number(number)?,
                    },
                    milestone: input.milestone.map(app::number).transpose()?,
                    version: input.version,
                    actor: (&actor).into(),
                },
            )
            .await,
    )?)?;
    Ok(Json(
        json!({"milestone":value.milestone.as_ref().map(view),"version":value.version,"can_manage":true}),
    ))
}

async fn items(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    app::actor(&principal)?;
    let routed = app::repository_cell(&server, &repo)?;
    let input = ListMilestoneItemsInput {
        number: app::number(number)?,
        before: params.before()?,
        limit: params.limit()?,
    };
    let page = app::query_output(
        routed
            .client
            .query::<ListMilestoneItems>(&routed.target, None, input)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    Ok(Json(json!(page)))
}

#[cfg(test)]
mod tests;
