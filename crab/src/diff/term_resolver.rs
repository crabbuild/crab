//! CLI Adapter for read-domain term resolution.

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::cache::LocalCache;
use crate::core::Result;
use crate::storage::StoreLayout;
use crab_cache_store::CachingStore;
use crab_diff::chunk_sequence::ChunkSequence;
use crab_diff::types::ChunkSequenceSourceKind;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::FileDataSequenceEntry;

/// CLI facade over [`crab_read::TermResolver`].
pub struct TermResolver {
    inner: crab_read::TermResolver,
}

impl TermResolver {
    /// Create a resolver from the current CLI storage layout facade.
    #[must_use]
    pub fn new(
        store: CachingStore,
        router: StoreLayout,
        cache: Arc<LocalCache>,
        concurrency: usize,
    ) -> Self {
        let router = crab_read::ReadStoreLayout::with_global_prefix(
            router.store().as_storage().clone(),
            router.repo_prefix().to_owned(),
            router.global_prefix().to_owned(),
        );
        Self {
            inner: crab_read::TermResolver::new(store, router, cache, concurrency),
        }
    }

    /// Resolve file hashes to reconstruction terms.
    pub async fn resolve_batch(
        &self,
        file_hashes: &[(MerkleHash, Option<MerkleHash>)],
        cancel: &CancellationToken,
    ) -> Result<HashMap<MerkleHash, Vec<FileDataSequenceEntry>>> {
        self.inner
            .resolve_batch(file_hashes, cancel)
            .await
            .map_err(Into::into)
    }

    /// Resolve file hashes to chunk sequences.
    pub async fn resolve_sequences_batch(
        &self,
        files: &[(MerkleHash, Option<MerkleHash>, u64)],
        source: ChunkSequenceSourceKind,
        cancel: &CancellationToken,
    ) -> Result<HashMap<MerkleHash, ChunkSequence>> {
        self.inner
            .resolve_sequences_batch(files, source, cancel)
            .await
            .map_err(Into::into)
    }
}
