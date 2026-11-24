use std::{ops::Range, path::Path, sync::Arc};

use crab_cache_store::{CacheConfig, CachingStore};
use crab_coordination::read_retention_runtime::RetainedRead;
use crab_metadata::{
    capsule_protocol::PointerCatalogLookup, file_index_lookup::SharedFileIndexLookup,
};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use tokio_util::sync::CancellationToken;

use super::{ReadRuntimeBuilder, ReadStoreLayout, ReconstructionStream, ReconstructionWork};
use crate::{ReadError, Result};

/// Identifies stored file content from a caller-verified Git pointer blob.
pub struct ArtifactPointer(Pointer);

impl ArtifactPointer {
    /// Parse the pointer bytes selected by the caller's Git path authorization.
    pub fn parse(data: &[u8]) -> Result<Self> {
        Ok(Self(Pointer::parse(data)?))
    }

    /// Logical size declared by the pointer.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.0.size
    }

    /// Full-file content identity declared by the pointer.
    #[must_use]
    pub fn hash(&self) -> blake3::Hash {
        blake3::Hash::from_bytes(self.0.file_hash)
    }

    /// Verify that the selected catalog authorizes the exact file and size.
    pub async fn bind(
        self,
        layout: ReadStoreLayout,
        catalog: PointerCatalogLookup,
    ) -> Result<CapturedArtifact> {
        let key = MerkleHash::from(self.0.file_hash).hex();
        let files = catalog
            .files(layout.store(), std::slice::from_ref(&key))
            .await?;
        let binding = files
            .files()
            .get(&key)
            .ok_or(ReadError::UnauthorizedObject)?;
        if binding.size() != self.size() {
            return Err(ReadError::CorruptObject {
                path: key,
                reason: "committed pointer and catalog file sizes differ".into(),
            });
        }
        Ok(CapturedArtifact {
            pointer: self.0,
            layout,
            catalog,
        })
    }
}

/// A file whose pointer is bound to its captured repository catalog.
pub struct CapturedArtifact {
    pointer: Pointer,
    layout: ReadStoreLayout,
    catalog: PointerCatalogLookup,
}

impl CapturedArtifact {
    /// Catalog to pin in the caller's read retention claim before streaming.
    #[must_use]
    pub fn catalog(&self) -> &PointerCatalogLookup {
        &self.catalog
    }

    /// Prepare a retained range and its cleanup worker on a blocking runtime thread.
    ///
    /// The caller supplies private staging for cache metadata and retains it until
    /// the worker completes. The transfer keeps no persistent payload cache.
    pub fn prepare_range(
        self,
        cache: &Path,
        retained: RetainedRead,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<(ReconstructionStream, ReconstructionWork)> {
        let local = crab_cache::LocalCache::with_limits(cache.to_owned(), 0_u64, Some(0));
        let store = CachingStore::new_with_local_cache(
            self.layout.store().clone(),
            CacheConfig::default(),
            Arc::new(local),
        )?;
        let lookup = SharedFileIndexLookup::for_pointer_catalog(self.layout.clone(), self.catalog);
        ReadRuntimeBuilder::new(store, self.layout, 4)
            .build()?
            .with_read_retention(retained)
            .prepare_captured_range_stream(&self.pointer, range, lookup, cancel)
    }
}
