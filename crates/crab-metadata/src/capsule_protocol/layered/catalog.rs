use super::*;
use crate::capsule_protocol::pointer::paged::{Descriptor, MAX_CATALOG_BYTES};

/// Authenticated page roots and identity of an immutable checkpoint catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCatalog {
    hash: String,
    bytes: u64,
    pages: Descriptor,
}

impl CheckpointCatalog {
    /// Return the content identity retained by checkpoints and captured readers.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the full encoded size used for read admission.
    #[must_use]
    pub const fn encoded_bytes(&self) -> u64 {
        self.bytes
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_content_hash(&self.hash, "checkpoint catalog", "layered checkpoint")?;
        if self.bytes == 0 || self.bytes > MAX_CATALOG_BYTES {
            return Err(corrupt("checkpoint catalog size is out of bounds"));
        }
        self.pages.validate(self.bytes)
    }

    #[cfg(feature = "storage")]
    fn verify(&self, bytes: &Bytes) -> Result<()> {
        self.validate()?;
        if bytes.len() as u64 != self.bytes
            || blake3::hash(bytes).to_hex().as_str() != self.hash
            || Descriptor::decode(bytes)? != self.pages
        {
            return Err(corrupt(
                "checkpoint catalog differs from its committed roots",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "storage")]
    async fn read(&self, layout: &crab_storage::StoreLayout<crab_storage::Store>) -> Result<Bytes> {
        let hash = blake3::Hash::from_hex(&self.hash)
            .map_err(|_| corrupt("checkpoint catalog has an invalid content identity"))?;
        let bytes = layout
            .store()
            .verify_bounded(
                &layout.capsule_catalog_path(&self.hash),
                hash.as_bytes(),
                self.bytes,
            )
            .await?;
        self.verify(&bytes)?;
        Ok(bytes)
    }

    #[cfg(feature = "storage")]
    pub(super) fn lookup(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        maximum: u64,
    ) -> Result<super::super::PointerCatalogLookup> {
        self.validate()?;
        super::super::PointerCatalogLookup::checkpoint(
            layout.capsule_catalog_path(&self.hash),
            PackRange::from_parts(0, self.bytes, self.hash.clone())?,
            self.pages.clone(),
            maximum,
        )
    }
}

pub(super) enum CatalogInput {
    New(PointerCatalog),
    Retained(Option<Box<CheckpointCatalog>>),
}

impl CatalogInput {
    pub(super) fn prepare(self) -> Result<(Option<CheckpointCatalog>, Option<Bytes>)> {
        let catalog = match self {
            Self::New(catalog) => catalog,
            Self::Retained(source) => {
                if let Some(source) = &source {
                    source.validate()?;
                }
                return Ok((source.map(|source| *source), None));
            }
        };
        if catalog.is_empty() {
            return Ok((None, None));
        }
        let bytes = catalog.encode()?;
        let source = CheckpointCatalog {
            hash: blake3::hash(&bytes).to_hex().to_string(),
            bytes: bytes.len() as u64,
            pages: Descriptor::decode(&bytes)?,
        };
        source.validate()?;
        Ok((Some(source), Some(bytes)))
    }
}

impl LayeredCheckpoint {
    /// Capture the authenticated checkpoint identity, including footer-only readers.
    pub fn pointer(&self) -> Result<super::super::CheckpointPointer> {
        super::super::CheckpointPointer::new_layered(
            self.hash(),
            self.object_size,
            self.control_offset(),
            self.control_size(),
            self.footer_hash(),
            self.covered_generation(),
            self.covered_root_digest(),
            self.pack_count()?,
            self.object_count()?,
        )
    }

    /// Return the aggregate encoded checkpoint and catalog size.
    pub fn encoded_metadata_bytes(&self) -> Result<u64> {
        self.object_size
            .checked_add(self.catalog().map_or(0, CheckpointCatalog::encoded_bytes))
            .ok_or_else(|| corrupt("checkpoint dependency size overflows"))
    }

    /// Return the catalog commitment without loading catalog pages.
    #[must_use]
    pub fn catalog(&self) -> Option<&CheckpointCatalog> {
        self.footer.catalog.as_ref()
    }

    /// Persist newly built catalog bytes before publishing their checkpoint.
    ///
    /// A retained commitment must come from a captured published checkpoint;
    /// the caller owns its GC fence until the replacement root is visible.
    #[cfg(feature = "storage")]
    pub async fn publish_catalog(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
    ) -> Result<()> {
        if let (Some(source), Some(bytes)) = (&self.footer.catalog, &self.catalog_bytes) {
            source.verify(bytes)?;
            layout
                .store()
                .put_if_absent_verified(&layout.capsule_catalog_path(source.hash()), bytes.clone())
                .await?;
        }
        Ok(())
    }

    /// Materialize only the catalog body using its captured origin and byte limit.
    #[cfg(feature = "storage")]
    pub async fn pointer_catalog_from_store(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        maximum_bytes: u64,
    ) -> Result<PointerCatalog> {
        let Some(source) = &self.footer.catalog else {
            return Ok(PointerCatalog::new());
        };
        if source.bytes > maximum_bytes {
            return Err(MetadataError::CheckpointSize {
                bytes: source.bytes,
                maximum: maximum_bytes,
            });
        }
        if self.catalog_bytes.is_some() {
            return self.pointer_catalog();
        }
        PointerCatalog::decode(&source.read(layout).await?)
    }

    #[cfg(feature = "storage")]
    fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let Some(source) = &self.footer.catalog else {
            return Ok(PointerCatalog::new());
        };
        let bytes = self
            .catalog_bytes
            .as_ref()
            .ok_or_else(|| corrupt("checkpoint catalog bytes have not been loaded"))?;
        source.verify(bytes)?;
        PointerCatalog::decode(bytes)
    }
}

#[cfg(all(test, feature = "storage"))]
mod tests {
    use super::*;
    use crate::capsule_protocol::{FileCatalogEntry, PackLayer, ShardCatalogEntry};
    use crab_storage::{Store, StoreLayout};
    use std::sync::Arc;

