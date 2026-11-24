use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    sync::{Arc, Mutex},
    time::Duration,
};

use crab_coordination::LFS_LOCKS_RESOURCE;
use crab_git::receive_wire;
use crab_lfs::LfsLockManager;
use crab_read::{dependency_proof::DependencyProofLimits, pointer_proof::PointerProofLimits};
use crab_remote::publication::LeaseOptions;
use crab_remote_git::RepositoryOptions;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{
    CandidatePublication, MERGE_CANDIDATE_PREFIX, PullPublication, ReceiveError, Result,
    check_cancelled, merge_candidate_ref, validate,
};
use crate::{
    auth::Principal,
    cells::repository::collaboration::links::{commits, keywords},
    repository::{MaintenanceTrigger, Repository, commit_closures, settings},
    server::{Server, webhooks},
};

pub(super) const TTL: Duration = Duration::from_secs(300);
const MAX_ACTIVE_LFS_LOCKS: usize = 10_000;
const MAX_IMPORT_REF_UPDATES: usize = 100_000;
const MAX_IMPORT_COMMAND_BYTES: usize = 32 * 1024 * 1024;
const MAX_RECEIVE_LOGICAL_OBJECTS: u64 = 5_000_000;
const MAX_RECEIVE_STORAGE_REQUESTS: u64 = 6_000_000;

fn repository_options(server: &Server) -> crate::Result<RepositoryOptions> {
    let mut operation = server.options.operation_limits();
    operation.max_duration = super::REQUEST_BUDGET;
    operation.max_logical_objects = MAX_RECEIVE_LOGICAL_OBJECTS;
    operation.max_storage_requests = MAX_RECEIVE_STORAGE_REQUESTS;
    Ok(RepositoryOptions::new(
        server.options.object_limits(),
        operation,
    )?)
}

#[derive(Serialize)]
struct NativePlanBinding<'a> {
    repository_owner: &'a str,
    repository_name: &'a str,
    actor_issuer: &'a str,
    actor_subject: &'a str,
    body_digest: [u8; 32],
}

fn native_plan_id(
    key: &(String, String),
    principal: &Principal,
    body_digest: [u8; 32],
) -> Result<String> {
    let identity = principal.identity().ok_or(ReceiveError::Forbidden)?;
    let binding = NativePlanBinding {
        repository_owner: &key.0,
        repository_name: &key.1,
        actor_issuer: &identity.issuer,
        actor_subject: &identity.subject,
        body_digest,
    };
    let request_digest = crab_metadata::receipts::publication_request_digest(&binding)?;
    // Native Git has no idempotency header. A fixed nonce makes an identical
    // wire request resolve to the same durable plan after a lost response.
    let plan = crab_metadata::receipts::publication_plan_id(&request_digest, &[0; 16]);
    Ok(blake3::Hash::from(plan).to_hex().to_string())
}

pub(super) enum Publication {
    NativePush,
    PullBranch(super::PullBranchPublication),
    ReviewSuggestions(super::SuggestionPublication),
    BranchUpdate(super::BranchUpdatePublication),
    ForkSync {
        source: Arc<Repository>,
        plan_id: String,
    },
    PullRequest(PullPublication),
}

impl Publication {
    fn plan_id(&self) -> Option<String> {
        match self {
            Self::NativePush => None,
            Self::PullBranch(edit) => Some(edit.plan_id.clone()),
            Self::ReviewSuggestions(batch) => Some(batch.plan_id.clone()),
            Self::BranchUpdate(update) => Some(update.plan_id.clone()),
            Self::ForkSync { plan_id, .. } => Some(plan_id.clone()),
            Self::PullRequest(pull) => pull.plan_id.clone(),
        }
    }

    pub(super) fn cancellation<'a>(
        &'a self,
        server: &'a CancellationToken,
    ) -> &'a CancellationToken {
        match self {
            Self::NativePush
            | Self::PullBranch(_)
            | Self::ReviewSuggestions(_)
            | Self::BranchUpdate(_)
            | Self::ForkSync { .. } => server,
            Self::PullRequest(pull) => &pull.cancel,
        }
    }
}

pub(super) struct PackPublication {
    pub visibility_base: Option<(String, gix_hash::ObjectId)>,
    pub kind: Publication,
}

struct ReceiveInput {
    pack: Option<BufReader<std::fs::File>>,
    plan_id: Option<String>,
    publication: Publication,
    visibility_bases: BTreeMap<String, (String, gix_hash::ObjectId)>,
    defer_readiness: bool,
    wire: bool,
}

struct PublishAttempt {
    directory: Arc<crate::local_disk::StagingDirectory>,
    plan_id: Option<String>,
    stack: Option<crate::api::stacked_pulls::Stack>,
    native_deletions: Option<crate::api::stacked_pulls::native::Deletions>,
}

impl PublishAttempt {
    fn stacks(&self) -> impl Iterator<Item = &crate::api::stacked_pulls::Stack> {
        self.stack.iter().chain(
            self.native_deletions
                .iter()
                .flat_map(crate::api::stacked_pulls::native::Deletions::stacks),
        )
    }
}

pub(super) async fn run(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: crate::local_disk::StagingDirectory,
    body_digest: [u8; 32],
    defer_readiness: bool,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let path = directory.path().join("receive");
    let (mut request, input) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut input = BufReader::new(std::fs::File::open(path)?);
        let request = if defer_readiness {
            receive_wire::read_request_with_limits(
                &mut input,
                receive_wire::ReceiveRequestLimits {
                    max_commands: MAX_IMPORT_REF_UPDATES,
                    max_command_bytes: MAX_IMPORT_COMMAND_BYTES,
                },
            )?
        } else {
            receive_wire::read_request(&mut input)?
        };
        if request.updates.is_empty() && !input.fill_buf()?.is_empty() {
            return Err(ReceiveError::Request("Unexpected data after receive probe"));
        }
        Ok((request, input))
    })
    .await??;
    if defer_readiness {
        // The loopback importer owns both ends of this exchange.  Always emit
        // a terminal receive-pack report so a client that omitted the
        // capability cannot wait indefinitely for an empty HTTP body.
        request.report_status = true;
    }
    if request.updates.is_empty() {
        return Ok(vec![]);
    }
    let plan_id = native_plan_id(key, principal, body_digest)?;
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: Some(input),
            plan_id: Some(plan_id),
            publication: Publication::NativePush,
            visibility_bases: BTreeMap::new(),
            defer_readiness,
            wire: true,
        },
        cancel,
    )
    .await
}

