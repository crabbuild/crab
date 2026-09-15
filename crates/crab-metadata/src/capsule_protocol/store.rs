use crab_storage::{ETag, Store, StoreLayout};

use crate::capsule_protocol::{CapsuleRun, Checkpoint, MAX_ROOT_BYTES, PointerCatalog, RootRecord};
use crate::error::MetadataError;
use crate::error::Result;

/// A verified stored root and the provider token protecting its next update.
#[derive(Debug, Clone)]
pub struct RootSnapshot {
    record: RootRecord,
    etag: ETag,
}

impl RootSnapshot {
    /// Return the immutable generation and ref state captured by this snapshot.
    #[must_use]
    pub fn record(&self) -> &RootRecord {
        &self.record
    }

    /// Return the opaque provider token required for a root CAS update.
    #[must_use]
    pub fn etag(&self) -> &ETag {
        &self.etag
    }

    /// Bind a successful root CAS result to its exact predecessor snapshot.
    pub fn committed_successor(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        let generation = self
            .record
            .root()
            .generation()
            .checked_add(1)
            .ok_or_else(|| contract_error("root generation overflowed"))?;
        if record.root().generation() != generation
            || record.root().parent_root_digest() != Some(self.record.digest())
        {
            return Err(contract_error(
                "committed root does not directly extend its CAS snapshot",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a checkpoint-only root replacement to its exact CAS predecessor.
    pub fn committed_checkpoint(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().head() != self.record.root().head()
            || !record.root().capsule_frontier().is_empty()
            || record.root().checkpoint().is_none()
        {
            return Err(contract_error(
                "committed checkpoint root does not replace its exact CAS snapshot",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a GC fence transition that preserves all logical repository state.
    pub fn committed_maintenance(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().head() != self.record.root().head()
            || record.root().checkpoint() != self.record.root().checkpoint()
            || record.root().capsule_frontier() != self.record.root().capsule_frontier()
        {
            return Err(contract_error(
                "committed maintenance root changed logical repository state",
            ));
        }
        Ok(Self { record, etag })
    }
}

/// Create the first root at an empty v2 publication key.
pub async fn create_root(router: &StoreLayout<Store>, record: RootRecord) -> Result<RootSnapshot> {
    if record.root().generation() != 0 {
        return Err(contract_error(
            "initial stored root must be generation zero",
        ));
    }
    let etag = router
        .store()
        .create_strict_with_etag(&router.capsule_root_path(), record.bytes().clone())
        .await?;
    Ok(RootSnapshot { record, etag })
}

/// Load and verify the single root used by readers and publication CAS.
pub async fn load_root(router: &StoreLayout<Store>) -> Result<RootSnapshot> {
    let (bytes, etag) = router
        .store()
        .get_with_etag_bounded(&router.capsule_root_path(), MAX_ROOT_BYTES)
        .await?;
    Ok(RootSnapshot {
        record: RootRecord::decode(bytes)?,
        etag,
    })
}

/// Load the complete authenticated pointer catalog named by one v2 root.
pub async fn load_pointer_catalog(router: &StoreLayout<Store>) -> Result<PointerCatalog> {
    let snapshot = load_root(router).await?;
    let root = snapshot.record().root();
    let mut catalog = if let Some(pointer) = root.checkpoint() {
        let path = router.capsule_checkpoint_path(pointer.hash());
        let (bytes, _) = router
            .store()
            .get_with_etag_bounded(&path, pointer.size())
            .await?;
        let checkpoint = Checkpoint::decode(bytes)?;
        let object_count = checkpoint
            .git_packs()
            .iter()
            .try_fold(0_u64, |total, pack| total.checked_add(pack.object_count()))
            .ok_or_else(|| corrupt(&path, "checkpoint object count overflowed"))?;
        if checkpoint.hash() != pointer.hash()
            || checkpoint.bytes().len() as u64 != pointer.size()
            || checkpoint.covered_generation() != pointer.covered_generation()
            || checkpoint.covered_root_digest() != pointer.covered_root_digest()
            || checkpoint.git_packs().len() as u32 != pointer.pack_count()
            || object_count != pointer.object_count()
        {
            return Err(corrupt(&path, "checkpoint does not match its root pointer"));
        }
        checkpoint.pointer_catalog()?
    } else {
        PointerCatalog::new()
    };
    for pointer in root.capsule_frontier() {
        let path = router.capsule_path(pointer.hash());
        let (bytes, _) = router
            .store()
            .get_with_etag_bounded(&path, pointer.size())
            .await?;
        let run = CapsuleRun::decode(bytes)?;
        if run.hash() != pointer.hash()
            || run.bytes().len() as u64 != pointer.size()
            || run.level() != pointer.level()
            || run.transaction_ids() != pointer.transaction_ids()
            || run.newest_base_root_digest() != pointer.newest_base_root_digest()
        {
            return Err(corrupt(
                &path,
                "capsule run does not match its root pointer",
            ));
        }
        for capsule in run.capsules() {
            if let Some(delta) = capsule.pointer_catalog_delta()? {
                catalog.apply(&delta)?;
            }
        }
    }
    Ok(catalog)
}

fn corrupt(path: &object_store::path::Path, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_string(),
        reason: reason.into(),
    }
}

fn contract_error(reason: impl Into<String>) -> crate::error::MetadataError {
    crate::error::MetadataError::CapsuleContract {
        record: "stored root",
        reason: reason.into(),
    }
}
