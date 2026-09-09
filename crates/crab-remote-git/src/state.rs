use std::collections::HashMap;
use std::sync::Arc;

use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectCatalogIdentity, GitPackInventoryEntry,
};
use crab_metadata::shallow_closure::ShallowClosureDescriptor;
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;

use crate::commit_graph::CommitGraphIndex;
use crate::reader::RemoteGitReader;
use crate::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions, RepositoryRefs};

/// Immutable repository facts shared by handles and snapshots.
pub(crate) struct RepositoryState {
    pub(crate) store: Store,
    pub(crate) layout: StoreLayout<Store>,
    pub(crate) runtime: Arc<RemoteGitRuntime>,
    pub(crate) identity: RepositoryIdentity,
    pub(crate) options: RepositoryOptions,
    pub(crate) generation: u64,
    pub(crate) git_validation_digest: Arc<str>,
    pub(crate) manifest_etag: String,
    pub(crate) shard_index_hash: Arc<str>,
    pub(crate) catalog_identity: Option<GitObjectCatalogIdentity>,
    pub(crate) inventory: HashMap<MerkleHash, GitPackInventoryEntry>,
    pub(crate) refs: RepositoryRefs,
    pub(crate) reader: Option<Arc<RemoteGitReader>>,
    pub(crate) commit_graph: Option<Arc<CommitGraphIndex>>,
    pub(crate) shallow_closure: Option<Arc<ShallowClosureDescriptor>>,
}

impl RepositoryState {
    pub(crate) fn coverage(&self) -> Option<GitLocatorCoverage> {
        self.catalog_identity.map(|catalog| GitLocatorCoverage {
            generation: catalog.generation,
            pack_index_hash: catalog.pack_index_hash,
        })
    }
}
