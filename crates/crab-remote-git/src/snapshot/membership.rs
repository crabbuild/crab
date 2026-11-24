use std::collections::HashSet;

use gix_hash::ObjectId;

use super::{RemoteGitSnapshot, commit_ancestors};
use crate::{BudgetDimension, OperationContext, Result};

/// Checks successive candidate batches against one captured commit.
pub struct CommitMembership<'a> {
    snapshot: &'a RemoteGitSnapshot,
    operation: &'a OperationContext,
    ancestors: Option<HashSet<ObjectId>>,
}

impl RemoteGitSnapshot {
    /// Bind membership checks to this snapshot and its admitted read operation.
    ///
    /// Without an index, the first nonempty batch walks history once. Later
    /// batches share the result and still charge the same operation budget.
    pub fn commit_membership_reader<'a>(
        &'a self,
        operation: &'a OperationContext,
    ) -> Result<CommitMembership<'a>> {
        self.ensure_operation(operation)?;
        Ok(CommitMembership {
            snapshot: self,
            operation,
            ancestors: None,
        })
    }
}

impl CommitMembership<'_> {
    /// Report whether each candidate is the root commit or one of its ancestors.
    pub async fn contains(&mut self, candidates: &[ObjectId]) -> Result<Vec<bool>> {
        let operation = self.operation;
        operation.ensure_active()?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let root = self.snapshot.commit_oid;
        if let Some(graph) = self.snapshot.commit_graph.as_deref() {
            return graph
                .reachable_from_roots(candidates, &[root], operation)
                .await;
        }
        operation
            .charge(BudgetDimension::Entries, candidates.len() as u64)
            .await?;
        let ancestors = match &mut self.ancestors {
            Some(ancestors) => ancestors,
            None => self
                .ancestors
                .insert(commit_ancestors(root, operation, None).await?),
        };
        Ok(candidates
            .iter()
            .map(|oid| ancestors.contains(oid))
            .collect())
    }
}
