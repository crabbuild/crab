use std::collections::BTreeMap;

use futures_util::{StreamExt, stream};

use super::*;

const INTERVAL: Duration = Duration::from_secs(3);
const REPOSITORIES_PER_PASS: usize = 8;
const WORKERS: usize = 4;

pub(crate) async fn run(server: Arc<Server>) -> crate::Result<()> {
    let mut offset = 0;
    let mut pages = BTreeMap::new();
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(INTERVAL) => {}
        }
        let mut repos = server.repositories.values();
        if repos.is_empty() {
            continue;
        }
        repos.sort_unstable_by_key(|repo| repo.id);
        pages.retain(|id, _| repos.binary_search_by_key(id, |repo| repo.id).is_ok());
        let start = offset % repos.len();
        let count = repos.len().min(REPOSITORIES_PER_PASS);
        let mut jobs = Vec::new();
        for index in 0..count {
            let repo = &repos[(start + index) % repos.len()];
            let result = pending(&server, repo, pages.get(&repo.id).copied()).await;
            match result {
                Ok(mut page) => {
                    let before = page
                        .last()
                        .filter(|_| page.len() == cell::PAGE)
                        .map(|item| item.pull);
                    if let Some(before) = before {
                        pages.insert(repo.id, before);
                    } else {
                        pages.remove(&repo.id);
                    }
                    page.sort_by_key(Job::sweep_order);
                    jobs.extend(page.into_iter().map(|job| (repo, job)));
                }
                Err(error) => {
                    tracing::debug!(repository_id = %repo.id, error = ?error, "auto-merge queue read deferred")
                }
            }
        }
        offset = (start + count) % repos.len();
        stream::iter(jobs).for_each_concurrent(WORKERS, |(repo, job)| {
            let server = &server;
            async move {
                if !server.cancellation.is_cancelled()
                    && let Err(error) = with_lock(server, repo, job.pull, |cancel| reconcile(server, repo, job, cancel)).await
                {
                    tracing::debug!(repository_id = %repo.id, error = ?error, "auto-merge attempt deferred");
                }
            }
        }).await;
    }
}

async fn pending(server: &Server, repo: &Repository, before: Option<u64>) -> Result<Vec<Job>> {
    let routed = app::repository_cell(server, repo)?;
    app::query_output(
        routed
            .client
            .query::<cell::Pending>(&routed.target, None, before)
            .await,
    )
}