pub(crate) async fn publish_existing_objects(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    update: crab_git::receive_plan::RefUpdate,
    publication: Publication,
    cancel: &CancellationToken,
) -> Result<()> {
    let directory = server.local_staging.create(1, cancel).await?;
    let request = receive_wire::ReceiveRequest {
        updates: vec![update],
        report_status: false,
    };
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: None,
            plan_id: publication.plan_id(),
            publication,
            visibility_bases: BTreeMap::new(),
            defer_readiness: false,
            wire: false,
        },
        cancel,
    )
    .await
    .map(drop)
}

pub(crate) async fn publish_default_branch(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    expected_head: &str,
    branch: &str,
    expected_oid: gix_hash::ObjectId,
    cancel: &CancellationToken,
) -> Result<()> {
    let entry = server.repositories.get(key).ok_or(ReceiveError::NotFound)?;
    let current = crate::app::membership(server, principal, &entry).await?;
    if !current.principal.can_admin(entry.id) {
        return Err(ReceiveError::Forbidden);
    }
    let leased_entry = Arc::clone(&entry);
    crab_remote::publication::with_leases(
        &entry.store,
        &entry.layout,
        [branch.to_owned()],
        TTL,
        cancel,
        move |_holders, cancel| {
            let entry = Arc::clone(&leased_entry);
            async move {
                let entry = entry.as_ref();
                let changed = settings::with_ruleset_lease(
                    server,
                    settings::RulesetScope::Repository(entry),
                    &cancel,
                    |cancel| async move {
                        check_cancelled(&cancel)?;
                        let view = entry.open_ref_view().await?;
                        if view.head() != expected_head {
                            return Err(ReceiveError::DefaultBranchChanged);
                        }
                        let expected_oid = expected_oid.to_string();
                        if view.refs().get(branch) != Some(&expected_oid) {
                            return Err(ReceiveError::BranchChanged);
                        }
                        if view.head() == branch {
                            return Ok(false);
                        }
                        let current = crate::app::membership(server, principal, entry).await?;
                        if !current.principal.can_admin(entry.id) {
                            return Err(ReceiveError::Forbidden);
                        }
                        match crab_write::capsule_protocol::retarget_head(
                            &entry.layout,
                            view.root_snapshot().clone(),
                            expected_head,
                            branch,
                        )
                        .await
                        {
                            Ok(_) => Ok(true),
                            Err(crab_write::WriteError::CapsuleRootChanged { .. }) => {
                                Err(ReceiveError::DefaultBranchChanged)
                            }
                            Err(error) => Err(error.into()),
                        }
                    },
                )
                .await?;
                if changed {
                    let _readiness = crab_remote::publication::finish_committed(async {
                        entry.invalidate().await;
                        let repository = entry
                            .open_current(server, repository_options(server)?, &cancel)
                            .await?;
                        Ok::<_, crate::Error>(repository.generation())
                    })
                    .await;
                }
                Ok(())
            }
        },
    )
    .await
}

pub(super) async fn publish_pack(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: crate::local_disk::StagingDirectory,
    pack: BufReader<std::fs::File>,
    update: crab_git::receive_plan::RefUpdate,
    publication: PackPublication,
    cancel: &CancellationToken,
) -> Result<()> {
    let visibility_bases = publication
        .visibility_base
        .map(|base| BTreeMap::from([(update.name.clone(), base)]))
        .unwrap_or_default();
    let request = receive_wire::ReceiveRequest {
        updates: vec![update],
        report_status: false,
    };
    run_request(
        server,
        principal,
        key,
        directory,
        request,
        ReceiveInput {
            pack: Some(pack),
            plan_id: publication.kind.plan_id(),
            publication: publication.kind,
            visibility_bases,
            defer_readiness: false,
            wire: false,
        },
        cancel,
    )
    .await
    .map(drop)
}

