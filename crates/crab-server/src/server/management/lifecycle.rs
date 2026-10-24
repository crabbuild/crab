use crate::cells::repository::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
};
use crate::{
    app::{self, Error, Result},
    cells::repository::issues::lifecycle::{GetIssueLifecycleStatus, PrepareIssueLifecycle},
    server::Server,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use std::sync::Arc;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/internal/repositories/{owner}/{name}/issue-state",
            get(status::<GetIssueLifecycleStatus>).post(prepare::<PrepareIssueLifecycle>),
        )
        .route(
            "/internal/repositories/{owner}/{name}/pull-state",
            get(status::<crate::cells::repository::GetPullLifecycleStatus>)
                .post(prepare::<crate::cells::repository::PreparePullLifecycle>),
        )
        .route(
            "/internal/repositories/{owner}/{name}/review-state",
            get(status::<crate::cells::repository::review_dismissals::GetReviewState>)
                .post(prepare::<crate::cells::repository::review_dismissals::PrepareReviewState>),
        )
        .route(
            "/internal/repositories/{owner}/{name}/discussion-submissions/{kind}",
            get(submissions_status).post(prepare_submissions),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024))
}
async fn status<R: crate::cells::application::Read<Input = (), Output = LifecycleStatus>>(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
) -> Result<Json<LifecycleStatus>> {
    let repository = server.repositories.get(&key).ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    Ok(Json(app::query_output(
        cell.client.query::<R>(&cell.target, None, ()).await,
    )?))
}
async fn prepare<
    M: crate::cells::application::Mutation<
            Input = PrepareLifecycleInput,
            Output = PrepareLifecycleOutcome,
        >,
>(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
    Json(input): Json<PrepareLifecycleInput>,
) -> Result<Json<LifecycleStatus>> {
    if server.cancellation.is_cancelled() {
        return Err(Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| Error::CellUnavailable)?;
    let repository = server.repositories.get(&key).ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    match app::command_output(
        cell.client
            .command::<M>(&cell.target, app::mutation_identity()?, input)
            .await,
    )? {
        PrepareLifecycleOutcome::Applied(status) => Ok(Json(status)),
        PrepareLifecycleOutcome::Conflict(_) => Err(Error::Conflict),
    }
}

async fn submissions_status(
    State(server): State<Arc<Server>>,
    Path((owner, name, kind)): Path<(
        String,
        String,
        crate::cells::repository::collaboration::submissions::Kind,
    )>,
) -> Result<Json<LifecycleStatus>> {
    let repository = server
        .repositories
        .get(&(owner, name))
        .ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    Ok(Json(app::query_output(
        cell.client
            .query::<crate::cells::repository::collaboration::submissions::preparation::GetPreparation>(
                &cell.target,
                None,
                kind,
            )
            .await,
    )?))
}
async fn prepare_submissions(
    State(server): State<Arc<Server>>,
    Path((owner, name, kind)): Path<(
        String,
        String,
        crate::cells::repository::collaboration::submissions::Kind,
    )>,
    Json(input): Json<PrepareLifecycleInput>,
) -> Result<Json<LifecycleStatus>> {
    if server.cancellation.is_cancelled() {
        return Err(Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| Error::CellUnavailable)?;
    let repository = server
        .repositories
        .get(&(owner, name))
        .ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    match app::command_output(
        cell.client
            .command::<crate::cells::repository::collaboration::submissions::preparation::Prepare>(
                &cell.target,
                app::mutation_identity()?,
                crate::cells::repository::collaboration::submissions::preparation::Input {
                    kind,
                    version: input.version,
                },
            )
            .await,
    )? {
        PrepareLifecycleOutcome::Applied(status) => Ok(Json(status)),
        PrepareLifecycleOutcome::Conflict(_) => Err(Error::Conflict),
    }
}
