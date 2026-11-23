use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};
use crab_git::receive_plan::RefUpdate;
use crab_remote_git::{OperationKind, RemoteGitRepository};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Kind, WriteTo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    MergeCheckTarget, auto_merge, branch_tip,
    git::{self, PullGit},
    merge, requirements,
    storage::{self, MergeMethod, NewPullMerge, PullRequest, PullState},
};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::pull_auto_merge::{Job, State as QueueState},
    git::receive::{
        self, CandidatePublication, PullPublication, ReceiveError, merge_candidate_ref,
    },
    repository::{Repository, settings},
    server::Server,
};

const TRAILER: &str = "\n\nCrab-Merge-Candidate-v1: ";
const LINEAR_HEADER: &str = "crab-merge-candidate-v1";
type Address = (String, String, u64);

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/merge-candidate",
        get(detail).post(prepare).delete(remove),
    )
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    pub request_id: Uuid,
    pub version: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub previous_oid: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remove {
    commit: String,
}

#[derive(Deserialize, Serialize, Eq, PartialEq)]
struct Source {
    repository: Uuid,
    pull: u64,
    base_ref: String,
    head_ref: String,
    head_repository: Option<Uuid>,
    comparison_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue: Option<Uuid>,
}

impl Source {
    fn captured(repo: &Repository, pull: &PullRequest, queue: Option<Uuid>) -> Self {
        Self {
            repository: repo.id,
            pull: pull.number,
            base_ref: pull.base_ref.clone(),
            head_ref: pull.head_ref.clone(),
            head_repository: pull.head_repository,
            comparison_revision: pull.retargeted_base.as_ref().map(|base| base.revision),
            queue,
        }
    }
}

struct Candidate {
    oid: ObjectId,
    base: ObjectId,
    head: ObjectId,
    source: Source,
    method: MergeMethod,
}

#[derive(PartialEq, Eq)]
pub(super) struct QueueBase {
    pub reference: String,
    pub oid: String,
    pub order_revision: u64,
}

