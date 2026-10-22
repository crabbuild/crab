use super::write::rejected;
use crate::{
    api::{issues, profiles::attribution::Authors, pulls, reactions},
    app::{self, Error},
    auth::Principal,
    cells::repository::collaboration::{
        Subject, SubjectKind, Target,
        state::{self, ChangeState, Outcome},
    },
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type Result<T> = std::result::Result<T, Response>;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/state",
            post(issue),
        )
        .route("/api/repos/{owner}/{name}/pulls/{number}/state", post(pull))
        .layer(axum::extract::DefaultBodyLimit::max(80 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    request_id: String,
    version: u64,
    state: state::State,
    reason: Option<crate::cells::repository::issues::lifecycle::Reason>,
    body: Option<String>,
}
async fn issue(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<Value>> {
    change(
        &server,
        &principal,
        (owner, name),
        Subject {
            kind: SubjectKind::Issue,
            number,
        },
        input
            .map_err(Error::from)
            .map_err(IntoResponse::into_response)?
            .0,
    )
    .await
}
async fn pull(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<Value>> {
    change(
        &server,
        &principal,
        (owner, name),
        Subject {
            kind: SubjectKind::Pull,
            number,
        },
        input
            .map_err(Error::from)
            .map_err(IntoResponse::into_response)?
            .0,
    )
    .await
}
async fn change(
    server: &Server,
    principal: &Principal,
    key: (String, String),
    mut subject: Subject,
    input: Input,
) -> Result<Json<Value>> {
    let repo = app::repository(server, principal, &key).map_err(IntoResponse::into_response)?;
    subject.number = app::number(subject.number).map_err(rejected)?;
    app::number(input.version).map_err(rejected)?;
    if let Some(body) = &input.body {
        app::body(body, true).map_err(rejected)?;
    }
    let author = app::actor(principal).map_err(IntoResponse::into_response)?;
    let membership = app::membership(server, principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    let routed = app::repository_cell(server, &repo).map_err(IntoResponse::into_response)?;
    let identity = app::mutation_identity().map_err(IntoResponse::into_response)?;
    let submission_id = app::submission_id(&input.request_id).map_err(rejected)?;
    let output = app::command_output(
        routed
            .client
            .command::<ChangeState>(
                &routed.target,
                identity,
                state::Input {
                    submission_id,
                    subject,
                    actor: (&author).into(),
                    can_triage: membership.principal.can_triage(repo.id),
                    version: input.version,
                    state: input.state,
                    reason: input.reason,
                    body: input.body,
                },
            )
            .await,
    )
    .map_err(IntoResponse::into_response)?;
    let comment = match output {
        Outcome::Applied(comment) => comment,
        Outcome::Rejected(state::Rejection::Archived) => return Err(rejected(Error::Archived)),
        Outcome::Rejected(state::Rejection::NotFound) => return Err(rejected(Error::NotFound)),
        Outcome::Rejected(state::Rejection::Forbidden) => return Err(rejected(Error::Forbidden)),
        Outcome::Rejected(state::Rejection::Conflict) => return Err(rejected(Error::Conflict)),
        Outcome::RequestConflict => return Err(Error::RequestConflict.into_response()),
        Outcome::Rejected(state::Rejection::InvalidState) => {
            return Err(rejected(Error::Invalid(
                "The requested discussion state or close reason is invalid",
            )));
        }
        Outcome::Rejected(state::Rejection::MergePending) => {
            return Err(rejected(Error::MergePending));
        }
    };
    let value = if let Some(comment) = comment {
        let (actor, target) = match &comment {
            state::Comment::Issue(value) => (
                &value.author,
                Target::IssueComment {
                    issue: subject.number,
                    number: value.number,
                },
            ),
            state::Comment::Pull(value) => (
                &value.author,
                Target::PullComment {
                    pull: subject.number,
                    number: value.number,
                },
            ),
        };
        let authors = Authors::load(server, principal, std::iter::once(actor))
            .await
            .map_err(IntoResponse::into_response)?;
        let reaction = reactions::one(server, &repo, &author, target)
            .await
            .map_err(IntoResponse::into_response)?;
        match comment {
            state::Comment::Issue(value) => {
                issues::view::comment(&authors, &value, &author, &reaction)
            }
            state::Comment::Pull(value) => pulls::comments::comment_view(
                &authors,
                &pulls::storage::from_comment(value),
                &author,
                &reaction,
            ),
        }
    } else {
        Value::Null
    };
    // Enrichment can cross Directory/native awaits; response visibility still
    // requires the caller's current repository membership after that work.
    app::membership(server, principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(Json(json!({"comment":value})))
}

#[cfg(test)]
mod tests;
