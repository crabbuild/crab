use std::collections::{HashMap, HashSet};

use gix_hash::ObjectId;

use super::queue_all_parents;
use crate::commit_graph::CommitGraphIndex;
use crate::objects::SharedCommit;
use crate::{BudgetDimension, OperationContext, Result};

const PROBE_COMMITS: usize = 16;
type PrefetchedCommits = HashMap<ObjectId, Result<SharedCommit>>;

pub(super) struct BaseWalk {
    remaining: Vec<ObjectId>,
    visited: HashSet<ObjectId>,
}

impl BaseWalk {
    pub(super) fn new(root: ObjectId) -> Self {
        Self {
            remaining: vec![root],
            visited: HashSet::new(),
        }
    }

    async fn next(
        &mut self,
        operation: &OperationContext,
        graph: Option<&CommitGraphIndex>,
    ) -> Result<Option<SharedCommit>> {
        while let Some(oid) = self.remaining.pop() {
            operation.ensure_active()?;
            if self.visited.insert(oid) {
                let commit = read_commit(oid, operation).await?;
                queue_all_parents(&mut self.remaining, &commit.value, graph, operation).await?;
                return Ok(Some(commit));
            }
        }
        Ok(None)
    }

    pub(super) async fn finish(
        mut self,
        operation: &OperationContext,
        graph: Option<&CommitGraphIndex>,
    ) -> Result<HashSet<ObjectId>> {
        while self.next(operation, graph).await?.is_some() {}
        Ok(self.visited)
    }
}

pub(super) async fn read_commit(
    oid: ObjectId,
    operation: &OperationContext,
) -> Result<SharedCommit> {
    operation.charge(BudgetDimension::HistoryCommits, 1).await?;
    operation.read_commit_shared(oid).await
}

pub(super) async fn raw_base_ancestry(
    head: ObjectId,
    base: ObjectId,
    head_operation: &OperationContext,
    base_operation: &OperationContext,
    graph: Option<&CommitGraphIndex>,
) -> Result<(HashSet<ObjectId>, PrefetchedCommits)> {
    let mut base_walk = BaseWalk::new(base);
    let mut prefetched = HashMap::new();
    // Preserve the ordinary walk when the budget is small or a legacy graph
    // already supplies base traversal. All speculative reads remain charged.
    if graph.is_none() && head_operation.remaining_history_reads().await >= 4 * PROBE_COMMITS as u64
    {
        let mut children = HashMap::<ObjectId, Vec<ObjectId>>::new();
        for _ in 0..PROBE_COMMITS {
            let Some(commit) = base_walk.next(base_operation, None).await? else {
                return Ok((base_walk.visited, prefetched));
            };
            if commit.value.parents.len() > PROBE_COMMITS {
                return Ok((base_walk.finish(base_operation, graph).await?, prefetched));
            }
            for parent in &commit.value.parents {
                children.entry(*parent).or_default().push(commit.value.oid);
            }
        }
        let frontier = base_walk
            .remaining
            .iter()
            .copied()
            .filter(|oid| !base_walk.visited.contains(oid))
            .collect::<HashSet<_>>();
        if frontier.is_empty() {
            return Ok((base_walk.visited, prefetched));
        }
        if frontier.len() <= PROBE_COMMITS {
            let shared = base_walk
                .visited
                .union(&frontier)
                .copied()
                .collect::<HashSet<_>>();
            let mut cut = shared.clone();
            // Every unseen path from the base must start at a frontier node.
            // Only shared commits descending from all of those nodes certify
            // that a newer topic commit cannot also be in the unseen base.
            for oid in frontier {
                let mut descendants = HashSet::new();
                let mut pending = vec![oid];
                while let Some(oid) = pending.pop() {
                    if descendants.insert(oid)
                        && let Some(children) = children.get(&oid)
                    {
                        pending.extend(children);
                    }
                }
                cut.retain(|oid| descendants.contains(oid));
            }
            if !cut.is_empty()
                && inspect_topic(head, head_operation, &shared, &cut, &mut prefetched).await?
            {
                return Ok((shared, prefetched));
            }
        }
    }
    Ok((base_walk.finish(base_operation, graph).await?, prefetched))
}

async fn inspect_topic(
    head: ObjectId,
    operation: &OperationContext,
    shared: &HashSet<ObjectId>,
    cut: &HashSet<ObjectId>,
    prefetched: &mut PrefetchedCommits,
) -> Result<bool> {
    let mut pending = vec![head];
    while let Some(oid) = pending.pop() {
        operation.ensure_active()?;
        if cut.contains(&oid) || prefetched.contains_key(&oid) {
            continue;
        }
        if shared.contains(&oid) || prefetched.len() == PROBE_COMMITS {
            return Ok(false);
        }
        let commit = read_commit(oid, operation).await;
        let proved_parents = commit
            .as_ref()
            .ok()
            .filter(|commit| (1..=PROBE_COMMITS).contains(&commit.value.parents.len()));
        if let Some(commit) = proved_parents {
            pending.extend(commit.value.parents.iter().rev().copied());
        }
        let can_continue = proved_parents.is_some();
        // A later page can encounter a bad object after this page's boundary.
        // Keep the error until that commit is actually reached by the page.
        prefetched.insert(oid, commit);
        if !can_continue {
            return Ok(false);
        }
    }
    Ok(true)
}
