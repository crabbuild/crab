use super::*;
use crate::{
    api::write,
    cells::repository::{PullChildKey, review_dismissals},
    repository::settings::{self, RulesetScope, with_ruleset_lease},
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
    let pull_number = app::number(pull)?;
    let key = PullChildKey {
        pull: pull_number,
        number: app::number(review)?,
    };
    let submission_id = storage::submission_id(&input.request_id)?;
    let version = app::number(input.version)?;
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let outcome = with_ruleset_lease(
            &server,
            RulesetScope::Repository(&repo),
            &server.cancellation,
            |cancel| {
                let server = server.as_ref();
                let repo = repo.as_ref();
                async move {
                    let pull = storage::pull(server, repo, pull_number)
                        .await?
                        .ok_or(Error::NotFound)?;
                    let grant = app::membership(server, &principal, repo).await?;
                    let can_write = grant.principal.can_write(repo.id);
                    let policy = if can_write {
                        settings::load_ref_policy(server, repo)
                            .await?
                            .review_dismissal(server, repo, &grant, &actor, &pull.base_ref)
                            .await?
                    } else {
                        ReviewDismissalPolicy::default()
                    };
                    if cancel.is_cancelled() {
                        return Err(Error::MergeBusy);
                    }
                    let cell = app::repository_cell(server, repo)?;
                    let outcome = app::command_output(
                        cell.client
                            .command::<review_dismissals::DismissReview>(
                                &cell.target,
                                app::mutation_identity()?,
                                review_dismissals::Input {
                                    submission_id,
                                    key,
                                    version,
                                    actor: (&actor).into(),
                                    body,
                                    can_write,
                                    can_admin: grant.principal.can_admin(repo.id),
                                    configured_protected: configured_protected(
                                        repo,
                                        &pull.base_ref,
                                    ),
                                    base_ref: pull.base_ref,
                                    policy,
                                },
                            )
                            .await,
                    )?;
                    if outcome == review_dismissals::Outcome::Dismissed {
                        app::membership(server, &principal, repo).await?;
                    }
                    Ok(outcome)
                }
            },
        )
        .await;
        let _ = send.send(outcome);
    });
    let outcome = reply.await.map_err(|_| Error::MergeBusy)??;
    use review_dismissals::Outcome;
    let error = match outcome {
        Outcome::Dismissed => {
            return Ok(Json(json!({"dismissed":true})).into_response());
        }
        Outcome::NotFound => Error::NotFound,
        Outcome::Forbidden => Error::ReviewDismissalPermission,
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
