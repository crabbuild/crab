use std::collections::{HashMap, HashSet};

use crab_metadata::split_commit_graph::{SplitCommitGraph, load_split_commit_graph};
use crab_storage::{Store, StoreLayout};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::{CorruptionStage, Error, Result};

/// Validated, generation-bound acceleration data for raw commit traversal.
#[derive(Debug)]
pub(crate) struct CommitGraphIndex {
    graph: SplitCommitGraph,
}

impl CommitGraphIndex {
    #[expect(
        clippy::too_many_arguments,
        reason = "graph loading validates manifest identity and two cancellation scopes"
    )]
    pub(crate) async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        content_hash: Option<&str>,
        expected_generation: u64,
        expected_pack_index_hash: &str,
        expected_validation_digest: &str,
        expected_roots: &[ObjectId],
        max_bytes: u64,
        cancellation: &CancellationToken,
        runtime_cancellation: &CancellationToken,
    ) -> Result<Option<Self>> {
        let Some(content_hash) = content_hash else {
            return Ok(None);
        };
        let graph = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = load_split_commit_graph(store, layout, content_hash, max_bytes) => result?,
        };
        if graph.descriptor.generation != expected_generation
            || graph.descriptor.pack_index_hash != expected_pack_index_hash
            || graph.descriptor.git_validation_digest != expected_validation_digest
            || expected_roots
                .iter()
                .any(|root| sha1_bytes(*root).is_none_or(|root| !graph.contains(&root)))
        {
            return Err(Error::Corrupt {
                stage: CorruptionStage::CommitGraph,
            });
        }
        Ok(Some(Self { graph }))
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
            }],
        };
        SplitCommitGraph::new(descriptor, vec![layer]).unwrap()
    }

    #[test]
    fn positional_parents_match_raw_commit_order() {
        let index = CommitGraphIndex {
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
