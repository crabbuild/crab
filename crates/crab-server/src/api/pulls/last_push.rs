use std::{collections::BTreeSet, sync::Arc};

use serde::Serialize;

use crate::cells::repository::{
    ReviewComparison,
    push_attribution::{self, Latest, LatestInput, RefVersion},
};

use super::{Error, MergeReviewers, PullRequest, PullState, Repository, Result, Server, app, git};

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Report {
    Approved,
    Pending,
    PusherUnavailable,
}

fn fork(server: &Server, repo: &Repository, pull: &PullRequest) -> Result<Option<Arc<Repository>>> {
    pull.head_repository
        .map(|id| git::fork_repository(server, repo, id))
        .transpose()
}

async fn head_version(
    source: &Repository,
    reference: &str,
    oid: &str,
) -> Result<Option<RefVersion>> {
    let names = BTreeSet::from([reference.to_owned()]);
    let view = source.ref_view_for_refs(&names).await?;
    if view.refs().get(reference).map(String::as_str) != Some(oid) {
        return Err(Error::Conflict);
    }
    Ok(RefVersion::visible(&view, reference))
}

async fn comparison_base(
    server: &Server,
    repo: &Repository,
    pull: &PullRequest,
    base: &str,
    head: &str,
) -> Result<String> {
    let git = git::PullGit::open(server, repo, pull.head_repository).await?;
    super::super::query::pull_comparison_base(
        &git.base,
        git.fork.as_ref(),
        base,
        head,
        &server.cancellation,
    )
    .await
    .map_err(|error| crate::Error::Remote(error).into())
}

pub(super) async fn capture(
    server: &Server,
    repo: &Repository,
    pull: &PullRequest,
    head_oid: &str,
    comparison: Option<&str>,
) -> Result<Option<ReviewComparison>> {
    let fork = fork(server, repo, pull)?;
    let Some(head) =
        head_version(fork.as_deref().unwrap_or(repo), &pull.head_ref, head_oid).await?
    else {
        return Ok(None);
    };
    let base_oid = match comparison {
        Some(oid) => oid.to_owned(),
        None => {
            let Some(base) = repo.ref_oid(&pull.base_ref).await? else {
                return Ok(None);
            };
            comparison_base(server, repo, pull, &base, head_oid).await?
        }
    };
    Ok(Some(ReviewComparison { head, base_oid }))
}

pub(super) async fn evaluate(
    server: &Server,
    repo: &Repository,
    pull: &PullRequest,
    base_oid: Option<&str>,
    head_oid: &str,
    reviewers: &MergeReviewers,
) -> Result<Option<Report>> {
    if pull.state != PullState::Open {
        return Ok(None);
    }
    let Some(base_oid) = base_oid else {
        return Ok(Some(Report::PusherUnavailable));
    };
    let fork = fork(server, repo, pull)?;
    let source = fork.as_deref().unwrap_or(repo);
    let Some(version) = head_version(source, &pull.head_ref, head_oid).await? else {
        return Ok(Some(Report::PusherUnavailable));
    };
    let cell = app::repository_cell(server, source)?;
    let pusher = app::query_output(
        cell.client
            .query::<Latest>(
                &cell.target,
                None,
                LatestInput {
                    reference: pull.head_ref.clone(),
                    visible: version.clone(),
                },
            )
            .await,
    )?;
    let Some(pusher) = pusher else {
        return Ok(Some(Report::PusherUnavailable));
    };
    let mut approvals = pull.review_decisions.iter().filter_map(|decision| {
        let comparison = decision.comparison.as_ref()?;
        let author = &decision.author;
        (decision.state == super::storage::ReviewState::Approved
            && decision.commit_oid == head_oid
            && comparison.head == version
            && reviewers.contains(&author.issuer, &author.subject)
            && push_attribution::actor(&author.issuer, &author.subject) != pusher)
            .then_some(comparison)
    });
    let Some(first) = approvals.next() else {
        return Ok(Some(Report::Pending));
    };
    let base = comparison_base(server, repo, pull, base_oid, head_oid).await?;
    Ok(Some(
        if first.base_oid == base || approvals.any(|approval| approval.base_oid == base) {
            Report::Approved
        } else {
            Report::Pending
        },
    ))
}
