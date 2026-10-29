//! Immutable physical source closure for a captured reader's GC claim.

use std::collections::BTreeMap;

use bytes::Bytes;
use crab_storage::{Store, StoreLayout};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    CapsulePointer, CheckpointPointer, MAX_CAPSULE_FRONTIER, MAX_CAPSULE_REF_FRONTIER,
    MAX_CAPSULE_REF_HEADS, PointerCatalogLookup, RootRecord,
};
use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

const RUNS_PER_PAGE: usize = 64;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CAPTURE_BYTES: u64 = 4096;
const MAX_RUNS: usize = MAX_CAPSULE_REF_HEADS * MAX_CAPSULE_REF_FRONTIER + MAX_CAPSULE_FRONTIER;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectPointer {
    hash: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    version: u32,
    owner: String,
    repository_id: String,
    root_digest: String,
    checkpoint: Option<CheckpointPointer>,
    runs: usize,
    pages: usize,
    bytes: u64,
    first: Option<ObjectPointer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedRun {
    pointer: CapsulePointer,
    members: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page<T> {
    version: u32,
    runs: Vec<T>,
    next: Option<ObjectPointer>,
}

/// Store a captured checkpoint and exact immutable run inventory without loading payloads.
///
/// `runs` must come from the same validated view as `root`, including captured
/// ref heads and active transactions. `catalog` must be the lookup from that
/// same view, preserving its selected logical members and order. This closure retains physical sources;
/// it does not authorize Git visibility. The caller registers and validates
/// its renewing claim before using the capture. Returns descriptor hash and size.
pub async fn store_read_capture(
    layout: &StoreLayout<Store>,
    owner: &str,
    root: &RootRecord,
    runs: &[CapsulePointer],
    catalog: &PointerCatalogLookup,
) -> Result<(String, u64)> {
    validate_content_hash(owner, "read owner", "read capture")?;
    if runs.len() > MAX_RUNS {
        return Err(corrupt(
            "captured run inventory exceeds the ref-format bound",
        ));
    }
    let mut sorted = runs.iter().collect::<Vec<_>>();
    sorted.sort_unstable_by_key(|pointer| pointer.hash());
    if sorted
        .windows(2)
        .any(|pair| pair[0].hash() == pair[1].hash())
    {
        return Err(corrupt("captured run inventory repeats a source"));
    }
    let mut members = catalog.retained_members()?;
    let mut capture = Capture {
        version: 1,
        owner: owner.to_owned(),
        repository_id: root.root().repository_id().to_owned(),
        root_digest: root.digest().to_owned(),
        checkpoint: root.root().checkpoint().cloned(),
        runs: runs.len(),
        pages: 0,
        bytes: 0,
        first: None,
    };
    // Pages contain only bounded pointer records. Catalog pages and payloads
    // remain cold; a million mutable refs never become one unbounded claim.
    for chunk in sorted.rchunks(RUNS_PER_PAGE) {
        for pointer in chunk {
            super::root::validate_capsule_pointer(pointer)?;
        }
        let runs = chunk
            .iter()
            .map(|pointer| CapturedRun {
                pointer: (*pointer).clone(),
                members: members
                    .remove(&layout.capsule_path(pointer.hash()))
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        let page = Page {
            version: 1,
            runs,
            next: capture.first.take(),
        };
        let pointer = write_object(layout, owner, &page, MAX_PAGE_BYTES).await?;
        capture.pages += 1;
        capture.bytes = capture
            .bytes
            .checked_add(pointer.size)
            .ok_or_else(|| corrupt("capture byte accounting overflows"))?;
        capture.first = Some(pointer);
    }
    if !members.is_empty() {
        return Err(corrupt(
            "selected catalog source is absent from captured runs",
        ));
    }
    let pointer = write_object(layout, owner, &capture, MAX_CAPTURE_BYTES).await?;
    Ok((pointer.hash, pointer.size))
}

/// Bounded cursor over the immutable dependencies of one retained read capture.
///
/// The GC caller must consume through `None` before deleting anything; malformed
/// or missing pages invalidate the whole mark. No mutable ref heads are read.
pub struct RetainedReadCapture {
    layout: StoreLayout<Store>,
    capture: Capture,
    previous_hash: Option<String>,
}

impl RetainedReadCapture {
    /// Authenticate a descriptor against its private owner and physical repository.
    pub async fn open(
        layout: StoreLayout<Store>,
        owner: &str,
        repository_id: &str,
        hash: &str,
        size: u64,
    ) -> Result<Self> {
        validate_content_hash(owner, "read owner", "read capture")?;
        let capture: Capture = read_object(&layout, owner, hash, size, MAX_CAPTURE_BYTES).await?;
        validate_content_hash(&capture.root_digest, "captured root", "read capture")?;
        if capture.version != 1
            || capture.owner != owner
            || capture.repository_id != repository_id
            || capture.runs > MAX_RUNS
            || capture.pages != capture.runs.div_ceil(RUNS_PER_PAGE)
            || capture.bytes > capture.pages as u64 * MAX_PAGE_BYTES
            || (capture.pages == 0) != capture.first.is_none()
            || (capture.pages == 0) != (capture.bytes == 0)
        {
            return Err(corrupt("capture identity or resource accounting differs"));
        }
        if let Some(checkpoint) = &capture.checkpoint {
            super::root::validate_checkpoint_pointer(checkpoint)?;
        }
        Ok(Self {
            layout,
            capture,
            previous_hash: None,
        })
    }

    /// Return the exact checkpoint from which this read view was built.
    #[must_use]
    pub fn checkpoint(&self) -> Option<&CheckpointPointer> {
        self.capture.checkpoint.as_ref()
    }

    /// Read the next bounded, authenticated batch of source pointers.
    pub async fn next_runs(&mut self) -> Result<Option<Vec<CapsulePointer>>> {
        Ok(self
            .next_page()
            .await?
            .map(|runs| runs.into_iter().map(|run| run.pointer).collect()))
    }

    /// Authenticate selected catalog dependencies and return their complete shard roots.
    ///
    /// This replays immutable selected members, never current ref heads. Physical
    /// run order cannot choose file precedence: ranks come from the captured view.
    pub async fn into_shard_hashes(mut self) -> Result<Vec<String>> {
        use super::pointer::inspection::Inspector;

        let maximum = super::MAX_CATALOG_BYTES;
        let mut inspector = Inspector::new(maximum);
        let checked = async {
            if let Some(pointer) = self.checkpoint() {
                let checkpoint =
                    super::load_layered_checkpoint_control(&self.layout, pointer).await?;
                if let Some(catalog) = checkpoint.catalog() {
                    catalog
                        .reader(&self.layout)?
                        .inspect_into(self.layout.store(), maximum, &mut inspector)
                        .await?;
                }
            }
            while let Some(runs) = self.next_page().await? {
                for mut run in runs {
                    if run.members.is_empty() {
                        continue;
                    }
                    let (_, controls) =
                        super::load_capsule_run_git_control(&self.layout, &run.pointer).await?;
                    for control in controls {
                        let Some(rank) = run.members.remove(control.hash()) else {
                            continue;
                        };
                        let delta = control
                            .materialize_catalog(self.layout.store())
                            .await?
                            .ok_or_else(|| corrupt("selected member has no catalog"))?;
                        inspector.delta(delta, rank).await?;
                    }
                    if !run.members.is_empty() {
                        return Err(corrupt("selected catalog member is absent from its run"));
                    }
                }
            }
            Ok(())
        }
        .await;
        // The shared inspector owns spills on success, failure and abandonment.
        // Complete dependency proof precedes exposing any root to a GC planner.
        inspector.finish(checked).await?.into_shard_hashes().await
    }

    async fn next_page(&mut self) -> Result<Option<Vec<CapturedRun>>> {
        let Some(pointer) = &self.capture.first else {
            if self.capture.runs != 0 || self.capture.pages != 0 || self.capture.bytes != 0 {
                return Err(corrupt("capture ended before its declared inventory"));
            }
            return Ok(None);
        };
        let page: Page<CapturedRun> = read_object(
            &self.layout,
            &self.capture.owner,
            &pointer.hash,
            pointer.size,
            MAX_PAGE_BYTES,
        )
        .await?;
        if page.version != 1
            || page.runs.is_empty()
            || page.runs.len() > RUNS_PER_PAGE
            || page.runs.len() > self.capture.runs
            || self.capture.pages == 0
            || pointer.size > self.capture.bytes
        {
            return Err(corrupt("capture page exceeds its declared inventory"));
        }
        let mut previous = self.previous_hash.as_deref();
        for run in &page.runs {
            super::root::validate_capsule_pointer(&run.pointer)?;
            if run.members.len() > super::MAX_CAPSULES_PER_RUN {
                return Err(corrupt("selected catalog members exceed the run bound"));
            }
            for (hash, rank) in &run.members {
                validate_content_hash(hash, "catalog member", "read capture")?;
                if *rank == 0 || *rank > MAX_RUNS as u64 * super::MAX_CAPSULES_PER_RUN as u64 {
                    return Err(corrupt("selected catalog rank exceeds the capture bound"));
                }
            }
            if previous.is_some_and(|hash| hash >= run.pointer.hash()) {
                return Err(corrupt("captured sources are not strictly ordered"));
            }
            previous = Some(run.pointer.hash());
        }
        // Advance only after I/O and every validation succeed. Cancellation
        // leaves this page available and cannot skip part of the retained root.
        self.previous_hash = previous.map(str::to_owned);
        self.capture.runs -= page.runs.len();
        self.capture.pages -= 1;
        self.capture.bytes -= pointer.size;
        self.capture.first = page.next;
        Ok(Some(page.runs))
    }
}

async fn write_object(
    layout: &StoreLayout<Store>,
    owner: &str,
    value: &impl Serialize,
    maximum: u64,
) -> Result<ObjectPointer> {
    let bytes = serde_json::to_vec(value).map_err(record_error)?;
    if bytes.len() as u64 > maximum {
        return Err(corrupt("capture object exceeds its byte bound"));
    }
    let pointer = ObjectPointer {
        hash: blake3::hash(&bytes).to_hex().to_string(),
        size: bytes.len() as u64,
    };
    layout
        .store()
        .put_if_absent_verified(
            &layout.read_retention_object_path(owner, &pointer.hash),
            Bytes::from(bytes),
        )
        .await?;
    Ok(pointer)
}

async fn read_object<T: DeserializeOwned>(
    layout: &StoreLayout<Store>,
    owner: &str,
    hash: &str,
    size: u64,
    maximum: u64,
) -> Result<T> {
    validate_content_hash(hash, "capture object", "read capture")?;
    if size == 0 || size > maximum {
        return Err(corrupt("capture object exceeds its byte bound"));
    }
    let expected =
        blake3::Hash::from_hex(hash).map_err(|_| corrupt("invalid capture object identity"))?;
    let bytes = layout
        .store()
        .verify_bounded(
            &layout.read_retention_object_path(owner, hash),
            expected.as_bytes(),
            size,
        )
        .await?;
    if bytes.len() as u64 != size {
        return Err(corrupt("capture object length differs"));
    }
    serde_json::from_slice(&bytes).map_err(record_error)
}

fn record_error(source: serde_json::Error) -> MetadataError {
    MetadataError::SourceRetentionRecord { source }
}

fn corrupt(reason: &str) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol read capture".to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture() -> (StoreLayout<Store>, String, RootRecord) {
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repo".to_owned(),
        );
        let root = RootRecord::encode(
            super::super::RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        (layout, "2".repeat(64), root)
    }

    fn pointer(index: usize) -> CapsulePointer {
        CapsulePointer::new(
            format!("{index:064x}"),
            20,
            10,
            10,
            "3".repeat(64),
            0,
            vec![format!("{index:064x}")],
            "4".repeat(64),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn capture_pages_preserve_exact_sources_without_reading_payloads() {
        let (layout, owner, root) = fixture();
        for count in [
            0,
            1,
            RUNS_PER_PAGE,
            RUNS_PER_PAGE + 1,
            RUNS_PER_PAGE * 2 + 7,
        ] {
            let runs = (1..=count).rev().map(pointer).collect::<Vec<_>>();
            let (hash, size) = store_read_capture(
                &layout,
                &owner,
                &root,
                &runs,
                &PointerCatalogLookup::empty(super::super::MAX_CATALOG_BYTES),
            )
            .await
            .unwrap();
            let mut capture = RetainedReadCapture::open(
                layout.clone(),
                &owner,
                root.root().repository_id(),
                &hash,
                size,
            )
            .await
            .unwrap();
            let mut observed = Vec::new();
            while let Some(runs) = capture.next_runs().await.unwrap() {
                observed.extend(runs);
            }
            assert_eq!(observed, (1..=count).map(pointer).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn retained_capture_rejects_cross_repository_identity_and_missing_pages() {
        let (layout, owner, root) = fixture();
        let (hash, size) = store_read_capture(
            &layout,
            &owner,
            &root,
            &[pointer(1)],
            &PointerCatalogLookup::empty(super::super::MAX_CATALOG_BYTES),
        )
        .await
        .unwrap();
        assert!(
            RetainedReadCapture::open(layout.clone(), &owner, &"5".repeat(64), &hash, size)
                .await
                .is_err()
        );
        let mut capture = RetainedReadCapture::open(
            layout.clone(),
            &owner,
            root.root().repository_id(),
            &hash,
            size,
        )
        .await
        .unwrap();
        let page = capture.capture.first.as_ref().unwrap().hash.clone();
        layout
            .store()
            .delete(&layout.read_retention_object_path(&owner, &page))
            .await
            .unwrap();
        assert!(capture.next_runs().await.is_err());
    }
    fn catalog_capsule(index: usize, shard: &str, include_shard: bool) -> super::super::Capsule {
        use super::super::{
            Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
            FileCatalogEntry, PointerCatalog, ShardCatalogEntry,
        };
        let mut catalog = PointerCatalog::new();
        catalog
            .insert_file("a".repeat(64), FileCatalogEntry::new(7, shard))
            .unwrap();
        if include_shard {
            catalog
                .insert_shard(shard, ShardCatalogEntry::new(8, Vec::new()))
                .unwrap();
        }
        let transaction = CapsuleTransaction::new(
            &"b".repeat(64),
            vec![CapsuleRefEdit::new(
                format!("refs/tags/{index}"),
                None,
                Some("c".repeat(40)),
                None,
            )],
        )
        .unwrap();
        Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            )],
        )
        .unwrap()
    }

    async fn save_run(
        layout: &StoreLayout<Store>,
        run: &super::super::CapsuleRun,
    ) -> CapsulePointer {
        layout
            .store()
            .put_if_absent_verified(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap()
    }

    async fn open_capture(
        layout: &StoreLayout<Store>,
        owner: &str,
        root: &RootRecord,
        runs: &[CapsulePointer],
        catalog: &PointerCatalogLookup,
    ) -> RetainedReadCapture {
        let (hash, size) = store_read_capture(layout, owner, root, runs, catalog)
            .await
            .unwrap();
        RetainedReadCapture::open(
            layout.clone(),
            owner,
            root.root().repository_id(),
            &hash,
            size,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn retained_catalog_excludes_unselected_members_of_a_compacted_run() {
        use super::super::CapsuleRun;
        let (layout, owner, root) = fixture();
        let old = catalog_capsule(0, &"d".repeat(64), true);
        let selected = catalog_capsule(1, &"e".repeat(64), true);
        let order = vec![selected.hash().to_owned()];
        let run = CapsuleRun::compact(vec![
            CapsuleRun::leaf(old).unwrap(),
            CapsuleRun::leaf(selected).unwrap(),
        ])
        .unwrap();
        let pointer = save_run(&layout, &run).await;
        let catalog = PointerCatalogLookup::empty(super::super::MAX_CATALOG_BYTES)
            .with_run_sources(vec![run.catalog_source(&layout).unwrap().unwrap()], &order)
            .unwrap();
        let capture = open_capture(&layout, &owner, &root, &[pointer], &catalog).await;
        assert_eq!(
            capture.into_shard_hashes().await.unwrap(),
            vec!["e".repeat(64)]
        );
    }

    #[tokio::test]
    async fn retained_catalog_uses_captured_precedence_and_requires_complete_selected_dependencies()
    {
        use super::super::CapsuleRun;
        let (layout, owner, root) = fixture();
        let old = CapsuleRun::leaf(catalog_capsule(0, &"d".repeat(64), false)).unwrap();
        let new = CapsuleRun::leaf(catalog_capsule(1, &"e".repeat(64), true)).unwrap();
        let pointers = vec![save_run(&layout, &new).await, save_run(&layout, &old).await];
        let sources = vec![
            old.catalog_source(&layout).unwrap().unwrap(),
            new.catalog_source(&layout).unwrap().unwrap(),
        ];
        for latest_is_complete in [true, false] {
            let mut order = vec![
                old.capsules()[0].hash().to_owned(),
                new.capsules()[0].hash().to_owned(),
            ];
            if !latest_is_complete {
                order.reverse();
            }
            let catalog = PointerCatalogLookup::empty(super::super::MAX_CATALOG_BYTES)
                .with_run_sources(sources.clone(), &order)
                .unwrap();
            let capture = open_capture(&layout, &owner, &root, &pointers, &catalog).await;
            let result = capture.into_shard_hashes().await;
            if latest_is_complete {
                assert_eq!(result.unwrap(), vec!["e".repeat(64)]);
            } else {
                assert!(
                    result.is_err(),
                    "a selected missing dependency became an empty root"
                );
            }
        }
    }

    #[tokio::test]
    async fn capture_rejects_a_catalog_source_outside_its_physical_inventory() {
        use super::super::CapsuleRun;
        let (layout, owner, root) = fixture();
        let run = CapsuleRun::leaf(catalog_capsule(0, &"d".repeat(64), true)).unwrap();
        let catalog = PointerCatalogLookup::empty(super::super::MAX_CATALOG_BYTES)
            .with_run_sources(
                vec![run.catalog_source(&layout).unwrap().unwrap()],
                &[run.capsules()[0].hash().to_owned()],
            )
            .unwrap();
        assert!(
            store_read_capture(&layout, &owner, &root, &[], &catalog)
                .await
                .is_err()
        );
    }
}