async fn run_request(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    directory: crate::local_disk::StagingDirectory,
    request: receive_wire::ReceiveRequest,
    input: ReceiveInput,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let entry = super::target(server, principal, key).ok_or(ReceiveError::NotFound)?;
    if matches!(&input.publication, Publication::BranchUpdate(update) if update.repository != entry.id)
        || matches!(&input.publication, Publication::PullBranch(edit)
            if edit.source.id != entry.id || edit.source.ownership_epoch != entry.ownership_epoch
                || edit.source.identity.placement_generation() != entry.identity.placement_generation())
    {
        return Err(ReceiveError::NotFound);
    }
    let current = crate::app::membership(server, principal, &entry).await?;
    if !current.principal.can_write(entry.id) {
        return Err(ReceiveError::Forbidden);
    }
    let stack = if let (Publication::PullBranch(edit), [update]) =
        (&input.publication, request.updates.as_slice())
        && update.new.is_none()
        && edit.upstream.id == entry.id
    {
        if committed_pull_branch(server, &entry, &request.updates, edit).await? {
            return Ok(vec![]);
        }
        crate::api::stacked_pulls::plan(server, &entry, edit).await?
    } else {
        None
    };
    let native_deletions = if matches!(input.publication, Publication::NativePush)
        && !input.defer_readiness
        && server.cells().is_some()
        && request
            .updates
            .iter()
            .any(|update| update.new.is_none() && update.name.starts_with("refs/heads/"))
    {
        let plan = input.plan_id.as_deref().filter(|_| input.wire);
        if let Some(plan) = plan
            && committed_plan(&entry, &request.updates, plan).await?
        {
            let mut bytes = Vec::new();
            if request.report_status {
                receive_wire::report(&mut bytes, &request.updates, None, None)?;
            }
            return Ok(bytes);
        }
        match crate::api::stacked_pulls::native::plan(server, &entry, &request.updates, plan).await
        {
            Ok(deletions) => Some(deletions),
            Err(error) => return report_rejection(&request, error.into()),
        }
    } else {
        None
    };
    let plan_id = input.plan_id.clone().or_else(|| {
        if sends_publication_webhook(server, principal, &entry, &request.updates)
            || tracks_issue_closures(server, principal, &request.updates)
        {
            // A ref can be deleted and recreated with identical old and new
            // OIDs. Give an unplanned write its own receipt for this operation.
            Some(
                blake3::hash(uuid::Uuid::now_v7().as_bytes())
                    .to_hex()
                    .to_string(),
            )
        } else {
            None
        }
    });
    let attempt = PublishAttempt {
        directory: Arc::new(directory),
        plan_id: plan_id.clone(),
        stack,
        native_deletions,
    };
    let names: Vec<_> = request
        .updates
        .iter()
        .map(|update| update.name.clone())
        .chain(
            attempt
                .stacks()
                .flat_map(crate::api::stacked_pulls::Stack::local_refs),
        )
        .chain(match &input.publication {
            Publication::NativePush
            | Publication::PullBranch(_)
            | Publication::ReviewSuggestions(_)
            | Publication::ForkSync { .. } => None,
            Publication::PullRequest(pull)
                if pull.head_repository.is_none()
                    && !matches!(pull.candidate, Some(CandidatePublication::Remove)) =>
            {
                Some(pull.head_ref.clone())
            }
            Publication::BranchUpdate(update) if update.upstream.is_none() => {
                Some(update.base_ref.clone())
            }
            Publication::PullRequest(_) | Publication::BranchUpdate(_) => None,
        })
        .chain(match &input.publication {
            Publication::BranchUpdate(update) => {
                update.previous_head.as_ref().map(|(name, _)| name.clone())
            }
            _ => None,
        })
        .chain(match &input.publication {
            Publication::PullRequest(PullPublication {
                candidate: Some(CandidatePublication::Prepare { base_ref, .. }),
                ..
            }) => Some(base_ref.clone()),
            Publication::PullRequest(pull) if pull.tested_candidate => {
                Some(merge_candidate_ref(pull.number))
            }
            _ => None,
        })
        .chain(match &input.publication {
            Publication::PullRequest(PullPublication {
                candidate:
                    Some(CandidatePublication::Prepare {
                        base_ref,
                        build_ref,
                        ..
                    }),
                ..
            }) if base_ref != build_ref => Some(build_ref.clone()),
            _ => None,
        })
        .collect();
    let fork = match &input.publication {
        Publication::PullRequest(pull)
            if !matches!(pull.candidate, Some(CandidatePublication::Remove)) =>
        {
            pull.head_repository
                .as_ref()
                .map(|repo| (Arc::clone(repo), pull.head_ref.clone()))
        }
        _ => None,
    };
    let upstream = match &input.publication {
        Publication::BranchUpdate(update) => update
            .upstream
            .as_ref()
            .map(|repo| (Arc::clone(repo), update.base_ref.clone())),
        _ => None,
    };
    let entry = entry.as_ref();
    let request = &request;
    let defer_readiness = input.defer_readiness;
    let publish_destination = |cancel: CancellationToken| async move {
        crab_remote::publication::with_leases(
            &entry.store,
            &entry.layout,
            names,
            TTL,
            &cancel,
            move |_holders, cancel| async move {
                let run = |cancel| async move {
                    let mut fork_leases = Vec::new();
                    let result = async {
                        let stacks = attempt.stacks().collect::<Vec<_>>();
                        if !stacks.is_empty() {
                            crate::api::stacked_pulls::lease_forks(
                                &stacks,
                                LeaseOptions::renewing(TTL),
                                &cancel,
                                &mut fork_leases,
                            )
                            .await?;
                        }
                        // The prepared capsule future is large on debug Tokio workers.
                        Box::pin(publish_attempt(
                            server, principal, entry, request, input, attempt, &cancel,
                        ))
                        .await
                    }
                    .await;
                    while let Some(lease) = fork_leases.pop() {
                        lease.release().await;
                    }
                    result
                };
                if let Some((fork, branch)) = fork {
                    // Fork sync also locks upstream before fork. Hold both
                    // leases through publication so neither tip can move.
                    crab_remote::publication::with_leases(
                        &fork.store,
                        &fork.layout,
                        [branch],
                        TTL,
                        &cancel,
                        |_holders, scoped| run(scoped),
                    )
                    .await
                } else {
                    run(cancel).await
                }
            },
        )
        .await
    };
    let publish = |cancel: CancellationToken| async move {
        // Base-to-fork updates and fork-to-base merges share upstream-first
        // lock order even though they publish to opposite repositories.
        if let Some((upstream, branch)) = upstream {
            crab_remote::publication::with_leases(
                &upstream.store,
                &upstream.layout,
                [branch],
                TTL,
                &cancel,
                |_holders, scoped| publish_destination(scoped),
            )
            .await
        } else {
            publish_destination(cancel).await
        }
    };
    let result = match plan_id {
        Some(plan_id) => {
            let result = crab_remote::publication::with_capsule_plan(
                &entry.store,
                &entry.layout,
                &plan_id,
                TTL,
                cancel,
                publish,
            )
            .await;
            match result {
                Err(
                    error @ (ReceiveError::Write(_)
                    | ReceiveError::Protected
                    | ReceiveError::RefRule(_)),
                )
                | Err(
                    error @ ReceiveError::Metadata(
                        crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. },
                    ),
                ) => {
                    recover_native_plan(
                        server,
                        entry,
                        request,
                        &plan_id,
                        defer_readiness,
                        cancel,
                        error,
                    )
                    .await
                }
                response => response,
            }
        }
        None => publish(cancel.child_token()).await,
    };
    match result {
        Ok(bytes) => Ok(bytes),
        Err(error) => report_rejection(request, error),
    }
}

