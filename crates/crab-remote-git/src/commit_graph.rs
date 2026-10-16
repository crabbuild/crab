use std::collections::{HashMap, HashSet};

use crab_metadata::split_commit_graph::{
    CommitGraphReader, CommitGraphRecord, CommitGraphView, SplitCommitGraph,
    load_split_commit_graph, load_split_commit_graph_descriptor,
};
use crab_storage::{Store, StoreLayout};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::{BudgetDimension, CorruptionStage, Error, OperationContext, RemoteGitRuntime, Result};

#[derive(Debug)]
enum Graph {
    Manifest(SplitCommitGraph),
    Capsule(Box<CommitGraphReader>),
}

/// Validated positional graph bound to captured ref roots, never physical retention.
#[derive(Debug)]
pub(crate) struct CommitGraphIndex {
    graph: Graph,
    roots: Vec<ObjectId>,
}

impl CommitGraphIndex {
    pub(crate) fn view(&self) -> CommitGraphView<'_> {
        match &self.graph {
            Graph::Manifest(graph) => CommitGraphView::Materialized(graph),
            Graph::Capsule(graph) => CommitGraphView::Paged(graph),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "graph loading binds captured state and operation admission"
    )]
    pub(crate) async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        content_hash: &str,
        expected_generation: u64,
        expected_pack_index_hash: &str,
        expected_validation_digest: &str,
        refs: &crate::RepositoryRefs,
        max_bytes: u64,
        cancellation: &CancellationToken,
        runtime: &RemoteGitRuntime,
        operation: &OperationContext,
    ) -> Result<Self> {
        let runtime_cancellation = runtime.background_cancellation();
        let graph = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = async {
                let descriptor = load_split_commit_graph_descriptor(store, layout, content_hash, max_bytes).await?;
                // Release-tagged manifest graph v1 needs complete validation. Capsule
                // graphs use authenticated pages; corruption cannot select the old reader.
                if descriptor.version == 1 {
                    load_split_commit_graph(store, layout, content_hash, max_bytes).await.map(Graph::Manifest)
                } else {
                    CommitGraphReader::load(store, layout, content_hash, max_bytes).await.map(Box::new).map(Graph::Capsule)
                }
            } => result?,
        };
        let mut index = Self {
            graph,
            roots: Vec::new(),
        };
        let view = index.view();
        let descriptor = view.descriptor();
        if descriptor.generation != expected_generation
            || descriptor.pack_index_hash != expected_pack_index_hash
            || descriptor.git_validation_digest != expected_validation_digest
        {
            return Err(corrupt());
        }
        let roots = crate::repository::resolved_commit_roots(refs, Some(view), operation).await?;
        let ordinals = index.ordinals(&roots, operation).await?;
        if ordinals.iter().any(Option::is_none) {
            return Err(corrupt());
        }
        index.roots = roots;
        Ok(index)
    }

    pub(crate) async fn ordinals(
        &self,
        oids: &[ObjectId],
        operation: &OperationContext,
    ) -> Result<Vec<Option<u32>>> {
        operation.ensure_active()?;
        operation
            .charge(BudgetDimension::Entries, oids.len() as u64)
            .await?;
        let mut positions = Vec::new();
        let mut keys = Vec::new();
        for (position, oid) in oids.iter().enumerate() {
            if let ObjectId::Sha1(bytes) = oid {
                positions.push(position);
                keys.push(*bytes);
            }
        }
        let store = operation.metadata_store();
        let ordinals = operation
            .read_metadata(self.view().ordinals(&store, &keys))
            .await?;
        operation.ensure_active()?;
        let mut output = vec![None; oids.len()];
        for (position, ordinal) in positions.into_iter().zip(ordinals) {
            output[position] = ordinal;
        }
        Ok(output)
    }

    async fn records(
        &self,
        ordinals: &[u32],
        operation: &OperationContext,
    ) -> Result<Vec<Option<CommitGraphRecord>>> {
        operation.ensure_active()?;
        operation
            .charge(BudgetDimension::Entries, ordinals.len() as u64)
            .await?;
        let store = operation.metadata_store();
        let records = operation
            .read_metadata(self.view().records(&store, ordinals))
            .await?;
        operation.ensure_active()?;
        Ok(records)
    }

    pub(crate) async fn is_reachable(
        &self,
        oid: ObjectId,
        operation: &OperationContext,
    ) -> Result<bool> {
        operation.ensure_active()?;
        // Opening verifies and sorts these captured commit roots. A root is
        // already visible; rereading its ancestor set adds no authority proof.
        if self.roots.binary_search(&oid).is_ok() {
            operation.charge(BudgetDimension::Entries, 1).await?;
            operation.ensure_active()?;
            return Ok(true);
        }
        Ok(self
            .reachable_from_roots(&[oid], &self.roots, operation)
            .await?[0])
    }

    pub(crate) async fn parents_match(
        &self,
        commits: &[crate::Commit],
        operation: &OperationContext,
    ) -> Result<bool> {
        let oids = commits.iter().map(|commit| commit.oid).collect::<Vec<_>>();
        let ordinals = self.ordinals(&oids, operation).await?;
        if ordinals.iter().any(Option::is_none) {
            return Ok(false);
        }
        let ordinals = ordinals.into_iter().flatten().collect::<Vec<_>>();
        let records = self.records(&ordinals, operation).await?;
        let parent_ordinals = records
            .iter()
            .flatten()
            .flat_map(|record| record.parents.iter().copied())
            .collect::<Vec<_>>();
        let parents = self.records(&parent_ordinals, operation).await?;
        let parents = parent_ordinals
            .into_iter()
            .zip(parents)
            .collect::<HashMap<_, _>>();
        Ok(records.iter().zip(commits).all(|(record, commit)| {
            record.as_ref().is_some_and(|record| {
                record.parents.len() == commit.parents.len()
                    && record
                        .parents
                        .iter()
                        .zip(&commit.parents)
                        .all(|(ordinal, raw)| {
                            parents
                                .get(ordinal)
                                .and_then(Option::as_ref)
                                .is_some_and(|parent| ObjectId::Sha1(parent.oid) == *raw)
                        })
            })
        }))
    }

    pub(crate) async fn generations(
        &self,
        oids: &[ObjectId],
        operation: &OperationContext,
    ) -> Result<Vec<Option<u64>>> {
        let ordinals = self.ordinals(oids, operation).await?;
        let ids = ordinals.iter().flatten().copied().collect::<Vec<_>>();
        let records = self.records(&ids, operation).await?;
        let mut records = records.into_iter();
        Ok(ordinals
            .into_iter()
            .map(|ordinal| {
                ordinal.and_then(|_| {
                    records
                        .next()
                        .flatten()
                        .map(|record| record.corrected_generation)
                })
            })
            .collect())
    }

    pub(crate) async fn first_parent_chain(
        &self,
        start: ObjectId,
        maximum: usize,
        operation: &OperationContext,
    ) -> Result<Option<Vec<ObjectId>>> {
        let Some(mut ordinal) = self.ordinals(&[start], operation).await?[0] else {
            return Ok(None);
        };
        let mut chain = Vec::new();
        for _ in 0..maximum {
            let record = self
                .records(&[ordinal], operation)
                .await?
                .remove(0)
                .ok_or_else(corrupt)?;
            chain.push(ObjectId::Sha1(record.oid));
            let Some(parent) = record.parents.first().copied() else {
                break;
            };
            ordinal = parent;
        }
        Ok(Some(chain))
    }

    pub(crate) async fn reachable_from_roots(
        &self,
        candidates: &[ObjectId],
        roots: &[ObjectId],
        operation: &OperationContext,
    ) -> Result<Vec<bool>> {
        let ordinals = self.ordinals(candidates, operation).await?;
        let roots = self.ordinals(roots, operation).await?;
        let mut output = vec![false; candidates.len()];
        let positions = ordinals
            .iter()
            .enumerate()
            .filter_map(|(position, ordinal)| ordinal.map(|ordinal| (position, ordinal)))
            .collect::<Vec<_>>();
        let requested = positions
            .iter()
            .map(|(_, ordinal)| *ordinal)
            .collect::<Vec<_>>();
        match &self.graph {
            Graph::Capsule(graph) => {
                for root in roots.into_iter().flatten() {
                    operation.ensure_active()?;
                    let store = operation.metadata_store();
                    let membership = operation
                        .read_metadata(graph.ancestry_membership(&store, root, &requested))
                        .await?;
                    for ((position, _), present) in positions.iter().zip(membership) {
                        output[*position] |= present;
                    }
                }
            }
            Graph::Manifest(graph) => {
                let mut pending = roots.into_iter().flatten().collect::<Vec<_>>();
                let mut visited = HashSet::new();
                let mut requested = positions.into_iter().fold(
                    HashMap::<u32, Vec<usize>>::new(),
                    |mut grouped, (position, ordinal)| {
                        grouped.entry(ordinal).or_default().push(position);
                        grouped
                    },
                );
                while !requested.is_empty()
                    && let Some(ordinal) = pending.pop()
                {
                    operation.ensure_active()?;
                    if visited.contains(&ordinal) {
                        continue;
                    }
                    // Released graph proofs do not consume raw-history reads.
                    // Admit metadata work before growing the visited set.
                    operation.charge(BudgetDimension::Entries, 1).await?;
                    visited.insert(ordinal);
                    if let Some(positions) = requested.remove(&ordinal) {
                        for position in positions {
                            output[position] = true;
                        }
                    }
                    let record = graph.record(ordinal).ok_or_else(corrupt)?;
                    pending.extend(record.parents.iter().copied());
                }
            }
        }
        operation.ensure_active()?;
        Ok(output)
    }
}

fn corrupt() -> Error {
    Error::Corrupt {
        stage: CorruptionStage::CommitGraph,
    }
}
