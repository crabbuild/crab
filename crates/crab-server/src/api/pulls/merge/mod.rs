use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use crab_remote_git::RepositoryOptions;
use gix_hash::ObjectId;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub(super) mod conflicts;
mod rebase;
pub(super) mod tree;

use super::{MergeCheckTarget, pull_view, requirements, storage};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    git::receive::{self, PullPublication, ReceiveError},
    repository::Repository,
    server::Server,
};
use storage::{MergeMethod, NewPullMerge, PullMerge, PullRequest, PullState};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/merge",
        post(execute),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeInput {
    request_id: String,
    version: u64,
    method: MergeMethod,
    base_oid: String,
    head_oid: String,
    #[serde(default)]
    message: String,
    tested_candidate_oid: Option<String>,
    #[serde(default)]
    bypass_rulesets: Vec<crate::cells::repository::rulesets::RulesetId>,
}

fn oid(value: &str) -> Result<ObjectId> {
    ObjectId::from_hex(value.as_bytes())
        .ok()
        .filter(|oid| !oid.is_null())
        .ok_or(Error::Invalid("Merge requires exact SHA-1 commit IDs"))
}

fn current_ref(repository: &crab_remote_git::RemoteGitRepository, name: &str) -> Option<String> {
    repository
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.target.to_string())
}

pub(super) fn map_receive(error: ReceiveError) -> Error {
    match &error {
        ReceiveError::Archived => Error::Archived,
        ReceiveError::Settings(error) if matches!(**error, Error::MergeBypassUnavailable) => {
            Error::MergeBypassUnavailable
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeUnresolvedConversations) => {
            Error::MergeUnresolvedConversations
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeHeadBehind) => {
            Error::MergeHeadBehind
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeCandidateChecks) => {
            Error::MergeCandidateChecks
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeQueued) => {
            Error::MergeQueued
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeQueueRequired) => {
            Error::MergeQueueRequired
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeBlocked) => {
            Error::MergeBlocked
        }
        ReceiveError::Settings(error) if matches!(**error, Error::AutoMergeDisabled) => {
            Error::AutoMergeDisabled
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeMethodDisabled) => {
            Error::MergeMethodDisabled
        }
        ReceiveError::Settings(error) if matches!(**error, Error::MergeConflict) => {
            Error::MergeConflict
        }
        ReceiveError::Busy
        | ReceiveError::Coordination(
            crab_coordination::CoordinationError::PushLockHeld { .. }
            | crab_coordination::CoordinationError::GcFenceHeld { .. },
        ) => Error::MergeBusy,
        ReceiveError::Request(_)
        | ReceiveError::BranchChanged
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Stale { .. })
        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::NonFastForward {
            ..
        })
        | ReceiveError::Write(crab_write::WriteError::RefChanged { .. }) => Error::MergeConflict,
        _ => Error::Merge(Box::new(error)),
    }
}

fn same_request(record: &PullMerge, input: &NewPullMerge) -> bool {
    // Any current writer may recover a recorded merge after the initiating
    // session disappears, but the immutable request details must still match.
    record.request_id == input.request_id
        && record.method == input.method
        && record.pull_version == input.pull_version
        && record.base_oid == input.base_oid
        && record.head_oid == input.head_oid
        && record.message == input.message
        && record.bypass_rulesets == input.bypass_rulesets
        && record
            .tested_candidate
            .then_some(record.commit_oid.as_str())
            == input.tested_candidate_oid.as_deref()
}

pub(super) fn validate_bypass_actor(
    record: &PullMerge,
    actor: &crate::auth::Identity,
) -> Result<()> {
    if !record.bypass_rulesets.is_empty()
        && (record.author.issuer != actor.issuer || record.author.subject != actor.subject)
    {
        return Err(Error::Forbidden);
    }
    Ok(())
}

