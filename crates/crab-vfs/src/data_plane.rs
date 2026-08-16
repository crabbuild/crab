//! Synchronous data-plane contracts used by the VFS hydration workers.

use std::ops::Range;

use crate::{Result, VfsError};

/// One xorb byte range needed to reconstruct a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconstructionTerm {
    pub xorb_hash: [u8; 32],
    pub offset: u64,
    pub length: u64,
    pub chunk_hash: [u8; 32],
}

/// Resolves a file hash to the shard that describes it.
pub trait FileIndexResolver: Send + Sync {
    fn resolve_file_index(
        &self,
        file_hash: &[u8; 32],
        shard_hint: Option<&[u8; 32]>,
    ) -> Result<Option<[u8; 32]>>;

    fn scan_shard_list_for_file(&self, file_hash: &[u8; 32]) -> Result<Option<[u8; 32]>>;
}

/// Loads reconstruction terms for a file from a shard.
pub trait ShardLoader: Send + Sync {
    fn load_reconstruction_terms(
        &self,
        shard_hash: &[u8; 32],
        file_hash: &[u8; 32],
    ) -> Result<Vec<ReconstructionTerm>>;
}

/// Fetches byte ranges from content-addressed xorbs.
pub trait XorbFetcher: Send + Sync {
    fn fetch_range(&self, xorb_hash: &[u8; 32], range: Range<u64>) -> Result<Vec<u8>>;
}

#[derive(Debug, Default)]
pub struct NoopFileIndexResolver;

impl FileIndexResolver for NoopFileIndexResolver {
    fn resolve_file_index(
        &self,
        _file_hash: &[u8; 32],
        _shard_hint: Option<&[u8; 32]>,
    ) -> Result<Option<[u8; 32]>> {
        Ok(None)
    }

    fn scan_shard_list_for_file(&self, _file_hash: &[u8; 32]) -> Result<Option<[u8; 32]>> {
        Ok(None)
    }
}

#[derive(Debug, Default)]
pub struct NoopShardLoader;

impl ShardLoader for NoopShardLoader {
    fn load_reconstruction_terms(
        &self,
        _shard_hash: &[u8; 32],
        _file_hash: &[u8; 32],
    ) -> Result<Vec<ReconstructionTerm>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Default)]
pub struct NoopXorbFetcher;

impl XorbFetcher for NoopXorbFetcher {
    fn fetch_range(&self, xorb_hash: &[u8; 32], _range: Range<u64>) -> Result<Vec<u8>> {
        Err(VfsError::NotFound {
            path: format!("xorb/{}", crab_types::pointer::hex_encode(xorb_hash)),
        })
    }
}
