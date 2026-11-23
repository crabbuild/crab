use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};

use gix_hash::ObjectId;

use super::RemoteGitSnapshot;
use crate::{BudgetDimension, OperationContext, Result};

impl RemoteGitSnapshot {
    /// Find the best common ancestors of two snapshots from the same capture.
    ///
    /// Results are ordered by object ID. An empty result means the histories
    /// do not intersect. Incomplete walks return the operation's error.
    pub async fn merge_bases(
        &self,
        other: &Self,
        operation: &OperationContext,
    ) -> Result<Vec<ObjectId>> {
        self.merge_bases_across_captures(other, operation, operation)
            .await
    }

    /// Find common ancestors of separately captured and authorized snapshots.
    ///
    /// Each operation must belong to its corresponding snapshot. Each walk
    /// follows only commit parents reached from that snapshot's tip. Finish
    /// both operations even if a read fails.
    pub async fn merge_bases_across_captures(
        &self,
        other: &Self,
        operation: &OperationContext,
        other_operation: &OperationContext,
    ) -> Result<Vec<ObjectId>> {
        self.ensure_operation(operation)?;
        other.ensure_operation(other_operation)?;

        if self.commit_oid == other.commit_oid {
            operation.ensure_active()?;
            other_operation.ensure_active()?;
            return Ok(vec![self.commit_oid]);
        }

        const HEAD: u8 = 0b001;
        const BASE: u8 = 0b010;
        const BOTH: u8 = HEAD | BASE;
        const ANCESTOR_OF_BASE: u8 = 0b100;
        let mut pending = VecDeque::from([self.commit_oid, other.commit_oid]);
        let mut queued = HashSet::from([self.commit_oid, other.commit_oid]);
        let mut reached = HashMap::from([(self.commit_oid, HEAD), (other.commit_oid, BASE)]);
        let mut common = HashSet::new();
        let mut parent_edges = HashMap::<ObjectId, Vec<ObjectId>>::new();
        let mut ancestors = Vec::new();
        while let Some(oid) = pending.pop_front() {
            operation.ensure_active()?;
            other_operation.ensure_active()?;
            queued.remove(&oid);
            let mut flags = reached.get(&oid).copied().unwrap_or_default();
            if flags & ANCESTOR_OF_BASE != 0
                && !pending.iter().any(|oid| {
                    reached
                        .get(oid)
                        .is_some_and(|flags| flags & ANCESTOR_OF_BASE == 0)
                })
            {
                // Walking a shared tail cannot discover another best base.
                break;
            }
            if flags == BOTH {
                common.insert(oid);
                flags |= ANCESTOR_OF_BASE;
            }
            // A parent reached only from the upstream tip need not exist in
            // the fork; only common ancestors can use either object reader.
            let reader = if flags & HEAD != 0 {
                operation
            } else {
                other_operation
            };
            let parents = match parent_edges.entry(oid) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    reader.charge(BudgetDimension::HistoryCommits, 1).await?;
                    entry.insert(reader.read_commit(oid).await?.parents)
                }
            };
            reader
                .charge(BudgetDimension::Entries, parents.len() as u64)
                .await?;
            ancestors.extend(parents.iter().rev().copied());
            while let Some(parent) = ancestors.pop() {
                let seen = reached.entry(parent).or_default();
                if *seen | flags == *seen {
                    continue;
                }
                *seen |= flags;
                if flags & ANCESTOR_OF_BASE != 0
                    && let Some(parents) = parent_edges.get(&parent)
                {
                    // Mark verified parent edges immediately. A shortcut into
                    // shared history can otherwise stay ahead of the paint walk.
                    reader
                        .charge(BudgetDimension::Entries, parents.len() as u64)
                        .await?;
                    ancestors.extend(parents.iter().rev().copied());
                } else if queued.insert(parent) {
                    pending.push_back(parent);
                }
            }
        }

        common.retain(|oid| {
            reached
                .get(oid)
                .is_some_and(|flags| flags & ANCESTOR_OF_BASE == 0)
        });
        if common.len() > 1 {
            // A shorter path can discover an older common commit first. Remove
            // ancestors of any candidate before selecting an effective base.
            let mut pending = Vec::new();
            let mut candidates = common.iter().copied().collect::<Vec<_>>();
            candidates.sort_unstable();
            for oid in candidates {
                operation.charge(BudgetDimension::HistoryCommits, 1).await?;
                let commit = operation.read_commit(oid).await?;
                operation
                    .charge(BudgetDimension::Entries, commit.parents.len() as u64)
                    .await?;
                pending.extend(commit.parents);
            }
            let mut seen = HashSet::new();
            while let Some(oid) = pending.pop() {
                operation.ensure_active()?;
                if common.remove(&oid) && common.len() == 1 {
                    break;
                }
                if seen.insert(oid) {
                    operation.charge(BudgetDimension::HistoryCommits, 1).await?;
                    let commit = operation.read_commit(oid).await?;
                    operation
                        .charge(BudgetDimension::Entries, commit.parents.len() as u64)
                        .await?;
                    pending.extend(commit.parents);
                }
            }
        }
        let mut bases = common.into_iter().collect::<Vec<_>>();
        bases.sort_unstable();
        Ok(bases)
    }
}
