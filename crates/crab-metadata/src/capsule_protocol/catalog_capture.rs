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
use crate::git_visibility::{GitVisibilityIndex, decode_oid};
use crab_storage::{Store, StoreLayout};
use futures_util::{StreamExt, TryStreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

/// Capture catalog roots against the supplied checkpoint replay base.
///
/// The caller retains the captured roots against GC. Missing dependencies remain
/// errors. Control intake and each selected catalog query use the format byte bound.
pub async fn load_pointer_catalog_lookup_from_root(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
) -> Result<PointerCatalogLookup> {
    capture(layout, root).await?.lookup(layout)
}

/// Capture current catalog authority across same-epoch checkpoint replacement.
///
/// The caller retains GC admission. The initial root anchors repository identity
/// and epoch; explicit snapshot lookup remains bound to its supplied checkpoint.
pub async fn load_current_pointer_catalog_lookup(
    layout: &StoreLayout<Store>,
    initial: &RootSnapshot,
) -> Result<PointerCatalogLookup> {
    let mut root = initial.clone();
    for attempt in 0_u32..32 {
        let result = load_pointer_catalog_lookup_from_root(layout, &root).await;
        let current = super::load_root(layout).await?;
        if current.record().root().repository_id() != initial.record().root().repository_id()
            || current.record().root().ref_epoch() != initial.record().root().ref_epoch()
        {
            return Err(MetadataError::CapsuleContract {
                record: "current catalog",
                reason: "repository authority changed during catalog capture".to_owned(),
            });
        }
        if current.record().digest() == root.record().digest() {
            return result;
        }
        // A checkpoint can retire the ref-head prefix that the initial root
        // still needs. Retry only after proving root replacement; errors under
        // unchanged authority remain errors, including missing immutable pages.
        drop(result);
        root = current;
        if attempt < 31 {
            tokio::time::sleep(std::time::Duration::from_millis(10_u64 << attempt.min(4))).await;
        }
    }
    Err(MetadataError::CapsuleContract {
        record: "current catalog",
        reason: "repository root changed during every bounded catalog capture".to_owned(),
    })
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
    let started = std::time::Instant::now();
    let root = snapshot.record().root();
    let (heads, active) = capture_ref_heads(layout, root).await?;
    tracing::debug!(
        ref_heads = heads.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "catalog ref heads captured"
    );
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
    let read_ahead = AtomicU64::new(MAX_CATALOG_BYTES - intake);
    let checkpoint = match root.checkpoint() {
        Some(pointer) => Some(load_layered_checkpoint_control(layout, pointer).await?),
        None => None,
    };
    let run_layout = layout.clone();
    let headroom = &read_ahead;
    let runs = futures_util::stream::iter(visible.pointers.into_iter().map(move |pointer| {
        let layout = run_layout.clone();
        async move {
            let (control, capsules) =
                super::store::load_run_git_control(&layout, &pointer, Some(headroom)).await?;
            if !control.git_packs().is_empty() {
                control.source_descriptor()?;
            }
            Ok::<_, MetadataError>((control.hash().to_owned(), (control, capsules)))
        }
    }))
    .buffer_unordered(32)
    .try_collect::<BTreeMap<_, _>>()
    .await?;
    tracing::debug!(
        frontier_runs = runs.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "catalog run controls opened"
    );
    let selected = select_ref_control_frontiers(root, &visible.frontiers, &runs)?;
    let pending = select_ref_journal_controls(root, &selected.required, &runs)?;
    // Reconciliation must check every selected new tip. Fetch those immutable
    // point proofs in bounded waves; serial closure ordering still validates
    // prior tips, removals and final membership before accepting the catalog.
    let controls = selected.root_controls.iter().chain(pending.values());
    futures_util::stream::iter(controls)
        .map(Ok::<_, MetadataError>)
        .try_for_each_concurrent(32, |control| async move {
            if let Some((reader, headers)) =
                control.paged_visibility_headers(layout.store()).await?
            {
                for (edit, header) in control.transaction().edits().iter().zip(headers) {
                    if let Some(header) = header {
                        let _ = reader
                            .lookup(
                                layout.store(),
                                edit.ref_name(),
                                &[decode_oid(header.new_oid())?],
                            )
                            .await?;
                    }
                }
            }
            Ok(())
        })
        .await?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "catalog new tip proofs opened"
    );
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
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "catalog checkpoint visibility opened"
    );
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
    tracing::debug!(
        logical_capsules = controls.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "catalog visibility reconciled"
    );
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn current_catalog_does_not_recapture_another_repository_or_epoch() {
        for field in ["repository_id", "ref_epoch"] {
            let layout = StoreLayout::new(
                Store::new(Arc::new(object_store::memory::InMemory::new())),
                "repositories/current-authority".to_owned(),
            );
            let initial = RootRecord::encode(
                RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
            )
            .unwrap();
            let root = create_root(&layout, initial.clone()).await.unwrap();
            let mut replacement = serde_json::to_value(initial.root()).unwrap();
            replacement[field] = serde_json::Value::String("2".repeat(64));
            let replacement =
                RootRecord::encode(serde_json::from_value(replacement).unwrap()).unwrap();
            layout
                .store()
                .put_overwrite(&layout.capsule_root_path(), replacement.bytes().clone())
                .await
                .unwrap();
            assert!(matches!(
                load_current_pointer_catalog_lookup(&layout, &root).await,
                Err(MetadataError::CapsuleContract {
                    record: "current catalog",
                    ..
                })
            ));
        }
    }

    async fn capture_reads(
        file_data_bytes: usize,
        objects_per_ref: usize,
        history_len: usize,
        delay: std::time::Duration,
    ) -> (usize, usize, u64, std::time::Duration) {
        let reads = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let observed = reads.clone();
        let observed_bytes = bytes.clone();
        let origin = Arc::new(object_store::throttle::ThrottledStore::new(
            object_store::memory::InMemory::new(),
            object_store::throttle::ThrottleConfig::default(),
        ));
        let store = Store::new(origin.clone())
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
            let last_object = index * objects_per_ref;
            let mut oid = format!("{last_object:040x}");
            let objects = (last_object - objects_per_ref + 1..=last_object)
                .map(|object| format!("{object:040x}"))
                .collect();
            let transaction = CapsuleTransaction::new(
                root.record().digest(),
                vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
            )
            .unwrap();
            let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
                name.clone(),
                GitVisibilityEdit::from_replacement_objects(None, oid.clone(), objects),
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
            let first =
                CapsuleRun::leaf(Capsule::build(&transaction, Vec::new(), sections).unwrap())
                    .unwrap();
            let mut leaves = vec![first];
            for update in 1..history_len {
                let next = format!(
                    "{:040x}",
                    count * objects_per_ref + index * history_len + update
                );
                let transaction = CapsuleTransaction::new(
                    root.record().digest(),
                    vec![CapsuleRefEdit::new(
                        &name,
                        Some(oid.clone()),
                        Some(next.clone()),
                        None,
                    )],
                )
                .unwrap();
                let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
                    name.clone(),
                    GitVisibilityEdit::from_delta_objects(
                        Some(oid),
                        next.clone(),
                        vec![next.clone()],
                        Vec::new(),
                    ),
                )]))
                .unwrap();
                leaves.push(
                    CapsuleRun::leaf(
                        Capsule::build(
                            &transaction,
                            Vec::new(),
                            vec![CapsuleSection::new(
                                CapsuleSectionKind::VisibilityDelta,
                                visibility.encode().unwrap(),
                            )],
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                );
                oid = next;
            }
            let run = if leaves.len() == 1 {
                leaves.pop().unwrap()
            } else {
                CapsuleRun::compact(leaves).unwrap()
            };
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
                    run.transaction_ids().last().unwrap().clone(),
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
        origin.config_mut(|config| config.wait_get_per_call = delay);
        let started = tokio::time::Instant::now();
        fn send<F: std::future::Future + Send>(future: F) -> F {
            future
        }
        let lookup = send(load_pointer_catalog_lookup_from_root(&layout, &root))
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
        (
            count,
            observed,
            bytes.load(Ordering::SeqCst),
            started.elapsed(),
        )
    }

    #[tokio::test]
    async fn small_ref_leaf_capture_coalesces_metadata_only() {
        let (count, observed, _, _) = capture_reads(0, 1, 1, std::time::Duration::ZERO).await;
        assert_eq!(
            observed,
            3 * count,
            "one head and two metadata reads per ref"
        );
    }

    #[tokio::test]
    async fn large_ref_leaves_keep_payloads_cold_and_coalesce_metadata() {
        let (count, observed, _, _) =
            capture_reads(64 * 1024, 1, 1, std::time::Duration::ZERO).await;
        assert_eq!(
            observed,
            3 * count,
            "one head and two metadata reads per ref"
        );
    }

    #[tokio::test]
    async fn catalog_capture_keeps_small_leaf_payloads_cold() {
        let (count, _, baseline, _) = capture_reads(0, 1, 1, std::time::Duration::ZERO).await;
        let (_, _, with_payload, _) =
            capture_reads(8 * 1024, 1, 1, std::time::Duration::ZERO).await;
        assert!(
            with_payload <= baseline + (count * 256) as u64,
            "payload changed control traffic from {baseline} to {with_payload} bytes"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn leaf_proof_latency_is_bounded_by_concurrent_waves() {
        let delay = std::time::Duration::from_millis(8);
        let (_, _, _, elapsed) = capture_reads(0, 1, 1, delay).await;
        assert!(
            elapsed <= delay * 16,
            "capture serialized proof reads: {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn paged_new_tip_proofs_follow_concurrent_capture_waves() {
        let delay = std::time::Duration::from_millis(8);
        for (objects, history) in [(4096, 1), (1, 4)] {
            let (_, _, _, elapsed) = capture_reads(0, objects, history, delay).await;
            assert!(
                elapsed <= delay * 16,
                "capture serialized {objects}-object/{history}-edit tip proofs: {elapsed:?}"
            );
        }
    }

    #[tokio::test]
    async fn paged_tip_capture_reuses_authenticated_inline_roots() {
        let (count, reads, _, _) = capture_reads(0, 4096, 1, std::time::Duration::ZERO).await;
        assert_eq!(
            reads,
            count * 4,
            "head, run, scalar with inline root, and tip leaf"
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
