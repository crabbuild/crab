use std::collections::{HashMap, HashSet};

use crab_metadata::split_commit_graph::{SplitCommitGraph, load_split_commit_graph};
use crab_storage::{Store, StoreLayout};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::{BudgetDimension, CorruptionStage, Error, RemoteGitRuntime, Result};

/// Validated, generation-bound acceleration data for raw commit traversal.
#[derive(Debug)]
pub(crate) struct CommitGraphIndex {
    graph: SplitCommitGraph,
    pinned_reachable: Vec<bool>,
}

impl CommitGraphIndex {
    pub(crate) const fn graph(&self) -> &SplitCommitGraph {
        &self.graph
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "graph loading validates manifest identity and two cancellation scopes"
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
        budget: &crate::budget::OperationBudget,
        operation: &crate::OperationContext,
    ) -> Result<Self> {
        let runtime_cancellation = runtime.background_cancellation();
        let graph = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = load_split_commit_graph(store, layout, content_hash, max_bytes) => result?,
        };
        if graph.descriptor.generation != expected_generation
            || graph.descriptor.pack_index_hash != expected_pack_index_hash
            || graph.descriptor.git_validation_digest != expected_validation_digest
        {
            return Err(Error::Corrupt {
                stage: CorruptionStage::CommitGraph,
            });
        }
        let roots = crate::repository::resolved_commit_roots(refs, Some(&graph), operation).await?;
        if roots
            .iter()
            .any(|root| sha1_bytes(*root).is_none_or(|root| !graph.contains(&root)))
        {
            return Err(Error::Corrupt {
                stage: CorruptionStage::CommitGraph,
            });
        }
        // One byte per ordinal is bounded by the verified graph byte limit.
        // Charge before allocation; reuse this proof only for this pinned ref set.
        budget
            .charge(
                BudgetDimension::InflatedBytes,
                u64::from(graph.descriptor.commit_count),
            )
            .await?;
        let permit = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            permit = runtime.decode_permit(cancellation) => permit?,
        };
        let cancellation = cancellation.clone();
        runtime
            .spawn_blocking(move || {
                let _permit = permit;
                let pinned_reachable =
                    pinned_reachability(&graph, &roots, &cancellation, &runtime_cancellation)?;
                Ok(Self {
                    graph,
                    pinned_reachable,
                })
            })
            .await
            .map_err(|source| Error::DecodeTask { source })?
    }

    pub(crate) fn is_reachable(&self, oid: ObjectId) -> bool {
        sha1_bytes(oid)
            .and_then(|oid| self.graph.ordinal(&oid))
            .and_then(|ordinal| self.pinned_reachable.get(ordinal as usize))
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn parents_match(&self, oid: ObjectId, raw_parents: &[ObjectId]) -> bool {
        let Some(ordinal) = sha1_bytes(oid).and_then(|oid| self.graph.ordinal(&oid)) else {
            return false;
        };
        let Some(record) = self.graph.record(ordinal) else {
            return false;
        };
        record.parents.len() == raw_parents.len()
            && record.parents.iter().zip(raw_parents).all(|(parent, raw)| {
                self.graph
                    .record(*parent)
                    .is_some_and(|record| ObjectId::Sha1(record.oid) == *raw)
            })
    }

    pub(crate) fn generation(&self, oid: &ObjectId) -> Option<u64> {
        let ordinal = self.graph.ordinal(&sha1_bytes(*oid)?)?;
        self.graph
            .record(ordinal)
            .map(|entry| entry.corrected_generation)
    }

    pub(crate) fn first_parent_chain(
        &self,
        start: ObjectId,
        maximum: usize,
    ) -> Option<Vec<ObjectId>> {
        let mut ordinal = self.graph.ordinal(&sha1_bytes(start)?)?;
        let mut chain = Vec::new();
        for _ in 0..maximum {
            let record = self.graph.record(ordinal)?;
            chain.push(ObjectId::Sha1(record.oid));
            let Some(parent) = record.parents.first().copied() else {
                break;
            };
            ordinal = parent;
        }
        Some(chain)
    }

    pub(crate) fn reachable_from_roots(
        &self,
        candidates: &[ObjectId],
        roots: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<Vec<bool>> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut reachable = vec![false; candidates.len()];
        let mut candidates_by_ordinal = HashMap::<u32, Vec<usize>>::new();
        for (index, candidate) in candidates.iter().enumerate() {
            if let Some(ordinal) = sha1_bytes(*candidate).and_then(|oid| self.graph.ordinal(&oid)) {
                candidates_by_ordinal
                    .entry(ordinal)
                    .or_default()
                    .push(index);
            }
        }
        if candidates_by_ordinal.is_empty() {
            return Ok(reachable);
        }
        let mut pending = roots
            .iter()
            .filter_map(|root| sha1_bytes(*root).and_then(|oid| self.graph.ordinal(&oid)))
            .collect::<Vec<_>>();
        let mut visited = HashSet::new();
        while let Some(ordinal) = pending.pop() {
            if !visited.insert(ordinal) {
                continue;
            }
            if visited.len().is_multiple_of(1024) && cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(indices) = candidates_by_ordinal.remove(&ordinal) {
                for index in indices {
                    reachable[index] = true;
                }
                if candidates_by_ordinal.is_empty() {
                    break;
                }
            }
            let record = self.graph.record(ordinal).ok_or(Error::Corrupt {
                stage: CorruptionStage::CommitGraph,
            })?;
            pending.extend(record.parents.iter().copied());
        }
        Ok(reachable)
    }
}

