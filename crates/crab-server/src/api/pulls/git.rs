use std::sync::Arc;

use crab_remote_git::{RemoteGitRepository, RepositoryOptions};
use uuid::Uuid;

use crate::{
    app::{Error, Result},
    repository::Repository,
    server::Server,
};

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
