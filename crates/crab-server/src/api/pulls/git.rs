use std::sync::Arc;

use crab_remote_git::{RemoteGitRepository, RepositoryOptions};
use uuid::Uuid;

use crate::{
    app::{Error, Result},
    repository::Repository,
    server::Server,
};

pub(super) async fn committed_branch_edit(
    repository: &Repository,
    plan_id: &str,
    branch: Option<&str>,
    previous: Option<&str>,
) -> Result<Option<String>> {
    let receipt = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
        &repository.store,
        &repository.layout,
        plan_id,
    )
    .await
    .map_err(|error| super::merge::map_receive(error.into()))?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    let [edit] = receipt.transaction().edits() else {
        return Err(Error::CellContract(
            "Branch publication receipt has unexpected ref edits",
        ));
    };
    // A replay can arrive after the pull's head branch has been switched or
    // deleted. The request-bound receipt remains proof of the original edit.
    let expected = edit.ref_name().starts_with("refs/heads/")
        && branch.is_none_or(|branch| branch == edit.ref_name())
        && edit.expected_old() == previous;
    edit.new_oid()
        .filter(|_| expected)
        .map(|commit| Some(commit.to_owned()))
        .ok_or(Error::CellContract(
            "Branch publication receipt has an unexpected ref",
        ))
}

pub(super) async fn head_contains_base(
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    head_ref: &str,
    base_oid: &str,
) -> Result<bool> {
    Ok(view
        .git_visibility_index()
        .await
        .map_err(crate::Error::from)?
        .contains_hex_in_ref(head_ref, base_oid))
}

pub(super) async fn head_up_to_date(
    server: &Server,
    upstream: &Repository,
    pull: &super::storage::PullRequest,
    base_oid: &str,
    head_oid: &str,
) -> Result<bool> {
    let fork = pull
        .head_repository
        .map(|id| fork_repository(server, upstream, id))
        .transpose()?;
    let repo = fork.as_deref().unwrap_or(upstream);
    let view = repo.open_git_view().await?;
    // The index answers reachability from a named ref. Check its tip in the
    // same capture so an advanced head cannot make stale checks look valid.
    if view.refs().get(&pull.head_ref).map(String::as_str) != Some(head_oid) {
        return Ok(false);
    }
    head_contains_base(&view, &pull.head_ref, base_oid).await
}

pub(super) fn fork_repository(
    server: &Server,
    upstream: &Repository,
    fork: Uuid,
) -> Result<Arc<Repository>> {
    server
        .repositories
        .by_id(fork)
        .filter(|entry| {
            entry
                .fork_source(server)
                .is_some_and(|source| source.id == upstream.id)
        })
        .ok_or(Error::NotFound)
}

pub(in crate::api) async fn review_head(
    server: &Server,
    upstream: &Repository,
    fork: Option<Uuid>,
    head_ref: &str,
    merged_head: Option<String>,
) -> Result<Option<String>> {
    // A merged review follows the recorded source even if its ref disappears.
    if merged_head.is_some() {
        return Ok(merged_head);
    }
    match fork {
        None => Ok(upstream.ref_oid(head_ref).await?),
        Some(id) => match fork_repository(server, upstream, id) {
            Ok(source) => Ok(source.ref_oid(head_ref).await?),
            Err(Error::NotFound) => Ok(None),
            Err(error) => Err(error),
        },
    }
}

pub(super) struct PullGit {
    pub base: RemoteGitRepository,
    pub fork: Option<RemoteGitRepository>,
}

impl PullGit {
    pub(super) async fn open(
        server: &Server,
        upstream: &Repository,
        fork: Option<Uuid>,
    ) -> Result<Self> {
        let fork = fork
            .map(|id| fork_repository(server, upstream, id))
            .transpose()?;
        let base =
            upstream.open_current(server, RepositoryOptions::default(), &server.cancellation);
        let (base, fork) = if let Some(fork) = fork {
            let (base, fork) = tokio::try_join!(
                base,
                fork.open_current(server, RepositoryOptions::default(), &server.cancellation),
            )?;
            (base, Some(fork))
        } else {
            (base.await?, None)
        };
        Ok(Self { base, fork })
    }

    pub(super) fn head(&self) -> &RemoteGitRepository {
        self.fork.as_ref().unwrap_or(&self.base)
    }

    pub(super) fn tips(&self, base: &str, head: &str) -> Option<(String, String)> {
        Some((
            super::branch_tip(&self.base, base)?,
            super::branch_tip(self.head(), head)?,
        ))
    }
}