pub(super) async fn prospective_base(
    server: &Server,
    repo: &Repository,
    git: &RemoteGitRepository,
    state: &QueueState,
    live_base: &str,
) -> Result<Option<QueueBase>> {
    let Some(ahead) = &state.ahead else {
        return Ok(None);
    };
    let mut base = QueueBase {
        reference: state.job.base_ref.clone(),
        oid: live_base.to_owned(),
        order_revision: state.order_revision,
    };
    if ahead.is_empty() {
        return Ok(Some(base));
    }
    let (configured, required) = tokio::try_join!(
        super::merge_methods::load_for_ref(server, repo, &state.job.base_ref),
        settings::requires_merge_queue(server, repo, &state.job.base_ref),
    )?;
    let mut oids = Vec::with_capacity(ahead.len());
    for prior in ahead {
        if !prior.settings.enabled(&configured, required)
            || (!prior.ready && prior.publishing.is_none())
        {
            return Ok(None);
        }
        let Some(tip) = branch_tip(git, &merge_candidate_ref(prior.pull)) else {
            return Ok(None);
        };
        oids.push(crate::api::statuses::parse_oid(&tip)?);
    }
    // This is one pinned snapshot and one read budget, even for the largest
    // prefix. The reader batches locator and pack access in request order.
    let operation = git
        .operation(OperationKind::Commit, &server.cancellation)
        .await
        .map_err(crate::Error::from)?;
    let result = operation.read_objects(&oids).await;
    let objects = operation.finish(result).await.map_err(crate::Error::from)?;
    if objects.len() != ahead.len() {
        return Err(Error::CellContract(
            "Incomplete merge queue candidate prefix",
        ));
    }
    for ((prior, oid), object) in ahead.iter().zip(oids).zip(objects) {
        let candidate = decode_candidate(oid, &object)?;
        let source = &candidate.source;
        if source.repository != repo.id
            || source.pull != prior.pull
            || source.base_ref != state.job.base_ref
            || source.head_ref != prior.head_ref
            || source.head_repository != prior.head_repository
            || source.comparison_revision != prior.comparison_revision
            || source.queue != Some(prior.candidate_id)
            || candidate.method != prior.settings.method
            || candidate.head.to_string() != prior.head_oid
        {
            return Ok(None);
        }
        let tip = candidate.oid.to_string();
        if base.reference == state.job.base_ref
            && tip == base.oid
            && prior.publishing.as_deref() == Some(tip.as_str())
        {
            // The branch can already contain a predecessor whose Cell
            // completion still needs to recover the publication receipt.
            continue;
        }
        if !prior.ready
            || candidate.base.to_string() != base.oid
            || live_head(server, repo, git, prior.head_repository, &prior.head_ref)
                .await?
                .as_deref()
                != Some(prior.head_oid.as_str())
        {
            return Ok(None);
        }
        base.reference = merge_candidate_ref(prior.pull);
        base.oid = tip;
    }
    Ok(Some(base))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LinearComparison {
    source: Source,
    base: String,
    head: String,
    method: MergeMethod,
}

impl LinearComparison {
    fn bind(self, plan: &mut merge::tree::Plan) -> Result<()> {
        let Some((Kind::Commit, bytes)) = plan.objects.last_mut() else {
            return Err(Error::CellContract("Expected a generated candidate tip"));
        };
        if candidate_oid(bytes)? != plan.oid {
            return Err(Error::CellContract("Candidate tip does not match its plan"));
        }
        // Put provenance on the final commit without changing rebased messages
        // or introducing a synthetic commit into the tested history.
        let mut commit = CommitRef::from_bytes(bytes, gix_hash::Kind::Sha1)
            .and_then(CommitRef::into_owned)
            .map_err(|error| Error::MergeObject(Box::new(error.into())))?;
        let value = serde_json::to_vec(&self)
            .map_err(|_| Error::CellContract("Invalid merge candidate comparison"))?;
        commit
            .extra_headers
            .push((LINEAR_HEADER.into(), value.into()));
        bytes.clear();
        commit
            .write_to(bytes)
            .map_err(|error| Error::MergeObject(Box::new(error.into())))?;
        plan.oid = candidate_oid(bytes)?;
        Ok(())
    }
}

fn candidate_oid(bytes: &[u8]) -> Result<ObjectId> {
    crab_remote::objects::object_id(Kind::Commit, bytes)
        .map_err(|error| Error::MergeObject(Box::new(error.into())))
}

async fn read_candidate(
    server: &Server,
    git: &RemoteGitRepository,
    number: u64,
) -> Result<Option<Candidate>> {
    let Some(tip) = branch_tip(git, &merge_candidate_ref(number)) else {
        return Ok(None);
    };
    let oid = crate::api::statuses::parse_oid(&tip)?;
    let operation = git
        .operation(OperationKind::Commit, &server.cancellation)
        .await
        .map_err(crate::Error::from)?;
    let result = operation.read_object(oid).await;
    let object = operation.finish(result).await.map_err(crate::Error::from)?;
    decode_candidate(oid, &object).map(Some)
}

fn decode_candidate(oid: ObjectId, object: &crab_remote_git::RemoteGitObject) -> Result<Candidate> {
    if object.kind != Kind::Commit {
        return Err(Error::CellContract("Expected a candidate commit"));
    }
    let commit = CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|error| Error::MergeObject(Box::new(error.into())))?;
    if let Some(value) = commit.extra_headers().find(LINEAR_HEADER) {
        let comparison: LinearComparison = serde_json::from_slice(value)
            .map_err(|_| Error::CellContract("Invalid linear candidate comparison"))?;
        let base = crate::api::statuses::parse_oid(&comparison.base)?;
        let head = crate::api::statuses::parse_oid(&comparison.head)?;
        let parent = match commit.parents.as_slice() {
            [parent] => crate::api::statuses::parse_oid(
                std::str::from_utf8(parent)
                    .map_err(|_| Error::CellContract("Invalid candidate parent"))?,
            )?,
            _ => return Err(Error::CellContract("Expected a single-parent candidate")),
        };
        if comparison.source.queue.is_none()
            || !matches!(comparison.method, MergeMethod::Squash | MergeMethod::Rebase)
            || (comparison.method == MergeMethod::Squash && parent != base)
        {
            return Err(Error::CellContract("Invalid linear merge candidate"));
        }
        return Ok(Candidate {
            oid,
            base,
            head,
            source: comparison.source,
            method: comparison.method,
        });
    }
    let parents: Vec<_> = commit.parents().collect();
    let [base, head] = parents.as_slice() else {
        return Err(Error::CellContract("Expected a two-parent merge candidate"));
    };
    let message = std::str::from_utf8(&commit.message)
        .map_err(|_| Error::CellContract("Invalid merge candidate message"))?;
    let source = message
        .split_once(TRAILER)
        .and_then(|(_, source)| serde_json::from_str(source).ok())
        .ok_or(Error::CellContract("Missing merge candidate comparison"))?;
    Ok(Candidate {
        oid,
        base: *base,
        head: *head,
        source,
        method: MergeMethod::MergeCommit,
    })
}

