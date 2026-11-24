use super::{
    members::roster::Roster,
    organizations::{issue_types, properties},
    write::rejected,
};
use crate::{
    app::{self, Error},
    auth::Principal,
    cells::repository::{
        assignments::Assignee,
        collaboration::{
            Subject, SubjectKind,
            bulk::{self, Action, Failure, IssueTypeAuthority, Outcome},
        },
    },
    repository::Repository,
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type Result<T> = std::result::Result<T, super::write::ResponseError>;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/discussions/selection",
            get(select),
        )
        .route(
            "/api/repos/{owner}/{name}/discussions/bulk",
            axum::routing::post(edit),
        )
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionQuery {
    repository_id: uuid::Uuid,
    items: String,
}
async fn select(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(query): Query<SelectionQuery>,
) -> app::Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    if query.repository_id != repo.id {
        return Err(Error::Invalid(
            "Repository identity changed; reload the selection",
        ));
    }
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_triage(repo.id)
    {
        return Err(Error::Forbidden);
    }
    if query.items.len() > 8192 {
        return Err(Error::Invalid("Selection is too large"));
    }
    let items = query
        .items
        .split(',')
        .map(|value| {
            let parts: Vec<_> = value.split(':').collect();
            let [kind, number, version] = parts.as_slice() else {
                return Err(Error::Invalid("Invalid discussion selection"));
            };
            let kind = match *kind {
                "issue" => SubjectKind::Issue,
                "pull" => SubjectKind::Pull,
                _ => return Err(Error::Invalid("Invalid discussion kind")),
            };
            Ok(bulk::Versioned {
                subject: Subject {
                    kind,
                    number: number
                        .parse()
                        .map_err(|_| Error::Invalid("Invalid discussion number"))?,
                },
                version: version
                    .parse()
                    .map_err(|_| Error::Invalid("Invalid discussion version"))?,
            })
        })
        .collect::<app::Result<Vec<_>>>()?;
    let routed = app::repository_cell(&server, &repo)?;
    let selected = app::query_output(
        routed
            .client
            .query::<bulk::Select>(&routed.target, None, items)
            .await,
    )?;
    let items = match selected {
        Outcome::Applied(items) => items,
        Outcome::Rejected(reason) => return Err(failure(reason)),
    };
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({"repository_id":repo.id,"items":items})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    repository_id: uuid::Uuid,
    request_id: String,
    items: Vec<bulk::Selected>,
    action: Action,
}
async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key).map_err(IntoResponse::into_response)?;
    let Json(input) = input
        .map_err(Error::from)
        .map_err(IntoResponse::into_response)?;
    if input.repository_id != repo.id {
        return Err(Error::Invalid(
            "Repository identity changed; recover the original action from its repository",
        )
        .into_response()
        .into());
    }
    let actor = app::actor(&principal).map_err(IntoResponse::into_response)?;
    let submission_id =
        app::submission_id(&input.request_id).map_err(IntoResponse::into_response)?;
    let eligible = if let Action::Assignees { add, .. } = &input.action {
        let roster = Roster::load(&server, &principal, &repo)
            .await
            .map_err(IntoResponse::into_response)?;
        let mut eligible: Vec<_> = roster
            .items
            .into_iter()
            .filter(|person| add.contains(&person.id))
            .map(|person| Assignee {
                id: person.id,
                name: person.name,
            })
            .collect();
        eligible.sort_unstable_by_key(|person| person.id);
        eligible
    } else {
        vec![]
    };
    let candidates = if matches!(
        input.action,
        Action::State {
            state: crate::cells::repository::collaboration::state::State::Open,
            ..
        }
    ) && input.items.len() <= bulk::MAX_ITEMS
    {
        input
            .items
            .iter()
            .filter_map(|selected| {
                let item = &selected.item;
                (item.subject.kind == SubjectKind::Pull
                    && app::number(item.subject.number).is_ok()
                    && app::number(item.version).is_ok())
                .then_some((item.subject.number, item.version))
            })
            .collect()
    } else {
        vec![]
    };
    let command = bulk::Input {
        submission_id,
        actor: (&actor).into(),
        can_triage: false,
        items: input.items,
        action: input.action,
        eligible,
        reopen_available: Default::default(),
        issue_type_authority: None,
    };
    let output = if let (Action::IssueType { type_id }, Some(organization)) =
        (&command.action, repo.organization)
    {
        let type_id = *type_id;
        let source = Arc::clone(&repo);
        let author = principal.clone();
        // Hold the same lease as individual assignments and schema changes
        // through the commit of all selected issues, even if the client leaves.
        properties::policy_write(
            Arc::clone(&server),
            organization,
            move |server, _, cancel| async move {
                let mut command = command;
                command.issue_type_authority = match type_id {
                    Some(id) => issue_types::active_selection(&server, &source, id)
                        .await?
                        .map(IssueTypeAuthority::Assign),
                    None => Some(IssueTypeAuthority::Clear),
                };
                let grant = app::membership(&server, &author, &source).await?;
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                command.can_triage = grant.principal.can_triage(source.id);
                submit(&server, &source, command).await
            },
        )
        .await
    } else {
        let worker = Arc::clone(&server);
        let source = Arc::clone(&repo);
        super::pulls::reopen::with_available(
            &server,
            &principal,
            &repo,
            candidates,
            super::pulls::reopen::Access::Triage,
            move |reopen_available, grant| async move {
                let mut command = command;
                command.can_triage = grant.principal.can_triage(source.id);
                command.reopen_available = reopen_available;
                submit(&worker, &source, command).await
            },
        )
        .await
    }
    .map_err(IntoResponse::into_response)?;
    let count = match output {
        Outcome::Applied(count) => count,
        Outcome::Rejected(Failure::RequestConflict) => {
            return Err(Error::RequestConflict.into_response().into());
        }
        Outcome::Rejected(reason) => return Err(rejected(failure(reason)).into()),
    };
    app::membership(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(Json(json!({"count":count})))
}

async fn submit(
    server: &Server,
    repo: &Repository,
    input: bulk::Input,
) -> app::Result<Outcome<u16>> {
    let routed = app::repository_cell(server, repo)?;
    app::command_output(
        routed
            .client
            .command::<bulk::Edit>(&routed.target, app::mutation_identity()?, input)
            .await,
    )
}

fn failure(reason: Failure) -> Error {
    match reason {
        Failure::Invalid => {
            Error::Invalid("Select 1–100 distinct, versioned discussions and a valid change")
        }
        Failure::Archived => Error::Archived,
        Failure::Forbidden => Error::Forbidden,
        Failure::NotFound => Error::NotFound,
        Failure::Conflict => Error::Conflict,
        Failure::RequestConflict => Error::RequestConflict,
        Failure::MergePending => Error::MergePending,
        Failure::BranchesUnavailable => Error::PullBranchesUnavailable,
        Failure::InvalidState => {
            Error::Invalid("The selection contains a discussion that cannot enter that state")
        }
        Failure::InvalidLabels => {
            Error::Invalid("Labels must exist and each item supports at most 20 labels")
        }
        Failure::InvalidAssignees => Error::Invalid(
            "Add current repository members; each item supports at most 10 assignees",
        ),
        Failure::InvalidIssueType => {
            Error::Invalid("Choose an enabled issue type from the repository's organization")
        }
    }
}

#[cfg(test)]
mod tests;
