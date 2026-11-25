use super::{branch_update::associated_pull, *};
use crate::cells::repository::pull_branch::{HasCleanup, InUse, Reference};
use crate::git::receive::{self, PullBranchBinding, PullBranchPublication, ReceiveError};
use crate::repository::settings;
use crab_git::receive_plan::{ReceivePlanError, RefUpdate};
use gix_hash::ObjectId;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

pub(super) mod automatic;
pub(crate) mod stacked;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/head-branch",
        get(availability).post(edit),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: Action,
    request_id: Uuid,
    version: u64,
    repository_id: Uuid,
    head_oid: String,
    retarget_token: Option<String>,
}

#[derive(Serialize)]
struct Branch {
    action: Action,
    version: u64,
    repository_id: Uuid,
    head_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retarget: Option<stacked::Preview>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Delete,
    Restore,
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

#[derive(Deserialize)]
struct Route {
    upstream: Option<Uuid>,
}

fn discussion(
    server: &Server,
    source: &Arc<Repository>,
    upstream: Option<Uuid>,
) -> Result<Arc<Repository>> {
    match upstream {
        None => Ok(Arc::clone(source)),
        Some(id) => source
            .fork_source(server)
            .filter(|repo| repo.id == id)
            .ok_or(Error::NotFound),
    }
}

async fn availability(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(route): Query<Route>,
) -> Result<Json<Value>> {
    let source = app::repository(&server, &principal, &(owner, name))?;
    let upstream = discussion(&server, &source, route.upstream)?;
    if upstream.id != source.id {
        app::require_related_read(&server, &principal, &source, &upstream).await?;
    }
    let pull = associated_pull(&server, &upstream, &source, app::number(number)?).await?;
    let current = app::membership(&server, &principal, &source).await?;
    if !current.principal.can_write(source.id) {
        return Ok(Json(json!({ "branch": null })));
    }
    // Acknowledge cleanup after the ref changes. Read the job first so its
    // removal cannot hide a concurrent deletion from the browser's refresh.
    let cleanup_pending = if pull.state == PullState::Merged {
        let cell = app::repository_cell(&server, &upstream)?;
        app::query_output(
            cell.client
                .query::<HasCleanup>(&cell.target, None, pull.number)
                .await,
        )?
    } else {
        false
    };
    let branch = check_availability(&server, &current, &source, &pull).await?;
    if upstream.id != source.id {
        app::require_related_read(&server, &principal, &source, &upstream).await?;
    }
    let current = app::membership(&server, &principal, &source).await?;
    if !current.principal.can_write(source.id) {
        return Ok(Json(json!({ "branch": null })));
    }
    let mut result = json!({ "branch": branch });
    if cleanup_pending && branch.is_some_and(|branch| matches!(branch.action, Action::Delete)) {
        result["cleanup_pending"] = json!(true);
    }
    Ok(Json(result))
}

async fn check_availability(
    server: &Server,
    grant: &app::Membership,
    source: &Repository,
    pull: &PullRequest,
) -> Result<Option<Branch>> {
    let Some(head) = saved_head(pull) else {
        return Ok(None);
    };
    if settings::load_lifecycle(server, source).await?.archived {
        return Ok(None);
    }
    let (action, retarget) = match source.ref_oid(&pull.head_ref).await? {
        None => (Action::Restore, None),
        Some(ref current_head) if current_head == head => {
            let stack = stacked::can_retarget(pull);
            let (allowed, used) = tokio::try_join!(
                may_delete(server, source, grant, &pull.head_ref, head),
                in_use(server, source, &pull.head_ref, stack),
            )?;
            if !allowed || used {
                return Ok(None);
            }
            let retarget = if stack {
                match stacked::preview(server, source, pull).await {
                    Ok(preview) => preview,
                    Err(Error::PullBranchInUse) => return Ok(None),
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            (Action::Delete, retarget)
        }
        Some(_) => return Ok(None),
    };
    Ok(Some(Branch {
        action,
        version: pull.version,
        repository_id: source.id,
        head_oid: head.to_owned(),
        retarget,
    }))
}

async fn may_delete(
    server: &Server,
    source: &Repository,
    grant: &app::Membership,
    reference: &str,
    head: &str,
) -> Result<bool> {
    let policy = settings::load_ref_policy(server, source).await?;
    let default = source.default_ref().await?;
    if reference == default || policy.protections.protection(reference).is_some() {
        return Ok(false);
    }
    let teams = policy.organization_teams(server, grant).await?;
    let actor = grant
        .bypass_context(source.id)
        .organization_teams(teams.as_deref());
    let update = RefUpdate {
        name: reference.to_owned(),
        old: Some(oid(head)?),
        new: None,
    };
    Ok(policy
        .active()?
        .matches(reference, &default)
        .all(|(id, rule)| {
            rule.violation(&update, false).is_none()
                || rule.allows_bypass(actor.for_rule(id), false)
        }))
}

async fn in_use(
    server: &Server,
    source: &Repository,
    reference: &str,
    head_only: bool,
) -> Result<bool> {
    async fn used(
        server: &Server,
        repository: &Repository,
        reference: &str,
        fork: Option<Uuid>,
        head_only: bool,
    ) -> Result<bool> {
        let cell = app::repository_cell(server, repository)?;
        app::query_output(
            cell.client
                .query::<InUse>(
                    &cell.target,
                    None,
                    Reference {
                        name: reference.to_owned(),
                        fork,
                        head_only,
                    },
                )
                .await,
        )
    }
    if used(server, source, reference, None, head_only).await? {
        return Ok(true);
    }
    if let Some(upstream) = source.fork_source(server) {
        return used(server, &upstream, reference, Some(source.id), true).await;
    }
    Ok(false)
}

fn oid(head: &str) -> Result<ObjectId> {
    ObjectId::from_hex(head.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::CellContract("Saved pull request head is invalid"))
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(route): Query<Route>,
    Json(input): Json<Input>,
) -> Result<impl IntoResponse> {
    let source = app::repository(&server, &principal, &(owner, name))?;
    let upstream = discussion(&server, &source, route.upstream)?;
    if upstream.id != source.id {
        app::require_related_read(&server, &principal, &source, &upstream).await?;
    }
    let number = app::number(number)?;
    let pull = associated_pull(&server, &upstream, &source, number).await?;
    let head = saved_head(&pull).ok_or(Error::PullBranchChanged)?;
    if input.request_id.is_nil()
        || input.version != pull.version
        || input.head_oid != head
        || input.repository_id != source.id
        || input.retarget_token.as_ref().is_some_and(|token| {
            !matches!(input.action, Action::Delete)
                || !stacked::can_retarget(&pull)
                || token.len() != 64
                || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(Error::PullBranchChanged);
    }
    let actor = app::actor(&principal)?;
    let commit = oid(head)?;
    let (domain, old, new) = match input.action {
        Action::Delete => ("delete-pull-branch/v1", Some(commit), None),
        Action::Restore => ("restore-pull-branch/v1", None, Some(commit)),
    };
    let plan_id = blake3::hash(&serde_json::to_vec(&(
        domain,
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
    receive::edit_pull_branch(
        server,
        principal,
        RefUpdate {
            name: pull.head_ref.clone(),
            old,
            new,
        },
        PullBranchPublication {
            upstream,
            source,
            pull: number,
            binding: PullBranchBinding::Manual {
                version: input.version,
                retarget_token: input.retarget_token,
            },
            plan_id,
        },
    )
    .await
    .map_err(edit_error)?;
    Ok(Json(json!({ "branch": pull.head_ref, "commit": head })))
}

fn edit_error(error: ReceiveError) -> Error {
    match error {
        ReceiveError::Forbidden => Error::BranchUpdatePermission,
        ReceiveError::Archived => Error::Archived,
        ReceiveError::Protected => {
            Error::RefRule("Branch protection prevents changing this branch")
        }
        ReceiveError::Graph(ReceivePlanError::Ref { reason, .. }) => Error::RefRule(reason),
        ReceiveError::RefRule(message) => Error::RefRule(message),
        ReceiveError::BranchChanged
        | ReceiveError::Request(_)
        | ReceiveError::Graph(ReceivePlanError::Stale { .. } | ReceivePlanError::Namespace(_))
        | ReceiveError::Write(
            crab_write::WriteError::RefChanged { .. } | crab_write::WriteError::Namespace(_),
        ) => Error::PullBranchChanged,
        ReceiveError::Graph(ReceivePlanError::Missing { .. })
        | ReceiveError::Remote(crab_remote_git::Error::ObjectNotFound { .. }) => {
            Error::CommitNotFound
        }
        ReceiveError::NotFound => Error::NotFound,
        ReceiveError::Settings(cause) => *cause,
        ReceiveError::Busy
        | ReceiveError::Coordination(
            crab_coordination::CoordinationError::PushLockHeld { .. }
            | crab_coordination::CoordinationError::GcFenceHeld { .. },
        ) => Error::MergeBusy,
        other => Error::PullBranch(Box::new(other)),
    }
}

pub(crate) async fn authorize_pull_branch(
    server: &Server,
    source: &Repository,
    edit: &PullBranchPublication,
    update: &RefUpdate,
    principal: &Principal,
    stack: Option<&stacked::Stack>,
    cancel: &CancellationToken,
) -> std::result::Result<(), ReceiveError> {
    if (edit.upstream.id != source.id
        && source.fork_source(server).is_none_or(|current| {
            current.id != edit.upstream.id
                || current.ownership_epoch != edit.upstream.ownership_epoch
        }))
        || edit.source.id != source.id
        || edit.source.ownership_epoch != source.ownership_epoch
    {
        return Err(ReceiveError::NotFound);
    }
    if edit.upstream.id != source.id && matches!(edit.binding, PullBranchBinding::Manual { .. }) {
        app::require_related_read(server, principal, source, &edit.upstream).await?;
    }
    let pull = associated_pull(server, &edit.upstream, source, edit.pull).await?;
    let bound = match &edit.binding {
        PullBranchBinding::Manual { version, .. } => pull.version == *version,
        PullBranchBinding::Automatic {
            merge,
            setting_generation,
            ..
        } => {
            let policy = merge_methods::load(server, source).await?;
            update.new.is_none()
                && policy.delete_head_branches
                && policy.delete_head_branches_generation == *setting_generation
                && pull
                    .merge
                    .as_ref()
                    .is_some_and(|record| Uuid::parse_str(&record.request_id) == Ok(*merge))
        }
    };
    if !bound
        || update.old.is_some() == update.new.is_some()
        || pull.head_ref != update.name
        || saved_head(&pull)
            != update
                .new
                .or(update.old)
                .map(|oid| oid.to_string())
                .as_deref()
    {
        return Err(Error::PullBranchChanged.into());
    }
    if update.new.is_none() && source.default_ref().await? == update.name {
        return Err(ReceiveError::Protected);
    }
    // New dependencies on this branch take the same ref lease at creation,
    // retargeting, or reopening. Hold it through the deletion marker.
    if update.new.is_none() {
        if in_use(server, source, &update.name, stack.is_some()).await? {
            return Err(Error::PullBranchInUse.into());
        }
        if let Some(stack) = stack {
            Box::pin(stack.apply(server, source, principal, &pull, edit, cancel)).await?;
            if in_use(server, source, &update.name, false).await? {
                return Err(Error::PullBranchInUse.into());
            }
        }
    }
    Ok(())
}
