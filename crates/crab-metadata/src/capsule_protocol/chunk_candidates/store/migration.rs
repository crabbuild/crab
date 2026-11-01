//! Explicit development-format migration, never a runtime fallback reader.
use super::*;
use crate::pages::object::{ObjectPages, Reference};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PagedRoot {
    version: u32,
    records: Reference,
}

/// Convert paged development hints while preserving every exact placement ID.
///
/// The caller holds global GC writer admission through verified run publication
/// and the root CAS. Cancellation or CAS failure never advertises partial hints.
pub async fn migrate_capsule_chunk_candidates(
    layout: &StoreLayout<Store>,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<bool> {
    let index = CapsuleChunkIndex::new(layout);
    let mut work = PageWork::new(crate::capsule_protocol::MAX_CATALOG_BYTES, 0);
    let pages = ObjectPages::new(index.prefix.clone());
    for attempt in 0..WRITE_ATTEMPTS {
        if cancelled() {
            return Err(MetadataError::CapsuleHintCancelled);
        }
        let (body, etag) = match layout
            .store()
            .get_with_etag_bounded(&index.root, ROOT_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        work.admit_object(*blake3::hash(&body).as_bytes(), body.len() as u64)?;
        let version: Version = serde_json::from_slice(&body)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if version.version == 2 {
            Root::decode(&body)?;
            return Ok(false);
        }
        let old: PagedRoot = serde_json::from_slice(&body)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if old.version != 1 || body.len() > 4096 {
            return Err(corrupt(
                "capsule hint migration source version is unsupported",
            ));
        }
        let mut rows = BTreeMap::new();
        for (key, value) in pages
            .scan(layout.store(), Some(&old.records), KIND, &mut work)
            .await?
        {
            validate_row(&key, &value)?;
            let mut encoded = 1_u64.to_le_bytes().to_vec();
            encoded.extend_from_slice(&value);
            rows.insert(key, encoded);
        }
        let (run, body) = Run::build(&rows)?;
        work.admit_object(run.hash, run.bytes)?;
        if cancelled() {
            return Err(MetadataError::CapsuleHintCancelled);
        }
        layout
            .store()
            .put_if_absent_verified(&run.path(&index.prefix), body)
            .await?;
        let mut levels = vec![None; run.level() + 1];
        let level = run.level();
        levels[level] = Some(run);
        let body = Root {
            version: 2,
            sequence: 1,
            levels,
        }
        .encode()?;
        if cancelled() {
            return Err(MetadataError::CapsuleHintCancelled);
        }
        match layout.store().update(&index.root, body, etag).await {
            Ok(_) => return Ok(true),
            Err(StorageError::StateConflict { .. }) if attempt + 1 < WRITE_ATTEMPTS => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(StorageError::StateConflict {
        path: index.root.to_string(),
    }
    .into())
}
