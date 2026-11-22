use std::sync::Arc;

use crab_storage::{ETag, StorageError};
use serde::Deserialize;

use super::{
    CachedCatalog, CatalogDocument, CatalogError, CatalogStore, MAX_CATALOG_BYTES,
    MembershipSnapshot, SCHEMA_VERSION,
};

#[derive(Deserialize)]
struct CatalogSchema {
    schema_version: u32,
}

impl CatalogStore {
    pub async fn load(&self) -> Result<(CatalogDocument, Option<ETag>), CatalogError> {
        let (document, etag) = self.load_snapshot().await?;
        Ok((document.as_ref().clone(), etag))
    }

    async fn load_snapshot(&self) -> Result<(Arc<CatalogDocument>, Option<ETag>), CatalogError> {
        let previous = self.latest.read().await.clone();
        if let Some(previous) = previous {
            // Every reuse needs a fresh provider token, or a warm reader could
            // retain revoked access. Cache locks never cover provider I/O.
            let current = match self.root.store.head(&self.path).await {
                Ok(current) => current,
                Err(StorageError::NotFound { .. }) => {
                    *self.latest.write().await = None;
                    return Ok((Arc::new(CatalogDocument::default()), None));
                }
                Err(error) => return Err(error.into()),
            };
            if current.e_tag == previous.etag.e_tag
                && current.version == previous.etag.version
                && current.size == previous.size
            {
                return Ok((previous.document, Some(previous.etag)));
            }
        }
        let (body, etag) = match self
            .root
            .store
            .get_with_etag_bounded(&self.path, MAX_CATALOG_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => {
                *self.latest.write().await = None;
                return Ok((Arc::new(CatalogDocument::default()), None));
            }
            Err(error) => return Err(error.into()),
        };
        let schema: CatalogSchema = serde_json::from_slice(&body)?;
        if schema.schema_version != SCHEMA_VERSION {
            return Err(CatalogError::Invalid("unsupported catalog schema"));
        }
        let document: CatalogDocument = serde_json::from_slice(&body)?;
        document.validate(&self.root)?;
        let document = Arc::new(document);
        let validated = (etag.e_tag.is_some() || etag.version.is_some()).then(|| CachedCatalog {
            document: document.clone(),
            etag: etag.clone(),
            size: body.len() as u64,
        });
        *self.latest.write().await = validated;
        Ok((document, Some(etag)))
    }

    pub async fn membership(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<MembershipSnapshot, CatalogError> {
        let (document, _) = self.load_snapshot().await?;
        let record = document
            .repositories
            .iter()
            .find(|record| {
                record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
            })
            .ok_or(CatalogError::NotFound)?;
        document.membership_snapshot(record)
    }
}
