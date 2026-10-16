use super::*;
#[cfg(feature = "storage")]
use crate::capsule_protocol::pointer::object::CatalogPages;
use crate::capsule_protocol::pointer::object::{Descriptor, MAX_DESCRIPTOR_BYTES};
use std::collections::BTreeMap;

/// Authenticated roots and identity of an immutable object-paged checkpoint catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCatalog {
    hash: String,
    bytes: u64,
    pages: Descriptor,
}

impl CheckpointCatalog {
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the descriptor byte length admitted during repository opening.
    #[must_use]
    pub const fn encoded_bytes(&self) -> u64 {
        self.bytes
    }

    fn new(pages: Descriptor) -> Result<Self> {
        let body = pages.encode()?;
        Ok(Self {
            hash: blake3::hash(&body).to_hex().to_string(),
            bytes: body.len() as u64,
            pages,
        })
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_content_hash(&self.hash, "checkpoint catalog", "layered checkpoint")?;
        let body = self.pages.encode()?;
        if self.bytes == 0
            || self.bytes > MAX_DESCRIPTOR_BYTES
            || body.len() as u64 != self.bytes
            || blake3::hash(&body).to_hex().as_str() != self.hash
        {
            return Err(corrupt(
                "checkpoint catalog descriptor differs from its commitment",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "storage")]
    fn reader(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
    ) -> Result<CatalogPages> {
        self.validate()?;
        let identity = super::super::pointer::hash(&self.hash)?;
        CatalogPages::new(
            layout.capsule_catalog_prefix(),
            self.pages.clone(),
            identity,
            self.bytes,
        )
    }

    #[cfg(feature = "storage")]
    pub(super) fn lookup(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        maximum: u64,
    ) -> Result<super::super::PointerCatalogLookup> {
        Ok(super::super::PointerCatalogLookup::checkpoint(
            std::sync::Arc::new(self.reader(layout)?),
            maximum,
        ))
    }
}

pub(super) enum CatalogInput {
    New(PointerCatalog),
    Durable(Option<Box<CheckpointCatalog>>),
}

pub(super) struct PreparedCatalog {
    pub(super) source: Option<CheckpointCatalog>,
    pub(super) objects: BTreeMap<[u8; 32], Bytes>,
}

impl CatalogInput {
    pub(super) fn prepare(self) -> Result<PreparedCatalog> {
        let catalog = match self {
            Self::New(catalog) => catalog,
            Self::Durable(source) => {
                if let Some(source) = &source {
                    source.validate()?;
                }
                return Ok(PreparedCatalog {
                    source: source.map(|source| *source),
                    objects: BTreeMap::new(),
                });
            }
        };
        if catalog.is_empty() {
            return Ok(PreparedCatalog {
                source: None,
                objects: BTreeMap::new(),
            });
        }
        let (pages, objects) = Descriptor::build(&catalog)?;
        Ok(PreparedCatalog {
            source: Some(CheckpointCatalog::new(pages)?),
            objects,
        })
    }
}

#[cfg(feature = "storage")]
async fn publish_objects(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    objects: &BTreeMap<[u8; 32], Bytes>,
) -> Result<()> {
    for (identity, body) in objects {
        if blake3::hash(body).as_bytes() != identity {
            return Err(corrupt("pending catalog object identity differs"));
        }
        let identity = blake3::Hash::from_bytes(*identity).to_hex().to_string();
        layout
            .store()
            .put_if_absent_verified(&layout.capsule_catalog_path(&identity), body.clone())
            .await?;
    }
    Ok(())
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

    /// Return the checkpoint and root-descriptor bytes admitted when opening a view.
    pub fn encoded_metadata_bytes(&self) -> Result<u64> {
        self.object_size
            .checked_add(self.catalog().map_or(0, CheckpointCatalog::encoded_bytes))
            .ok_or_else(|| corrupt("checkpoint dependency size overflows"))
    }

    /// Return all locally prepared immutable bytes before publication admission.
    pub fn publication_metadata_bytes(&self) -> Result<u64> {
        if self.catalog_objects.is_empty() {
            return self.encoded_metadata_bytes();
        }
        self.catalog_objects
            .values()
            .try_fold(self.object_size, |bytes, body| {
                bytes
                    .checked_add(body.len() as u64)
                    .ok_or_else(|| corrupt("prepared catalog byte count overflows"))
            })
    }

    #[must_use]
    pub fn catalog(&self) -> Option<&CheckpointCatalog> {
        self.footer.catalog.as_ref()
    }

    /// Verify and persist every newly prepared catalog object before its checkpoint.
    ///
    /// Retained roots must already be durable; the caller retains its GC fence
    /// until the replacement root becomes visible.
    #[cfg(feature = "storage")]
    pub async fn publish_catalog(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
    ) -> Result<()> {
        if let Some(source) = &self.footer.catalog
            && !self.catalog_objects.is_empty()
        {
            let objects = &self.catalog_objects;
            source.validate()?;
            let identity = super::super::pointer::hash(&source.hash)?;
            let body = objects
                .get(&identity)
                .ok_or_else(|| corrupt("pending catalog has no root descriptor"))?;
            if Descriptor::decode(body)? != source.pages {
                return Err(corrupt("pending catalog roots differ"));
            }
            publish_objects(layout, objects).await?;
        }
        Ok(())
    }

    /// Materialize the catalog from its authenticated origin with one aggregate page budget.
    #[cfg(feature = "storage")]
    pub async fn pointer_catalog_from_store(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        maximum_bytes: u64,
    ) -> Result<PointerCatalog> {
        let Some(source) = self.catalog() else {
            return Ok(PointerCatalog::new());
        };
        source
            .reader(layout)?
            .full(
                layout.store(),
                &mut crate::pages::PageWork::new(maximum_bytes, 0),
            )
            .await
    }

    /// Authenticate catalog objects and their encoded sizes for maintenance.
    #[cfg(feature = "storage")]
    pub async fn catalog_objects(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        maximum_bytes: u64,
    ) -> Result<Vec<(object_store::path::Path, u64)>> {
        let Some(source) = self.catalog() else {
            return Ok(Vec::new());
        };
        source
            .reader(layout)?
            .inspected_objects(layout.store(), maximum_bytes)
            .await
    }
}

/// Fold selected ordered deltas into a durable catalog without scanning unrelated pages.
///
/// The caller must retain its captured root and hold its publication GC fence.
/// New immutable pages and the descriptor are verified before this returns.
#[cfg(feature = "storage")]
pub async fn fold_checkpoint_catalog(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    existing: Option<&CheckpointCatalog>,
    deltas: &[PointerCatalog],
    maximum_bytes: u64,
) -> Result<Option<CheckpointCatalog>> {
    if deltas.iter().all(PointerCatalog::is_empty) {
        return Ok(existing.cloned());
    }
    let Some(existing) = existing else {
        super::super::pointer::admit_catalog_rows(deltas, maximum_bytes)?;
        let mut assembly = super::super::PointerCatalogAssembly::new(PointerCatalog::new());
        for delta in deltas {
            assembly.apply(delta)?;
        }
        let prepared = CatalogInput::New(assembly.finish()?).prepare()?;
        if !prepared.objects.is_empty() {
            let objects = &prepared.objects;
            let bytes = objects.values().try_fold(0u64, |bytes, body| {
                bytes
                    .checked_add(body.len() as u64)
                    .ok_or_else(|| corrupt("pending catalog byte count overflows"))
            })?;
            if bytes > maximum_bytes {
                return Err(MetadataError::CheckpointSize {
                    bytes,
                    maximum: maximum_bytes,
                });
            }
            publish_objects(layout, objects).await?;
        }
        return Ok(prepared.source);
    };
    let mut work = crate::pages::PageWork::new(maximum_bytes, 0);
    let descriptor = existing
        .reader(layout)?
        .apply(layout.store(), deltas, &mut work)
        .await?;
    if descriptor == existing.pages {
        return Ok(Some(existing.clone()));
    }
    let source = CheckpointCatalog::new(descriptor)?;
    let body = source.pages.encode()?;
    work.admit_object(*blake3::hash(&body).as_bytes(), body.len() as u64)?;
    layout
        .store()
        .put_if_absent_verified(&layout.capsule_catalog_path(&source.hash), body)
        .await?;
    Ok(Some(source))
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
        let next = LayeredCheckpoint::build_with_catalog_root(
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
    #[tokio::test]
    async fn catalog_inspection_reports_every_physical_object_once() {
        let (layout, checkpoint, _) = fixture();
        checkpoint.publish_catalog(&layout).await.unwrap();
        let inspected = checkpoint
            .catalog_objects(&layout, 1024 * 1024)
            .await
            .unwrap();
        let stored = layout
            .store()
            .list_prefix(&layout.capsule_catalog_prefix())
            .await
            .unwrap();
        assert_eq!(
            inspected.into_iter().collect::<BTreeMap<_, _>>(),
            stored
                .into_iter()
                .map(|meta| (meta.location, meta.size))
                .collect(),
        );
    }
}