async fn reconcile(
    server: &Arc<Server>,
    repo: &Repository,
    job: Job,
    cancel: CancellationToken,
) -> Result<()> {
    let number = job.pull;
    let Some(state) = current(server, repo, number)
        .await?
        .filter(|state| state.job.id == job.id && state.stopped.is_none())
    else {
        return Ok(());
    };
    let job = &state.job;
    let pull = storage::pull(server, repo, number)
        .await?
        .ok_or(Error::NotFound)?;
    let pending = match &pull.merge_pending {
        Some(pending) if matches_attempt(&state, pending) => Some(pending),
        Some(_) => return Ok(()),
        None => None,
    };
    // A durable Git receipt is sufficient to finalize a merge even if its
    // initiating grant disappeared before the Cell received completion.
    if let Some(pending) = pending
        && merge::receipt_proven(repo, &pull, pending).await?
    {
        storage::complete_merge(server, repo, number, pending).await?;
        return stop(server, repo, number, job.id, Stop::Merged).await;
    }
    if cancel.is_cancelled() {
        return Ok(());
    }
    let actor = Identity {
        issuer: job.actor.issuer.clone(),
        subject: job.actor.subject.clone(),
        name: job.actor.name.clone(),
    };
    let principal = durable_principal(server, repo, actor.clone()).await?;
    let (configured, queue_required) = tokio::try_join!(
        merge_methods::load_for_ref(server, repo, &pull.base_ref),
        settings::requires_merge_queue(server, repo, &pull.base_ref),
    )?;
    let stopped = if pull.state == PullState::Merged {
        Some(Stop::Merged)
    } else if principal.is_none() {
        Some(Stop::PermissionChanged)
    } else if queue_required && job.queue.is_none() {
        Some(Stop::QueueRequired)
    } else if job.queue.is_some() && !job.queue_method_matches(&configured) {
        Some(Stop::MethodDisabled)
    } else if !job.enabled(&configured, queue_required) {
        Some(Stop::Disabled)
    } else if job.queue.is_none() && !configured.enabled.contains(&job.method) {
        Some(Stop::MethodDisabled)
    } else if pull.state != PullState::Open
        || pull.draft
        || settings::load_lifecycle(server, repo).await?.archived
        || pull.base_ref != job.base_ref
        || pull.head_ref != job.head_ref
        || pull.head_repository != job.head_repository
        || pull.retargeted_base.as_ref().map(|base| base.revision) != job.comparison_revision
    {
        Some(Stop::PullChanged)
    } else {
        None
    };
    if let Some(reason) = stopped {
        if let Some(pending) = pending {
            match merge::reject_unpublished(
                server,
                repo,
                number,
                pending,
                &cancel,
                Error::AutoMergeDisabled,
            )
            .await
            {
                Err(Error::AutoMergeDisabled) => (),
                other => return other,
            }
        }
        return stop(server, repo, number, job.id, reason).await;
    }
    if job.queue.is_some() && state.ahead.is_none() {
        return Ok(());
    }
    let principal = principal.ok_or(Error::MergePermission)?;
    if let Some(pending) = pending {
        if job.queue.is_some() {
            return super::queue::finish(server, repo, &principal, job, pending, cancel).await;
        }
        merge::finish_locked(server, &principal, repo, number, pending, cancel).await?;
        return stop(server, repo, number, job.id, Stop::Merged).await;
    }
    let git = match PullGit::open(server, repo, pull.head_repository).await {
        Ok(git) => git,
        Err(Error::NotFound) => return stop(server, repo, number, job.id, Stop::HeadChanged).await,
        Err(error) => return Err(error),
    };
    let Some((base_oid, head_oid)) = git.tips(&pull.base_ref, &pull.head_ref) else {
        return stop(server, repo, number, job.id, Stop::HeadChanged).await;
    };
    if head_oid != job.head_oid || (job.queue.is_some() && base_oid == head_oid) {
        return stop(server, repo, number, job.id, Stop::HeadChanged).await;
    }
    if job.queue.is_some() {
        let Some(base) =
            super::super::candidate::prospective_base(server, repo, &git.base, &state, &base_oid)
                .await?
        else {
            return Ok(());
        };
        return super::queue::reconcile(
            server,
            repo,
            &principal,
            &pull,
            &state,
            &git,
            (&base, configured.merge_queue_timeout_minutes),
            cancel,
        )
        .await;
    }
    if job.method == MergeMethod::FastForward
        && !super::super::git::head_up_to_date(server, repo, &pull, &base_oid, &head_oid).await?
    {
        return stop(server, repo, number, job.id, Stop::Conflict).await;
    }
    let candidate = NewPullMerge {
        author: actor,
        request_id: job
            .publication_id(pull.version, &base_oid, None, 0)
            .to_string(),
        method: job.method,
        pull_version: pull.version,
        base_oid,
        head_oid,
        message: job.message.clone(),
        bypass_rulesets: vec![],
        tested_candidate_oid: None,
    };
    let record = match merge::admit(server, repo, &principal, &pull, &candidate).await {
        Ok(record) => record,
        Err(
            Error::MergeBlocked
            | Error::MergeUpdatesRestricted
            | Error::MergeUnresolvedConversations
            | Error::MergeHeadBehind
            | Error::DraftPull,
        ) => return Ok(()),
        Err(Error::MergeConflict | Error::UnsafeRebase) => {
            let current = storage::pull(server, repo, number).await?;
            let git = PullGit::open(server, repo, pull.head_repository).await?;
            if current.is_some_and(|current| current.version == pull.version)
                && git
                    .tips(&pull.base_ref, &pull.head_ref)
                    .is_some_and(|(base, head)| {
                        base == candidate.base_oid && head == candidate.head_oid
                    })
            {
                return stop(server, repo, number, job.id, Stop::Conflict).await;
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    merge::finish_locked(server, &principal, repo, number, &record, cancel).await?;
    stop(server, repo, number, job.id, Stop::Merged).await
}
