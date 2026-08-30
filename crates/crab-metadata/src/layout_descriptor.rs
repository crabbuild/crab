//! Authoritative canonical-v1 repository layout descriptor.

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};

pub const LAYOUT_DESCRIPTOR_SCHEMA_VERSION: u32 = 1;
pub const CANONICAL_PARTITION_BITS: u8 = 8;
pub const MIN_PARTITION_BITS: u8 = 4;
pub const MAX_PARTITION_BITS: u8 = 12;
pub const CANONICAL_RECIPE_PAGE_ENTRIES: u32 = 512;
pub const CANONICAL_RECIPE_PAGE_MAX_BYTES: u32 = 64 * 1024;
pub const MAX_LAYOUT_DESCRIPTOR_BYTES: u64 = 64 * 1024;

/// The sole repository layout admitted by canonical v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteLayout {
    Partitioned,
}

/// Versioned repository routing and bounded-recipe contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutDescriptor {
    pub schema_version: u32,
    pub layout: RemoteLayout,
    pub chunk_partition_bits: u8,
    pub file_partition_bits: u8,
    pub receipt_partition_bits: u8,
    pub recipe_page_entries: u32,
    pub recipe_page_max_bytes: u32,
    pub digest: String,
}

impl LayoutDescriptor {
    /// Returns the one descriptor accepted by canonical v1 readers and writers.
    #[must_use]
    pub fn canonical() -> Self {
        let mut descriptor = Self {
            schema_version: LAYOUT_DESCRIPTOR_SCHEMA_VERSION,
            layout: RemoteLayout::Partitioned,
            chunk_partition_bits: CANONICAL_PARTITION_BITS,
            file_partition_bits: CANONICAL_PARTITION_BITS,
            receipt_partition_bits: CANONICAL_PARTITION_BITS,
            recipe_page_entries: CANONICAL_RECIPE_PAGE_ENTRIES,
            recipe_page_max_bytes: CANONICAL_RECIPE_PAGE_MAX_BYTES,
            digest: String::new(),
        };
        descriptor.digest = descriptor.expected_digest();
        descriptor
    }

    /// Validates the strict v1 contract and its content digest.
    pub fn validate(&self, path: &str) -> Result<()> {
        if self.schema_version != LAYOUT_DESCRIPTOR_SCHEMA_VERSION {
            return Err(corrupt(
                path,
                format!(
                    "layout schema {} is not canonical v1; reset this isolated development repository",
                    self.schema_version
                ),
            ));
        }
        for (name, bits) in [
            ("chunk_partition_bits", self.chunk_partition_bits),
            ("file_partition_bits", self.file_partition_bits),
            ("receipt_partition_bits", self.receipt_partition_bits),
        ] {
            if !(MIN_PARTITION_BITS..=MAX_PARTITION_BITS).contains(&bits) {
                return Err(corrupt(
                    path,
                    format!("{name}={bits} is outside {MIN_PARTITION_BITS}..={MAX_PARTITION_BITS}"),
                ));
            }
        }
        let canonical = Self::canonical();
        if self.layout != canonical.layout
            || self.chunk_partition_bits != canonical.chunk_partition_bits
            || self.file_partition_bits != canonical.file_partition_bits
            || self.receipt_partition_bits != canonical.receipt_partition_bits
            || self.recipe_page_entries != canonical.recipe_page_entries
            || self.recipe_page_max_bytes != canonical.recipe_page_max_bytes
        {
            return Err(corrupt(
                path,
                "layout parameters are not the canonical v1 contract; reset this isolated development repository",
            ));
        }
        let expected = self.expected_digest();
        if self.digest != expected {
            return Err(corrupt(
                path,
                format!(
                    "layout digest {} does not match canonical content digest {expected}",
                    self.digest
                ),
            ));
        }
        Ok(())
    }

    /// Serializes the stable canonical writer shape after validation.
    pub fn to_canonical_bytes(&self, path: &str) -> Result<Vec<u8>> {
        self.validate(path)?;
        serde_json::to_vec(self).map_err(|error| {
            MetadataError::Internal(format!("serialize canonical layout descriptor: {error}"))
        })
    }

    /// Parses one strict canonical-v1 descriptor.
    pub fn parse(path: &str, bytes: &[u8]) -> Result<Self> {
        let descriptor: Self = serde_json::from_slice(bytes).map_err(|error| {
            corrupt(
                path,
                format!(
                    "invalid canonical v1 layout descriptor: {error}; reset this isolated development repository"
                ),
            )
        })?;
        descriptor.validate(path)?;
        Ok(descriptor)
    }

