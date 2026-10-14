use std::sync::Arc;

use crab_metadata::error::MetadataError;
use crab_metadata::path_state::PathStateReader;
use crab_storage::{StorageError, Store, StoreLayout};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::commit_graph::CommitGraphIndex;
use crate::{
    BudgetDimension, CommitSummary, CorruptionStage, Error, GitPath, OperationContext, Result,
};

/// Validated generation-bound path attribution metadata.
#[derive(Debug)]
pub(crate) struct PathStateIndex {
    index: PathStateReader,
    commit_graph: Arc<CommitGraphIndex>,
}

impl PathStateIndex {
    pub(crate) async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        content_hash: &str,
        commit_graph: &Arc<CommitGraphIndex>,
        max_bytes: u64,
        cancellation: &CancellationToken,
        runtime_cancellation: &CancellationToken,
    ) -> Result<Self> {
        let index = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = PathStateReader::load(
                store,
                layout,
                content_hash,
                commit_graph.view(),
                max_bytes,
            ) => result.map_err(metadata_error)?,
        };
        Ok(Self {
            index,
            commit_graph: Arc::clone(commit_graph),
        })
    }

    pub(crate) async fn latest(
        &self,
        store: &Store,
        commit: ObjectId,
        paths: &[GitPath],
        operation: &OperationContext,
    ) -> Result<Vec<CommitSummary>> {
        operation.ensure_active()?;
        let ObjectId::Sha1(commit) = commit else {
            return Err(Error::UnsupportedObjectFormat);
        };
        operation
            .charge(BudgetDimension::LogicalObjects, paths.len() as u64)
            .await?;
        operation
            .charge(BudgetDimension::Entries, paths.len() as u64)
            .await?;
        let depth = paths
            .iter()
            .map(|path| path.components().count())
            .max()
            .unwrap_or(0);
        operation
            .charge(BudgetDimension::Depth, depth as u64)
            .await?;
        let paths = paths.iter().map(GitPath::as_bytes).collect::<Vec<_>>();
        // Keep admission local to this invocation. The cached index must not
        // retain a finished operation's budget or cancellation token.
        let store = store
            .clone()
            .with_read_admission(operation.read_admission());
        let summaries = tokio::select! {
            biased;
            () = operation.cancellation().cancelled() => return Err(Error::Cancelled),
            result = self.index.latest(&store, self.commit_graph.view(), &commit, &paths) => {
                result.map_err(metadata_error)?
            },
        };
        operation.ensure_active()?;
        let mut output = Vec::with_capacity(summaries.len());
        for summary in summaries {
            operation
                .charge(
                    BudgetDimension::ResponseBytes,
                    (summary.author.len() + summary.message.len() + 28) as u64,
                )
                .await?;
            output.push(CommitSummary {
                oid: ObjectId::Sha1(summary.oid),
                author: summary.author.into(),
                author_seconds: summary.author_seconds,
                message: summary.message.into(),
            });
        }
        Ok(output)
    }
}

fn metadata_error(error: MetadataError) -> Error {
    match error {
        MetadataError::CorruptObject { .. }
        | MetadataError::Storage {
            source: StorageError::CorruptObject { .. } | StorageError::NotFound { .. },
        } => Error::Corrupt {
            stage: CorruptionStage::PathState,
        },
        error => Error::Metadata(error),
    }
}