fn pinned_reachability(
    graph: &SplitCommitGraph,
    roots: &[ObjectId],
    cancellation: &CancellationToken,
    runtime_cancellation: &CancellationToken,
) -> Result<Vec<bool>> {
    if cancellation.is_cancelled() || runtime_cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let count = graph.descriptor.commit_count as usize;
    let mut reachable = Vec::new();
    reachable
        .try_reserve_exact(count)
        .map_err(|source| Error::Allocation {
            requested: count,
            source,
        })?;
    reachable.resize(count, false);
    for root in roots {
        let ordinal = sha1_bytes(*root)
            .and_then(|oid| graph.ordinal(&oid))
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::CommitGraph,
            })?;
        reachable[ordinal as usize] = true;
    }
    // The metadata codec proves every parent ordinal precedes its child.
    // A descending pass visits each edge once, without a per-query history walk.
    for ordinal in (0..graph.descriptor.commit_count).rev() {
        if ordinal.is_multiple_of(1024)
            && (cancellation.is_cancelled() || runtime_cancellation.is_cancelled())
        {
            return Err(Error::Cancelled);
        }
        if !reachable[ordinal as usize] {
            continue;
        }
        let record = graph.record(ordinal).ok_or(Error::Corrupt {
            stage: CorruptionStage::CommitGraph,
        })?;
        for parent in &record.parents {
            reachable[*parent as usize] = true;
        }
    }
    if cancellation.is_cancelled() || runtime_cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(reachable)
}

