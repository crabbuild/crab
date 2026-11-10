//! Thin wrapper around SlateDB for the desktop indexer cache.
//!
//! Keys follow the format `"{repo_id}:{kind}:{rev}"` where `kind` is
//! one of `"file_summary"`, `"git_log"`, or `"pointer_meta"`.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use tracing::debug;

/// Wrapper around a SlateDB database instance for the indexer cache.
pub struct IndexStore {
    db: slatedb::Db,
}

impl IndexStore {
    /// Open or create the SlateDB database at the given filesystem path.
    ///
    /// Uses a local filesystem `object_store` backend — appropriate for
    /// the desktop sidecar where all data lives on the user's machine.
    pub async fn open(path: &Path) -> Result<Self, IndexStoreError> {
        // Ensure the directory exists.
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| IndexStoreError::Io(e.to_string()))?;

        let store = Arc::new(
            LocalFileSystem::new_with_prefix(path)
                .map_err(|e| IndexStoreError::Open(e.to_string()))?,
        );

        let db = slatedb::Db::builder(ObjectPath::from("/"), store)
            .with_db_cache(crab_metadata::default_slatedb_cache())
            .build()
            .await
            .map_err(|e| IndexStoreError::Open(e.to_string()))?;

        debug!(path = %path.display(), "index store opened");
        Ok(Self { db })
    }

    /// Build a cache key from the components.
    ///
    /// Format: `"{repo_id}:{kind}:{rev}"`
    pub fn make_key(repo_id: &str, kind: &str, rev: &str) -> Vec<u8> {
        format!("{repo_id}:{kind}:{rev}").into_bytes()
    }

    /// Read a cached value by key.
    pub async fn get(&self, key: &[u8]) -> Result<Option<Bytes>, IndexStoreError> {
        self.db
            .get(key)
            .await
            .map_err(|e| IndexStoreError::Read(e.to_string()))
    }

    /// Write a value to the cache.
    pub async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), IndexStoreError> {
        self.db
            .put(key, value)
            .await
            .map(|_| ())
            .map_err(|e| IndexStoreError::Write(e.to_string()))
    }

    /// Remove a cached value.
    pub async fn delete(&self, key: &[u8]) -> Result<(), IndexStoreError> {
        self.db
            .delete(key)
            .await
            .map(|_| ())
            .map_err(|e| IndexStoreError::Write(e.to_string()))
    }

    /// Close the database, flushing any pending writes.
    pub async fn close(self) -> Result<(), IndexStoreError> {
        self.db
            .close()
            .await
            .map_err(|e| IndexStoreError::Close(e.to_string()))
    }
}

/// Errors from the index store.
#[derive(Debug, thiserror::Error)]
pub enum IndexStoreError {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("failed to open index store: {0}")]
    Open(String),

    #[error("read error: {0}")]
    Read(String),

    #[error("write error: {0}")]
    Write(String),

    #[error("failed to close index store: {0}")]
    Close(String),
}