pub(super) async fn build_merge_plan(
    git: &super::git::PullGit,
    cancellation: &CancellationToken,
    base: ObjectId,
    head: ObjectId,
    strategy: tree::Strategy<'_>,
    resolutions: Option<Vec<conflicts::Edit>>,
) -> Result<tree::Plan> {
    let rebase = strategy.method == MergeMethod::Rebase;
    let conflicts = resolutions
        .map(conflicts::Mode::apply)
        .transpose()
        .map_err(|error| map_tree_error(error, false))?
        .unwrap_or_default();
    match with_merge_read(
        git,
        cancellation,
        base,
        head,
        MergeInputKind::Commit(strategy, conflicts),
    )
    .await
    .map_err(|error| map_tree_error(error, rebase))?
    {
        MergeOutput::Commit(plan) => Ok(plan),
        MergeOutput::Preview(_) => Err(Error::CellContract("Expected a merge commit plan")),
    }
}

pub(super) async fn preview_conflicts(
    git: &super::git::PullGit,
    cancellation: &CancellationToken,
    head: ObjectId,
    base: ObjectId,
) -> Result<Vec<conflicts::File>> {
    match with_merge_read(git, cancellation, head, base, MergeInputKind::Preview)
        .await
        .map_err(|error| map_tree_error(error, false))?
    {
        MergeOutput::Preview(files) => Ok(files),
        MergeOutput::Commit(_) => Err(Error::CellContract("Expected a conflict preview")),
    }
}

enum MergeInputKind<'a> {
    Commit(tree::Strategy<'a>, conflicts::Mode),
    Preview,
}

enum MergeOutput {
    Commit(tree::Plan),
    Preview(Vec<conflicts::File>),
}

async fn with_merge_read(
    git: &super::git::PullGit,
    cancellation: &CancellationToken,
    current: ObjectId,
    incoming: ObjectId,
    input: MergeInputKind<'_>,
) -> std::result::Result<MergeOutput, tree::Error> {
    let operation = git
        .base
        .operation(crab_remote_git::OperationKind::Repository, cancellation)
        .await
        .map_err(crab_remote::objects::Error::from)?;
    let other = if let Some(fork) = &git.fork {
        match fork
            .operation(crab_remote_git::OperationKind::Repository, cancellation)
            .await
        {
            Ok(other) => Some(other),
            Err(error) => {
                return operation
                    .finish(Err(error))
                    .await
                    .map_err(|error| crab_remote::objects::Error::from(error).into());
            }
        }
    } else {
        None
    };
    let readers = tree::Readers {
        base: &operation,
        head: other.as_ref().unwrap_or(&operation),
    };
    let built = match input {
        MergeInputKind::Commit(strategy, conflicts) => {
            tree::build(git, &readers, current, incoming, strategy, conflicts)
                .await
                .map(MergeOutput::Commit)
        }
        MergeInputKind::Preview => tree::preview_conflicts(git, &readers, current, incoming)
            .await
            .map(MergeOutput::Preview),
    };
    let built = if let Some(other) = other {
        finish_merge_read(other, built).await
    } else {
        built
    };
    finish_merge_read(operation, built).await
}

fn map_tree_error(error: tree::Error, rebase: bool) -> Error {
    match error {
        tree::Error::Object(crab_remote::objects::Error::Remote(error)) => {
            Error::Repository(crate::Error::Remote(error))
        }
        tree::Error::Conflict | tree::Error::History | tree::Error::Rebase if rebase => {
            Error::UnsafeRebase
        }
        tree::Error::Conflict | tree::Error::History | tree::Error::Rebase => Error::MergeConflict,
        tree::Error::Resolution(message) => Error::Invalid(message),
        tree::Error::Object(error) => Error::MergeObject(Box::new(error)),
    }
}

async fn finish_merge_read(
    operation: crab_remote_git::OperationContext,
    built: std::result::Result<MergeOutput, tree::Error>,
) -> std::result::Result<MergeOutput, tree::Error> {
    match built {
        Err(tree::Error::Object(crab_remote::objects::Error::Remote(error))) => operation
            .finish(Err(error))
            .await
            .map_err(|error| crab_remote::objects::Error::from(error).into()),
        built => {
            operation
                .finish(Ok(()))
                .await
                .map_err(crab_remote::objects::Error::from)?;
            built
        }
    }
}

impl From<crab_remote::publication::Error> for Error {
    fn from(error: crab_remote::publication::Error) -> Self {
        map_receive(ReceiveError::from(error))
    }
}

