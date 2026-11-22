use super::*;
use crate::cells::repository::{ReviewState, pending_reviews as pending};
use uuid::Uuid;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/pending-review",
            get(detail).put(change),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/pending-review/submit",
            axum::routing::post(submit),
        )
}

fn view(draft: pending::Draft) -> Value {
    let comments = draft
        .comments
        .into_iter()
        .map(|comment| {
            json!({
                "id": Uuid::from_bytes(comment.submission_id),
                "path": api::display_path(&comment.path), "path_hex": path_hex(&comment.path),
                "base_oid":comment.base_oid, "head_oid":comment.head_oid,
                "side":comment.side, "start_line":comment.start_line, "end_line":comment.end_line,
                "body":comment.body, "suggested_text":comment.suggested_text,
            })
        })
        .collect::<Vec<_>>();
    json!({"version":draft.version,"comments":comments})
}

async fn draft(server: &Server, repo: &Repository, key: pending::Key) -> Result<pending::Draft> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<pending::Get>(&cell.target, None, key)
            .await,
    )?
    .ok_or(Error::NotFound)
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let draft = draft(
        &server,
        &repo,
        pending::Key {
            pull: app::number(number)?,
            actor: (&actor).into(),
        },
    )
    .await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(view(draft)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeInput {
    expected: Option<Uuid>,
    request_id: Uuid,
    edit: Edit,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Edit {
    Save {
        comment: NewThread,
        replace: Option<Uuid>,
    },
    Remove {
        id: Uuid,
    },
    Discard,
}

async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<ChangeInput>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let pull = app::number(number)?;
    let Json(input) = input?;
    let edit = match input.edit {
        Edit::Save { comment, replace } => {
            if pull_storage::submission_id(&comment.request_id)? != *input.request_id.as_bytes() {
                return Err(Error::Invalid("Pending comment request IDs must match"));
            }
            let record = load_pull(&server, &repo, pull).await?;
            let comparison = comparison(&server, &repo, &record).await?;
            if !comparison.live {
                return Err(Error::Conflict);
            }
            let comment = prepare_thread(&server, pull, &actor, &comparison, comment).await?;
            pending::Edit::Save { comment, replace }
        }
        Edit::Remove { id } => pending::Edit::Remove(id),
        Edit::Discard => pending::Edit::Discard,
    };
    app::membership(&server, &principal, &repo).await?;
    let cell = app::repository_cell(&server, &repo)?;
    let outcome = app::command_output(
        cell.client
            .command::<pending::Change>(
                &cell.target,
                app::mutation_identity()?,
                pending::EditInput {
                    key: pending::Key {
                        pull,
                        actor: (&actor).into(),
                    },
                    expected: input.expected,
                    request_id: input.request_id,
                    edit,
                },
            )
            .await,
    )?;
    match outcome {
        pending::Edited::Saved(draft) => Ok(Json(view(draft))),
        pending::Edited::NotFound => Err(Error::NotFound),
        pending::Edited::Archived => Err(Error::Archived),
        pending::Edited::Conflict => Err(Error::Conflict),
        pending::Edited::AtCapacity => Err(Error::Invalid(
            "A pending review can hold at most 20 comments and 96 KiB of draft data",
        )),
        pending::Edited::Closed => Err(Error::Invalid("Closed pull requests cannot be reviewed")),
        pending::Edited::MergePending => Err(Error::MergePending),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitInput {
    expected: Uuid,
    request_id: Uuid,
    body: String,
    state: ReviewState,
    base_revision: Option<u64>,
}

async fn submit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<SubmitInput>, JsonRejection>,
) -> Result<Json<pending::Published>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let pull = app::number(number)?;
    let Json(input) = input?;
    app::body(&input.body, false)?;
    let key = pending::Key {
        pull,
        actor: (&actor).into(),
    };
    let saved = draft(&server, &repo, key.clone()).await?;
    // A consumed draft is handled by its durable receipt, even if the old
    // branch has moved or disappeared since publication.
    let mut source_error = None;
    let source = if saved.version == Some(input.expected) && !saved.comments.is_empty() {
        let record = load_pull(&server, &repo, pull).await?;
        match comparison(&server, &repo, &record).await {
            Ok(source) if source.live => Some((source.base_oid, source.head_oid)),
            Ok(_) => None,
            Err(error) => {
                source_error = Some(error);
                None
            }
        }
    } else {
        None
    };
    let cell = app::repository_cell(&server, &repo)?;
    let membership = app::membership(&server, &principal, &repo).await?;
    let outcome = app::command_output(
        cell.client
            .command::<pending::Submit>(
                &cell.target,
                app::mutation_identity()?,
                pending::SubmitInput {
                    review_teams: membership.review_teams,
                    key,
                    expected: input.expected,
                    request_id: input.request_id,
                    body: input.body,
                    state: input.state,
                    comparison: source,
                    base_revision: input.base_revision,
                },
            )
            .await,
    )?;
    match outcome {
        pending::Submitted::Published(result) => Ok(Json(result)),
        pending::Submitted::Conflict => Err(Error::Conflict),
        pending::Submitted::ComparisonUnavailable => Err(source_error.unwrap_or(Error::Conflict)),
        pending::Submitted::RequestConflict => Err(Error::RequestConflict),
        pending::Submitted::ReviewRejected(reason) => {
            pull_storage::review_result(reason)?;
            Err(Error::CellContract(
                "pending review rejection includes a created record",
            ))
        }
    }
}