fn report_rejection(
    request: &receive_wire::ReceiveRequest,
    error: ReceiveError,
) -> Result<Vec<u8>> {
    let reason = match &error {
        ReceiveError::Protected => Some("protected branch requires a pull request"),
        ReceiveError::RefRule(message) => Some(*message),
        ReceiveError::Settings(error) => match error.as_ref() {
            crate::app::Error::StackedDeletionBlocked(message) => Some(*message),
            crate::app::Error::PullBranchInUse => {
                Some("an open pull request still uses the branch")
            }
            crate::app::Error::PullBranchChanged => {
                Some("the stacked pull requests changed; retry the push")
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(reason) = reason.filter(|_| request.report_status) {
        let mut bytes = Vec::new();
        receive_wire::report(&mut bytes, &request.updates, None, Some(reason))?;
        Ok(bytes)
    } else {
        Err(error)
    }
}

fn sends_publication_webhook(
    server: &Server,
    principal: &Principal,
    repository: &Repository,
    updates: &[crab_git::receive_plan::RefUpdate],
) -> bool {
    principal.import_grant().is_none()
        && webhooks::observes_publication(server, repository, updates)
}

struct RefMutation<'a> {
    updates: &'a [crab_git::receive_plan::RefUpdate],
    publication: &'a Publication,
    stack: Option<&'a crate::api::stacked_pulls::Stack>,
    native_deletions: Option<&'a crate::api::stacked_pulls::native::Deletions>,
    has_branch: bool,
    initial_head: Option<(crab_write::capsule_protocol::RootSnapshot, String, String)>,
    closing_ref: Option<&'a crab_git::receive_plan::RefUpdate>,
    closing_issues: BTreeMap<u64, String>,
    linear_history: Option<BTreeMap<String, bool>>,
}

fn tracks_issue_closures(
    server: &Server,
    principal: &Principal,
    updates: &[crab_git::receive_plan::RefUpdate],
) -> bool {
    server.cells().is_some()
        && principal.import_grant().is_none()
        && updates.iter().any(|update| {
            update.name.starts_with("refs/heads/")
                && update.new.is_some()
                && update.old != update.new
        })
}

fn updated_default<'a>(
    updates: &'a [crab_git::receive_plan::RefUpdate],
    default_ref: &str,
) -> Option<&'a crab_git::receive_plan::RefUpdate> {
    updates.iter().find(|update| {
        update.name == default_ref && update.new.is_some() && update.old != update.new
    })
}

impl RefMutation<'_> {
    fn linear_violation(&self, update: &crab_git::receive_plan::RefUpdate) -> Option<&'static str> {
        if update.new.is_none() {
            return None;
        }
        match self.linear_history.as_ref()?.get(&update.name) {
            Some(true) => None,
            Some(false) => Some("ref requires linear history; merge commits are prohibited"),
            None => Some("linear history policy changed; retry the ref update"),
        }
    }

    fn check_policy(
        &self,
        policy: &settings::RefPolicy,
        default_ref: &str,
        actor: crate::cells::repository::rulesets::BypassContext<'_>,
    ) -> Result<std::collections::BTreeSet<crate::cells::repository::rulesets::RulesetId>> {
        let pull = matches!(self.publication, Publication::PullRequest(_));
        let automatic_cleanup = matches!(
            self.publication,
            Publication::PullBranch(super::PullBranchPublication {
                binding: super::PullBranchBinding::Automatic { .. },
                ..
            })
        );
        if let Publication::PullRequest(PullPublication {
            candidate: Some(candidate),
            number,
            ..
        }) = self.publication
        {
            let [update] = self.updates else {
                return Err(ReceiveError::Request(
                    "Merge candidates require a single ref edit",
                ));
            };
            if update.name != merge_candidate_ref(*number)
                || update.new.is_some() != matches!(candidate, CandidatePublication::Prepare { .. })
            {
                return Err(ReceiveError::Request(
                    "Merge candidate publication targets a reserved ref",
                ));
            }
            return Ok(Default::default());
        }
        let rules = policy.active()?;
        let mut bypassed = std::collections::BTreeSet::new();
        for update in self.updates {
            if update.name.starts_with(MERGE_CANDIDATE_PREFIX)
                || matches!(update.name.as_str(), "refs/crab" | "refs/crab/merge-queue")
            {
                return Err(ReceiveError::RefRule(
                    "Merge candidate refs are managed by Crab",
                ));
            }
            for (id, rule) in rules.matches(&update.name, default_ref) {
                let violation = rule.violation(update, pull).or_else(|| {
                    if !pull && rule.require_linear_history {
                        self.linear_violation(update)
                    } else {
                        None
                    }
                });
                if let Some(message) = violation {
                    if pull || automatic_cleanup || !rule.allows_bypass(actor.for_rule(id), false) {
                        return Err(ReceiveError::RefRule(message));
                    }
                    bypassed.insert(id);
                }
            }
            if !pull && let Some(protection) = policy.protections.protection(&update.name) {
                if update.old.is_some() || self.has_branch {
                    return Err(if protection.require_merge_queue {
                        ReceiveError::RefRule("protected branch requires the merge queue")
                    } else {
                        ReceiveError::Protected
                    });
                }
                if protection.require_linear_history
                    && let Some(message) = self.linear_violation(update)
                {
                    return Err(ReceiveError::RefRule(message));
                }
            }
        }
        Ok(bypassed)
    }
}

