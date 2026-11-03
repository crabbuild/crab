//! One shared authenticated control capture for current-state catalog queries.
use super::pointer::paged::MAX_CATALOG_BYTES;
use super::{
    CapsuleControl, CapturedVisibility, LayeredCheckpoint, PointerCatalog, PointerCatalogAssembly,
    PointerCatalogInspection, PointerCatalogLookup, RootSnapshot, RunCatalogSource,
    VisibilityReconciliation, apply_control_refs, capture_ref_heads, load_layered_checkpoint,
    load_layered_checkpoint_control, materialize_visible_ref_heads, order_ref_controls,
    select_ref_control_frontiers, select_ref_journal_controls,
};
use crate::error::{MetadataError, Result};
use crate::git_visibility::GitVisibilityIndex;
use crab_storage::{Store, StoreLayout};
use futures_util::{StreamExt, TryStreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Capture current-state catalog roots with bounded control and small-leaf reads.
///
/// The caller retains the captured roots against GC. Missing dependencies remain
/// errors. Control intake and each selected catalog query use the format byte bound.
pub async fn load_pointer_catalog_lookup_from_root(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
) -> Result<PointerCatalogLookup> {
    capture(layout, root).await?.lookup(layout)
}

/// Inspect the complete catalog from one authenticated current-state control capture.
///
/// The caller retains captured roots against GC and must finish the returned
/// spill cursor. Logical rows are normalized without retaining the recipe map.
pub async fn inspect_pointer_catalog_from_root(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
) -> Result<PointerCatalogInspection> {
    let captured = capture(layout, root).await?;
    super::inspect_pointer_catalog(
        layout,
        captured
            .checkpoint
            .as_ref()
            .and_then(LayeredCheckpoint::catalog),
        &[],
        &captured.controls,
        MAX_CATALOG_BYTES,
    )
    .await
}

pub(super) async fn materialize_from_root(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
) -> Result<PointerCatalog> {
    let captured = capture(layout, root).await?;
    let base_catalog = match root.record().root().checkpoint() {
        Some(pointer) => {
            load_layered_checkpoint(layout, pointer, MAX_CATALOG_BYTES)
                .await?
                .pointer_catalog_from_store(layout, MAX_CATALOG_BYTES)
                .await?
        }
        None => PointerCatalog::new(),
    };
    // Concurrent ref histories have a canonical binding order, not a global
    // payload dependency order. Validate completeness after collecting the set.
    let mut catalog = PointerCatalogAssembly::new(base_catalog);
    for control in captured.controls {
        if let Some(delta) = control.materialize_catalog(layout.store()).await? {
            catalog.apply(&delta)?;
        }
    }
    catalog.finish()
}

struct Capture {
    checkpoint: Option<LayeredCheckpoint>,
    controls: Vec<CapsuleControl>,
    sources: Vec<RunCatalogSource>,
}

impl Capture {
    fn lookup(self, layout: &StoreLayout<Store>) -> Result<PointerCatalogLookup> {
        let lookup = self
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.pointer_catalog_lookup(layout, MAX_CATALOG_BYTES))
            .transpose()?
            .unwrap_or_else(|| PointerCatalogLookup::empty(MAX_CATALOG_BYTES));
        let order = self
            .controls
            .iter()
            .map(|control| control.hash().to_owned())
            .collect::<Vec<_>>();
        lookup.with_run_sources(self.sources, &order)
    }
}

async fn capture(layout: &StoreLayout<Store>, snapshot: &RootSnapshot) -> Result<Capture> {
    let root = snapshot.record().root();
    let (heads, active) = capture_ref_heads(layout, root).await?;
    let visible = materialize_visible_ref_heads(root, &heads, &active)?;
    let mut intake = root
        .checkpoint()
        .map_or(0, |pointer| pointer.control_size());
    for pointer in &visible.pointers {
        intake = intake
            .checked_add(pointer.control_size())
            .ok_or_else(|| corrupt("catalog control intake overflows"))?;
    }
    if intake > MAX_CATALOG_BYTES {
        return Err(corrupt(
            "catalog control intake exceeds the format byte bound",
        ));
    }
    let inline = inline_leaf_reads(&visible.pointers, MAX_CATALOG_BYTES - intake);
    let checkpoint = match root.checkpoint() {
        Some(pointer) => Some(load_layered_checkpoint_control(layout, pointer).await?),
        None => None,
    };
    let run_layout = layout.clone();
    let runs = futures_util::stream::iter(visible.pointers.into_iter().zip(inline).map(
        move |(pointer, inline)| {
            let layout = run_layout.clone();
            async move {
                let (control, capsules) =
                    super::store::load_run_git_control(&layout, &pointer, inline).await?;
                if !control.git_packs().is_empty() {
                    control.source_descriptor()?;
                }
                Ok::<_, MetadataError>((control.hash().to_owned(), (control, capsules)))
            }
        },
    ))
    .buffer_unordered(32)
    .try_collect::<BTreeMap<_, _>>()
    .await?;
    let selected = select_ref_control_frontiers(root, &visible.frontiers, &runs)?;
    let pending = select_ref_journal_controls(root, &selected.required, &runs)?;
    let initial = match checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.visibility_reader(layout, MAX_CATALOG_BYTES))
        .transpose()?
        .flatten()
    {
        Some(reader) => {
            CapturedVisibility::from_checkpoint(
                layout.store().clone(),
                Arc::new(reader),
                root.refs().keys().cloned(),
            )
            .await?
        }
        None => CapturedVisibility::new(
            layout.store().clone(),
            GitVisibilityIndex::new(0, "0".repeat(64), "0".repeat(64), BTreeMap::new())?,
            root.refs().keys().cloned(),
        ),
    };
    let mut visibility = VisibilityReconciliation::Paged(Box::new(initial));
    let mut controls = selected.root_controls;
    for control in &controls {
        visibility.apply(control).await?;
    }
    let ordered = order_ref_controls(
        root.refs(),
        root.peeled_refs(),
        &mut visibility,
        &selected.predecessors,
        pending,
    )
    .await?;
    let mut refs = root.refs().clone();
    let mut peeled = root.peeled_refs().clone();
    // Root-owned controls are already reflected in the root's ref values.
    // Only journal controls advance them during this captured reconciliation.
    for control in &ordered {
        apply_control_refs(control, &mut refs, &mut peeled)?;
    }
    controls.extend(ordered);
    if refs != visible.refs || peeled != visible.peeled_refs {
        return Err(corrupt(
            "materialized controls differ from the captured ref-head state",
        ));
    }
    let sources = runs
        .values()
        .map(|(run, _)| run.catalog_source(layout))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(Capture {
        checkpoint,
        controls,
        sources,
    })
}