fn plan_id(
    repo: &Repository,
    pull: &PullRequest,
    candidate: &NewPullMerge,
    commit: ObjectId,
) -> Result<String> {
    let mut binding = json!({
        "kind": "pull_request_merge",
        "repository": repo.id,
        "placement": repo.identity.placement_generation(),
        "pull": pull.number,
        "version": candidate.pull_version,
        "method": candidate.method,
        "base_ref": pull.base_ref,
        "base_oid": candidate.base_oid,
        "head_ref": pull.head_ref,
        "head_oid": candidate.head_oid,
        "commit": commit.to_string(),
        "message": candidate.message,
    });
    if !candidate.bypass_rulesets.is_empty() {
        binding["bypass_rulesets"] = json!(candidate.bypass_rulesets);
    }
    if candidate.tested_candidate_oid.is_some() {
        binding["tested_candidate"] = json!(true);
    }
    if let Some(fork) = pull.head_repository {
        binding["head_repository"] = json!(fork);
    }
    let digest = crab_metadata::receipts::publication_request_digest(&binding)
        .map_err(|error| map_receive(error.into()))?;
    let nonce = uuid::Uuid::parse_str(&candidate.request_id)
        .map_err(|_| Error::Invalid("Invalid merge request ID"))?;
    Ok(crate::api::encode_hex(
        &crab_metadata::receipts::publication_plan_id(&digest, nonce.as_bytes()),
    ))
}

async fn finish(
    server: &Arc<Server>,
    principal: &Principal,
    repo: &Arc<Repository>,
    number: u64,
    record: &PullMerge,
) -> Result<Value> {
    let worker_server = Arc::clone(server);
    let worker_repo = Arc::clone(repo);
    let worker_principal = principal.clone();
    let record = record.clone();
    let (send, result) = tokio::sync::oneshot::channel();
    // The tracked worker drains the lease even if the browser times out. Keep
    // terminal Cell transitions and retries serialized around the Git outcome.
    server.receives.spawn(async move {
        let resource = lease_name(number);
        let completed = crab_remote::publication::with_internal_lease(
            &worker_repo.store,
            &worker_repo.layout,
            &resource,
            Duration::from_secs(300),
            &worker_server.cancellation,
            |cancel| {
                Box::pin(finish_locked(
                    &worker_server,
                    &worker_principal,
                    &worker_repo,
                    number,
                    &record,
                    cancel,
                ))
            },
        )
        .await;
        let _ = send.send(completed);
    });
    result.await.map_err(|_| Error::MergeBusy)??;
    pull_view(
        number,
        &app::actor(principal)?,
        server,
        repo,
        principal,
        None,
    )
    .await
}

pub(super) fn lease_name(number: u64) -> String {
    format!("pull-request-merge-{number}")
}

