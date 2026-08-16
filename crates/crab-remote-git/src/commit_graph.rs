use std::collections::HashMap;

use crab_metadata::commit_graph::CommitGraphSummary;
use crab_storage::{Store, StoreLayout};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::{CorruptionStage, Error, Result};

#[derive(Debug)]
struct CommitGraphEntry {
    generation: u64,
    parents: Vec<ObjectId>,
}

/// Validated, disposable acceleration data for raw commit traversal.
#[derive(Debug)]
pub(crate) struct CommitGraphIndex {
    entries: HashMap<ObjectId, CommitGraphEntry>,
}

impl CommitGraphIndex {
    pub(crate) async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        content_hash: Option<&str>,
        max_bytes: u64,
        cancellation: &CancellationToken,
        runtime_cancellation: &CancellationToken,
    ) -> Result<Option<Self>> {
        let Some(content_hash) = content_hash else {
            return Ok(None);
        };
        let path = layout.bulk_manifest_path("commit-graph", content_hash);
        let expected_hash = blake3::Hash::from_hex(content_hash)
            .map_err(|_| Error::Corrupt {
                stage: CorruptionStage::CommitGraph,
            })?
            .as_bytes()
            .to_owned();
        let metadata = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = store.head(&path) => result?,
        };
        if metadata.size > max_bytes {
            return Err(Error::LimitExceeded {
                limit: "commit graph bytes",
                actual: metadata.size,
                maximum: max_bytes,
            });
        }
        let bytes = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = store.verify(&path, &expected_hash) => result?,
        };
        if bytes.len() as u64 > max_bytes {
            return Err(Error::LimitExceeded {
                limit: "commit graph bytes",
                actual: bytes.len() as u64,
                maximum: max_bytes,
            });
        }
        let summary =
            serde_json::from_slice(&bytes).map_err(|source| Error::CommitGraphParse { source })?;
        Self::from_summary(summary).map(Some)
    }

    fn from_summary(summary: CommitGraphSummary) -> Result<Self> {
        if summary.commits.len() > CommitGraphSummary::DEFAULT_MAX_COMMITS {
            return Err(Error::LimitExceeded {
                limit: "commit graph entries",
                actual: summary.commits.len() as u64,
                maximum: CommitGraphSummary::DEFAULT_MAX_COMMITS as u64,
            });
        }
        let mut entries = HashMap::new();
        entries
            .try_reserve(summary.commits.len())
            .map_err(|source| Error::Allocation {
                requested: summary
                    .commits
                    .len()
                    .saturating_mul(std::mem::size_of::<CommitGraphEntry>()),
                source,
            })?;
        for entry in summary.commits {
            let oid = parse_oid(&entry.oid)?;
            let mut parents = Vec::new();
            parents
                .try_reserve_exact(entry.parents.len())
                .map_err(|source| Error::Allocation {
                    requested: entry
                        .parents
                        .len()
                        .saturating_mul(std::mem::size_of::<ObjectId>()),
                    source,
                })?;
            for parent in entry.parents {
                let parent = parse_oid(&parent)?;
                if parent == oid {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::CommitGraph,
                    });
                }
                parents.push(parent);
            }
            if entries
                .insert(
                    oid,
                    CommitGraphEntry {
                        generation: entry.gen_number,
                        parents,
                    },
                )
                .is_some()
            {
                return Err(Error::Corrupt {
                    stage: CorruptionStage::CommitGraph,
                });
            }
        }
        for entry in entries.values() {
            if entry.parents.iter().any(|parent| {
                entries
                    .get(parent)
                    .is_some_and(|parent| parent.generation >= entry.generation)
            }) {
                return Err(Error::Corrupt {
                    stage: CorruptionStage::CommitGraph,
                });
            }
        }
        Ok(Self { entries })
    }

    pub(crate) fn parents_match(&self, oid: ObjectId, raw_parents: &[ObjectId]) -> bool {
        self.entries
            .get(&oid)
            .is_some_and(|entry| entry.parents == raw_parents)
    }

    pub(crate) fn generation(&self, oid: &ObjectId) -> Option<u64> {
        self.entries.get(oid).map(|entry| entry.generation)
    }
}

fn parse_oid(value: &str) -> Result<ObjectId> {
    ObjectId::from_hex(value.as_bytes()).map_err(|_| Error::Corrupt {
        stage: CorruptionStage::CommitGraph,
    })
}

#[cfg(test)]
mod tests {
    use crab_metadata::commit_graph::CommitEntry;

    use super::*;

    fn entry(oid: char, generation: u64, parents: &[char]) -> CommitEntry {
        CommitEntry {
            oid: oid.to_string().repeat(40),
            gen_number: generation,
            parents: parents
                .iter()
                .map(|parent| parent.to_string().repeat(40))
                .collect(),
        }
    }

    #[test]
    fn rejects_duplicate_commits() {
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![entry('1', 0, &[]), entry('1', 0, &[])],
        };
        assert!(matches!(
            CommitGraphIndex::from_summary(summary),
            Err(Error::Corrupt {
                stage: CorruptionStage::CommitGraph
            })
        ));
    }

    #[test]
    fn rejects_non_topological_generations() {
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![entry('1', 1, &['2']), entry('2', 1, &[])],
        };
        assert!(matches!(
            CommitGraphIndex::from_summary(summary),
            Err(Error::Corrupt {
                stage: CorruptionStage::CommitGraph
            })
        ));
    }
}
