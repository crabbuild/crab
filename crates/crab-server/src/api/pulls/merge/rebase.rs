use std::collections::BTreeMap;

use crab_remote::objects;
use crab_remote_git::{Commit, PageRequest, RemoteGitRepository, RemoteGitSnapshot, Revision};
use gix_hash::ObjectId;
use gix_object::Kind;

use super::tree::{self, Error, Generated, Plan, Readers, Strategy};
use crate::git::objects::rebased_commit_bytes;

const MAX_REBASE_COMMITS: usize = 256;

pub(super) async fn build(
    repository: &RemoteGitRepository,
    readers: &Readers<'_>,
    base: &RemoteGitSnapshot,
    head: &RemoteGitSnapshot,
    ancestor: &RemoteGitSnapshot,
    strategy: Strategy<'_>,
) -> Result<Plan, Error> {
    let commits = topic_commits(repository, readers, base, head, ancestor.commit_oid()).await?;
    // The capped graph is small enough to pick parents first with a stable OID
    // tiebreaker. Indexed and raw history walks can deliver commits in different orders.
    let mut ahead: BTreeMap<_, _> = commits
        .into_iter()
        .map(|commit| (commit.oid, commit))
        .collect();
    let mut generated = Generated::bounded();
    let mut current = base.commit_oid();
    let mut current_tree = base.root_tree_oid();
    while !ahead.is_empty() {
        let oid = ahead
            .values()
            .find(|commit| {
                commit
                    .parents
                    .iter()
                    .all(|parent| !ahead.contains_key(parent))
            })
            .map(|commit| commit.oid)
            .ok_or(Error::Rebase)?;
        let commit = ahead.remove(&oid).ok_or(Error::Rebase)?;
        let ancestor = match commit.parents.as_slice() {
            [] => None,
            [parent] => Some(
                repository
                    .snapshot(&Revision::Commit(*parent), readers.head)
                    .await
                    .map_err(objects::Error::from)?
                    .root_tree_oid(),
            ),
            // Drop merge commits when flattening, then require an exact tree match
            // below so a merge-only resolution can never silently disappear.
            _ => continue,
        };
        if ancestor == Some(commit.tree) {
            continue;
        }
        let next_tree =
            tree::merge_tree(readers, ancestor, current_tree, commit.tree, &mut generated).await?;
        if next_tree == current_tree {
            continue;
        }
        let original = readers
            .head
            .read_object(oid)
            .await
            .map_err(objects::Error::from)?;
        let bytes = rebased_commit_bytes(
            next_tree,
            current,
            &original.data,
            strategy.actor,
            strategy.seconds,
        )?;
        let bytes =
            if objects::object_id(Kind::Commit, &bytes).map_err(objects::Error::from)? == oid {
                // A same-second rebase by the original committer still rewrites the commit.
                rebased_commit_bytes(
                    next_tree,
                    current,
                    &original.data,
                    strategy.actor,
                    strategy.seconds + 1,
                )?
            } else {
                bytes
            };
        current = generated.add(Kind::Commit, bytes)?;
        current_tree = next_tree;
    }
    let expected = tree::merged_tree(
        readers,
        ancestor.root_tree_oid(),
        base,
        head,
        &mut Generated::bounded(),
    )
    .await?;
    // A branch update can move the head to the existing base if every change
    // is already present there. A PR merge requires an update to the base ref.
    if current_tree != expected || (current == base.commit_oid() && !strategy.allow_empty_rebase) {
        return Err(Error::Rebase);
    }
    Ok(generated.plan(current))
}

async fn topic_commits(
    repository: &RemoteGitRepository,
    readers: &Readers<'_>,
    base: &RemoteGitSnapshot,
    head: &RemoteGitSnapshot,
    ancestor: ObjectId,
) -> Result<Vec<Commit>, Error> {
    // Most topic branches are linear after their best shared ancestor. Stop
    // there so a short rebase on a large repository spends its budget on the PR.
    let mut commits = Vec::new();
    let mut current = head.commit_oid();
    while current != ancestor {
        if commits.len() == MAX_REBASE_COMMITS {
            return Err(Error::Rebase);
        }
        let snapshot = repository
            .snapshot(&Revision::Commit(current), readers.head)
            .await
            .map_err(objects::Error::from)?;
        let commit = snapshot
            .commit(readers.head)
            .await
            .map_err(objects::Error::from)?;
        let [parent] = commit.parents.as_slice() else {
            // A parent can enter shared history below the merge base. The
            // all-parent walker excludes the complete base ancestry in this case.
            let page = PageRequest::new(MAX_REBASE_COMMITS, None).map_err(objects::Error::from)?;
            let ahead = head
                .ahead_history_across_captures(base, &page, readers.head, readers.base)
                .await
                .map_err(objects::Error::from)?;
            return if ahead.next.is_none() {
                Ok(ahead.items)
            } else {
                Err(Error::Rebase)
            };
        };
        current = *parent;
        commits.push(commit);
    }
    Ok(commits)
}