fn eligible(pull: &PullRequest) -> bool {
    pull.state == PullState::Open && !pull.draft && pull.merge_pending.is_none()
}

async fn live_tips(
    server: &Server,
    repo: &Repository,
    git: &RemoteGitRepository,
    pull: &PullRequest,
) -> Result<Option<(String, String)>> {
    let Some(base) = branch_tip(git, &pull.base_ref) else {
        return Ok(None);
    };
    Ok(
        live_head(server, repo, git, pull.head_repository, &pull.head_ref)
            .await?
            .map(|head| (base, head)),
    )
}

async fn live_head(
    server: &Server,
    repo: &Repository,
    git: &RemoteGitRepository,
    head_repository: Option<Uuid>,
    head_ref: &str,
) -> Result<Option<String>> {
    let Some(id) = head_repository else {
        return Ok(branch_tip(git, head_ref));
    };
    match git::fork_repository(server, repo, id) {
        Ok(fork) => Ok(fork.ref_oid(head_ref).await?),
        Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_current(
    candidate: &Candidate,
    repo: &Repository,
    pull: &PullRequest,
    base: Option<&str>,
    head: Option<&str>,
) -> bool {
    pull.state == PullState::Open
        && !pull.draft
        && candidate.source == Source::captured(repo, pull, candidate.source.queue)
        && base.is_some_and(|base| base == candidate.base.to_string())
        && head.is_some_and(|head| head == candidate.head.to_string())
}

pub(super) async fn verify_comparison(
    server: &Server,
    repo: &Repository,
    repository: &RemoteGitRepository,
    pull: &PullRequest,
    input: &NewPullMerge,
    tested_oid: &str,
    queued: bool,
) -> Result<()> {
    let queue = if queued {
        let active = auto_merge::active_queue(server, repo, pull.number)
            .await?
            .filter(|state| state.first)
            .ok_or(Error::MergeConflict)?;
        Some(active.job.candidate_id(active.order_revision))
    } else {
        None
    };
    let candidate = read_candidate(server, repository, pull.number)
        .await?
        .filter(|candidate| {
            candidate.oid.to_string() == tested_oid
                && candidate.base.to_string() == input.base_oid
                && candidate.head.to_string() == input.head_oid
                && candidate.method == input.method
                && candidate.source.queue == queue
                && candidate.source == Source::captured(repo, pull, candidate.source.queue)
        });
    if candidate.is_none() {
        return Err(Error::MergeConflict);
    }
    Ok(())
}

async fn promotion_status(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    pull: &PullRequest,
    candidate: &Candidate,
) -> Result<(&'static str, Vec<Value>)> {
    let grant = app::membership(server, principal, repo).await?;
    let oid = candidate.oid.to_string();
    let report = requirements(
        server,
        repo,
        pull,
        Some(&candidate.base.to_string()),
        &candidate.head.to_string(),
        &grant,
        &grant.merge_reviewers(server).await?,
        if candidate.source.queue.is_some() {
            MergeCheckTarget::Queue(Some(&oid))
        } else {
            MergeCheckTarget::Tested(&oid)
        },
    )
    .await?;
    if candidate.source.queue.is_none()
        && !super::merge_methods::load(server, repo)
            .await?
            .enabled
            .contains(&candidate.method)
    {
        return Ok(("method_disabled", vec![]));
    }
    let checks = report.candidate_checks.as_deref().unwrap_or_default();
    let status = match report.enforce() {
        Ok(()) => "ready",
        Err(Error::MergeCandidateChecks) if checks.is_empty() => "checks_unconfigured",
        Err(Error::MergeCandidateChecks) => "checks_pending",
        Err(Error::MergeBlocked | Error::MergeUnresolvedConversations | Error::MergeHeadBehind) => {
            "pull_blocked"
        }
        Err(error) => return Err(error),
    };
    Ok((
        status,
        checks.iter().map(|check| check.view(&oid)).collect(),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<Address>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let pull = storage::pull(&server, &repo, app::number(number)?)
        .await?
        .ok_or(Error::NotFound)?;
    let writable =
        principal.can_write(repo.id) && !settings::load_lifecycle(&server, &repo).await?.archived;
    let repository = repo
        .open_current(&server, server.options, &server.cancellation)
        .await?;
    let candidate = read_candidate(&server, &repository, number).await?;
    let tips = live_tips(&server, &repo, &repository, &pull).await?;
    let queued = auto_merge::active_queue(&server, &repo, number).await?;
    let queue = queued
        .as_ref()
        .map(|state| state.job.candidate_id(state.order_revision));
    let queue_required = settings::requires_merge_queue(&server, &repo, &pull.base_ref).await?;
    let queue_current = match &queued {
        Some(state) => state.job.enabled(
            &super::merge_methods::load_for_ref(&server, &repo, &pull.base_ref).await?,
            queue_required,
        ),
        None => !queue_required,
    };
    let preview = tips
        .as_ref()
        .filter(|(base, head)| {
            base != head && writable && eligible(&pull) && !queue_required && queue.is_none()
        })
        .map(|(base, head)| Input {
            request_id: Uuid::now_v7(),
            version: pull.version,
            base_oid: base.clone(),
            head_oid: head.clone(),
            previous_oid: candidate
                .as_ref()
                .map(|candidate| candidate.oid.to_string()),
        });
    let effective = match (&queued, &tips) {
        (Some(state), Some((base, _))) if candidate.is_some() && queue_current => {
            prospective_base(&server, &repo, &repository, state, base)
                .await?
                .map(|base| base.oid)
        }
        (None, Some((base, _))) => Some(base.clone()),
        _ => None,
    };
    let current = candidate.as_ref().is_some_and(|candidate| {
        queue_current
            && candidate.source.queue == queue
            && queued
                .as_ref()
                .is_none_or(|state| candidate.method == state.job.method)
            && is_current(
                candidate,
                &repo,
                &pull,
                effective.as_deref(),
                tips.as_ref().map(|(_, head)| head.as_str()),
            )
    });
    let mirror_ref = if let Some(candidate) = candidate.as_ref().filter(|_| current) {
        crate::api::integrations::candidate_mirror_ref(
            &server,
            &repo,
            number,
            candidate.oid,
            queue.is_some(),
        )
        .await?
    } else {
        None
    };
    let (promotion, required_checks) = if let Some(candidate) = candidate
        .as_ref()
        .filter(|_| current && pull.merge_pending.is_none())
    {
        let (status, checks) =
            promotion_status(&server, &repo, &principal, &pull, candidate).await?;
        ((writable && queue.is_none()).then_some(status), checks)
    } else {
        (None, vec![])
    };
    let dispatches = if let Some(candidate) = candidate
        .as_ref()
        .filter(|candidate| candidate.source.queue.is_some())
    {
        crate::api::integrations::dispatch::view(&server, &repo, &candidate.oid.to_string()).await?
    } else {
        vec![]
    };
    Ok(Json(json!({
        "preview":preview,
        "candidate":candidate.map(|candidate| json!({
            "commit":candidate.oid.to_string(), "base_oid":candidate.base.to_string(), "head_oid":candidate.head.to_string(),
            "method":candidate.method, "grouped":current && queued.as_ref().is_some_and(|state| !state.first),
            "ref_name":merge_candidate_ref(number), "current":current,
            "mirror_ref":mirror_ref, "can_remove":writable && pull.merge_pending.is_none() && queue.is_none(),
            "promotion":promotion, "required_checks":required_checks, "dispatches":dispatches,
        })),
    })))
}

async fn prepare(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<Address>,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let number = app::number(number)?;
    let commit = stage(
        &server,
        &principal,
        &repo,
        number,
        &input,
        None,
        &server.cancellation,
    )
    .await?;
    Ok(Json(json!({"commit":commit.to_string()})))
}

pub(super) async fn queue_commit(
    server: &Server,
    repo: &Repository,
    git: &PullGit,
    pull: &PullRequest,
    job: &Job,
    base: &QueueBase,
) -> Result<Option<String>> {
    Ok(read_candidate(server, &git.base, pull.number)
        .await?
        .filter(|candidate| {
            candidate.source
                == Source::captured(repo, pull, Some(job.candidate_id(base.order_revision)))
                && candidate.method == job.method
                && candidate.base.to_string() == base.oid
                && candidate.head.to_string() == job.head_oid
        })
        .map(|candidate| candidate.oid.to_string()))
}

pub(super) async fn stage(
    server: &Arc<Server>,
    principal: &Principal,
    repo: &Repository,
    number: u64,
    input: &Input,
    plan: Option<(&Job, &QueueBase)>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ObjectId> {
    let job = plan.map(|(job, _)| job);
    let queue = plan.map(|(job, base)| job.candidate_id(base.order_revision));
    let method = job.map_or(MergeMethod::MergeCommit, |job| job.method);
    if !principal.can_write(repo.id) {
        return Err(Error::MergePermission);
    }
    if queue.is_none() {
        auto_merge::require_direct_access(server, repo, number).await?;
    }
    let actor = app::actor(principal)?;
    let seconds = input
        .request_id
        .get_timestamp()
        .filter(|_| input.request_id.get_version_num() == 7)
        .ok_or(Error::Invalid(
            "Use the request ID from the merge comparison",
        ))?
        .to_unix()
        .0;
    let base = crate::api::statuses::parse_oid(&input.base_oid)?;
    let head = crate::api::statuses::parse_oid(&input.head_oid)?;
    let previous = input
        .previous_oid
        .as_deref()
        .map(crate::api::statuses::parse_oid)
        .transpose()?;
    let name = merge_candidate_ref(number);
    let binding = (
        "merge_candidate",
        repo.id,
        repo.identity.placement_generation(),
        number,
        &actor.issuer,
        &actor.subject,
        input,
    );
    let digest = match (queue, method) {
        (Some(queue), MergeMethod::MergeCommit) => {
            crab_metadata::receipts::publication_request_digest(&(binding, queue))
        }
        (Some(queue), method) => {
            crab_metadata::receipts::publication_request_digest(&(binding, queue, method))
        }
        (None, _) => crab_metadata::receipts::publication_request_digest(&binding),
    }
    .map_err(|error| merge::map_receive(error.into()))?;
    let plan_id = crate::api::encode_hex(&crab_metadata::receipts::publication_plan_id(
        &digest,
        input.request_id.as_bytes(),
    ));
    if let Some(receipt) = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
        &repo.store,
        &repo.layout,
        &plan_id,
    )
    .await
    .map_err(|error| merge::map_receive(error.into()))?
    {
        let [edit] = receipt.transaction().edits() else {
            return Err(Error::CellContract("Unexpected merge candidate receipt"));
        };
        let commit = edit
            .new_oid()
            .filter(|_| {
                edit.ref_name() == name && edit.expected_old() == input.previous_oid.as_deref()
            })
            .ok_or(Error::CellContract(
                "Merge candidate receipt does not match its ref edit",
            ))?;
        return crate::api::statuses::parse_oid(commit);
    }
    let pull = storage::pull(server, repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    if queue.is_none() && settings::requires_merge_queue(server, repo, &pull.base_ref).await? {
        return Err(Error::MergeQueueRequired);
    }
    if settings::load_lifecycle(server, repo).await?.archived {
        return Err(Error::Archived);
    }
    if !eligible(&pull) || pull.version != input.version || base == head {
        return Err(Error::BranchUpdateChanged);
    }
    let read = crate::api::read_admission::acquire_bulk(server).map_err(|_| Error::MergeBusy)?;
    let git = PullGit::open(server, repo, pull.head_repository).await?;
    let Some((target, live_head)) = git.tips(&pull.base_ref, &pull.head_ref) else {
        return Err(Error::BranchUpdateChanged);
    };
    let build_ref = plan.map_or(pull.base_ref.as_str(), |(_, prospective)| {
        prospective.reference.as_str()
    });
    if live_head != input.head_oid
        || branch_tip(&git.base, build_ref).as_deref() != Some(input.base_oid.as_str())
        || branch_tip(&git.base, &name) != input.previous_oid
    {
        return Err(Error::BranchUpdateChanged);
    }
    if previous.is_some()
        && read_candidate(server, &git.base, number)
            .await?
            .is_none_or(|candidate| {
                candidate.source.repository != repo.id || candidate.source.pull != number
            })
    {
        return Err(Error::CellContract(
            "Reserved ref does not belong to this pull request",
        ));
    }
    let source = Source::captured(repo, &pull, queue);
    let message = if method == MergeMethod::MergeCommit {
        let source = serde_json::to_string(&source)
            .map_err(|_| Error::CellContract("Invalid merge candidate comparison"))?;
        format!(
            "Merge candidate for pull request #{number}\n\nRequest: {}{TRAILER}{source}",
            input.request_id
        )
    } else {
        merge::message(method, "", number, &pull.head_ref)?
    };
    let mut plan = merge::build_merge_plan(
        &git,
        cancel,
        base,
        head,
        merge::tree::Strategy {
            method,
            actor: &actor,
            message: &message,
            seconds,
            allow_empty_rebase: false,
        },
        None,
    )
    .await?;
    if method != MergeMethod::MergeCommit {
        LinearComparison {
            source,
            base: input.base_oid.clone(),
            head: input.head_oid.clone(),
            method,
        }
        .bind(&mut plan)?;
    }
    drop(read);
    if previous != Some(plan.oid) {
        let fork = pull
            .head_repository
            .map(|id| git::fork_repository(server, repo, id))
            .transpose()?;
        receive::publish_pull_request_objects(
            Arc::clone(server),
            principal.clone(),
            (repo.config.owner.clone(), repo.config.name.clone()),
            RefUpdate {
                name,
                old: previous,
                new: Some(plan.oid),
            },
            plan.objects,
            PullPublication {
                number,
                head_ref: pull.head_ref,
                head_repository: fork,
                head_oid: head,
                plan_id: Some(plan_id),
                cancel: cancel.clone(),
                tested_candidate: false,
                candidate: Some(CandidatePublication::Prepare {
                    build_ref: build_ref.to_owned(),
                    base_ref: pull.base_ref,
                    base_oid: base,
                    target_oid: crate::api::statuses::parse_oid(&target)?,
                    version: pull.version,
                    queue,
                    method,
                }),
            },
        )
        .await
        .map_err(publication_error)?;
    }
    Ok(plan.oid)
}

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<Address>,
    Json(input): Json<Remove>,
) -> Result<Json<Value>> {
    let key = (owner, name);
    let repo = app::repository(&server, &principal, &key)?;
    if !principal.can_write(repo.id) {
        return Err(Error::MergePermission);
    }
    let number = app::number(number)?;
    auto_merge::require_direct_access(&server, &repo, number).await?;
    let old = crate::api::statuses::parse_oid(&input.commit)?;
    let git = repo
        .open_current(&server, server.options, &server.cancellation)
        .await?;
    let Some(candidate) = read_candidate(&server, &git, number).await? else {
        return Ok(Json(json!({"removed":true})));
    };
    if candidate.oid != old
        || candidate.source.repository != repo.id
        || candidate.source.pull != number
    {
        return Err(Error::BranchUpdateChanged);
    }
    receive::remove_merge_candidate(
        Arc::clone(&server),
        principal,
        key,
        old,
        PullPublication {
            number,
            head_ref: candidate.source.head_ref,
            head_repository: None,
            head_oid: candidate.head,
            plan_id: None,
            cancel: server.cancellation.clone(),
            tested_candidate: false,
            candidate: Some(CandidatePublication::Remove),
        },
    )
    .await
    .map_err(publication_error)?;
    Ok(Json(json!({"removed":true})))
}

fn publication_error(error: ReceiveError) -> Error {
    match error {
        ReceiveError::Forbidden => Error::MergePermission,
        ReceiveError::BranchChanged => Error::BranchUpdateChanged,
        ReceiveError::RefRule(message) => Error::RefRule(message),
        error => merge::map_receive(error),
    }
}

pub(crate) async fn authorize_merge_candidate(
    server: &Server,
    repo: &Repository,
    publication: &PullPublication,
    candidate: &CandidatePublication,
) -> std::result::Result<(), ReceiveError> {
    let Some(pull) = storage::pull(server, repo, publication.number).await? else {
        return Err(ReceiveError::NotFound);
    };
    let active = auto_merge::active_queue(server, repo, pull.number).await?;
    let (queue, queue_required) = match candidate {
        CandidatePublication::Prepare { queue, .. } => (
            *queue,
            settings::requires_merge_queue(server, repo, &pull.base_ref).await?,
        ),
        CandidatePublication::Remove => (None, false),
    };
    if queue_required && queue.is_none() {
        return Err(ReceiveError::Settings(Box::new(Error::MergeQueueRequired)));
    }
    let allowed = match (queue, active) {
        (None, None) => true,
        (Some(id), Some(state)) if state.job.candidate_id(state.order_revision) == id => {
            let CandidatePublication::Prepare {
                method,
                build_ref,
                base_oid,
                target_oid,
                ..
            } = candidate
            else {
                return Err(ReceiveError::BranchChanged);
            };
            if *method != state.job.method
                || !state.job.enabled(
                    &super::merge_methods::load_for_ref(server, repo, &pull.base_ref).await?,
                    queue_required,
                )
            {
                return Err(ReceiveError::Settings(Box::new(Error::MergeQueued)));
            }
            if state.first {
                *base_oid == *target_oid && *build_ref == pull.base_ref
            } else {
                // The lower Git publisher holds the target, source ref and
                // policy leases while this check validates the live prefix.
                let git = repo
                    .open_current(server, server.options, &server.cancellation)
                    .await?;
                prospective_base(server, repo, &git, &state, &target_oid.to_string())
                    .await?
                    .is_some_and(|base| {
                        base.reference == *build_ref && base.oid == base_oid.to_string()
                    })
            }
        }
        _ => false,
    };
    if !allowed {
        return Err(ReceiveError::Settings(Box::new(Error::MergeQueued)));
    }
    if let CandidatePublication::Prepare {
        base_ref, version, ..
    } = candidate
        && (!eligible(&pull)
            || pull.version != *version
            || pull.base_ref != *base_ref
            || pull.head_ref != publication.head_ref
            || pull.head_repository != publication.head_repository.as_ref().map(|fork| fork.id))
    {
        return Err(ReceiveError::BranchChanged);
    }
    Ok(())
}

pub(in crate::api) async fn require_check_target(
    server: &Server,
    repo: &Repository,
    number: u64,
    oid: ObjectId,
) -> Result<bool> {
    let pull = storage::pull(server, repo, number)
        .await?
        .ok_or(Error::CommitNotFound)?;
    let git = repo
        .open_current(server, server.options, &server.cancellation)
        .await?;
    let candidate = read_candidate(server, &git, number)
        .await?
        .ok_or(Error::CommitNotFound)?;
    let Some((base, head)) = live_tips(server, repo, &git, &pull).await? else {
        return Err(Error::CommitNotFound);
    };
    let active = auto_merge::active_queue(server, repo, number).await?;
    let queue_required = settings::requires_merge_queue(server, repo, &pull.base_ref).await?;
    let effective = if let Some(state) = &active {
        let configured = super::merge_methods::load_for_ref(server, repo, &pull.base_ref).await?;
        if candidate.method != state.job.method || !state.job.enabled(&configured, queue_required) {
            return Err(Error::CommitNotFound);
        }
        prospective_base(server, repo, &git, state, &base)
            .await?
            .map(|base| base.oid)
    } else {
        Some(base)
    };
    if candidate.oid != oid
        || (active.is_none() && queue_required)
        || candidate.source.queue
            != active
                .as_ref()
                .map(|state| state.job.candidate_id(state.order_revision))
        || !is_current(&candidate, repo, &pull, effective.as_deref(), Some(&head))
    {
        return Err(Error::CommitNotFound);
    }
    Ok(candidate.source.queue.is_some())
}
