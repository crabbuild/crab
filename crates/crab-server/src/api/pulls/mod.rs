use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use crab_remote_git::RemoteGitRepository;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BranchProtection,
    api::assignees::Selection,
    api::checks::{self, CheckRun},
    api::labels,
    api::reactions,
    api::statuses::{self, StatusState},
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::{MergeReviewers, collaboration::Target},
    repository::Repository,
    server::Server,
};

pub(crate) mod auto_merge;
mod base;
mod branch_update;
pub(super) mod candidate;
pub(crate) use candidate::authorize_merge_candidate;
mod codeowners;
pub(crate) use branch_update::authorize_branch_update;
pub(super) mod comments;
mod git;
pub(super) use git::review_head;
mod merge;
mod merge_methods;
mod readiness;
mod review_requests;
mod review_threads;
pub(crate) use review_threads::authorize_suggestion_publication;
mod reviewers;
pub(super) mod reviews;
pub(super) mod rollups;
pub(super) mod storage;
mod viewed;
use crate::cells::repository::{ListPulls, PullSummary};
use storage::{NewPullRequest, PullRequest, PullState};

pub(super) async fn require_check_head(
    server: &Server,
    repo: &Repository,
    number: u64,
    oid: gix_hash::ObjectId,
) -> Result<String> {
    let pull = storage::pull(server, repo, app::number(number)?)
        .await?
        .ok_or(Error::CommitNotFound)?;
    if pull.head_repository.is_none() {
        return statuses::require_commit(server, repo, oid).await;
    }
    verified_fork_check_head(server, repo, &pull, oid).await
}

pub(super) async fn require_fork_check_head(
    server: &Server,
    repo: &Repository,
    number: u64,
    oid: gix_hash::ObjectId,
) -> Result<String> {
    let pull = storage::pull(server, repo, number)
        .await?
        .ok_or(Error::CommitNotFound)?;
    verified_fork_check_head(server, repo, &pull, oid).await
}

async fn verified_fork_check_head(
    server: &Server,
    repo: &Repository,
    pull: &PullRequest,
    oid: gix_hash::ObjectId,
) -> Result<String> {
    let id = pull.head_repository.ok_or(Error::CommitNotFound)?;
    let requested = oid.to_string();
    let current = if let Some(merge) = &pull.merge {
        Some(merge.head_oid.clone())
    } else {
        git::fork_repository(server, repo, id)?
            .ref_oid(&pull.head_ref)
            .await?
    };
    if current.as_deref() != Some(requested.as_str()) {
        return Err(Error::CommitNotFound);
    }
    Ok(requested)
}

fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/pulls", get(list).post(create))
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}",
            get(detail).patch(edit),
        )
        .merge(comments::routes())
        .merge(branch_update::routes())
        .merge(base::routes())
        .merge(reviews::routes())
        .merge(review_threads::routes())
        .merge(merge::routes())
        .merge(candidate::routes())
        .merge(auto_merge::routes())
        .merge(merge_methods::routes())
        .merge(codeowners::routes())
        .merge(readiness::routes())
        .merge(review_requests::routes())
        .merge(reviewers::routes())
        .merge(viewed::routes())
        .layer(axum::extract::DefaultBodyLimit::max(160 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn pull_view(
    number: u64,
    actor: &Identity,
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    current: Option<&(String, String)>,
) -> Result<Value> {
    let details = storage::detail(server, repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    let pull = &details.pull;
    let head_repository = pull
        .head_repository
        .and_then(|id| git::fork_repository(server, repo, id).ok());
    let base_oid = pull.merge.as_ref().map_or_else(
        || current.map_or(pull.comparison_snapshot().0, |value| value.0.as_str()),
        |merge| merge.base_oid.as_str(),
    );
    let head_oid = pull.merge.as_ref().map_or_else(
        || current.map_or(pull.comparison_snapshot().1, |value| value.1.as_str()),
        |merge| merge.head_oid.as_str(),
    );
    let branches_available = pull.merge.is_some() || current.is_some();
    let grant = app::membership(server, principal, repo).await?;
    let reviewers = grant.merge_reviewers(server).await?;
    let labels = if pull.label_ids.is_empty() {
        vec![]
    } else {
        labels::catalog(server, repo).await?
    };
    let (requirements, merge_methods, github_actions_ref) = tokio::try_join!(
        requirements(
            server,
            repo,
            pull,
            branches_available.then_some(base_oid),
            head_oid,
            &grant,
            &reviewers,
            MergeCheckTarget::default(),
        ),
        merge_methods::load(server, repo),
        async {
            if head_repository.is_some() && pull.state == PullState::Open && current.is_some() {
                super::integrations::fork_mirror_ref(server, repo, pull.number).await
            } else {
                Ok(None)
            }
        },
    )?;
    let auto_merge_configured =
        merge_methods.auto_merge_generation > 0 || merge_methods.merge_queue_generation > 0;
    let merge_queued = pull.state == PullState::Open
        && auto_merge::active_queue(server, repo, pull.number)
            .await?
            .is_some();
    let merge_methods = merge_methods.enabled;
    let eligible_bypass = pull.state == PullState::Open
        && !merge_queued
        && !pull.draft
        && pull.merge_pending.is_none()
        && branches_available
        && !merge_methods.is_empty()
        && (requirements.queue_required || !requirements.satisfied)
        && !requirements.bypass_options.is_empty();
    let can_bypass = if eligible_bypass {
        let requested: Vec<_> = requirements
            .bypass_options
            .iter()
            .map(|rule| rule.id)
            .collect();
        match self::requirements(
            server,
            repo,
            pull,
            Some(base_oid),
            head_oid,
            &grant,
            &reviewers,
            MergeCheckTarget::Head(&requested),
        )
        .await
        {
            Ok(bypassed) => bypassed.satisfied && !bypassed.queue_required,
            Err(Error::MergeBypassUnavailable) => false,
            Err(error) => return Err(error),
        }
    } else {
        false
    };
    let reaction = reactions::one(
        server,
        repo,
        actor,
        Target::Pull {
            number: pull.number,
        },
    )
    .await?;
    let authors = super::profiles::attribution::Authors::load(
        server,
        principal,
        std::iter::once(&pull.author)
            .chain(pull.merge.as_ref().map(|merge| &merge.author))
            .chain(pull.merge_pending.as_ref().map(|merge| &merge.author)),
    )
    .await?;
    let assignees = Selection::load(server, principal, repo, pull.assignees.iter()).await?;
    let can_write = assignees.principal.can_write(repo.id);
    let can_triage = assignees.principal.can_triage(repo.id);
    let can_write_head = if can_write
        && pull.state == PullState::Open
        && pull.merge_pending.is_none()
        && current.is_some()
    {
        let upstream = crate::repository::settings::load_lifecycle(server, repo).await?;
        if let Some(fork) = &head_repository {
            !upstream.archived
                && !crate::repository::settings::load_lifecycle(server, fork)
                    .await?
                    .archived
        } else {
            !upstream.archived && pull.head_repository.is_none()
        }
    } else {
        false
    };
    Ok(json!({
        "draft": pull.draft,
        "number": pull.number,
        "title": pull.title,
        "body": pull.body,
        "reactions": reaction,
        "state": pull.state,
        "author": authors.view(&pull.author),
        "base_ref": pull.base_ref,
        "base_oid": base_oid,
        "base_revision": pull.retargeted_base.as_ref().map(|base| base.revision),
        "head_ref": pull.head_ref,
        "head_repository": head_repository.as_ref().map(|fork| json!({"id":fork.id,"owner":fork.config.owner,"name":fork.config.name})),
        "head_oid": head_oid,
        "github_actions_ref": github_actions_ref,
        "original_base_oid": pull.base_oid,
        "original_head_oid": pull.head_oid,
        "version": pull.version,
        "created_at": pull.created_at,
        "closed_at":details.lifecycle.closed_at_ms,
        "merged_at":details.lifecycle.merged_at_ms,
        "updated_at": pull.updated_at,
        "labels": labels::selection_view(&pull.label_ids, &labels),
        "assignees": assignees.view(&pull.assignees),
        "can_label": can_triage,
        "can_assign": can_triage,
        "can_apply_suggestions": can_write_head,
        "can_update_branch": can_write_head,
        "can_change_base": pull.state == PullState::Open && pull.merge_pending.is_none()
            && (can_write || same_author(&pull.author, actor)),
        "can_edit": same_author(&pull.author, actor),
        "can_manage": pull.state != PullState::Merged
            && pull.merge_pending.is_none()
            && (can_triage || same_author(&pull.author, actor)),
        "can_decide": pull.state == PullState::Open
            && current.is_some()
            && !same_author(&pull.author, actor),
        "review_counts_for_merge": reviewers.contains(&actor.issuer, &actor.subject),
        "can_merge": pull.state == PullState::Open
            && !merge_queued
            && !pull.draft
            && can_write
            && !merge_methods.is_empty()
            && (pull.merge_pending.is_some()
                || (!requirements.queue_required && current.is_some() && requirements.satisfied)),
        "branches_available": branches_available,
        "merge_methods": merge_methods,
        "auto_merge_configured": auto_merge_configured,
        "merge_queued": merge_queued,
        "merge_queue_required": requirements.queue_required,
        "merge_requirements": {
            "bypass_options": if can_bypass { requirements.bypass_options.as_slice() } else { &[] },
            "protected": requirements.protected,
            "required_approvals": requirements.required_approvals,
            "rulesets": requirements.rulesets,
            "approvals": requirements.approvals,
            "changes_requested": requirements.changes_requested,
            "code_owners": requirements.code_owners,
            "unresolved_conversations": requirements.unresolved_conversations,
            "head_up_to_date": requirements.head_up_to_date,
            "checks_satisfied": requirements.checks_satisfied,
            "checks": requirements.checks.iter().map(|check| check.view(head_oid)).collect::<Vec<_>>(),
            "satisfied": requirements.satisfied,
        },
        "merge": pull.merge.as_ref().map(|merge| json!({
            "author": authors.view(&merge.author),
            "method": merge.method,
            "commit_oid": merge.commit_oid,
            "message": merge.message,
            "created_at": merge.created_at,
            "bypass_rulesets": merge.bypass_rulesets,
            "tested_candidate_oid": merge.tested_candidate.then_some(&merge.commit_oid),
        })),
        "merge_pending": pull.merge_pending.as_ref().map(|merge| json!({
            "request_id": merge.request_id,
            "author": authors.view(&merge.author),
            "method": merge.method,
            "pull_version": merge.pull_version,
            "base_oid": merge.base_oid,
            "head_oid": merge.head_oid,
            "message": merge.message,
            "created_at": merge.created_at,
            "bypass_rulesets": merge.bypass_rulesets,
            "tested_candidate_oid": merge.tested_candidate.then_some(&merge.commit_oid),
        })),
    }))
}

struct MergeRequirements {
    queue_required: bool,
    protected: bool,
    rulesets: Vec<String>,
    bypass_options: Vec<crate::repository::settings::BypassOption>,
    required_approvals: usize,
    approvals: usize,
    changes_requested: usize,
    code_owners: Option<codeowners::Report>,
    unresolved_conversations: Option<u64>,
    head_up_to_date: Option<bool>,
    checks_satisfied: bool,
    checks: Vec<RequiredCheck>,
    candidate_checks: Option<Vec<RequiredCheck>>,
    satisfied: bool,
}

impl MergeRequirements {
    fn require_queue(&self, target: MergeCheckTarget<'_>) -> Result<()> {
        if self.queue_required && !matches!(target, MergeCheckTarget::Queue(_)) {
            return Err(Error::MergeQueueRequired);
        }
        Ok(())
    }

    fn enforce(&self) -> Result<()> {
        if self.unresolved_conversations.is_some_and(|count| count > 0) {
            return Err(Error::MergeUnresolvedConversations);
        }
        if self.head_up_to_date == Some(false) {
            return Err(Error::MergeHeadBehind);
        }
        if !self.satisfied {
            return Err(Error::MergeBlocked);
        }
        if self.candidate_checks.as_ref().is_some_and(|checks| {
            checks.is_empty()
                || checks
                    .iter()
                    .any(|check| check.state != Some(StatusState::Success))
        }) {
            return Err(Error::MergeCandidateChecks);
        }
        Ok(())
    }
}

struct RequiredCheck {
    context: String,
    state: Option<StatusState>,
    description: Option<String>,
    target_url: Option<String>,
    author: Option<String>,
    updated_at: Option<u64>,
    run_id: Option<u64>,
    provider: &'static str,
    integration: Option<Uuid>,
}

impl RequiredCheck {
    fn view(&self, oid: &str) -> Value {
        json!({
            "context": self.context,
            "state": self.state,
            "description": self.description,
            "target_url": self.target_url,
            "author": self.author,
            "updated_at": self.updated_at,
            "run_id": self.run_id,
            "provider": self.provider,
            "integration_id": self.integration,
            "head_sha": oid,
        })
    }

    fn run(run: &CheckRun) -> Self {
        let integration = run.integration_id();
        Self {
            context: run.name.clone(),
            state: Some(run.requirement_state()),
            description: Some(run.output_title.clone()),
            target_url: run.details_url.clone(),
            author: Some(run.author.name.clone()),
            updated_at: Some(run.updated_at),
            run_id: Some(run.number),
            provider: if integration.is_some() {
                "github_actions"
            } else {
                "crab_check_run"
            },
            integration,
        }
    }
}

#[derive(Clone, Copy)]
enum MergeCheckTarget<'a> {
    Head(&'a [crate::cells::repository::rulesets::RulesetId]),
    Tested(&'a str),
    Queue(Option<&'a str>),
}

impl<'a> MergeCheckTarget<'a> {
    fn publication(
        automatic: bool,
        tested: Option<&'a str>,
        bypass: &'a [crate::cells::repository::rulesets::RulesetId],
    ) -> Self {
        match tested {
            Some(oid) if automatic => Self::Queue(Some(oid)),
            Some(oid) => Self::Tested(oid),
            None => Self::Head(bypass),
        }
    }
}

impl Default for MergeCheckTarget<'_> {
    fn default() -> Self {
        Self::Head(&[])
    }
}

async fn requirements(
    server: &Server,
    repo: &Repository,
    pull: &PullRequest,
    base_oid: Option<&str>,
    head_oid: &str,
    grant: &app::Membership,
    reviewers: &MergeReviewers,
    target: MergeCheckTarget<'_>,
) -> Result<MergeRequirements> {
    let policy = crate::repository::settings::load_ref_policy(server, repo).await?;
    let default_ref = policy.default_ref(repo).await?;
    let organization_teams = policy.organization_teams(server, grant).await?;
    let actor = grant
        .bypass_context(repo.id)
        .organization_teams(organization_teams.as_deref());
    let (bypass, tested_oid) = match target {
        MergeCheckTarget::Head(bypass) => (bypass, None),
        MergeCheckTarget::Tested(oid) => (&[][..], Some(oid)),
        MergeCheckTarget::Queue(oid) => (&[][..], oid),
    };
    let policy = policy.merge_protection(&pull.base_ref, &default_ref, actor, bypass)?;
    let protection = policy.protection.as_ref();
    let (checks, candidate_checks, unresolved, code_owners, head_up_to_date) = match protection {
        Some(rule) => tokio::try_join!(
            load_required_checks(server, repo, head_oid, rule, false),
            async {
                if let Some(oid) = tested_oid {
                    load_required_checks(
                        server,
                        repo,
                        oid,
                        rule,
                        matches!(target, MergeCheckTarget::Queue(_)),
                    )
                    .await
                    .map(Some)
                } else {
                    Ok(None)
                }
            },
            async {
                if rule.require_conversation_resolution {
                    review_threads::storage::unresolved_conversations(server, repo, pull.number)
                        .await
                        .map(Some)
                } else {
                    Ok(None)
                }
            },
            async {
                if !rule.required_code_owner_reviews {
                    return Ok(None);
                }
                let Some(base_oid) = base_oid else {
                    return Ok(Some(codeowners::Report::unavailable()));
                };
                let report = async {
                    let git = git::PullGit::open(server, repo, pull.head_repository).await?;
                    codeowners::evaluate(
                        server,
                        repo,
                        &git,
                        grant,
                        reviewers,
                        pull,
                        (base_oid, head_oid),
                    )
                    .await
                }
                .await;
                Ok(Some(report.unwrap_or_else(|error| {
                    tracing::debug!(repository = %repo.id, pull = pull.number, %error, "failed to evaluate code owners");
                    codeowners::Report::unavailable()
                })))
            },
            async {
                if !rule.require_up_to_date
                    || pull.state != PullState::Open
                    || rule.require_merge_queue
                    || matches!(target, MergeCheckTarget::Queue(_))
                {
                    return Ok(None);
                }
                let Some(base_oid) = base_oid else {
                    return Ok(Some(false));
                };
                git::head_up_to_date(server, repo, pull, base_oid, head_oid)
                    .await
                    .map(Some)
            },
        )?,
        _ => (vec![], tested_oid.map(|_| vec![]), None, None, None),
    };
    let mut requirements = merge_requirements(pull, protection, head_oid, reviewers, checks);
    requirements.candidate_checks = candidate_checks;
    requirements.satisfied &= code_owners
        .as_ref()
        .is_none_or(codeowners::Report::satisfied);
    requirements.code_owners = code_owners;
    requirements.rulesets = policy.rulesets;
    requirements.bypass_options = policy.bypass_options;
    requirements.unresolved_conversations = unresolved;
    requirements.satisfied &= unresolved.is_none_or(|count| count == 0);
    requirements.head_up_to_date = head_up_to_date;
    requirements.satisfied &= head_up_to_date != Some(false);
    Ok(requirements)
}

pub(crate) async fn authorize_merge_publication(
    server: &Server,
    repo: &Repository,
    grant: &app::Membership,
    publication: &crate::git::receive::PullPublication,
    plan: Option<&str>,
    update: &crab_git::receive_plan::RefUpdate,
) -> Result<()> {
    let head_oid = publication.head_oid.to_string();
    let pull = storage::pull(server, repo, publication.number)
        .await?
        .ok_or(Error::NotFound)?;
    let pending = pull
        .merge_pending
        .as_ref()
        .filter(|pending| {
            // The strict check must use the base bound to the leased ref edit.
            pending.head_oid == head_oid
                && pending.tested_candidate == publication.tested_candidate
                && pull.head_ref == publication.head_ref
                && pull.head_repository == publication.head_repository.as_ref().map(|fork| fork.id)
                && pending.publication_plan.as_deref() == plan
                && update.name == pull.base_ref
                && update
                    .new
                    .is_some_and(|oid| oid.to_string() == pending.commit_oid)
                && update
                    .old
                    .is_some_and(|oid| oid.to_string() == pending.base_oid)
        })
        .ok_or(Error::MergeConflict)?;
    if !pending.bypass_rulesets.is_empty() {
        merge::validate_bypass_actor(pending, &app::actor(&grant.principal)?)?;
    }
    if grant.principal.is_auto_merge() {
        auto_merge::authorize(server, repo, &grant.principal, pending, pull.number).await?;
    } else {
        auto_merge::require_direct_access(server, repo, pull.number).await?;
    }
    let target = MergeCheckTarget::publication(
        grant.principal.is_auto_merge(),
        pending
            .tested_candidate
            .then_some(pending.commit_oid.as_str()),
        &pending.bypass_rulesets,
    );
    let report = requirements(
        server,
        repo,
        &pull,
        Some(&pending.base_oid),
        &head_oid,
        grant,
        &grant.merge_reviewers(server).await?,
        target,
    )
    .await?;
    report.require_queue(target)?;
    if !grant.principal.is_auto_merge() {
        merge_methods::require(server, repo, pending.method).await?;
    }
    report.enforce()?;

    // Git default-branch writes take this same publication policy lease.
    // Save the closure before the marker so finishing a proven receipt does
    // not reinterpret it using a later HEAD or a later edit to the links.
    let on_default = repo.default_ref().await? == update.name;
    storage::capture_issue_closure(server, repo, pull.number, pending, on_default).await
}

fn merge_requirements(
    pull: &PullRequest,
    rule: Option<&BranchProtection>,
    head_oid: &str,
    reviewers: &MergeReviewers,
    checks: Vec<RequiredCheck>,
) -> MergeRequirements {
    let Some(rule) = rule else {
        return MergeRequirements {
            queue_required: false,
            protected: false,
            rulesets: Vec::new(),
            bypass_options: Vec::new(),
            required_approvals: 0,
            approvals: 0,
            changes_requested: 0,
            code_owners: None,
            unresolved_conversations: None,
            head_up_to_date: None,
            checks_satisfied: true,
            checks,
            candidate_checks: None,
            satisfied: true,
        };
    };
    let mut approvals = 0;
    let mut changes_requested = 0;
    for decision in &pull.review_decisions {
        if (rule.required_approvals > 0 || rule.required_code_owner_reviews)
            && !reviewers.contains(&decision.author.issuer, &decision.author.subject)
        {
            continue;
        }
        // New commits require fresh approval; they cannot resolve a reviewer's
        // requested changes without a new decision or authorized dismissal.
        match decision.state {
            storage::ReviewState::Approved if decision.commit_oid == head_oid => approvals += 1,
            storage::ReviewState::ChangesRequested => changes_requested += 1,
            storage::ReviewState::Approved | storage::ReviewState::Commented => {}
        }
    }
    let required_approvals = usize::from(rule.required_approvals);
    let checks_satisfied = checks
        .iter()
        .all(|check| check.state == Some(StatusState::Success));
    let reviews_satisfied = (required_approvals == 0 && !rule.required_code_owner_reviews)
        || (changes_requested == 0 && approvals >= required_approvals);
    MergeRequirements {
        queue_required: rule.require_merge_queue,
        protected: true,
        rulesets: Vec::new(),
        bypass_options: Vec::new(),
        required_approvals,
        approvals,
        changes_requested,
        code_owners: None,
        unresolved_conversations: None,
        head_up_to_date: None,
        checks_satisfied,
        checks,
        candidate_checks: None,
        satisfied: reviews_satisfied && checks_satisfied,
    }
}

async fn load_required_checks(
    server: &Server,
    repo: &Repository,
    oid: &str,
    rule: &BranchProtection,
    queued_candidate: bool,
) -> Result<Vec<RequiredCheck>> {
    let ((statuses, check_runs), integrations) = tokio::try_join!(
        async {
            if rule.required_checks.is_empty() {
                Ok((vec![], vec![]))
            } else {
                tokio::try_join!(
                    statuses::latest(server, repo, oid),
                    checks::latest(server, repo, oid)
                )
            }
        },
        super::integrations::required(
            server,
            repo,
            oid,
            &rule.required_integrations,
            queued_candidate
        ),
    )?;
    let mut checks = rule
        .required_checks
        .iter()
        .map(|context| {
            let status = statuses
                .iter()
                .find(|status| statuses::same_context(&status.context, context));
            let run = check_runs.iter().find(|run| {
                run.integration_id().is_none() && statuses::same_context(&run.name, context)
            });
            if let Some(run) = run.filter(|run| {
                status.is_none_or(|status| {
                    // Both report kinds must pass. Show the report that blocks
                    // the merge first, and use recency only within that state.
                    let blocking = |state| match state {
                        StatusState::Error | StatusState::Failure => 2,
                        StatusState::Pending => 1,
                        StatusState::Success => 0,
                    };
                    (blocking(run.requirement_state()), run.updated_at)
                        >= (blocking(status.state), status.created_at)
                })
            }) {
                return RequiredCheck {
                    context: context.clone(),
                    ..RequiredCheck::run(run)
                };
            }
            RequiredCheck {
                context: context.clone(),
                state: status.map(|status| status.state),
                description: status.and_then(|status| status.description.clone()),
                target_url: status.and_then(|status| status.target_url.clone()),
                author: status.map(|status| status.author.name.clone()),
                updated_at: status.map(|status| status.created_at),
                run_id: None,
                provider: "crab_status",
                integration: None,
            }
        })
        .collect::<Vec<_>>();
    for id in &rule.required_integrations {
        let connection = integrations.iter().find(|connection| connection.id == *id);
        if let Some(run) = connection.and_then(|connection| connection.run.clone()) {
            checks.push(RequiredCheck::run(&checks::check_run(run)?));
            continue;
        }
        checks.push(RequiredCheck {
            context: connection
                .map_or_else(|| "GitHub Actions workflow".into(), |row| row.name.clone()),
            state: None,
            description: Some(
                if connection.is_some() {
                    "Waiting for GitHub Actions on this commit."
                } else {
                    "GitHub Actions connection is unavailable. Review the branch protection rule."
                }
                .into(),
            ),
            target_url: None,
            author: None,
            updated_at: None,
            run_id: None,
            provider: "github_actions",
            integration: Some(*id),
        });
    }
    Ok(checks)
}

pub(super) fn pull_list_view(
    authors: &super::profiles::attribution::Authors,
    pull: &PullSummary,
    labels: &[crate::cells::repository::search::index::LabelSummary],
    assignees: &Selection,
    rollups: &std::collections::BTreeMap<u64, crate::cells::repository::rollups::Rollup>,
) -> Result<Value> {
    let rollup = rollups
        .get(&pull.number)
        .ok_or(Error::CellContract("Pull rollup is missing"))?;
    Ok(json!({
        "kind": "pull",
        "rollup": rollup,
        "draft": pull.draft,
        "version": pull.version,
        "comment_count": pull.comment_count,
        "reaction_count": pull.reaction_count,
        "number": pull.number,
        "title": pull.title,
        "state": pull.state,
        "author": authors.view(&pull.author),
        "base_ref": pull.base_ref,
        "head_ref": pull.head_ref,
        "created_at": pull.created_at_ms,
        "closed_at":pull.closed_at_ms,
        "merged_at":pull.merged_at_ms,
        "updated_at": pull.updated_at_ms,
        "labels": pull.label_ids.iter().filter_map(|id|labels.iter().find(|label|label.id==*id)).collect::<Vec<_>>(),
        "assignees": assignees.view(&pull.assignees),
    }))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<super::discussion_search::ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let routed = app::repository_cell(&server, repo)?;
    let mut input = params.input("is:open")?;
    let review_teams =
        super::discussion_search::resolve_people(&server, &principal, repo, &mut input).await?;
    let review_source = input
        .review_context
        .as_ref()
        .and_then(|value| value.source_token.clone());
    let page = app::query_output(
        routed
            .client
            .query::<ListPulls>(&routed.target, None, input)
            .await,
    )?;
    let rollups = rollups::load(&server, repo, review_source.as_deref(), page.items.iter()).await?;
    let authors = super::profiles::attribution::Authors::load(
        &server,
        &principal,
        page.items.iter().map(|pull| &pull.author),
    )
    .await?;
    let assignees = Selection::load(
        &server,
        &principal,
        repo,
        page.items.iter().flat_map(|pull| pull.assignees.iter()),
    )
    .await?;
    let pulls = &page.items;
    let items = pulls
        .iter()
        .map(|pull| pull_list_view(&authors, pull, &page.labels, &assignees, &rollups))
        .collect::<Result<Vec<_>>>()?;
    if let Some(bound) = review_teams {
        bound.verify(&server, &principal, repo).await?;
    }
    Ok(Json(
        json!({"repository_id":repo.id,"can_bulk_edit":assignees.principal.can_triage(repo.id),"items":items,"next":page.next,"counts":page.counts,"matching":page.matching,"navigation":page.navigation,"authored_by_me":page.authored_by_me,"assigned_to_me":page.assigned_to_me,"review_requested_from_me":page.review_requested_from_me}),
    ))
}

fn branch_tip(repository: &RemoteGitRepository, name: &str) -> Option<String> {
    repository
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.target.to_string())
}

fn current_branches(repository: &git::PullGit, pull: &PullRequest) -> Option<(String, String)> {
    repository.tips(&pull.base_ref, &pull.head_ref)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPull {
    #[serde(default)]
    metadata: crate::cells::repository::collaboration::creation::Selection,
    #[serde(default)]
    draft: bool,
    request_id: String,
    title: String,
    body: String,
    base_ref: String,
    head_ref: String,
    head_repository: Option<Uuid>,
    expected: Option<crate::cells::repository::collaboration::creation::Comparison>,
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<NewPull>, JsonRejection>,
) -> Result<axum::response::Response> {
    let repo = app::repository(&server, &principal, &key)?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let title = app::title(&input.title)?;
    app::body(&input.body, false)?;
    let request_id = app::submission(&input.request_id)?;
    if input.head_repository.is_none() && input.base_ref == input.head_ref {
        return Err(Error::Invalid("Base and head branches must differ"));
    }
    if let Some(fork) = input.head_repository {
        git::fork_repository(&server, repo, fork)?;
        // Private same-organization forks inherit the upstream grant. Keep
        // the browser credential bound to the upstream collaboration route.
        if !app::membership(&server, &principal, repo)
            .await?
            .principal
            .can_write(repo.id)
        {
            return Err(Error::Forbidden);
        }
        if input.expected.is_none() {
            return Err(Error::Invalid(
                "Inspect the fork comparison before opening a pull request",
            ));
        }
    }
    if [&input.base_ref, &input.head_ref]
        .into_iter()
        .any(|name| name.strip_prefix("refs/heads/").is_none_or(str::is_empty))
    {
        return Err(Error::Invalid("Pull requests require branch refs"));
    }
    if input.expected.as_ref().is_some_and(|expected| {
        [&expected.base_oid, &expected.head_oid]
            .into_iter()
            .any(|oid| {
                oid.len() != 40
                    || !oid
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
    }) {
        return Err(Error::Invalid(
            "The comparison requires exact SHA-1 commit IDs",
        ));
    }
    let metadata =
        super::discussion_creation::resolve(&server, &principal, repo, input.metadata).await?;
    let existing = storage::pull_submission(&server, repo, &request_id).await?;
    let replay = existing.is_some();
    let (repository, comparison, mut stale) = if let Some(previous) = existing {
        // Recovery keeps the original comparison even after branch movement or
        // deletion. Git reads enrich the response; they cannot rebind its intent.
        let repository = git::PullGit::open(&server, repo, input.head_repository)
            .await
            .ok();
        (repository, input.expected.unwrap_or(previous), false)
    } else {
        let repository = git::PullGit::open(&server, repo, input.head_repository).await?;
        let tips = repository.tips(&input.base_ref, &input.head_ref);
        let (comparison, stale) = if let Some(expected) = input.expected {
            let stale = tips.as_ref().is_none_or(|(base, head)| {
                *base != expected.base_oid || *head != expected.head_oid
            });
            (expected, stale)
        } else {
            use crate::cells::repository::collaboration::creation::Comparison;
            let (base_oid, head_oid) =
                tips.ok_or(Error::Invalid("Base or head branch is unavailable"))?;
            (Comparison { base_oid, head_oid }, false)
        };
        if comparison.base_oid == comparison.head_oid {
            return Err(Error::Invalid("Head branch has no commits to compare"));
        }
        (Some(repository), comparison, stale)
    };
    let mut code_owner_requests = Default::default();
    if !replay
        && !stale
        && let Some(git) = &repository
        && git.fork.is_some()
    {
        let ancestor = super::query::pull_comparison_base(
            &git.base,
            git.fork.as_ref(),
            &comparison.base_oid,
            &comparison.head_oid,
            &server.cancellation,
        )
        .await
        .map_err(crate::Error::from)?;
        if ancestor == comparison.head_oid {
            return Err(Error::Invalid("Head branch has no commits to compare"));
        }
    }
    if !replay
        && !stale
        && !input.draft
        && let Some(git) = &repository
    {
        code_owner_requests = codeowners::requests(
            &server,
            repo,
            &principal,
            git,
            (&comparison.base_oid, &comparison.head_oid),
        )
        .await?;
        if !code_owner_requests.is_empty() {
            let live = git::PullGit::open(&server, repo, input.head_repository).await?;
            stale = live
                .tips(&input.base_ref, &input.head_ref)
                .as_ref()
                .is_none_or(|(base, head)| {
                    *base != comparison.base_oid || *head != comparison.head_oid
                });
            app::membership(&server, &principal, repo).await?;
        }
    }
    if !replay
        && input.head_repository.is_some()
        && !app::membership(&server, &principal, repo)
            .await?
            .principal
            .can_write(repo.id)
    {
        return Err(Error::Forbidden);
    }
    let result = storage::create_pull(
        &server,
        repo,
        NewPullRequest {
            metadata,
            code_owner_requests,
            draft: input.draft,
            author: actor.clone(),
            request_id,
            title,
            body: input.body,
            base_ref: input.base_ref,
            base_oid: comparison.base_oid,
            head_ref: input.head_ref,
            head_repository: input.head_repository,
            head_oid: comparison.head_oid,
        },
        stale,
    )
    .await;
    let pull = match result {
        Err(Error::CreationRejected(reason)) => {
            return Ok(super::write::rejected(Error::CreationRejected(reason)));
        }
        other => other?,
    };
    let current = repository
        .as_ref()
        .and_then(|repository| repository.tips(&pull.base_ref, &pull.head_ref));
    Ok((
        StatusCode::CREATED,
        Json(
            pull_view(
                pull.number,
                &actor,
                &server,
                repo,
                &principal,
                current.as_ref(),
            )
            .await?,
        ),
    )
        .into_response())
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let actor = app::actor(&principal)?;
    let pull = storage::pull(&server, repo, app::number(id)?)
        .await?
        .ok_or(Error::NotFound)?;
    let repository = git::PullGit::open(&server, repo, pull.head_repository)
        .await
        .ok();
    let current = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull));
    Ok(Json(
        pull_view(
            pull.number,
            &actor,
            &server,
            repo,
            &principal,
            current.as_ref(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullEdit {
    version: u64,
    title: Option<String>,
    body: Option<String>,
    state: Option<PullState>,
    label_ids: Option<Vec<u64>>,
    assignee_ids: Option<Vec<uuid::Uuid>>,
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<PullEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let number = app::number(id)?;
    let pull = storage::pull(&server, repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    let label_change = input.label_ids.is_some();
    let needs_label_catalog = input
        .label_ids
        .as_ref()
        .is_some_and(|labels| !labels.is_empty())
        || (input.label_ids.is_none() && !pull.label_ids.is_empty());
    let assignee_change = input.assignee_ids.is_some();
    let can_triage = app::membership(&server, &principal, repo)
        .await?
        .principal
        .can_triage(repo.id);
    let author = same_author(&pull.author, &actor);
    if pull.merge_pending.is_some() {
        return Err(Error::MergePending);
    }
    if (input.title.is_some() || input.body.is_some()) && !author {
        return Err(Error::Forbidden);
    }
    if input.state.is_some() && !(author || can_triage) {
        return Err(Error::Forbidden);
    }
    if label_change && !can_triage {
        return Err(Error::LabelPermission);
    }
    if assignee_change && !can_triage {
        return Err(Error::AssigneePermission);
    }
    if input.state == Some(PullState::Merged) || (pull.merge.is_some() && input.state.is_some()) {
        return Err(Error::Invalid("Merged pull requests cannot change state"));
    }
    if input.version != pull.version {
        return Err(Error::Conflict);
    }
    if input.title.is_none()
        && input.body.is_none()
        && input.state.is_none()
        && input.label_ids.is_none()
        && input.assignee_ids.is_none()
    {
        return Err(Error::Invalid("No pull request changes supplied"));
    }
    let labels = if needs_label_catalog {
        labels::catalog(&server, repo).await?
    } else {
        vec![]
    };
    let title = input.title.map(|value| app::title(&value)).transpose()?;
    let body = input
        .body
        .map(|value| {
            app::body(&value, false)?;
            Ok::<_, Error>(value)
        })
        .transpose()?;
    let label_ids = input
        .label_ids
        .map(|value| labels::validate_selection(value, &labels))
        .transpose()?;
    // Assignment eligibility and caller role must share the final current roster.
    let (can_triage, assignees) = if let Some(ids) = input.assignee_ids {
        let roster = crate::api::members::roster::Roster::load(&server, &principal, repo).await?;
        if !roster.membership.principal.can_triage(repo.id) {
            return Err(Error::AssigneePermission);
        }
        (true, Some(Selection::validate(&roster, ids)?))
    } else {
        let permissions = app::membership(&server, &principal, repo).await?;
        (permissions.principal.can_triage(repo.id), None)
    };
    if label_change && !can_triage {
        return Err(Error::LabelPermission);
    }
    let pull = storage::update_pull(
        &server,
        repo,
        number,
        storage::PullEdit {
            actor: actor.clone(),
            can_triage,
            version: input.version,
            title,
            body,
            state: input.state,
            label_ids,
            assignees,
        },
    )
    .await?;
    let repository = git::PullGit::open(&server, repo, pull.head_repository)
        .await
        .ok();
    let current = repository
        .as_ref()
        .and_then(|repository| current_branches(repository, &pull));
    Ok(Json(
        pull_view(
            pull.number,
            &actor,
            &server,
            repo,
            &principal,
            current.as_ref(),
        )
        .await?,
    ))
}

#[cfg(test)]
mod tests;
