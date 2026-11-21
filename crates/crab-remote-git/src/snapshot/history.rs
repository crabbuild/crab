use std::{collections::HashSet, mem};

use crate::traversal::HistoryPosition;
use crate::{
    BudgetDimension, Commit, CorruptionStage, CursorError, Error, HistoryTraversal,
    OperationContext, Page, PageCursor, PageRequest, Result,
};
use gix_hash::ObjectId;

use super::{
    RemoteGitSnapshot, commit_response_bytes, path_history_commit, path_history_commits,
    queue_all_parents,
};

impl RemoteGitSnapshot {
    /// Return one deterministic bounded page of commits reachable from this snapshot.
    ///
    /// New first-parent continuations resume at the next verified parent. Released
    /// offset cursors retain their bounded prefix walk. Authenticate imported
    /// cursors at the caller boundary before using them.
    pub async fn history(
        &self,
        traversal: HistoryTraversal,
        page: &PageRequest,
        operation: &OperationContext,
    ) -> Result<Page<Commit>> {
        self.ensure_operation(operation)?;
        let position = page
            .after()
            .map(|cursor| cursor.decode_history())
            .transpose()?;
        if position
            .as_ref()
            .is_some_and(|cursor| cursor.start != self.commit_oid || cursor.mode != traversal)
        {
            return Err(Error::InvalidCursor {
                reason: CursorError::ContextMismatch,
            });
        }
        match position.map(|cursor| cursor.position) {
            // Alpha.1 issued offset cursors. Keep their original traversal and
            // charging semantics; only newly issued first-parent pages seek.
            Some(HistoryPosition::Offset(skip)) => {
                self.offset_history(traversal, page, operation, skip).await
            }
            Some(HistoryPosition::FirstParent(next)) => {
                self.first_parent_history(next, page, operation).await
            }
            None if traversal == HistoryTraversal::FirstParent => {
                self.first_parent_history(self.commit_oid, page, operation)
                    .await
            }
            None => self.offset_history(traversal, page, operation, 0).await,
        }
    }

    async fn first_parent_history(
        &self,
        start: ObjectId,
        page: &PageRequest,
        operation: &OperationContext,
    ) -> Result<Page<Commit>> {
        const PREFETCH: usize = 16;

        let mut items = Vec::new();
        let mut commits = Vec::new();
        let mut next = Some(start);
        while items.len() < page.limit()
            && let Some(oid) = next
        {
            operation.ensure_active()?;
            commits.clear();
            if let Some(commit) = operation.cached_commit(oid).await {
                // Verified raw parents already determine the next commit. The
                // graph only accelerates cold batching; consulting it again
                // would add metadata work to every parsed-cache hit.
                operation.charge(BudgetDimension::HistoryCommits, 1).await?;
                operation.charge(BudgetDimension::LogicalObjects, 1).await?;
                commits.push(commit.value.clone());
            } else if let Some(graph) = &self.commit_graph {
                // Unknown commit messages can exhaust the response budget. Bound
                // speculation by the existing object/response limits first.
                let maximum_commit_bytes = operation
                    .object_limits()
                    .max_object_bytes
                    .saturating_add(mem::size_of::<Commit>() as u64);
                let response_bytes = operation.remaining_response_bytes().await;
                let byte_lanes = usize::try_from(response_bytes / maximum_commit_bytes)
                    .unwrap_or(usize::MAX)
                    .max(1);
                let maximum = PREFETCH.min(page.limit() - items.len()).min(byte_lanes);
                let oids = graph
                    .first_parent_chain(oid, maximum, operation)
                    .await?
                    .ok_or(Error::Corrupt {
                        stage: CorruptionStage::CommitGraph,
                    })?;
                if oids.first() != Some(&oid) {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::CommitGraph,
                    });
                }
                commits = path_history_commits(&oids, operation).await?;
                if !graph.parents_match(&commits, operation).await?
                    || !commits
                        .windows(2)
                        .all(|pair| pair[0].parents.first() == Some(&pair[1].oid))
                {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::CommitGraph,
                    });
                }
            } else {
                commits.push(path_history_commit(oid, operation).await?);
            }
            items
                .try_reserve(commits.len())
                .map_err(|source| Error::Allocation {
                    requested: commits.len().saturating_mul(mem::size_of::<Commit>()),
                    source,
                })?;
            for commit in commits.drain(..) {
                operation
                    .charge(
                        BudgetDimension::ResponseBytes,
                        commit_response_bytes(&commit),
                    )
                    .await?;
                next = commit.parents.first().copied();
                items.push(commit);
            }
        }
        let next = next.map(|oid| PageCursor::history_seek(self.commit_oid, oid));
        Ok(Page { items, next })
    }

    async fn offset_history(
        &self,
        traversal: HistoryTraversal,
        page: &PageRequest,
        operation: &OperationContext,
        skip: u64,
    ) -> Result<Page<Commit>> {
        let mut pending = vec![self.commit_oid];
        let mut visited = HashSet::new();
        let mut traversed = 0u64;
        let mut items = Vec::new();
        while let Some(oid) = pending.pop() {
            operation.ensure_active()?;
            if visited.contains(&oid) {
                continue;
            }
            if traversed >= skip && items.len() == page.limit() {
                pending.push(oid);
                break;
            }
            visited.insert(oid);
            operation.charge(BudgetDimension::HistoryCommits, 1).await?;
            let commit = operation.read_commit(oid).await?;
            match traversal {
                HistoryTraversal::FirstParent => {
                    if let Some(parent) = commit.parents.first() {
                        pending.push(*parent);
                    }
                }
                HistoryTraversal::AllParents => {
                    queue_all_parents(
                        &mut pending,
                        &commit,
                        self.commit_graph.as_deref(),
                        operation,
                    )
                    .await?;
                }
            }
            if traversed >= skip {
                operation
                    .charge(
                        BudgetDimension::ResponseBytes,
                        commit_response_bytes(&commit),
                    )
                    .await?;
                items.try_reserve(1).map_err(|source| Error::Allocation {
                    requested: mem::size_of::<Commit>(),
                    source,
                })?;
                items.push(commit);
            }
            traversed = traversed.checked_add(1).ok_or(Error::LimitExceeded {
                limit: "history commits",
                actual: u64::MAX,
                maximum: u64::MAX,
            })?;
        }
        while pending.last().is_some_and(|oid| visited.contains(oid)) {
            pending.pop();
        }
        let next = if pending.is_empty() {
            None
        } else {
            let next_skip = skip
                .checked_add(items.len() as u64)
                .ok_or(Error::LimitExceeded {
                    limit: "history commits",
                    actual: u64::MAX,
                    maximum: u64::MAX,
                })?;
            Some(PageCursor::history(self.commit_oid, traversal, next_skip))
        };
        Ok(Page { items, next })
    }
}