    fn fixture() -> (StoreLayout<Store>, LayeredCheckpoint, PointerCatalog) {
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "checkpoint-catalog".to_owned(),
        );
        let pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "1".repeat(40),
            1,
        )
        .unwrap();
        let source = PackLayer::build(&pack)
            .unwrap()
            .source_descriptor()
            .unwrap();
        let mut catalog = PointerCatalog::new();
        catalog
            .insert_shard("a".repeat(64), ShardCatalogEntry::new(1, Vec::new()))
            .unwrap();
        catalog
            .insert_file("b".repeat(64), FileCatalogEntry::new(1, "a".repeat(64)))
            .unwrap();
        let checkpoint =
            LayeredCheckpoint::build(1, &"c".repeat(64), vec![source], catalog.clone(), None)
                .unwrap();
        (layout, checkpoint, catalog)
    }

    #[tokio::test]
    async fn cold_checkpoint_load_admits_catalog_before_reading_and_verifies_its_body() {
        let (layout, checkpoint, catalog) = fixture();
        let pointer = checkpoint.pointer().unwrap();
        layout
            .store()
            .put_if_absent_verified(
                &layout.capsule_checkpoint_path(checkpoint.hash()),
                checkpoint.bytes().clone(),
            )
            .await
            .unwrap();
        // The asset is deliberately absent: a byte-budget error must precede
        // the otherwise observable missing dependency error.
        assert!(matches!(
            crate::capsule_protocol::load_layered_checkpoint(&layout, &pointer, pointer.size())
                .await,
            Err(MetadataError::CheckpointSize { .. })
        ));
        let cold = crate::capsule_protocol::load_layered_checkpoint(&layout, &pointer, u64::MAX)
            .await
            .unwrap();
        assert!(
            cold.pointer_catalog_from_store(&layout, u64::MAX)
                .await
                .is_err()
        );
        checkpoint.publish_catalog(&layout).await.unwrap();
        let loaded = crate::capsule_protocol::load_layered_checkpoint(&layout, &pointer, u64::MAX)
            .await
            .unwrap();
        assert_eq!(
            loaded
                .pointer_catalog_from_store(&layout, u64::MAX)
                .await
                .unwrap(),
            catalog
        );
        let path = layout.capsule_catalog_path(checkpoint.catalog().unwrap().hash());
        let mut bytes = catalog.encode().unwrap().to_vec();
        bytes[0] ^= 1;
        layout
            .store()
            .put_overwrite(&path, Bytes::from(bytes))
            .await
            .unwrap();
        assert!(
            loaded
                .pointer_catalog_from_store(&layout, u64::MAX)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn retained_catalog_survives_replacement_of_its_original_checkpoint() {
        let (layout, checkpoint, catalog) = fixture();
        checkpoint.publish_catalog(&layout).await.unwrap();
        let next = LayeredCheckpoint::build_reusing_catalog(
            2,
            &"d".repeat(64),
            checkpoint.sources().to_vec(),
            checkpoint.catalog().cloned(),
            None,
        )
        .unwrap();
        layout
            .store()
            .put_if_absent_verified(
                &layout.capsule_checkpoint_path(next.hash()),
                next.bytes().clone(),
            )
            .await
            .unwrap();
        let control = crate::capsule_protocol::load_layered_checkpoint_control(
            &layout,
            &next.pointer().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(control.pointer().unwrap(), next.pointer().unwrap());
        assert_eq!(
            control
                .pointer_catalog_from_store(&layout, u64::MAX)
                .await
                .unwrap(),
            catalog
        );
        // No original checkpoint was uploaded. The new commitment owns the
        // catalog directly, rather than borrowing that old object's lifetime.
        assert_eq!(control.catalog(), checkpoint.catalog());
    }
}
