use super::*;
use crate::git::receive::{self, BranchRestorePublication, ReceiveError};
use gix_hash::ObjectId;
use serde::Serialize;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/restore-branch",
        get(availability).post(restore),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    request_id: Uuid,
    version: u64,
    repository_id: Uuid,
    head_oid: String,
}

#[derive(Serialize)]
struct Branch {
    version: u64,
    repository_id: Uuid,
    head_oid: String,
}

fn saved_head(pull: &PullRequest) -> Option<&str> {
    if pull.state == PullState::Open || pull.merge_pending.is_some() {
        return None;
    }
    Some(
        pull.merge
            .as_ref()
            .map_or_else(|| pull.comparison_snapshot().1, |merge| &merge.head_oid),
    )
}

fn source(
    server: &Server,
    upstream: &Arc<Repository>,
    pull: &PullRequest,
) -> Result<Arc<Repository>> {
    pull.head_repository.map_or_else(
        || Ok(Arc::clone(upstream)),
        |id| git::fork_repository(server, upstream, id),
    )
}

async fn availability(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let upstream = app::repository(&server, &principal, &(owner, name))?;
    let pull = storage::pull(&server, &upstream, app::number(number)?)
        .await?
        .ok_or(Error::NotFound)?;
    let branch = check_availability(&server, &principal, &upstream, &pull).await?;
    app::membership(&server, &principal, &upstream).await?;
    Ok(Json(json!({ "branch": branch })))
}

async fn check_availability(
    server: &Server,
    principal: &Principal,
    upstream: &Arc<Repository>,
    pull: &PullRequest,
) -> Result<Option<Branch>> {
    let Some(head) = saved_head(pull) else {
        return Ok(None);
    };
    let source = match source(server, upstream, pull) {
        Ok(source) => source,
        Err(Error::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let current = match app::membership(server, principal, &source).await {
        Ok(grant) => grant,
        Err(Error::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    if !current.principal.can_write(source.id)
        || crate::repository::settings::load_lifecycle(server, &source)
            .await?
            .archived
        || source.ref_oid(&pull.head_ref).await?.is_some()
    {
        return Ok(None);
    }
    Ok(Some(Branch {
        version: pull.version,
        repository_id: source.id,
        head_oid: head.to_owned(),
    }))
}

async fn restore(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Json(input): Json<Input>,
) -> Result<impl IntoResponse> {
    let upstream = app::repository(&server, &principal, &(owner, name))?;
    let number = app::number(number)?;
    let pull = storage::pull(&server, &upstream, number)
        .await?
        .ok_or(Error::NotFound)?;
    let head = saved_head(&pull).ok_or(Error::BranchUpdateChanged)?;
    let source = source(&server, &upstream, &pull)?;
    if input.request_id.is_nil()
        || input.version != pull.version
        || input.head_oid != head
        || input.repository_id != source.id
    {
        return Err(Error::BranchUpdateChanged);
    }
    app::membership(&server, &principal, &upstream).await?;
    let actor = app::actor(&principal)?;
    let plan_id = blake3::hash(&serde_json::to_vec(&(
        "restore-pull-branch/v1",
        upstream.id,
        source.id,
        source.ownership_epoch,
        number,
        &actor.issuer,
        &actor.subject,
        input.request_id,
    ))?)
    .to_hex()
    .to_string();
    let commit = ObjectId::from_hex(head.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::CellContract("Saved pull request head is invalid"))?;
    receive::restore_pull_branch(
        server,
        principal,
        pull.head_ref.clone(),
        commit,
        BranchRestorePublication {
            upstream,
            source,
            pull: number,
            version: input.version,
            plan_id,
        },
    )
    .await
    .map_err(restore_error)?;
    Ok(Json(json!({ "branch": pull.head_ref, "commit": head })))
}

fn restore_error(error: ReceiveError) -> Error {
    match error {
        ReceiveError::Forbidden => Error::BranchUpdatePermission,
        ReceiveError::Archived => Error::Archived,
        ReceiveError::Protected => {
            Error::RefRule("Branch protection prevents restoring this branch")
        }
        ReceiveError::RefRule(message) => Error::RefRule(message),
        ReceiveError::BranchChanged
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Stale { .. })
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Namespace(_))
        | ReceiveError::Write(
            crab_write::WriteError::RefChanged { .. } | crab_write::WriteError::Namespace(_),
        ) => Error::BranchRestoreConflict,
        ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Missing { .. })
        | ReceiveError::Remote(crab_remote_git::Error::ObjectNotFound { .. }) => {
            Error::CommitNotFound
        }
        ReceiveError::NotFound => Error::NotFound,
        ReceiveError::Settings(cause) => *cause,
        other => Error::BranchRestore(Box::new(other)),
    }
}

pub(crate) async fn authorize_branch_restore(
    server: &Server,
    principal: &Principal,
    source: &Repository,
    restore: &BranchRestorePublication,
    update: &crab_git::receive_plan::RefUpdate,
) -> std::result::Result<(), ReceiveError> {
    app::membership(server, principal, &restore.upstream).await?;
    let pull = storage::pull(server, &restore.upstream, restore.pull)
        .await
        .map_err(|error| ReceiveError::Settings(Box::new(error)))?
        .ok_or(ReceiveError::NotFound)?;
    let target = self::source(server, &restore.upstream, &pull)?;
    if target.id != source.id
        || target.ownership_epoch != source.ownership_epoch
        || restore.source.id != source.id
        || restore.source.ownership_epoch != source.ownership_epoch
        || pull.version != restore.version
        || update.old.is_some()
        || pull.head_ref != update.name
        || saved_head(&pull) != update.new.map(|oid| oid.to_string()).as_deref()
    {
        return Err(ReceiveError::Settings(Box::new(Error::BranchUpdateChanged)));
    }
    Ok(())
}