pub(super) async fn finish_locked(
    server: &Arc<Server>,
    principal: &Principal,
    repo: &Repository,
    number: u64,
    record: &PullMerge,
    cancel: CancellationToken,
) -> Result<()> {
    let grant = app::membership(server, principal, repo).await?;
    if !grant.principal.can_write(repo.id) {
        return Err(Error::MergePermission);
    }
    let actor = app::actor(principal)?;
    let pull = storage::pull(server, repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    if pull
        .merge
        .as_ref()
        .is_some_and(|done| done.request_id == record.request_id)
    {
        return Ok(());
    }
    if pull.state != PullState::Open
        || pull
            .merge_pending
            .as_ref()
            .is_none_or(|pending| pending.request_id != record.request_id)
    {
        return Err(Error::MergeConflict);
    }
    if !receipt_proven(repo, &pull, record).await? {
        let repository = repo
            .open_current(server, RepositoryOptions::default(), &cancel)
            .await?;
        let base = current_ref(&repository, &pull.base_ref);
        // Alpha reservations did not carry a plan ID. Their recorded target
        // can still finish through the tagged retry contract during upgrades.
        let legacy_complete = record.publication_plan.is_none()
            && base.as_deref() == Some(record.commit_oid.as_str());
        if !legacy_complete {
            // A current writer may recover a committed receipt. An unpublished
            // exception still belongs to the contributor who requested it.
            validate_bypass_actor(record, &actor)?;
            let fork = pull
                .head_repository
                .map(|id| super::git::fork_repository(server, repo, id))
                .transpose()?;
            let head = if let Some(fork) = &fork {
                fork.ref_oid(&pull.head_ref).await?
            } else {
                current_ref(&repository, &pull.head_ref)
            };
            if base.as_deref() != Some(record.base_oid.as_str())
                || head.as_deref() != Some(record.head_oid.as_str())
            {
                return reject_unpublished(
                    server,
                    repo,
                    number,
                    record,
                    &cancel,
                    Error::MergeConflict,
                )
                .await;
            }
            let source = PullPublication {
                number,
                head_ref: pull.head_ref.clone(),
                head_repository: fork,
                head_oid: oid(&record.head_oid)?,
                plan_id: record.publication_plan.clone(),
                cancel: cancel.clone(),
                candidate: None,
                tested_candidate: record.tested_candidate,
            };
            let key = (repo.config.owner.clone(), repo.config.name.clone());
            let base_oid = oid(&record.base_oid)?;
            let update = match record.method {
                _ if record.tested_candidate => {
                    receive::fast_forward(
                        Arc::clone(server),
                        principal.clone(),
                        key,
                        pull.base_ref.clone(),
                        base_oid,
                        oid(&record.commit_oid)?,
                        source,
                    )
                    .await
                }
                MergeMethod::FastForward if source.head_repository.is_some() => {
                    receive::merge_fork_head(
                        server,
                        principal,
                        &key,
                        crab_git::receive_plan::RefUpdate {
                            name: pull.base_ref,
                            old: Some(base_oid),
                            new: Some(source.head_oid),
                        },
                        source,
                    )
                    .await
                }
                MergeMethod::FastForward => {
                    receive::fast_forward(
                        Arc::clone(server),
                        principal.clone(),
                        key,
                        pull.base_ref.clone(),
                        base_oid,
                        source.head_oid,
                        source,
                    )
                    .await
                }
                MergeMethod::MergeCommit | MergeMethod::Squash | MergeMethod::Rebase => {
                    let fork = if let Some(fork) = &source.head_repository {
                        Some(
                            fork.open_current(server, RepositoryOptions::default(), &cancel)
                                .await?,
                        )
                    } else {
                        None
                    };
                    let git = super::git::PullGit {
                        base: repository,
                        fork,
                    };
                    let plan = match build_merge_plan(
                        &git,
                        &cancel,
                        base_oid,
                        source.head_oid,
                        tree::Strategy {
                            method: record.method,
                            actor: &record.author,
                            message: &record.message,
                            seconds: record.created_at / 1_000,
                            allow_empty_rebase: false,
                        },
                        None,
                    )
                    .await
                    {
                        Ok(plan) if plan.oid.to_string() == record.commit_oid => plan,
                        Ok(_) | Err(Error::MergeConflict | Error::UnsafeRebase) => {
                            return reject_unpublished(
                                server,
                                repo,
                                number,
                                record,
                                &cancel,
                                Error::MergeConflict,
                            )
                            .await;
                        }
                        Err(error) => return Err(error),
                    };
                    receive::publish_pull_request_objects(
                        Arc::clone(server),
                        principal.clone(),
                        key,
                        crab_git::receive_plan::RefUpdate {
                            name: pull.base_ref,
                            old: Some(base_oid),
                            new: Some(plan.oid),
                        },
                        plan.objects,
                        source,
                    )
                    .await
                }
            };
            if let Err(error) = update {
                let mapped = map_receive(error);
                if matches!(
                    mapped,
                    Error::MergeConflict
                        | Error::MergeBlocked
                        | Error::MergeCandidateChecks
                        | Error::MergeQueued
                        | Error::MergeQueueRequired
                        | Error::MergeUnresolvedConversations
                        | Error::MergeHeadBehind
                        | Error::MergeBypassUnavailable
                        | Error::MergeMethodDisabled
                        | Error::Archived
                        | Error::AutoMergeDisabled
                ) {
                    return reject_unpublished(server, repo, number, record, &cancel, mapped).await;
                }
                return Err(mapped);
            }
        }
    }
    storage::complete_merge(server, repo, number, record)
        .await
        .map(drop)
}

pub(super) async fn reject_unpublished(
    server: &Server,
    repo: &Repository,
    number: u64,
    record: &PullMerge,
    cancel: &CancellationToken,
    error: Error,
) -> Result<()> {
    if let Some(plan) = &record.publication_plan {
        // A receipt miss cannot prove that a durable intent never reached its
        // marker. Leave that submission intact for the original plan to recover.
        match crab_metadata::capsule_protocol::ensure_capsule_plan_unattempted(
            &repo.store,
            &repo.layout,
            plan,
        )
        .await
        {
            Ok(()) => {}
            Err(crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. }) => {
                return Err(Error::MergePending);
            }
            Err(error) => return Err(map_receive(error.into())),
        }
    }
    if cancel.is_cancelled() {
        return Err(Error::MergeBusy);
    }
    storage::abort_merge(server, repo, number, record).await?;
    Err(error)
}

