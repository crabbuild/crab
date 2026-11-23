use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::get,
};
use crab_git::receive_plan::RefUpdate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    git::{PullGit, committed_branch_edit},
    merge, storage,
};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    git::receive::{self, BranchUpdatePublication, ReceiveError},
    repository::{Repository, settings},
    server::Server,
};

mod replacement;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/update-branch",
            get(preview)
                .post(update)
                .layer(DefaultBodyLimit::max(merge::conflicts::MAX_REQUEST_BYTES)),
        )
        .route(
            "/api/repos/{owner}/{name}/upstream-pulls/{number}/update-branch",
            get(preview_fork)
                .post(update_fork)
                .layer(DefaultBodyLimit::max(merge::conflicts::MAX_REQUEST_BYTES)),
        )
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Input {
    request_id: Uuid,
    version: u64,
    base_oid: String,
    head_oid: String,
    #[serde(default, skip_serializing_if = "UpdateMethod::is_merge_commit")]
    method: UpdateMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolutions: Option<Vec<merge::conflicts::Edit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    new_branch: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UpdateMethod {
    #[default]
    MergeCommit,
    Rebase,
}

impl UpdateMethod {
    fn is_merge_commit(&self) -> bool {
        // Keep existing merge-update receipt identities recoverable.
        *self == Self::MergeCommit
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Preview {
    conflicts: bool,
}

type PullPath = (String, String, u64);

struct Destination {
    head: Arc<Repository>,
    upstream: Option<Arc<Repository>>,
    number: u64,
}

impl Destination {
    fn for_route(
        server: &Server,
        principal: &Principal,
        path: PullPath,
        fork: bool,
    ) -> Result<Self> {
        let (owner, name, number) = path;
        let head = app::repository(server, principal, &(owner, name))?;
        let upstream = if fork {
            Some(head.fork_source(server).ok_or(Error::NotFound)?)
        } else {
            None
        };
        Ok(Self {
            head,
            upstream,
            number: app::number(number)?,
        })
    }

    fn discussion(&self) -> &Repository {
        self.upstream.as_deref().unwrap_or(&self.head)
    }

    async fn permit(&self, server: &Server, principal: &Principal) -> Result<()> {
        if !app::membership(server, principal, &self.head)
            .await?
            .principal
            .can_write(self.head.id)
        {
            return Err(Error::BranchUpdatePermission);
        }
        Ok(())
    }

    async fn pull(&self, server: &Server) -> Result<storage::PullRequest> {
        associated_pull(server, self.discussion(), &self.head, self.number).await
    }
}

async fn associated_pull(
    server: &Server,
    discussion: &Repository,
    head: &Repository,
    number: u64,
) -> Result<storage::PullRequest> {
    let expected_fork = (head.id != discussion.id).then_some(head.id);
    storage::pull(server, discussion, number)
        .await?
        .filter(|pull| pull.head_repository == expected_fork)
        .ok_or(Error::NotFound)
}

async fn require_active(
    server: &Server,
    upstream: &Repository,
    pull: &storage::PullRequest,
) -> Result<()> {
    if pull.state != storage::PullState::Open || pull.merge_pending.is_some() {
        return Err(Error::BranchUpdateChanged);
    }
    if settings::load_lifecycle(server, upstream).await?.archived {
        return Err(Error::Archived);
    }
    Ok(())
}

async fn capture(
    destination: &Destination,
    pull: &storage::PullRequest,
) -> Result<Option<(String, String, bool)>> {
    let head = destination.head.open_git_view().await?;
    let Some(head_oid) = head.refs().get(&pull.head_ref) else {
        return Ok(None);
    };
    let base_oid = if let Some(upstream) = &destination.upstream {
        upstream.ref_oid(&pull.base_ref).await?
    } else {
        head.refs().get(&pull.base_ref).cloned()
    };
    let Some(base_oid) = base_oid else {
        return Ok(None);
    };
    let includes_base = super::git::head_contains_base(&head, &pull.head_ref, &base_oid).await?;
    Ok(Some((base_oid, head_oid.clone(), includes_base)))
}

async fn preview(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<PullPath>,
    Query(input): Query<Preview>,
) -> Result<Json<Value>> {
    let destination = Destination::for_route(&server, &principal, path, false)?;
    preview_destination(&server, &principal, destination, input).await
}

async fn preview_fork(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<PullPath>,
    Query(input): Query<Preview>,
) -> Result<Json<Value>> {
    let destination = Destination::for_route(&server, &principal, path, true)?;
    preview_destination(&server, &principal, destination, input).await
}

async fn preview_destination(
    server: &Server,
    principal: &Principal,
    destination: Destination,
    input: Preview,
) -> Result<Json<Value>> {
    destination.permit(server, principal).await?;
    let pull = destination.pull(server).await?;
    require_active(server, destination.discussion(), &pull).await?;
    if destination.upstream.is_some()
        && settings::load_lifecycle(server, &destination.head)
            .await?
            .archived
    {
        return Err(Error::Archived);
    }
    let _read = crate::api::read_admission::acquire_bulk(server).map_err(|_| Error::MergeBusy)?;
    let ready =
        capture(&destination, &pull)
            .await?
            .and_then(|(base_oid, head_oid, includes_base)| {
                (!includes_base).then(|| Input {
                    request_id: Uuid::now_v7(),
                    version: pull.version,
                    base_oid,
                    head_oid,
                    method: UpdateMethod::MergeCommit,
                    resolutions: None,
                    new_branch: None,
                })
            });
    let conflicts = if let Some(update) = ready.as_ref().filter(|_| input.conflicts) {
        let git = git_for_update(server, destination.discussion(), &pull).await?;
        let head = crate::api::statuses::parse_oid(&update.head_oid)?;
        let base = crate::api::statuses::parse_oid(&update.base_oid)?;
        merge::preview_conflicts(&git, &server.cancellation, head, base)
            .await
            .map_err(branch_conflict)?
    } else {
        vec![]
    };
    destination.permit(server, principal).await?;
    Ok(Json(json!({"update":ready, "conflicts":conflicts})))
}

async fn update(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<PullPath>,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    let destination = Destination::for_route(&server, &principal, path, false)?;
    update_destination(server, principal, destination, input).await
}

async fn update_fork(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(path): Path<PullPath>,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    let destination = Destination::for_route(&server, &principal, path, true)?;
    update_destination(server, principal, destination, input).await
}

async fn update_destination(
    server: Arc<Server>,
    principal: Principal,
    destination: Destination,
    input: Input,
) -> Result<Json<Value>> {
    destination.permit(&server, &principal).await?;
    let rebase = input.method == UpdateMethod::Rebase;
    if rebase && input.resolutions.is_some() {
        return Err(Error::Invalid(
            "Conflict resolutions require a merge commit",
        ));
    }
    let new_branch = input
        .new_branch
        .as_deref()
        .map(crate::api::branches::branch_ref)
        .transpose()
        .map_err(Error::Invalid)?;
    let actor = app::actor(&principal)?;
    let base = crate::api::statuses::parse_oid(&input.base_oid)?;
    let head = crate::api::statuses::parse_oid(&input.head_oid)?;
    // The preview supplies a timestamped ID so the same input regenerates
    // the same commit if a publication stops before returning its receipt.
    let seconds = input
        .request_id
        .get_timestamp()
        .filter(|_| input.request_id.get_version_num() == 7)
        .ok_or(Error::Invalid(
            "Use the request ID from the branch comparison",
        ))?
        .to_unix()
        .0;
    let upstream = destination.discussion();
    let pull = destination.pull(&server).await?;
    let digest = crab_metadata::receipts::publication_request_digest(&(
        "pull_request_branch_update",
        destination.head.id,
        upstream.id,
        destination.head.identity.placement_generation(),
        pull.number,
        &actor.issuer,
        &actor.subject,
        &input,
    ))
    .map_err(|error| merge::map_receive(error.into()))?;
    let plan_id = crate::api::encode_hex(&crab_metadata::receipts::publication_plan_id(
        &digest,
        input.request_id.as_bytes(),
    ));
    let replacement = new_branch.map(|branch| replacement::Intent {
        branch,
        version: input.version,
        base,
        head,
        plan_id: plan_id.clone(),
    });
    if let Some(commit) = committed_branch_edit(
        &destination.head,
        &plan_id,
        replacement.as_ref().map(|intent| intent.branch.as_str()),
        replacement.is_none().then_some(input.head_oid.as_str()),
    )
    .await?
    {
        if let Some(intent) = replacement {
            return intent
                .complete(server, principal, destination, pull, commit)
                .await;
        }
        return Ok(Json(json!({"commit":commit})));
    }
    require_active(&server, upstream, &pull).await?;
    if input.version != pull.version {
        return Err(Error::BranchUpdateChanged);
    }
    if replacement.as_ref().is_some_and(|intent| {
        intent.branch == pull.head_ref
            || (destination.upstream.is_none() && intent.branch == pull.base_ref)
    }) {
        return Err(Error::BranchUpdateDestination);
    }
    let read = crate::api::read_admission::acquire_bulk(&server).map_err(|_| Error::MergeBusy)?;
    if capture(&destination, &pull)
        .await?
        .as_ref()
        .is_none_or(|(base, head, includes_base)| {
            *includes_base || *base != input.base_oid || *head != input.head_oid
        })
    {
        return Err(Error::BranchUpdateChanged);
    }
    let (git, first, second, method) = if rebase {
        (
            PullGit::open(&server, upstream, pull.head_repository).await?,
            base,
            head,
            storage::MergeMethod::Rebase,
        )
    } else {
        // The merge update uses the head as the first parent. Rebase replays
        // the head commits onto the base, so it keeps the original read order.
        (
            git_for_update(&server, upstream, &pull).await?,
            head,
            base,
            storage::MergeMethod::MergeCommit,
        )
    };
    let base_ref = short_ref(&pull.base_ref);
    let head_ref = short_ref(&pull.head_ref);
    let message = if destination.upstream.is_some() {
        format!(
            "Merge {}:{base_ref} into {}:{head_ref}",
            upstream.config.name, destination.head.config.name
        )
    } else {
        format!("Merge {base_ref} into {head_ref}")
    };
    let plan = merge::build_merge_plan(
        &git,
        &server.cancellation,
        first,
        second,
        merge::tree::Strategy {
            method,
            actor: &actor,
            message: &message,
            seconds,
            allow_empty_rebase: rebase,
        },
        input.resolutions,
    )
    .await
    .map_err(branch_conflict)?;
    drop(read);
    let commit = plan.oid;
    let previous_head = replacement.as_ref().map(|_| (pull.head_ref.clone(), head));
    let name = replacement
        .as_ref()
        .map_or_else(|| pull.head_ref.clone(), |intent| intent.branch.clone());
    let published = receive::publish_branch_update(
        Arc::clone(&server),
        principal.clone(),
        (
            destination.head.config.owner.clone(),
            destination.head.config.name.clone(),
        ),
        RefUpdate {
            name,
            old: replacement.is_none().then_some(head),
            new: Some(commit),
        },
        plan.objects,
        BranchUpdatePublication {
            repository: destination.head.id,
            upstream: destination.upstream.clone(),
            number: pull.number,
            version: pull.version,
            base_ref: pull.base_ref.clone(),
            base_oid: base,
            previous_head,
            plan_id,
            rebase,
        },
    )
    .await;
    let commit = match (published, &replacement) {
        (Ok(()), _) => commit.to_string(),
        (Err(error), Some(intent)) => {
            // A concurrent retry may have reached publication after this
            // request read the receipts and before its ref admission.
            committed_branch_edit(
                &destination.head,
                &intent.plan_id,
                Some(&intent.branch),
                None,
            )
            .await?
            .ok_or_else(|| publication_error(error, true))?
        }
        (Err(error), None) => return Err(publication_error(error, false)),
    };
    if let Some(intent) = replacement {
        return intent
            .complete(server, principal, destination, pull, commit)
            .await;
    }
    Ok(Json(json!({"commit":commit})))
}

fn publication_error(error: ReceiveError, replacement: bool) -> Error {
    match error {
        ReceiveError::Forbidden => Error::BranchUpdatePermission,
        ReceiveError::Protected => Error::RefRule("The head branch requires a pull request"),
        ReceiveError::RefRule(message) => Error::RefRule(message),
        ReceiveError::NotFound => Error::NotFound,
        ReceiveError::Locked => Error::RefRule("A changed path is locked by another user"),
        ReceiveError::Graph(
            crab_git::receive_plan::ReceivePlanError::Stale { .. }
            | crab_git::receive_plan::ReceivePlanError::Namespace(_),
        ) if replacement => Error::BranchUpdateDestination,
        error => match merge::map_receive(error) {
            Error::MergeConflict => Error::BranchUpdateChanged,
            error => error,
        },
    }
}

async fn git_for_update(
    server: &Server,
    upstream: &Repository,
    pull: &storage::PullRequest,
) -> Result<PullGit> {
    let git = PullGit::open(server, upstream, pull.head_repository).await?;
    Ok(if let Some(fork) = git.fork {
        PullGit {
            base: fork,
            fork: Some(git.base),
        }
    } else {
        git
    })
}

fn branch_conflict(error: Error) -> Error {
    match error {
        Error::MergeConflict => Error::BranchUpdateConflict,
        error => error,
    }
}

fn short_ref(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

pub(crate) async fn authorize_branch_update(
    server: &Server,
    head: &Repository,
    update: &BranchUpdatePublication,
    head_ref: &str,
) -> std::result::Result<(), ReceiveError> {
    let upstream = update.upstream.as_deref().unwrap_or(head);
    let pull = associated_pull(server, upstream, head, update.number).await?;
    let previous_head = update
        .previous_head
        .as_ref()
        .map_or(head_ref, |(name, _)| name.as_str());
    if pull.version != update.version
        || pull.base_ref != update.base_ref
        || pull.head_ref != previous_head
    {
        return Err(ReceiveError::BranchChanged);
    }
    require_active(server, upstream, &pull)
        .await
        .map_err(|error| match error {
            Error::BranchUpdateChanged => ReceiveError::BranchChanged,
            Error::Archived => ReceiveError::Archived,
            error => ReceiveError::from(error),
        })
}