async fn publish_attempt(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    input: ReceiveInput,
    attempt: PublishAttempt,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    check_cancelled(cancel)?;
    let current = crate::app::membership(server, principal, entry).await?;
    if !current.principal.can_write(entry.id) {
        return Err(ReceiveError::Forbidden);
    }
    if let Publication::PullBranch(edit) = &input.publication
        && committed_pull_branch(server, entry, &request.updates, edit).await?
    {
        // A later deletion or restoration does not reopen this action.
        return Ok(vec![]);
    }
    let defer_readiness = input.defer_readiness;
    let (mut view, mut repository) = entry
        .open_capsule_repository(server, repository_options(server)?, cancel)
        .await?;
    for _ in 0..2 {
        if request.updates.iter().all(|update| {
            view.ref_capsule_count(&update.name)
                < crate::repository::maintenance::FOREGROUND_CAPSULE_THRESHOLD
        }) {
            break;
        }
        entry.checkpoint_now(server, cancel).await?;
        (view, repository) = entry
            .open_capsule_repository(server, repository_options(server)?, cancel)
            .await?;
    }
    if request.updates.iter().any(|update| {
        view.ref_capsule_count(&update.name)
            >= crate::repository::maintenance::FOREGROUND_CAPSULE_THRESHOLD
    }) {
        return Err(crab_write::WriteError::Internal(
            "repository checkpoint could not bound the selected ref frontier".to_owned(),
        )
        .into());
    }
    let refs = view.refs().clone();
    if let (Publication::PullBranch(edit), [update]) =
        (&input.publication, request.updates.as_slice())
        && let super::PullBranchBinding::Automatic {
            ref_transaction, ..
        } = &edit.binding
        && view.visible_ref_transactions().get(&update.name) != Some(ref_transaction)
    {
        return Err(ReceiveError::BranchChanged);
    }
    if matches!(input.publication, Publication::ForkSync { .. })
        && request.updates.iter().any(|update| {
            refs.get(&update.name).map(String::as_str)
                != update.old.map(|oid| oid.to_string()).as_deref()
        })
    {
        return Err(ReceiveError::BranchChanged);
    }
    if let Some(grant) = principal.import_grant()
        && !grant.accepts_updates(view.head(), &refs, &request.updates)
    {
        return Err(ReceiveError::Request(
            "Git import must create exactly the pinned refs",
        ));
    }
    let mut inspect_visible_tip = None;
    if let Publication::PullRequest(pull) = &input.publication
        && !matches!(pull.candidate, Some(CandidatePublication::Remove))
    {
        // The source is leased alongside the destination. It participates in
        // admission only and never enters the set of refs being changed.
        let head = if let Some(fork) = &pull.head_repository {
            if !fork
                .fork_source(server)
                .is_some_and(|source| source.id == entry.id)
            {
                return Err(ReceiveError::NotFound);
            }
            fork.ref_oid(&pull.head_ref).await?
        } else {
            refs.get(&pull.head_ref).cloned()
        };
        if head.as_deref() != Some(pull.head_oid.to_string().as_str()) {
            return Err(ReceiveError::BranchChanged);
        }
        if pull.tested_candidate {
            let [update] = request.updates.as_slice() else {
                return Err(ReceiveError::Request(
                    "Expected one tested candidate ref update",
                ));
            };
            let tested_oid = update.new.ok_or(ReceiveError::BranchChanged)?;
            // A promotion must still point to the protected scratch commit.
            // Its paths need a fresh lock check even though Git can read it.
            if refs.get(&merge_candidate_ref(pull.number)) != Some(&tested_oid.to_string()) {
                return Err(ReceiveError::BranchChanged);
            }
            inspect_visible_tip = Some(tested_oid);
        }
        if let Some(CandidatePublication::Prepare {
            base_ref,
            build_ref,
            base_oid,
            target_oid,
            ..
        }) = &pull.candidate
            && (refs.get(base_ref) != Some(&target_oid.to_string())
                || refs.get(build_ref) != Some(&base_oid.to_string()))
        {
            return Err(ReceiveError::BranchChanged);
        }
    }
    if let Publication::BranchUpdate(update) = &input.publication {
        let base = if let Some(upstream) = &update.upstream {
            if !entry
                .fork_source(server)
                .is_some_and(|source| source.id == upstream.id)
            {
                return Err(ReceiveError::NotFound);
            }
            upstream.ref_oid(&update.base_ref).await?
        } else {
            refs.get(&update.base_ref).cloned()
        };
        if base.as_deref() != Some(update.base_oid.to_string().as_str()) {
            return Err(ReceiveError::BranchChanged);
        }
        if let Some((name, oid)) = &update.previous_head
            && refs.get(name) != Some(&oid.to_string())
        {
            return Err(ReceiveError::BranchChanged);
        }
    }
    let visibility = view.git_visibility_index().await?;
    let restored_pack = if let (Publication::PullBranch(edit), [update]) =
        (&input.publication, request.updates.as_slice())
        && let Some(commit) = update.new
    {
        crate::api::authorize_pull_branch(server, entry, edit, update, principal, None, cancel)
            .await?;
        if refs.contains_key(&update.name) {
            return Err(ReceiveError::BranchChanged);
        }
        super::fork_merge::restore_pack(&repository, &visibility, commit, cancel).await?
    } else {
        None
    };
    let pack = match restored_pack.as_ref() {
        Some(pack) if pack.size() <= super::MAX_BODY => {
            Some(BufReader::new(std::fs::File::open(pack.path())?))
        }
        Some(_) => return Err(ReceiveError::TooLarge),
        None => input.pack,
    };
    let has_branch = refs.keys().any(|name| name.starts_with("refs/heads/"));
    principal.identity().ok_or(ReceiveError::Forbidden)?;
    let initial_head = (!defer_readiness && !has_branch)
        .then(|| {
            request
                .updates
                .iter()
                .find(|update| update.name.starts_with("refs/heads/") && update.new.is_some())
        })
        .flatten()
        .filter(|update| update.name != view.head())
        .map(|update| {
            (
                view.root_snapshot().clone(),
                view.head().to_owned(),
                update.name.clone(),
            )
        });
    let closing_ref = tracks_issue_closures(server, principal, &request.updates)
        .then(|| updated_default(&request.updates, view.head()))
        .flatten();
    let mut mutation = RefMutation {
        updates: &request.updates,
        publication: &input.publication,
        stack: attempt.stack.as_ref(),
        native_deletions: attempt.native_deletions.as_ref(),
        has_branch,
        initial_head,
        closing_ref,
        closing_issues: BTreeMap::new(),
        linear_history: None,
    };
    let policy = if defer_readiness {
        settings::import_ref_policy(server, entry).await?
    } else {
        settings::load_ref_policy(server, entry).await?
    };
    let organization_teams = policy.organization_teams(server, &current).await?;
    let actor = current
        .bypass_context(entry.id)
        .organization_teams(organization_teams.as_deref());
    mutation.check_policy(&policy, view.head(), actor)?;
    let active = policy.active()?;
    let linear_refs = request
        .updates
        .iter()
        .filter(|update| {
            update.new.is_some()
                && (policy
                    .protections
                    .protection(&update.name)
                    .is_some_and(|rule| rule.require_linear_history)
                    || active
                        .matches(&update.name, view.head())
                        .any(|(_, rule)| rule.require_linear_history))
        })
        .map(|update| update.name.clone())
        .collect();
    let visibility_bases = input.visibility_bases;
    let default_branch = repository
        .refs()
        .head
        .as_ref()
        .map(|head| head.name.clone());
    let max_ref_updates = if defer_readiness {
        MAX_IMPORT_REF_UPDATES
    } else {
        1024
    };
    // Only the generated PR rebase can replace an existing head. It still
    // needs the exact leased refs, fresh PR authorization, and normal ref policy.
    let rewritten_ref = match (&input.publication, request.updates.as_slice()) {
        (Publication::BranchUpdate(branch), [update]) if branch.rebase => Some(update.name.clone()),
        (
            Publication::PullRequest(PullPublication {
                candidate: Some(CandidatePublication::Prepare { .. }),
                ..
            }),
            [update],
        ) => Some(update.name.clone()),
        _ => None,
    };
    let options = validate::options(
        entry.layout.clone(),
        default_branch,
        max_ref_updates,
        rewritten_ref,
        inspect_visible_tip,
        linear_refs,
    );
    let closing_issues = Arc::new(Mutex::new(BTreeMap::new()));
    let commit_observer = closing_ref.map(|update| {
        let issues = Arc::clone(&closing_issues);
        let observe: Box<crab_git::receive_plan::CommitObserver<'static>> =
            Box::new(move |oid, message| {
                if message.contains(&b'#') {
                    let mut issues = issues
                        .lock()
                        .map_err(|_| crab_git::receive_plan::ReceivePlanError::Cancelled)?;
                    for number in keywords::numbers(&String::from_utf8_lossy(message)) {
                        issues.entry(number).or_insert_with(|| oid.to_string());
                    }
                    if issues.len() > commits::MAX_CLOSURES {
                        return Err(crab_git::receive_plan::ReceivePlanError::Limit(
                            "closing issue references",
                        ));
                    }
                }
                Ok(())
            });
        (update.name.clone(), observe)
    });
    let scratch = server
        .local_staging
        .scratch(Arc::clone(&attempt.directory))?;
    let prepared = match crab_remote::prepare::prepare_capsule_with_commit_observer(
        repository.clone(),
        visibility,
        scratch,
        pack,
        request.updates.clone(),
        visibility_bases,
        cancel,
        options,
        commit_observer,
    )
    .await
    .map_err(validate::map_error)
    {
        Ok(prepared) => prepared,
        Err(
            error @ (ReceiveError::Pack(_) | ReceiveError::Graph(_) | ReceiveError::Request(_)),
        ) if request.report_status && error.staging_status().is_none() => {
            tracing::warn!(error = ?error, "Git receive validation rejected");
            let mut bytes = Vec::new();
            let unpack = matches!(error, ReceiveError::Pack(_)).then_some("incoming pack rejected");
            let reason = match &error {
                ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Ref {
                    reason,
                    ..
                }) => *reason,
                ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Limit(
                    "closing issue references",
                )) => "too many closing issue references",
                _ => "incoming refs or graph rejected",
            };
            receive_wire::report(&mut bytes, &request.updates, unpack, Some(reason))?;
            return Ok(bytes);
        }
        Err(error) => return Err(error),
    };
    mutation.closing_issues =
        std::mem::take(&mut *closing_issues.lock().map_err(|_| ReceiveError::Cancelled)?);
    let changed_path_hashes = prepared.plan().changed_path_hashes().clone();
    mutation.linear_history = Some(prepared.plan().linear_history().clone());
    let dependency_limits = dependency_limits();
    let pointers = prepared.plan().pointers();
    let source = match &input.publication {
        _ if pointers.is_empty() => None,
        Publication::ForkSync { source, .. } => Some(Arc::clone(source)),
        Publication::PullRequest(pull) => pull.head_repository.clone(),
        Publication::BranchUpdate(update) => update.upstream.clone(),
        _ => match principal.import_grant() {
            Some(grant) => grant.fork_source(server).await?,
            None => None,
        },
    };
    let artifacts = if let Some(source) = source {
        super::fork_lfs::copy(
            server,
            principal,
            entry,
            &source,
            pointers,
            dependency_limits.max_dependencies,
            cancel,
        )
        .await?;
        prepared
            .upload_capsule_from_shared_source(
                &view,
                source.layout.repo_prefix(),
                dependency_limits,
                cancel,
            )
            .await
    } else {
        prepared
            .upload_capsule(&view, dependency_limits, cancel)
            .await
    }
    .map_err(validate::map_error)?;
    let outcome = if changed_path_hashes.is_empty() {
        commit_prepared(
            server,
            principal,
            entry,
            artifacts,
            mutation,
            attempt.plan_id.as_deref(),
            cancel,
        )
        .await
    } else {
        let subject = principal.identity().ok_or(ReceiveError::Forbidden)?.subject;
        // Lock endpoints take this lease for every mutation. Keep the final lock
        // read and journal commit together so replicas cannot create a lock between them.
        crab_remote::publication::with_internal_lease(
            &entry.store,
            &entry.layout,
            LFS_LOCKS_RESOURCE,
            TTL,
            cancel,
            move |lease_cancel| async move {
                check_cancelled(&lease_cancel)?;
                let manager = LfsLockManager::lfs(entry.store.clone(), &entry.config.prefix);
                let locks = tokio::select! {
                    () = lease_cancel.cancelled() => return Err(ReceiveError::Cancelled),
                    result = manager.list_page(None, None, None, MAX_ACTIVE_LFS_LOCKS + 1) => result?,
                };
                if locks.len() > MAX_ACTIVE_LFS_LOCKS {
                    return Err(ReceiveError::LfsLockLimit);
                }
                if locks.iter().any(|lock| {
                    lock.owner != subject
                        && changed_path_hashes.contains(blake3::hash(lock.path.as_bytes()).as_bytes())
                }) {
                    return Err(ReceiveError::Locked);
                }
                commit_prepared(
                    server,
                    principal,
                    entry,
                    artifacts,
                    mutation,
                    attempt.plan_id.as_deref(),
                    &lease_cancel,
                )
                .await
            },
        )
        .await
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(ReceiveError::Locked) if request.report_status => {
            let mut bytes = Vec::new();
            receive_wire::report(
                &mut bytes,
                &request.updates,
                None,
                Some("path is locked by another user"),
            )?;
            return Ok(bytes);
        }
        Err(error) => return Err(error),
    };
    if let crab_remote::prepare::CapsuleCommitOutcome::Indeterminate { source, .. } = outcome {
        // The deterministic native plan can recover a committed receipt after
        // transport loss, but an absent receipt is not proof of rejection.
        return Err(ReceiveError::Write(*source));
    }
    // Acknowledge known ref commitment even if read indexes remain pending.
    // Imports explicitly defer this expensive rebuild until all refs arrive.
    if defer_readiness {
        entry.invalidate().await;
    } else {
        let _readiness = crab_remote::publication::finish_committed(async {
            entry.invalidate().await;
            let repository = entry
                .open_current(server, repository_options(server)?, cancel)
                .await?;
            entry
                .schedule_maintenance(server, MaintenanceTrigger::Repair)
                .await?;
            Ok::<_, crate::Error>(repository.generation())
        })
        .await;
    }
    let mut bytes = Vec::new();
    if request.report_status {
        receive_wire::report(&mut bytes, &request.updates, None, None)?;
    }
    Ok(bytes)
}

