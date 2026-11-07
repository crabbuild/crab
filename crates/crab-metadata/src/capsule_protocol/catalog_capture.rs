//! One shared authenticated control capture for current-state catalog queries.
use super::pointer::paged::MAX_CATALOG_BYTES;
use super::{
    CapsuleControl, CapsuleRunControl, CapturedVisibility, LayeredCheckpoint, PointerCatalog,
    PointerCatalogAssembly, PointerCatalogInspection, PointerCatalogLookup, RootSnapshot,
    RunCatalogSource, VisibilityReconciliation, VisibleRefHeads, apply_control_refs,
    capture_ref_heads, load_layered_checkpoint, load_layered_checkpoint_control,
    materialize_visible_ref_heads, order_ref_controls, select_ref_control_frontiers,
    select_ref_journal_controls,
};
use crate::error::{MetadataError, Result};
use crate::git_visibility::{GitVisibilityIndex, decode_oid};
use crate::pages::{Kind, PageWork};
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
    with_current_root(layout, initial, |root| async move {
        load_pointer_catalog_lookup_from_root(layout, &root).await
    })
    .await
}

/// Snapshot-bound publication discovery with complete proof before remote reuse.
///
/// The caller retains GC admission. Misses return no values or visibility
/// authority; every hit completes the same reconciliation as ordinary readers.
pub struct PublicationCatalogLookup {
    layout: StoreLayout<Store>,
    root: Arc<RootSnapshot>,
    captured: SourceCapture,
    probe: PointerCatalogLookup,
    authority: tokio::sync::OnceCell<PointerCatalogLookup>,
    pending_capsules: usize,
}

impl PublicationCatalogLookup {
    /// Return the distinct uncheckpointed transactions in this stable capture.
    #[must_use]
    pub fn pending_capsule_count(&self) -> usize {
        self.pending_capsules
    }

    /// Select files with their complete dependency closure after authority validation.
    pub async fn files(&self, hashes: &[String]) -> Result<PointerCatalog> {
        self.query(Kind::CatalogFiles, hashes).await
    }

    /// Select chunk placements with their verified xorb closure.
    pub async fn chunks(&self, hashes: &[String]) -> Result<PointerCatalog> {
        self.query(Kind::CatalogChunks, hashes).await
    }

    /// Select xorb descriptors after validating the captured publication authority.
    pub async fn xorbs(&self, hashes: &[String]) -> Result<PointerCatalog> {
        self.query(Kind::CatalogXorbs, hashes).await
    }

    async fn query(&self, kind: Kind, hashes: &[String]) -> Result<PointerCatalog> {
        let mut work = PageWork::new(MAX_CATALOG_BYTES, 0);
        if self.authority.get().is_none()
            && !self
                .probe
                .has_rows(self.layout.store(), kind, hashes, &mut work)
                .await?
        {
            return Ok(PointerCatalog::new());
        }
        // Cancellation leaves the immutable capture intact. OnceCell retries
        // failed/cancelled initialization; no partial proof becomes query authority.
        let authority = self
            .authority
            .get_or_try_init(|| async {
                reconcile(&self.layout, &self.root, &self.captured)
                    .await?
                    .lookup(&self.layout)
            })
            .await?;
        authority
            .query_with_work(self.layout.store(), kind, hashes, &mut work)
            .await
    }
}

/// Capture publication sources before admitting remote catalog reuse.
///
/// The caller retains GC admission and must independently verify every published
/// payload. Each query probes authenticated physical-source indexes; extra rows
/// can cause hits but cannot hide a selected value. Any hit completes ordinary
/// reconciliation against these same immutable sources before returning values.
pub async fn load_current_publication_catalog(
    layout: &StoreLayout<Store>,
    initial: &RootSnapshot,
) -> Result<PublicationCatalogLookup> {
    with_current_root(layout, initial, |root| async move {
        let captured = open_sources(layout, &root).await?;
        // Physical sources are a superset of selected logical members.
        // Extra rows can only prevent a miss; no row or precedence escapes
        // this probe without the complete reconciliation used by readers.
        let order = captured
            .runs
            .values()
            .flat_map(|(_, capsules)| capsules.iter().map(|capsule| capsule.hash().to_owned()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let probe = captured
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.pointer_catalog_lookup(layout, MAX_CATALOG_BYTES))
            .transpose()?
            .unwrap_or_else(|| PointerCatalogLookup::empty(MAX_CATALOG_BYTES))
            .with_run_sources(captured.sources(layout)?, &order)?;
        let count = usize::try_from(captured.visible.capsule_count)
            .map_err(|_| corrupt("pending capsule count cannot be represented"))?;
        Ok(PublicationCatalogLookup {
            layout: layout.clone(),
            root,
            captured,
            probe,
            authority: tokio::sync::OnceCell::new(),
            pending_capsules: count,
        })
    })
    .await
}

async fn with_current_root<T, Read, ReadFuture>(
    layout: &StoreLayout<Store>,
    initial: &RootSnapshot,
    read: Read,
) -> Result<T>
where
    Read: Fn(Arc<RootSnapshot>) -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<T>>,
{
    let mut root = Arc::new(initial.clone());
    for attempt in 0_u32..32 {
        let result = read(root.clone()).await;
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
        // Checkpoint replacement can retire the old ref prefix. Retry only
        // after proving replacement; unchanged-root corruption remains an error.
        drop(result);
        root = Arc::new(current);
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

struct SourceCapture {
    checkpoint: Option<LayeredCheckpoint>,
    visible: VisibleRefHeads,
    runs: BTreeMap<String, (CapsuleRunControl, Vec<CapsuleControl>)>,
}

impl SourceCapture {
    fn sources(&self, layout: &StoreLayout<Store>) -> Result<Vec<RunCatalogSource>> {
        self.runs
            .values()
            .map(|(run, _)| run.catalog_source(layout))
            .collect::<Result<Vec<_>>>()
            .map(|sources| sources.into_iter().flatten().collect())
    }
}

async fn open_sources(
    layout: &StoreLayout<Store>,
    snapshot: &RootSnapshot,
) -> Result<SourceCapture> {
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
    let runs = futures_util::stream::iter(visible.pointers.iter().cloned().map(move |pointer| {
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
    Ok(SourceCapture {
        checkpoint,
        visible,
        runs,
    })
}

async fn capture(layout: &StoreLayout<Store>, snapshot: &RootSnapshot) -> Result<Capture> {
    let captured = open_sources(layout, snapshot).await?;
    reconcile(layout, snapshot, &captured).await
}

async fn reconcile(
    layout: &StoreLayout<Store>,
    snapshot: &RootSnapshot,
    captured: &SourceCapture,
) -> Result<Capture> {
    let started = std::time::Instant::now();
    let root = snapshot.record().root();
    let sources = captured.sources(layout)?;
    let SourceCapture {
        checkpoint,
        visible,
        runs,
    } = captured;
    let selected = select_ref_control_frontiers(root, &visible.frontiers, runs)?;
    let pending = select_ref_journal_controls(root, &selected.required, runs)?;
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
    Ok(Capture {
        checkpoint: checkpoint.clone(),
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
mod tests;