fn inline_leaf_reads(pointers: &[super::CapsulePointer], mut headroom: u64) -> Vec<bool> {
    pointers
        .iter()
        .map(|pointer| {
            // Footer intake is already admitted. Only unused byte headroom may
            // pay for a complete leaf; larger histories retain paged controls.
            // Compacted histories retain selected-member paging even when small.
            let inline = pointer.level() == 0
                && pointer.size() <= crate::pages::TARGET_BYTES as u64
                && pointer.control_offset() <= headroom;
            if inline {
                headroom -= pointer.control_offset();
            }
            inline
        })
        .collect()
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol catalog capture".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::capsule_protocol::{
        Capsule, CapsulePointer, CapsuleRefEdit, CapsuleRefHead, CapsuleRun, CapsuleSection,
        CapsuleSectionKind, CapsuleTransaction, CapsuleVisibilityDelta, RepositoryRoot, RootRecord,
        capsule_ref_name_key, create_root,
    };
    #[cfg(feature = "file-index-reader")]
    use crate::file_index_lookup::FileIndexLookupSession;
    use crate::git_visibility::GitVisibilityEdit;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    #[test]
    fn inline_leaf_reads_stay_within_the_admitted_headroom() {
        let pointer = |index: u64, size, offset, count| {
            CapsulePointer::new(
                format!("{index:064x}"),
                size,
                offset,
                size - offset,
                "f".repeat(64),
                if count == 1 { 0 } else { 1 },
                (0..count)
                    .map(|n| format!("{:064x}", index * 10 + n))
                    .collect(),
                "0".repeat(64),
            )
            .unwrap()
        };
        let pointers = [
            pointer(1, 32_000, 25_000, 1),
            pointer(2, 8_000, 6_000, 1),
            pointer(3, 33_000, 1_000, 1),
            pointer(4, 8_000, 6_000, 2),
        ];
        for (headroom, expected) in [
            (0, [false, false, false, false]),
            (6_000, [false, true, false, false]),
            (31_000, [true, true, false, false]),
            (u64::MAX, [true, true, false, false]),
        ] {
            assert_eq!(inline_leaf_reads(&pointers, headroom), expected);
        }
    }

    async fn leaf_capture_reads(file_data_bytes: usize) -> (usize, usize) {
        let reads = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let observed = reads.clone();
        let observed_bytes = bytes.clone();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_read_request_observer(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            }))
            .with_read_byte_observer(Arc::new(move |count| {
                observed_bytes.fetch_add(count, Ordering::SeqCst);
            }));
        let layout = StoreLayout::new(store, "catalog-leaf-opening".to_owned());
        let root = create_root(
            &layout,
            RootRecord::encode(
                RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let count = 64;
        for index in 1..=count {
            let name = format!("refs/heads/agent-{index:04}");
            let oid = format!("{index:040x}");
            let transaction = CapsuleTransaction::new(
                root.record().digest(),
                vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
            )
            .unwrap();
            let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
                name.clone(),
                GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid.clone()]),
            )]))
            .unwrap();
            let mut sections = vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )];
            if file_data_bytes > 0 {
                sections.push(CapsuleSection::new(
                    CapsuleSectionKind::FileData,
                    bytes::Bytes::from(vec![17; file_data_bytes]),
                ));
            }
            let run = CapsuleRun::leaf(Capsule::build(&transaction, Vec::new(), sections).unwrap())
                .unwrap();
            layout
                .store()
                .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
                .await
                .unwrap();
            let pointer = CapsulePointer::new(
                run.hash(),
                run.bytes().len() as u64,
                run.control_offset(),
                run.control_size(),
                run.footer_hash(),
                run.level(),
                run.transaction_ids(),
                run.newest_base_root_digest(),
            )
            .unwrap();
            let head = CapsuleRefHead::from_root(
                &name,
                root.record().root().ref_epoch().to_owned(),
                None,
                None,
            )
            .unwrap();
            let state = head
                .successor_state(
                    &BTreeSet::new(),
                    Some(oid),
                    None,
                    transaction.id().unwrap(),
                    vec![pointer],
                )
                .unwrap();
            layout
                .store()
                .create_strict(
                    &layout.capsule_ref_head_path(&capsule_ref_name_key(&name)),
                    head.commit(state).unwrap().encode().unwrap(),
                )
                .await
                .unwrap();
        }
        reads.store(0, Ordering::SeqCst);
        bytes.store(0, Ordering::SeqCst);
        let lookup = load_pointer_catalog_lookup_from_root(&layout, &root)
            .await
            .unwrap();
        assert!(
            lookup
                .files(layout.store(), &[])
                .await
                .unwrap()
                .files()
                .is_empty()
        );
        let observed = reads.load(Ordering::SeqCst);
        assert!(bytes.load(Ordering::SeqCst) <= (count * 16 * 1024) as u64);
        (count, observed)
    }

    #[tokio::test]
    async fn small_ref_leaf_capture_uses_one_source_read_per_ref() {
        let (count, observed) = leaf_capture_reads(0).await;
        assert_eq!(observed, 2 * count, "one head and one source read per ref");
    }

    #[tokio::test]
    async fn large_ref_leaves_keep_payloads_cold_and_coalesce_metadata() {
        let (count, observed) = leaf_capture_reads(64 * 1024).await;
        assert_eq!(
            observed,
            4 * count,
            "one head and three metadata reads per ref"
        );
    }

    #[cfg(feature = "file-index-reader")]
    #[tokio::test]
    async fn current_catalog_opening_reads_follow_compacted_runs() {
        let mut observations = Vec::new();
        // Metadata-only history keeps the catalog empty. One physical run
        // isolates visibility replay from run fan-out and payload lookup.
        for count in [2, 8, 32] {
            let reads = Arc::new(AtomicUsize::new(0));
            let observed = reads.clone();
            let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
                .with_read_request_observer(Arc::new(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                }));
            let layout = StoreLayout::new(store, "catalog-opening".to_owned());
            let root = create_root(
                &layout,
                RootRecord::encode(
                    RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let mut leaves = Vec::new();
            let mut previous = None;
            for version in 1..=count {
                let oid = format!("{version:040x}");
                let transaction = CapsuleTransaction::new(
                    root.record().digest(),
                    vec![CapsuleRefEdit::new(
                        "refs/heads/main",
                        previous.clone(),
                        Some(oid.clone()),
                        None,
                    )],
                )
                .unwrap();
                let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
                    "refs/heads/main".to_owned(),
                    GitVisibilityEdit::from_delta_objects(
                        previous,
                        oid.clone(),
                        vec![oid.clone()],
                        Vec::new(),
                    ),
                )]))
                .unwrap();
                let capsule = Capsule::build(
                    &transaction,
                    Vec::new(),
                    vec![CapsuleSection::new(
                        CapsuleSectionKind::VisibilityDelta,
                        visibility.encode().unwrap(),
                    )],
                )
                .unwrap();
                leaves.push(CapsuleRun::leaf(capsule).unwrap());
                previous = Some(oid);
            }
            let run = CapsuleRun::compact(leaves).unwrap();
            layout
                .store()
                .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
                .await
                .unwrap();
            let pointer = CapsulePointer::new(
                run.hash(),
                run.bytes().len() as u64,
                run.control_offset(),
                run.control_size(),
                run.footer_hash(),
                run.level(),
                run.transaction_ids(),
                run.newest_base_root_digest(),
            )
            .unwrap();
            let head = CapsuleRefHead::from_root(
                "refs/heads/main",
                root.record().root().ref_epoch().to_owned(),
                None,
                None,
            )
            .unwrap();
            let state = head
                .successor_state(
                    &BTreeSet::new(),
                    previous,
                    None,
                    run.transaction_ids().last().unwrap().clone(),
                    vec![pointer],
                )
                .unwrap();
            layout
                .store()
                .create_strict(
                    &layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/main")),
                    head.commit(state).unwrap().encode().unwrap(),
                )
                .await
                .unwrap();
            reads.store(0, Ordering::SeqCst);
            let session =
                FileIndexLookupSession::open_for_storage(layout.store(), "catalog-opening")
                    .await
                    .unwrap();
            observations.push((count, reads.load(Ordering::SeqCst)));
            session.close().await.unwrap();
        }
        assert!(
            observations.windows(2).all(|pair| pair[0].1 == pair[1].1),
            "provider reads should follow compacted proof blocks rather than logical edits: {observations:?}"
        );
    }
}
