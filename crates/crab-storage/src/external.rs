//! Provider-neutral streaming reads for workflow and data commands.
//!
//! The trait deliberately has no eager bytes helper. Callers receive
//! provider metadata first and consume an incremental stream so memory use is
//! independent of object size.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use object_store::{ObjectMeta, ObjectStore, ObjectStoreExt, path::Path as ObjectPath};

use crate::error::{Result, StorageError};

/// Provider capabilities advertised by an external data adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalCapabilities {
    /// Object metadata is available.
    pub stat: bool,
    /// Prefix listing is available.
    pub list: bool,
    /// Streaming reads are available.
    pub read_stream: bool,
    /// Conditional validation is available.
    pub conditional: bool,
    /// Atomic writes are available.
    pub atomic_write: bool,
}

/// Provider metadata returned before a body stream is consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalObjectMeta {
    /// Canonical provider locator.
    pub locator: String,
    /// Provider-reported byte size.
    pub size: u64,
    /// Strong validator or version identity, never a Crab hash.
    pub validator: Option<String>,
    /// Optional last-modified value.
    pub last_modified: Option<String>,
}

/// Incremental external object body.
pub type ExternalByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Provider-neutral external data operations.
#[async_trait]
pub trait ExternalDataStore: Send + Sync {
    /// Return the operations proven by this adapter.
    fn capabilities(&self) -> ExternalCapabilities;
    /// Return one object's metadata without reading its body.
    async fn stat(&self, locator: &str) -> Result<ExternalObjectMeta>;
    /// List immediate or recursive members under a prefix.
    async fn list(&self, prefix: &str) -> Result<Vec<ExternalObjectMeta>>;
    /// Open a bounded-memory body stream.
    async fn open_stream(&self, locator: &str) -> Result<ExternalByteStream>;
}

/// Object-store adapter used by workflow, migration, and data commands.
pub struct ObjectStoreExternalDataStore {
    store: Arc<dyn ObjectStore>,
}

impl ObjectStoreExternalDataStore {
    /// Wrap an existing configured object store.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ExternalDataStore for ObjectStoreExternalDataStore {
    fn capabilities(&self) -> ExternalCapabilities {
        ExternalCapabilities {
            stat: true,
            list: true,
            read_stream: true,
            conditional: true,
            atomic_write: false,
        }
    }

    async fn stat(&self, locator: &str) -> Result<ExternalObjectMeta> {
        let location = ObjectPath::parse(locator)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let meta = self
            .store
            .head(&location)
            .await
            .map_err(|source| StorageError::ObjectStore { source })?;
        Ok(object_meta(meta))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ExternalObjectMeta>> {
        let location =
            ObjectPath::parse(prefix).map_err(|error| StorageError::Internal(error.to_string()))?;
        let mut stream = self.store.list(Some(&location));
        let mut result = Vec::new();
        while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
            let meta = item.map_err(|source| StorageError::ObjectStore { source })?;
            result.push(object_meta(meta));
        }
        result.sort_by(|left, right| left.locator.cmp(&right.locator));
        Ok(result)
    }

    async fn open_stream(&self, locator: &str) -> Result<ExternalByteStream> {
        let location = ObjectPath::parse(locator)
            .map_err(|error| StorageError::Internal(error.to_string()))?;
        let stream = self
            .store
            .get(&location)
            .await
            .map_err(|source| StorageError::ObjectStore { source })?
            .into_stream()
            .map(|item| item.map_err(|source| StorageError::ObjectStore { source }));
        Ok(Box::pin(stream))
    }
}

fn object_meta(meta: ObjectMeta) -> ExternalObjectMeta {
    ExternalObjectMeta {
        locator: meta.location.to_string(),
        size: meta.size,
        validator: meta.version.or_else(|| {
            meta.e_tag
                .filter(|value| !value.trim_start().starts_with("W/"))
        }),
        last_modified: Some(meta.last_modified.to_rfc3339()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn object_store_adapter_streams_without_eager_body() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let path = ObjectPath::from("object.bin");
        store
            .put(&path, object_store::PutPayload::from("payload"))
            .await
            .expect("put");
        let adapter = ObjectStoreExternalDataStore::new(store);
        assert!(adapter.capabilities().read_stream);
        let mut body = adapter.open_stream("object.bin").await.expect("stream");
        let mut bytes = Vec::new();
        while let Some(chunk) = futures_util::StreamExt::next(&mut body).await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(bytes, b"payload");
    }
}