async fn recover_native_plan(
    server: &Server,
    entry: &Repository,
    request: &receive_wire::ReceiveRequest,
    plan_id: &str,
    defer_readiness: bool,
    cancel: &CancellationToken,
    original: ReceiveError,
) -> Result<Vec<u8>> {
    match committed_plan(entry, &request.updates, plan_id).await {
        Ok(true) => (),
        Ok(false) => return Err(original),
        Err(error) => {
            tracing::warn!(%plan_id, %error, "native receive plan reconciliation was inconclusive");
            return Err(original);
        }
    }
    // The receipt proves the ref visibility boundary. Index readiness remains
    // best-effort and cannot turn a recovered commit into a rejection.
    if !defer_readiness {
        let _readiness = crab_remote::publication::finish_committed(async {
            entry.invalidate().await;
            let repository = entry
                .open_current(server, repository_options(server)?, cancel)
                .await?;
            entry
                .schedule_maintenance(server, MaintenanceTrigger::Repair)
                .await?;
            Ok::<_, crate::Error>(repository.generation())
        })
        .await;
    }
    let mut bytes = Vec::new();
    if request.report_status {
        receive_wire::report(&mut bytes, &request.updates, None, None)?;
    }
    Ok(bytes)
}

pub(super) async fn committed_plan(
    entry: &Repository,
    updates: &[crab_git::receive_plan::RefUpdate],
    plan_id: &str,
) -> Result<bool> {
    let Some(receipt) = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
        &entry.store,
        &entry.layout,
        plan_id,
    )
    .await?
    else {
        return Ok(false);
    };
    let receipt_updates = receipt
        .transaction()
        .edits()
        .iter()
        .map(|edit| (edit.ref_name(), (edit.expected_old(), edit.new_oid())))
        .collect::<BTreeMap<_, _>>();
    if updates.len() != receipt_updates.len()
        || updates.iter().any(|update| {
            let expected_old = update.old.map(|oid| oid.to_string());
            let new_oid = update.new.map(|oid| oid.to_string());
            receipt_updates
                .get(update.name.as_str())
                .is_none_or(|(receipt_old, receipt_new)| {
                    *receipt_old != expected_old.as_deref() || *receipt_new != new_oid.as_deref()
                })
        })
    {
        return Err(ReceiveError::Request(
            "Publication receipt does not match the ref update",
        ));
    }
    Ok(true)
}

