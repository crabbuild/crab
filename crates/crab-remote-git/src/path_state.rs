use std::sync::Arc;

use crab_metadata::error::MetadataError;
use crab_metadata::path_state::load_path_state;
use crab_storage::{StorageError, Store, StoreLayout};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::commit_graph::CommitGraphIndex;
use crate::{CommitSummary, CorruptionStage, Error, GitPath, Result};

/// Validated generation-bound path attribution metadata.
#[derive(Debug)]
pub(crate) struct PathStateIndex {
    index: crab_metadata::path_state::PathStateIndex,
    commit_graph: Arc<CommitGraphIndex>,
}

impl PathStateIndex {
    pub(crate) async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        content_hash: Option<&str>,
        commit_graph: Option<&Arc<CommitGraphIndex>>,
        max_bytes: u64,
        cancellation: &CancellationToken,
        runtime_cancellation: &CancellationToken,
    ) -> Result<Option<Self>> {
        let Some(content_hash) = content_hash else {
            return Ok(None);
        };
        let Some(commit_graph) = commit_graph else {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PathState,
            });
        };
        let index = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = load_path_state(
                store,
                layout,
                content_hash,
                commit_graph.graph(),
                max_bytes,
            ) => match result {
                Ok(index) => index,
                Err(
                    MetadataError::CorruptObject { .. }
                    | MetadataError::Storage {
                        source:
                            StorageError::CorruptObject { .. } | StorageError::NotFound { .. },
                    },
                ) => {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::PathState,
                    });
                }
                Err(error) => return Err(Error::Metadata(error)),
            },
        };
        Ok(Some(Self {
            index,
            commit_graph: Arc::clone(commit_graph),
        }))
    }

    pub(crate) fn latest(&self, commit: ObjectId, paths: &[GitPath]) -> Result<Vec<CommitSummary>> {
        let ObjectId::Sha1(commit) = commit else {
            return Err(Error::UnsupportedObjectFormat);
        };
        let paths = paths.iter().map(GitPath::as_bytes).collect::<Vec<_>>();
        self.index
            .latest(self.commit_graph.graph(), &commit, &paths)
            .map(|summaries| {
                summaries
                    .into_iter()
                    .map(|summary| CommitSummary {
                        oid: ObjectId::Sha1(summary.oid),
                        author: summary.author.into(),
                        author_seconds: summary.author_seconds,
                        message: summary.message.into(),
                    })
                    .collect()
            })
            .map_err(|_| Error::Corrupt {
                stage: CorruptionStage::PathState,
            })
    }
}
