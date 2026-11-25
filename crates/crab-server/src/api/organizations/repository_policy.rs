use super::*;
use crate::{
    cells::organization::repository_policy::{Get, Policy, Set, Update},
    repository::settings::{RulesetScope, with_ruleset_lease},
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/organizations/{id}/repository-policy",
        get(read).put(save),
    )
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Policy>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let policy = cells
        .query::<Get>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(policy))
}

async fn save(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(policy): Json<Policy>,
) -> Result<Json<Policy>> {
    policy.validate().map_err(Error::Invalid)?;
    let actor = actor(&server, &principal).await?;
    let address = target(id)?;
    let worker = Arc::clone(&server);
    let task = server.receives.spawn(async move {
        let worker = worker.as_ref();
        with_ruleset_lease(
            worker,
            RulesetScope::Organization(id),
            &worker.cancellation,
            |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                let cells = worker.cells().ok_or(Error::CellUnavailable)?;
                let saved = cells
                    .command::<Set>(address, app::mutation_identity()?, Update { actor, policy })
                    .await
                    .map_err(read_error)?
                    .output;
                saved.ok_or(Error::Conflict)
            },
        )
        .await
    });
    let policy = task.await.map_err(|_| Error::MergeBusy)??;
    app::account(&principal)?;
    Ok(Json(policy))
}