async fn committed_pull_branch(
    server: &Server,
    entry: &Repository,
    updates: &[crab_git::receive_plan::RefUpdate],
    edit: &super::PullBranchPublication,
) -> Result<bool> {
    if !committed_plan(entry, updates, &edit.plan_id).await? {
        return Ok(false);
    }
    if let [update] = updates
        && update.new.is_none()
        && edit.upstream.id == entry.id
    {
        crate::api::stacked_pulls::validate_replay(server, entry, &update.name, edit).await?;
    }
    Ok(true)
}

async fn commit_prepared(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    artifacts: crab_remote::prepare::CapsuleArtifacts,
    mutation: RefMutation<'_>,
    plan_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<crab_remote::prepare::CapsuleCommitOutcome> {
    let checks_ruleset_policy = matches!(mutation.publication, Publication::PullRequest(_))
        || mutation.updates.iter().any(|update| {
            update.name.starts_with("refs/heads/") || update.name.starts_with("refs/tags/")
        });
    // The policy and LFS leases nest this callback. Boxing it leaves room
    // for object-store I/O on a default debug worker stack.
    let commit = |cancel| {
        Box::pin(async move {
            commit_authorized(
                server, principal, entry, artifacts, mutation, plan_id, &cancel,
            )
            .await
        })
    };
    if checks_ruleset_policy {
        settings::with_ruleset_lease(
            server,
            settings::RulesetScope::Repository(entry),
            cancel,
            commit,
        )
        .await
    } else {
        commit(cancel.clone()).await
    }
}

async fn commit_authorized(
    server: &Server,
    principal: &Principal,
    entry: &Repository,
    artifacts: crab_remote::prepare::CapsuleArtifacts,
    mutation: RefMutation<'_>,
    plan_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<crab_remote::prepare::CapsuleCommitOutcome> {
    check_cancelled(cancel)?;
    // Pack preparation can outlive a grant. Revalidate while the ref lease is
    // held so a permission downgrade cannot use an earlier request snapshot.
    let current = crate::app::membership(server, principal, entry).await?;
    if !current.principal.can_write(entry.id) {
        return Err(ReceiveError::Forbidden);
    }
    let identity = principal.identity().ok_or(ReceiveError::Forbidden)?;
    if principal.import_grant().is_none()
        && crate::repository::settings::load_lifecycle(server, entry)
            .await
            .map_err(|error| ReceiveError::Settings(Box::new(error)))?
            .archived
    {
        return Err(ReceiveError::Archived);
    }
    let closure_default = if tracks_issue_closures(server, principal, mutation.updates) {
        Some(entry.default_ref().await?)
    } else {
        None
    };
    let closing_ref = if let Some(default_ref) = &closure_default {
        let update = updated_default(mutation.updates, default_ref);
        if update != mutation.closing_ref {
            return Err(ReceiveError::DefaultBranchChanged);
        }
        update
    } else {
        None
    };
    let bypassed = if !matches!(mutation.publication, Publication::PullRequest(_)) {
        let policy = if principal.import_grant().is_some() {
            settings::import_ref_policy(server, entry).await?
        } else {
            settings::load_ref_policy(server, entry).await?
        };
        let default_ref = match &closure_default {
            Some(reference) => reference.clone(),
            None => policy.default_ref(entry).await?,
        };
        let organization_teams = policy.organization_teams(server, &current).await?;
        let actor = current
            .bypass_context(entry.id)
            .organization_teams(organization_teams.as_deref());
        mutation.check_policy(&policy, &default_ref, actor)?
    } else {
        Default::default()
    };
    if let Publication::PullRequest(pull) = mutation.publication {
        let [update] = mutation.updates else {
            return Err(ReceiveError::Request(
                "Pull request merges require a single ref edit",
            ));
        };
        if let Some(candidate) = &pull.candidate {
            crate::api::authorize_merge_candidate(server, entry, pull, candidate).await?;
        } else {
            let linear = mutation
                .linear_history
                .as_ref()
                .and_then(|status| status.get(&update.name))
                .copied();
            crate::api::authorize_merge_publication(
                server, entry, &current, pull, plan_id, update, linear,
            )
            .await
            .map_err(|error| ReceiveError::Settings(Box::new(error)))?;
        }
    }
    if let Publication::ReviewSuggestions(suggestion) = mutation.publication {
        let [update] = mutation.updates else {
            return Err(ReceiveError::Request(
                "Review suggestions require a single branch update",
            ));
        };
        crate::api::authorize_suggestion_publication(server, entry, suggestion, &update.name)
            .await?;
    }
    if let Publication::BranchUpdate(branch) = mutation.publication {
        let [update] = mutation.updates else {
            return Err(ReceiveError::Request(
                "Branch updates require a single ref edit",
            ));
        };
        crate::api::authorize_branch_update(server, entry, branch, &update.name).await?;
    }
    if let Publication::PullBranch(edit) = mutation.publication {
        let [update] = mutation.updates else {
            return Err(ReceiveError::Request(
                "Pull request branch actions require one ref edit",
            ));
        };
        crate::api::authorize_pull_branch(
            server,
            entry,
            edit,
            update,
            &current.principal,
            mutation.stack,
            cancel,
        )
        .await?;
    }
    if let Some(deletions) = mutation.native_deletions {
        Box::pin(deletions.apply(server, entry, &current.principal, cancel)).await?;
    }
    if let Some(plan) = plan_id
        && sends_publication_webhook(server, principal, entry, mutation.updates)
    {
        // Reserve before publication so a process loss after the ref CAS
        // cannot lose the event. Delivery requires the exact plan receipt.
        webhooks::reserve_publication(server, entry, plan, mutation.updates).await?;
    }
    let mut closing_publication = None;
    if let Some(update) = closing_ref
        && !mutation.closing_issues.is_empty()
    {
        let plan = plan_id.ok_or(ReceiveError::Request(
            "Missing issue closure publication plan",
        ))?;
        let publication = commits::Publication {
            plan: plan.to_owned(),
            reference: update.name.clone(),
            before: update.old.map(|old| old.to_string()),
            after: update.new.ok_or(ReceiveError::BranchChanged)?.to_string(),
        };
        if commit_closures::reserve(
            server,
            entry,
            commits::Reservation {
                publication: publication.clone(),
                actor: (&identity).into(),
                issues: mutation.closing_issues,
            },
        )
        .await?
        {
            closing_publication = Some(publication);
        }
    }
    if let Some(plan) = plan_id
        && matches!(mutation.publication, Publication::NativePush)
        && principal.import_grant().is_none()
        && principal.automation_credential().is_none()
    {
        let default = if entry.forked_from.is_none() {
            mutation
                .initial_head
                .as_ref()
                .map(|(_, _, head)| head.as_str())
                .or(closure_default.as_deref())
        } else {
            None
        };
        // Persist the candidate ahead of the ref commit so a lost response
        // cannot erase a successful push. The browser verifies the receipt.
        super::recent_pushes::record(server, entry, &identity, plan, mutation.updates, default)
            .await;
    }
    let outcome = artifacts
        .commit(plan_id, TTL, cancel)
        .await
        .map_err(validate::map_error)?;
    if let crab_remote::prepare::CapsuleCommitOutcome::Committed { transaction_id } = &outcome {
        if let Some(publication) = closing_publication {
            commit_closures::finish(server, entry, publication).await;
        }
        if !bypassed.is_empty() {
            tracing::info!(repository = %entry.id, %transaction_id, rulesets = ?bypassed, "Git publication used ruleset bypass");
        }
        if let Some((root, expected_head, head)) = mutation.initial_head
            && let Err(error) = crab_write::capsule_protocol::retarget_head(
                &entry.layout,
                root,
                &expected_head,
                &head,
            )
            .await
        {
            // The ref transaction is committed. Acknowledge it even if HEAD needs repair.
            tracing::error!(%head, %error, "first branch committed but HEAD retargeting failed");
        }
    }
    Ok(outcome)
}

fn dependency_limits() -> DependencyProofLimits {
    DependencyProofLimits {
        max_dependencies: 1024,
        // Content is streamed; logical file sizes do not measure retained
        // memory or repository capacity. Bound metadata, workers and elapsed
        // time instead, while preserving full origin verification.
        max_total_file_bytes: u64::MAX,
        max_duration: super::REQUEST_BUDGET,
        lookup: crab_metadata::file_index_lookup::FileIndexLookupLimits {
            max_files: 1024,
            max_shard_visits: 4096,
            max_shard_bytes: 128 * 1024 * 1024,
            max_recipe_entries: 1_000_000,
        },
        content: PointerProofLimits {
            max_file_bytes: u64::MAX,
            max_shard_bytes: 128 * 1024 * 1024,
            max_xorb_bytes: 128 * 1024 * 1024,
            max_read_bytes: u64::MAX,
            max_chunks: 1_000_000,
            max_duration: super::REQUEST_BUDGET,
        },
    }
}