pub(super) async fn receipt_proven(
    repo: &Repository,
    pull: &PullRequest,
    record: &PullMerge,
) -> Result<bool> {
    let Some(plan) = &record.publication_plan else {
        return Ok(false);
    };
    let Some(receipt) = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
        &repo.store,
        &repo.layout,
        plan,
    )
    .await
    .map_err(|error| map_receive(error.into()))?
    else {
        return Ok(false);
    };
    let [edit] = receipt.transaction().edits() else {
        return Err(Error::CellContract(
            "pull request receipt has unexpected ref edits",
        ));
    };
    if edit.ref_name() != pull.base_ref
        || edit.expected_old() != Some(record.base_oid.as_str())
        || edit.new_oid() != Some(record.commit_oid.as_str())
    {
        return Err(Error::CellContract(
            "pull request receipt does not match the reserved ref edit",
        ));
    }
    Ok(true)
}

async fn execute(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<MergeInput>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(repo.id) {
        return Err(Error::MergePermission);
    }
    let id = app::number(id)?;
    let Json(mut input) = input?;
    input.bypass_rulesets.sort_unstable();
    if !crate::cells::repository::rulesets::valid_bypass_ids(&input.bypass_rulesets) {
        return Err(Error::Invalid("Select distinct rulesets to bypass"));
    }
    let tested_candidate_oid = input
        .tested_candidate_oid
        .as_deref()
        .map(oid)
        .transpose()?
        .map(|oid| oid.to_string());
    if tested_candidate_oid.is_some()
        && (input.method != MergeMethod::MergeCommit
            || !input.bypass_rulesets.is_empty()
            || !input.message.is_empty())
    {
        return Err(Error::Invalid(
            "A tested candidate requires the original merge commit and branch rules",
        ));
    }
    let actor = app::actor(&principal)?;
    let request_id = app::submission(&input.request_id)?;
    let pull = storage::pull(&server, &repo, id)
        .await?
        .ok_or(Error::NotFound)?;
    let candidate = NewPullMerge {
        author: actor,
        request_id,
        method: input.method,
        pull_version: input.version,
        base_oid: input.base_oid,
        head_oid: input.head_oid,
        bypass_rulesets: input.bypass_rulesets,
        message: if tested_candidate_oid.is_some() {
            String::new()
        } else {
            message(input.method, &input.message, id, &pull.head_ref)?
        },
        tested_candidate_oid,
    };
    if let Some(record) = &pull.merge {
        if !same_request(record, &candidate) {
            return Err(Error::MergeConflict);
        }
        return Ok((
            StatusCode::OK,
            Json(
                pull_view(
                    pull.number,
                    &candidate.author,
                    &server,
                    &repo,
                    &principal,
                    None,
                )
                .await?,
            ),
        ));
    }
    if let Some(record) = &pull.merge_pending {
        if !same_request(record, &candidate) {
            return Err(Error::MergeConflict);
        }
        let value = finish(&server, &principal, &repo, pull.number, record).await?;
        return Ok((StatusCode::OK, Json(value)));
    }
    let record = admit(&server, &repo, &principal, &pull, &candidate).await?;
    let value = finish(&server, &principal, &repo, id, &record).await?;
    Ok((StatusCode::OK, Json(value)))
}