    fn expected_digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab repository layout v1\0");
        hasher.update(&self.schema_version.to_le_bytes());
        hasher.update(match self.layout {
            RemoteLayout::Partitioned => b"partitioned\0",
        });
        hasher.update(&[
            self.chunk_partition_bits,
            self.file_partition_bits,
            self.receipt_partition_bits,
        ]);
        hasher.update(&self.recipe_page_entries.to_le_bytes());
        hasher.update(&self.recipe_page_max_bytes.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

fn corrupt(path: &str, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(feature = "storage")]
pub async fn read_canonical_layout(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<LayoutDescriptor> {
    use crab_storage::StorageError;

    let path = router.layout_descriptor_path();
    let bytes = match store
        .get_with_etag_bounded(&path, MAX_LAYOUT_DESCRIPTOR_BYTES)
        .await
    {
        Ok((bytes, _)) => bytes,
        Err(StorageError::NotFound { .. }) => {
            return Err(corrupt(
                path.as_ref(),
                "canonical v1 layout descriptor is missing; reset this isolated development repository",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    LayoutDescriptor::parse(path.as_ref(), &bytes)
}

#[cfg(feature = "storage")]
pub async fn ensure_canonical_layout(
    store: &crab_storage::Store,
    router: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<LayoutDescriptor> {
    use bytes::Bytes;
    use crab_storage::StorageError;

    let path = router.layout_descriptor_path();
    let canonical = LayoutDescriptor::canonical();
    let bytes = canonical.to_canonical_bytes(path.as_ref())?;
    match store.create_strict(&path, Bytes::from(bytes)).await {
        Ok(()) => {}
        Err(StorageError::StateConflict { .. }) => {
            let existing = read_canonical_layout(store, router).await?;
            if existing != canonical {
                return Err(corrupt(
                    path.as_ref(),
                    "repository layout conflicts with the canonical v1 descriptor",
                ));
            }
            return Ok(existing);
        }
        Err(error) => return Err(error.into()),
    }
    let persisted = read_canonical_layout(store, router).await?;
    if persisted != canonical {
        return Err(corrupt(
            path.as_ref(),
            "provider read-back differs from the canonical v1 descriptor",
        ));
    }
    Ok(persisted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn canonical_descriptor_roundtrips_stably() {
        let descriptor = LayoutDescriptor::canonical();
        let bytes = descriptor.to_canonical_bytes("repo/layout").unwrap();
        let parsed = LayoutDescriptor::parse("repo/layout", &bytes).unwrap();

        assert_eq!(parsed, descriptor);
        assert_eq!(parsed.to_canonical_bytes("repo/layout").unwrap(), bytes);
    }

    #[test]
    fn non_v1_unknown_fields_and_corrupt_digest_fail_closed() {
        let canonical = LayoutDescriptor::canonical();
        let mut non_v1 = serde_json::to_value(&canonical).unwrap();
        non_v1["schema_version"] = serde_json::json!(2);
        assert!(
            LayoutDescriptor::parse("repo/layout", &serde_json::to_vec(&non_v1).unwrap()).is_err()
        );

        let mut unknown = serde_json::to_value(&canonical).unwrap();
        unknown["legacy_layout"] = serde_json::json!(true);
        assert!(
            LayoutDescriptor::parse("repo/layout", &serde_json::to_vec(&unknown).unwrap()).is_err()
        );

        let mut corrupt = canonical;
        corrupt.digest = "0".repeat(64);
        assert!(
            LayoutDescriptor::parse("repo/layout", &serde_json::to_vec(&corrupt).unwrap()).is_err()
        );
    }

    #[test]
    fn alternate_parameters_are_not_a_second_v1_path() {
        let mut descriptor = LayoutDescriptor::canonical();
        descriptor.chunk_partition_bits = 9;
        descriptor.digest = descriptor.expected_digest();

        assert!(descriptor.validate("repo/layout").is_err());
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn ensure_is_idempotent_and_missing_read_creates_nothing() {
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = crab_storage::Store::new(inner);
        let router = crab_storage::StoreLayout::new(store.clone(), "repo".to_owned());

        assert!(read_canonical_layout(&store, &router).await.is_err());
        assert!(
            store
                .list_prefix(&router.repo_path(""))
                .await
                .unwrap()
                .is_empty()
        );

        let first = ensure_canonical_layout(&store, &router).await.unwrap();
        let second = ensure_canonical_layout(&store, &router).await.unwrap();

        assert_eq!(first, LayoutDescriptor::canonical());
        assert_eq!(second, first);
        assert_eq!(
            store
                .list_prefix(&router.repo_path(""))
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
