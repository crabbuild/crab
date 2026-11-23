use super::*;
use crate::api::{pulls::candidate, statuses::StatusState};

pub(super) async fn reconcile(
    server: &Arc<Server>,
    repo: &Repository,
    principal: &Principal,
    pull: &PullRequest,
    state: &cell::State,
    git: &PullGit,
    (prospective, timeout_minutes): (&candidate::QueueBase, u16),
    cancel: CancellationToken,
) -> Result<()> {
    let job = &state.job;
    let base = prospective.oid.as_str();
    let head = job.head_oid.as_str();
    let queue = job
        .queue
        .as_ref()
        .ok_or(Error::CellContract("Expected a merge queue entry"))?;
    let grant = app::membership(server, principal, repo).await?;
    let commit = candidate::queue_commit(server, repo, git, pull, job, prospective).await?;
    let report =
        queue_requirements(server, repo, pull, &grant, (base, head), commit.as_deref()).await?;
    if report.checks.is_empty() || !report.satisfied {
        return if state.first {
            stop_front(server, repo, state, Stop::RequirementsChanged).await
        } else {
            Ok(())
        };
    }
    let Some(commit) = commit else {
        let previous = super::super::branch_tip(
            &git.base,
            &crate::git::receive::merge_candidate_ref(pull.number),
        );
        let attempt = match &queue.testing {
            Some(attempt)
                if attempt.version == pull.version
                    && attempt.base_oid == base
                    && attempt.previous_oid == previous
                    && attempt.order_revision == state.order_revision =>
            {
                attempt.clone()
            }
            _ => {
                let attempt = cell::QueueTest {
                    request_id: Uuid::now_v7(),
                    version: pull.version,
                    base_oid: base.to_owned(),
                    previous_oid: previous,
                    order_revision: state.order_revision,
                };
                change(
                    server,
                    repo,
                    Change::Test {
                        pull: pull.number,
                        id: job.id,
                        attempt: attempt.clone(),
                        configured_queue_required: settings::configured_queue_required(
                            repo,
                            &pull.base_ref,
                        ),
                        ruleset_queue_required: settings::queue_policy(
                            server,
                            repo,
                            &pull.base_ref,
                        )
                        .await?
                        .ruleset,
                    },
                )
                .await?;
                attempt
            }
        };
        let input = candidate::Input {
            request_id: attempt.request_id,
            version: attempt.version,
            base_oid: attempt.base_oid,
            head_oid: head.to_owned(),
            previous_oid: attempt.previous_oid,
        };
        return match candidate::stage(
            server,
            principal,
            repo,
            pull.number,
            &input,
            Some((job, prospective)),
            &cancel,
        )
        .await
        {
            Ok(_) | Err(Error::BranchUpdateChanged) => Ok(()),
            Err(Error::MergeConflict | Error::UnsafeRebase) if state.first => {
                stop_front(server, repo, state, Stop::Conflict).await
            }
            Err(error) if state.first && path_locked(&error) => {
                stop_front(server, repo, state, Stop::PathLocked).await
            }
            Err(error) => Err(error),
        };
    };
    let checks = report.candidate_checks.as_deref().unwrap_or_default();
    if report.enforce().is_err() {
        let stopped = if checks
            .iter()
            .any(|check| matches!(check.state, Some(StatusState::Error | StatusState::Failure)))
        {
            Some(Stop::ChecksFailed)
        } else if check_timed_out(queue, timeout_minutes)? {
            Some(Stop::ChecksTimedOut)
        } else {
            None
        };
        if let Some(reason) = stopped {
            return if state.first && repo.ref_oid(&pull.base_ref).await?.as_deref() == Some(base) {
                stop_front(server, repo, state, reason).await
            } else {
                // A failure on a speculative prefix does not eject this pull
                // until that prefix becomes the actual base branch.
                Ok(())
            };
        }
        crate::api::integrations::dispatch::start_required(
            server,
            repo,
            pull.number,
            &commit,
            checks
                .iter()
                .filter_map(|check| check.integration)
                .collect(),
            &cancel,
        )
        .await?;
        return Ok(());
    }

    if !state.first {
        return Ok(());
    }
    let input = NewPullMerge {
        author: app::actor(principal)?,
        request_id: job
            .publication_id(pull.version, base, Some(&commit), state.order_revision)
            .to_string(),
        pull_version: pull.version,
        method: job.method,
        base_oid: base.to_owned(),
        head_oid: head.to_owned(),
        message: job.message.clone(),
        bypass_rulesets: vec![],
        tested_candidate_oid: Some(commit),
    };
    let record = merge::admit(server, repo, principal, pull, &input).await?;
    finish(server, repo, principal, job, &record, cancel).await
}

async fn stop_front(
    server: &Server,
    repo: &Repository,
    state: &cell::State,
    reason: Stop,
) -> Result<()> {
    change(
        server,
        repo,
        Change::Disarm {
            pull: state.job.pull,
            id: state.job.id,
            reason,
            front_revision: Some(state.order_revision),
        },
    )
    .await
}

fn check_timed_out(queue: &cell::QueueEntry, timeout_minutes: u16) -> Result<bool> {
    let test = queue.testing.as_ref().ok_or(Error::CellContract(
        "Merge queue candidate has no recorded test",
    ))?;
    let now = crate::cells::unix_now_ms().map_err(Error::Cell)?;
    test.timed_out(now, timeout_minutes).map_err(Error::Cell)
}

pub(super) async fn finish(
    server: &Arc<Server>,
    repo: &Repository,
    principal: &Principal,
    job: &Job,
    record: &PullMerge,
    cancel: CancellationToken,
) -> Result<()> {
    match merge::finish_locked(server, principal, repo, job.pull, record, cancel.clone()).await {
        Ok(()) => stop(server, repo, job.pull, job.id, Stop::Merged).await,
        Err(error) if path_locked(&error) => {
            match merge::reject_unpublished(server, repo, job.pull, record, &cancel, error).await {
                Err(error) if path_locked(&error) => {
                    stop(server, repo, job.pull, job.id, Stop::PathLocked).await
                }
                other => other,
            }
        }
        Err(error) => Err(error),
    }
}

fn path_locked(error: &Error) -> bool {
    matches!(error, Error::Merge(error) if matches!(error.as_ref(), crate::git::receive::ReceiveError::Locked))
}
