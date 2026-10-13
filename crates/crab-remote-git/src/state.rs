use std::collections::HashMap;
use std::sync::Arc;

use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectCatalogIdentity, GitPackInventoryEntry,
};
use crab_metadata::shallow_closure::ShallowClosureDescriptor;
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use tokio::sync::OnceCell;

use crate::commit_graph::CommitGraphIndex;
use crate::path_state::PathStateIndex;
use crate::reader::RemoteGitReader;
use crate::{
    CorruptionStage, Error, OperationContext, RemoteGitRuntime, RepositoryIdentity,
    RepositoryOptions, RepositoryRefs, Result,
};

/// Immutable repository facts shared by handles and snapshots.
#[derive(Clone)]
pub(crate) struct RepositoryState {
    pub(crate) store: Store,
    pub(crate) layout: StoreLayout<Store>,
    pub(crate) runtime: Arc<RemoteGitRuntime>,
    pub(crate) identity: RepositoryIdentity,
    pub(crate) options: RepositoryOptions,
    pub(crate) generation: u64,
    pub(crate) pack_index_hash: Arc<str>,
    pub(crate) git_validation_digest: Arc<str>,
    pub(crate) manifest_etag: String,
    pub(crate) shard_index_hash: Arc<str>,
    /// Catalog for exact-generation APIs such as visibility and complete-pack transfer.
    pub(crate) catalog_identity: Option<GitObjectCatalogIdentity>,
    /// Catalog usable for object lookup, including beneath a bounded journal overlay.
    pub(crate) lookup_catalog_identity: Option<GitObjectCatalogIdentity>,
    pub(crate) inventory: HashMap<MerkleHash, GitPackInventoryEntry>,
    pub(crate) refs: RepositoryRefs,
    pub(crate) reader: Option<Arc<RemoteGitReader>>,
    pub(crate) commit_graph: Option<Arc<CommitGraphIndex>>,
    pub(crate) path_state_hash: Option<Arc<str>>,
    pub(crate) path_state: OnceCell<Arc<PathStateIndex>>,
    pub(crate) shallow_closure: Option<Arc<ShallowClosureDescriptor>>,
}

impl RepositoryState {
    pub(crate) fn coverage(&self) -> Option<GitLocatorCoverage> {
        self.catalog_identity.map(|catalog| GitLocatorCoverage {
            generation: catalog.generation,
            pack_index_hash: catalog.pack_index_hash,
        })
    }

    pub(crate) async fn path_state(
        &self,
        operation: &OperationContext,
    ) -> Result<Option<&Arc<PathStateIndex>>> {
        let Some(hash) = self.path_state_hash.as_deref() else {
            return Ok(None);
        };
        operation.ensure_active()?;
        let graph = self.commit_graph.as_ref().ok_or(Error::Corrupt {
            stage: CorruptionStage::PathState,
        })?;
        let store = self
            .store
            .clone()
            .with_read_admission(operation.read_admission());
        let runtime_cancellation = self.runtime.background_cancellation();
        let load = self.path_state.get_or_try_init(|| async {
            PathStateIndex::load(
                &store,
                &self.layout,
                hash,
                graph,
                self.options.object_limits().max_path_state_bytes,
                operation.cancellation(),
                &runtime_cancellation,
            )
            .await
            .map(Arc::new)
        });
        let index = tokio::select! {
            biased;
            () = operation.cancellation().cancelled() => return Err(Error::Cancelled),
            () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
            result = load => result?,
        };
        Ok(Some(index))
    }
}
