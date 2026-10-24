use super::*;
use crate::{
    api::write,
    cells::repository::{PullChildKey, review_dismissals},
};
use axum::routing::put;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/reviews/{review}/dismissals",
        put(dismiss),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    request_id: String,
    version: u64,
    body: String,
}
async fn dismiss(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, pull, review)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<axum::response::Response> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let body = match app::body(&input.body, true) {
        Ok(()) => input.body,
        Err(error) => return Ok(write::rejected(error)),
    };
    let principal = app::membership(&server, &principal, &repo).await?.principal;
    let pull_number = app::number(pull)?;
    let pull = storage::pull(&server, &repo, pull_number)
        .await?
        .ok_or(Error::NotFound)?;
    let configured_protected = configured_protected(&repo, &pull.base_ref);
    let cell = app::repository_cell(&server, &repo)?;
    let outcome = app::command_output(
        cell.client
            .command::<review_dismissals::DismissReview>(
                &cell.target,
                app::mutation_identity()?,
                review_dismissals::Input {
                    submission_id: storage::submission_id(&input.request_id)?,
                    key: PullChildKey {
                        pull: pull_number,
                        number: app::number(review)?,
                    },
                    version: app::number(input.version)?,
                    actor: (&actor).into(),
                    body,
                    can_write: principal.can_write(repo.id),
                    can_admin: principal.can_admin(repo.id),
                    configured_protected,
                },
            )
            .await,
    )?;
    use review_dismissals::Outcome;
    let error = match outcome {
        Outcome::Dismissed => {
            app::membership(&server, &principal, &repo).await?;
            return Ok(Json(json!({"dismissed":true})).into_response());
        }
        Outcome::NotFound => Error::NotFound,
        Outcome::Forbidden => Error::Forbidden,
        Outcome::Archived => Error::Archived,
        Outcome::Conflict => Error::Conflict,
        Outcome::RequestConflict => return Err(Error::RequestConflict),
        Outcome::MergePending => Error::MergePending,
        Outcome::InvalidState => {
            Error::Invalid("Only an active approval or change request can be dismissed")
        }
    };
    Ok(write::rejected(error))
}
