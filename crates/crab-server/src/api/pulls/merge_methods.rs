use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        GetEffectiveMergeMethods, GetMergeMethods, MergeMethod, MergeMethods, ReplaceMergeMethods,
        ReplaceMergeMethodsInput, ReplaceMergeMethodsOutcome,
    },
    repository::{
        Repository,
        settings::{RulesetScope, with_ruleset_lease},
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/settings/merge-methods",
        get(read).put(replace),
    )
}

pub(super) async fn load(server: &Server, repo: &Repository) -> Result<MergeMethods> {
    let routed = app::repository_cell(server, repo)?;
    app::query_output(
        routed
            .client
            .query::<GetMergeMethods>(&routed.target, None, ())
            .await,
    )
}

pub(super) async fn load_for_ref(
    server: &Server,
    repo: &Repository,
    reference: &str,
) -> Result<MergeMethods> {
    let routed = app::repository_cell(server, repo)?;
    app::query_output(
        routed
            .client
            .query::<GetEffectiveMergeMethods>(&routed.target, None, reference.to_owned())
            .await,
    )
}

pub(super) async fn require(server: &Server, repo: &Repository, method: MergeMethod) -> Result<()> {
    if !load(server, repo).await?.enabled.contains(&method) {
        return Err(Error::MergeMethodDisabled);
    }
    Ok(())
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<MergeMethods>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let grant = app::membership(&server, &principal, &repo).await?;
    if !grant.principal.can_admin(repo.id) {
        return Err(Error::MergeSettingsPermission);
    }
    Ok(Json(load(&server, &repo).await?))
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<ReplaceMergeMethodsInput>,
) -> Result<Json<MergeMethods>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !input.valid() {
        return Err(Error::Invalid(
            "Choose valid queue settings and distinct exact branch names",
        ));
    }
    let grant = app::membership(&server, &principal, &repo).await?;
    if !grant.principal.can_admin(repo.id) {
        return Err(Error::MergeSettingsPermission);
    }
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let server = server.as_ref();
        let repo = repo.as_ref();
        let outcome = with_ruleset_lease(
            server,
            RulesetScope::Repository(repo),
            &server.cancellation,
            |cancel| async move {
                if !app::membership(server, &principal, repo)
                    .await?
                    .principal
                    .can_admin(repo.id)
                {
                    return Err(Error::MergeSettingsPermission);
                }
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                let routed = app::repository_cell(server, repo)?;
                app::command_output(
                    routed
                        .client
                        .command::<ReplaceMergeMethods>(
                            &routed.target,
                            app::mutation_identity()?,
                            input,
                        )
                        .await,
                )
            },
        )
        .await;
        let _ = send.send(outcome);
    });
    let outcome = reply.await.map_err(|_| Error::MergeBusy)??;
    match outcome {
        ReplaceMergeMethodsOutcome::Saved(settings) => Ok(Json(settings)),
        ReplaceMergeMethodsOutcome::Conflict => Err(Error::MergeMethodsChanged),
        ReplaceMergeMethodsOutcome::Archived => Err(Error::Archived),
    }
}
