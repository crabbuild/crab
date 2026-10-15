//! One shared authenticated control capture for current-state catalog queries.
use super::pointer::paged::MAX_CATALOG_BYTES;
use super::{
    CapsuleControl, CapturedVisibility, LayeredCheckpoint, PointerCatalog, PointerCatalogAssembly,
    PointerCatalogLookup, RootSnapshot, RunCatalogSource, VisibilityReconciliation,
    apply_control_refs, capture_ref_heads, load_capsule_run_git_control, load_layered_checkpoint,
    load_layered_checkpoint_control, materialize_visible_ref_heads, order_ref_controls,
    select_ref_control_frontiers, select_ref_journal_controls,
};
use crate::error::{MetadataError, Result};
use crate::git_visibility::GitVisibilityIndex;
use crab_storage::{Store, StoreLayout};
use futures_util::{StreamExt, TryStreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Capture current-state catalog roots without decoding catalog or Git pack bodies.
///
/// The caller retains the captured roots against GC. Missing dependencies remain
/// errors. Control intake and each selected catalog query use the format byte bound.
pub async fn load_pointer_catalog_lookup_from_root(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
) -> Result<PointerCatalogLookup> {
    capture(layout, root).await?.lookup(layout)
}

pub(super) async fn materialize_from_root(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
) -> Result<PointerCatalog> {
    let captured = capture(layout, root).await?;
    let base_catalog = match root.record().root().checkpoint() {
        Some(pointer) => load_layered_checkpoint(layout, pointer)
            .await?
            .pointer_catalog()?,
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
    let checkpoint = match root.checkpoint() {
        Some(pointer) => Some(load_layered_checkpoint_control(layout, pointer).await?),
        None => None,
    };
    let run_layout = layout.clone();
    let runs = futures_util::stream::iter(visible.pointers.into_iter().map(move |pointer| {
        let layout = run_layout.clone();
        async move {
            let (control, capsules) = load_capsule_run_git_control(&layout, &pointer).await?;
            if !control.git_packs().is_empty() {
                control.source_descriptor()?;
            }
            Ok::<_, MetadataError>((control.hash().to_owned(), (control, capsules)))
        }
    }))
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
        visibility,
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

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol catalog capture".to_owned(),
        reason: reason.into(),
    }
}