pub(super) fn message(
    method: MergeMethod,
    input: &str,
    number: u64,
    head_ref: &str,
) -> Result<String> {
    if matches!(method, MergeMethod::FastForward | MergeMethod::Rebase) {
        return Ok(String::new());
    }
    let message = input.trim();
    let message = if message.is_empty() {
        let verb = if method == MergeMethod::Squash {
            "Squash"
        } else {
            "Merge"
        };
        let branch = head_ref.strip_prefix("refs/heads/").unwrap_or(head_ref);
        format!("{verb} pull request #{number} from {branch}")
    } else {
        message.to_owned()
    };
    if message.chars().count() > 256 || message.chars().any(|ch| ch.is_control() && ch != '\n') {
        return Err(Error::Invalid(
            "Merge commit message must contain 1–256 characters",
        ));
    }
    Ok(message)
}

pub(super) async fn admit(
    server: &Arc<Server>,
    repo: &Repository,
    principal: &Principal,
    pull: &PullRequest,
    candidate: &NewPullMerge,
) -> Result<PullMerge> {
    let id = pull.number;
    if !principal.is_auto_merge() {
        super::auto_merge::require_direct_access(server, repo, id).await?;
    }
    if let Some(record) = storage::recover_merge(server, repo, candidate, id).await? {
        validate_bypass_actor(&record, &candidate.author)?;
        storage::begin_merge(server, repo, id, &record).await?;
        return Ok(record);
    }
    if pull.draft {
        return Err(Error::DraftPull);
    }
    if pull.state != PullState::Open
        || pull.version != candidate.pull_version
        || candidate.base_oid == candidate.head_oid
    {
        return Err(Error::MergeConflict);
    }
    let repository = super::git::PullGit::open(server, repo, pull.head_repository).await?;
    if repository
        .tips(&pull.base_ref, &pull.head_ref)
        .as_ref()
        .is_none_or(|(base, head)| *base != candidate.base_oid || *head != candidate.head_oid)
    {
        return Err(Error::MergeConflict);
    }
    let grant = app::membership(server, principal, repo).await?;
    if !grant.principal.can_write(repo.id) {
        return Err(Error::MergePermission);
    }
    let target = MergeCheckTarget::publication(
        principal.is_auto_merge(),
        candidate.tested_candidate_oid.as_deref(),
        &candidate.bypass_rulesets,
    );
    let report = requirements(
        server,
        repo,
        pull,
        Some(&candidate.base_oid),
        &candidate.head_oid,
        &grant,
        &grant.merge_reviewers(server).await?,
        target,
    )
    .await?;
    report.require_queue(target)?;
    if !matches!(target, MergeCheckTarget::Queue(_)) {
        super::merge_methods::require(server, repo, candidate.method).await?;
    }
    if let Some(tested_oid) = &candidate.tested_candidate_oid {
        super::candidate::verify_comparison(
            server,
            repo,
            &repository.base,
            pull,
            candidate,
            tested_oid,
            matches!(target, MergeCheckTarget::Queue(_)),
        )
        .await?;
    }
    report.enforce()?;
    let created_at = u64::try_from(crate::cells::unix_now_ms().map_err(Error::Cell)?)
        .map_err(|_| Error::CellContract("Cell clock is negative"))?;
    let commit_oid = match candidate.method {
        _ if let Some(tested_oid) = &candidate.tested_candidate_oid => oid(tested_oid)?,
        MergeMethod::FastForward => oid(&candidate.head_oid)?,
        MergeMethod::MergeCommit | MergeMethod::Squash | MergeMethod::Rebase => {
            build_merge_plan(
                &repository,
                &server.cancellation,
                oid(&candidate.base_oid)?,
                oid(&candidate.head_oid)?,
                tree::Strategy {
                    method: candidate.method,
                    actor: &candidate.author,
                    message: &candidate.message,
                    seconds: created_at / 1_000,
                    allow_empty_rebase: false,
                },
                None,
            )
            .await?
            .oid
        }
    };
    let publication = plan_id(repo, pull, candidate, commit_oid)?;
    let record = storage::reserve_merge(
        server,
        repo,
        id,
        candidate,
        commit_oid,
        created_at,
        publication,
    )
    .await?;
    storage::begin_merge(server, repo, id, &record).await?;
    Ok(record)
}