fn sha1_bytes(oid: ObjectId) -> Option<[u8; 20]> {
    match oid {
        ObjectId::Sha1(bytes) => Some(bytes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crab_metadata::split_commit_graph::{
        CommitGraphDescriptor, CommitGraphLayer, CommitGraphLayerRef, CommitGraphRecord,
        SplitCommitGraph, commit_graph_layer_path,
    };

    use super::*;

    fn oid(value: u8) -> [u8; 20] {
        [value; 20]
    }

    fn hash(value: u8) -> String {
        format!("{value:02x}").repeat(32)
    }

    #[test]
    fn pinned_membership_covers_both_merge_parents_but_excludes_retained_history() {
        let graph = graph(vec![
            CommitGraphRecord {
                oid: oid(1),
                tree_oid: oid(101),
                commit_time: 10,
                corrected_generation: 10,
                parents: vec![],
            },
            CommitGraphRecord {
                oid: oid(2),
                tree_oid: oid(102),
                commit_time: 20,
                corrected_generation: 20,
                parents: vec![0],
            },
            CommitGraphRecord {
                oid: oid(3),
                tree_oid: oid(103),
                commit_time: 20,
                corrected_generation: 20,
                parents: vec![0],
            },
            CommitGraphRecord {
                oid: oid(4),
                tree_oid: oid(104),
                commit_time: 30,
                corrected_generation: 30,
                parents: vec![1, 2],
            },
            CommitGraphRecord {
                oid: oid(5),
                tree_oid: oid(105),
                commit_time: 20,
                corrected_generation: 20,
                parents: vec![0],
            },
        ]);
        let cancellation = CancellationToken::new();
        let runtime_cancellation = CancellationToken::new();
        let pinned_reachable = pinned_reachability(
            &graph,
            &[ObjectId::Sha1(oid(4))],
            &cancellation,
            &runtime_cancellation,
        )
        .unwrap();
        let index = CommitGraphIndex {
            graph,
            pinned_reachable,
        };
        assert_eq!(
            (1..=6)
                .map(|value| index.is_reachable(ObjectId::Sha1(oid(value))))
                .collect::<Vec<_>>(),
            vec![true, true, true, true, false, false]
        );
    }

    #[test]
    fn pinned_membership_honors_caller_and_runtime_cancellation_even_for_empty_graph() {
        for runtime_cancelled in [false, true] {
            let cancellation = CancellationToken::new();
            let runtime_cancellation = CancellationToken::new();
            if runtime_cancelled {
                runtime_cancellation.cancel();
            } else {
                cancellation.cancel();
            }
            assert!(matches!(
                pinned_reachability(&graph(vec![]), &[], &cancellation, &runtime_cancellation),
                Err(Error::Cancelled)
            ));
        }
    }

    fn graph(records: Vec<CommitGraphRecord>) -> SplitCommitGraph {
        let bytes = 24
            + records
                .iter()
                .map(|record| 60 + record.parents.len() * 4)
                .sum::<usize>();
        let layer = CommitGraphLayer {
            base_ordinal: 0,
            records,
        };
        let descriptor = CommitGraphDescriptor {
            version: 1,
            generation: 7,
            pack_index_hash: hash(1),
            git_validation_digest: hash(2),
            commit_count: layer.records.len() as u32,
            layers: vec![CommitGraphLayerRef {
                hash: hash(3),
                path: commit_graph_layer_path(&hash(3)),
                base_ordinal: 0,
                commit_count: layer.records.len() as u32,
                bytes: bytes as u64,
            }]
            .into_iter()
            .filter(|layer| layer.commit_count > 0)
            .collect(),
        };
        SplitCommitGraph::new(
            descriptor,
            vec![layer]
                .into_iter()
                .filter(|layer| !layer.records.is_empty())
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn positional_parents_match_raw_commit_order() {
        let index = CommitGraphIndex {
            pinned_reachable: Vec::new(),
            graph: graph(vec![
                CommitGraphRecord {
                    oid: oid(1),
                    tree_oid: oid(101),
                    commit_time: 10,
                    corrected_generation: 10,
                    parents: vec![],
                },
                CommitGraphRecord {
                    oid: oid(2),
                    tree_oid: oid(102),
                    commit_time: 20,
                    corrected_generation: 20,
                    parents: vec![0],
                },
            ]),
        };
        assert!(index.parents_match(ObjectId::Sha1(oid(2)), &[ObjectId::Sha1(oid(1))]));
        assert_eq!(index.generation(&ObjectId::Sha1(oid(2))), Some(20));
    }

    #[test]
    fn missing_commit_cannot_validate_raw_parents() {
        let index = CommitGraphIndex {
            pinned_reachable: Vec::new(),
            graph: graph(vec![CommitGraphRecord {
                oid: oid(1),
                tree_oid: oid(101),
                commit_time: 10,
                corrected_generation: 10,
                parents: vec![],
            }]),
        };
        assert!(!index.parents_match(ObjectId::Sha1(oid(2)), &[]));
    }

    #[test]
    fn first_parent_chain_is_bounded_and_positional() {
        let index = CommitGraphIndex {
            pinned_reachable: Vec::new(),
            graph: graph(vec![
                CommitGraphRecord {
                    oid: oid(1),
                    tree_oid: oid(101),
                    commit_time: 10,
                    corrected_generation: 10,
                    parents: vec![],
                },
                CommitGraphRecord {
                    oid: oid(2),
                    tree_oid: oid(102),
                    commit_time: 20,
                    corrected_generation: 20,
                    parents: vec![0],
                },
                CommitGraphRecord {
                    oid: oid(3),
                    tree_oid: oid(103),
                    commit_time: 30,
                    corrected_generation: 30,
                    parents: vec![1, 0],
                },
            ]),
        };

        assert_eq!(
            index.first_parent_chain(ObjectId::Sha1(oid(3)), 2),
            Some(vec![ObjectId::Sha1(oid(3)), ObjectId::Sha1(oid(2))])
        );
        assert_eq!(
            index.first_parent_chain(ObjectId::Sha1(oid(2)), 8),
            Some(vec![ObjectId::Sha1(oid(2)), ObjectId::Sha1(oid(1))])
        );
        assert_eq!(
            index.first_parent_chain(ObjectId::Sha1(oid(3)), 0),
            Some(Vec::new())
        );
        assert_eq!(index.first_parent_chain(ObjectId::Sha1(oid(9)), 1), None);
    }

    #[test]
    fn reachable_from_roots_preserves_duplicates_and_rejects_other_history() {
        let index = CommitGraphIndex {
            pinned_reachable: Vec::new(),
            graph: graph(vec![
                CommitGraphRecord {
                    oid: oid(1),
                    tree_oid: oid(101),
                    commit_time: 10,
                    corrected_generation: 10,
                    parents: vec![],
                },
                CommitGraphRecord {
                    oid: oid(2),
                    tree_oid: oid(102),
                    commit_time: 20,
                    corrected_generation: 20,
                    parents: vec![0],
                },
                CommitGraphRecord {
                    oid: oid(3),
                    tree_oid: oid(103),
                    commit_time: 30,
                    corrected_generation: 30,
                    parents: vec![1],
                },
                CommitGraphRecord {
                    oid: oid(4),
                    tree_oid: oid(104),
                    commit_time: 20,
                    corrected_generation: 20,
                    parents: vec![0],
                },
            ]),
        };
        let candidates = [
            ObjectId::Sha1(oid(1)),
            ObjectId::Sha1(oid(2)),
            ObjectId::Sha1(oid(4)),
            ObjectId::Sha1(oid(9)),
            ObjectId::Sha1(oid(1)),
        ];

        assert_eq!(
            index
                .reachable_from_roots(
                    &candidates,
                    &[ObjectId::Sha1(oid(3))],
                    &CancellationToken::new(),
                )
                .expect("reachability"),
            vec![true, true, false, false, true]
        );
    }

    #[test]
    fn reachable_from_roots_honors_cancellation_before_work() {
        let index = CommitGraphIndex {
            pinned_reachable: Vec::new(),
            graph: graph(vec![CommitGraphRecord {
                oid: oid(1),
                tree_oid: oid(101),
                commit_time: 10,
                corrected_generation: 10,
                parents: vec![],
            }]),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(matches!(
            index.reachable_from_roots(
                &[ObjectId::Sha1(oid(1))],
                &[ObjectId::Sha1(oid(1))],
                &cancellation,
            ),
            Err(Error::Cancelled)
        ));
    }
}
