//! Shared captured ref values and transaction ordering for Git and catalog readers.
use crate::capsule_protocol::{
    CapsuleControl, CapsulePointer, CapsuleRunControl, RepositoryRoot, VisibilityReconciliation,
};
use crate::error::{MetadataError, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Visible ref values, publication positions and immutable frontiers for one capture.
pub struct VisibleRefHeads {
    pub refs: BTreeMap<String, String>,
    pub peeled_refs: BTreeMap<String, String>,
    pub transactions: BTreeMap<String, String>,
    /// First uncompacted transaction offset and its ordered physical runs per ref.
    pub frontiers: BTreeMap<String, (usize, Vec<CapsulePointer>)>,
    pub pointers: Vec<CapsulePointer>,
}

/// Materialize only ref-head metadata from one epoch-filtered stable capture.
pub fn materialize_visible_ref_heads(
    root: &crate::capsule_protocol::RepositoryRoot,
    heads: &[crate::capsule_protocol::CapsuleRefHead],
    active: &BTreeSet<String>,
) -> Result<VisibleRefHeads> {
    let mut refs = root.refs().clone();
    let mut peeled_refs = root.peeled_refs().clone();
    let mut transactions = BTreeMap::new();
    let mut frontiers = BTreeMap::new();
    let mut pointers = root.capsule_frontier().to_vec();
    for head in heads {
        let state = head.visible(active);
        if let Some(transaction_id) = state.transaction_id() {
            transactions.insert(head.ref_name().to_owned(), transaction_id.to_owned());
        }
        if state.transaction_id()
            == root
                .compacted_ref_transactions()
                .get(head.ref_name())
                .map(String::as_str)
        {
            continue;
        }
        match state.oid() {
            Some(oid) => {
                refs.insert(head.ref_name().to_owned(), oid.to_owned());
                match state.peeled_oid() {
                    Some(peeled) => {
                        peeled_refs.insert(head.ref_name().to_owned(), peeled.to_owned());
                    }
                    None => {
                        peeled_refs.remove(head.ref_name());
                    }
                }
            }
            None => {
                refs.remove(head.ref_name());
                peeled_refs.remove(head.ref_name());
            }
        }
        for pointer in state.frontier() {
            match pointers
                .iter()
                .find(|candidate| candidate.hash() == pointer.hash())
            {
                Some(candidate) if candidate != pointer => {
                    return Err(corrupt_path(
                        "capsule-protocol ref heads",
                        "capsule run identity has conflicting authenticated metadata",
                    ));
                }
                Some(_) => {}
                None => pointers.push(pointer.clone()),
            }
        }
        // Pointers commit to exact ordered transaction IDs; both run loaders
        // verify that binding. Resolve the checkpoint offset once for all readers.
        let ids = state
            .frontier()
            .iter()
            .flat_map(|pointer| pointer.transaction_ids())
            .collect::<Vec<_>>();
        let start = match root.compacted_ref_transactions().get(head.ref_name()) {
            Some(compacted) => match ids.iter().position(|id| *id == compacted) {
                Some(index) => index + 1,
                None if state.checkpoint_transaction_id() == Some(compacted.as_str()) => 0,
                None => {
                    return Err(frontier_contract(format!(
                        "ref {} does not extend its compacted transaction",
                        head.ref_name()
                    )));
                }
            },
            None if state.checkpoint_transaction_id().is_none() => 0,
            None => {
                return Err(frontier_contract(format!(
                    "ref {} names a checkpoint absent from the repository root",
                    head.ref_name()
                )));
            }
        };
        frontiers.insert(
            head.ref_name().to_owned(),
            (start, state.frontier().to_vec()),
        );
    }
    Ok(VisibleRefHeads {
        refs,
        peeled_refs,
        transactions,
        frontiers,
        pointers,
    })
}

/// Order selected controls and advance the caller-owned captured visibility proof.
pub async fn order_ref_controls(
    base_refs: &BTreeMap<String, String>,
    base_peeled: &BTreeMap<String, String>,
    visibility: &mut VisibilityReconciliation,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    mut pending: BTreeMap<String, CapsuleControl>,
) -> Result<Vec<CapsuleControl>> {
    let mut refs = base_refs.clone();
    let mut peeled = base_peeled.clone();
    let tracked = pending.keys().cloned().collect::<BTreeSet<_>>();
    let mut applied = BTreeSet::new();
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let mut ready = None;
        for (id, capsule) in &pending {
            if predecessors
                .get(capsule.transaction_id())
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|id| tracked.contains(id) && !applied.contains(id))
                })
            {
                continue;
            }
            if visibility.ready(capsule, &refs).await? {
                ready = Some(id.clone());
                break;
            }
        }
        let id = ready.ok_or_else(|| {
            corrupt_path(
                "capsule-protocol ref heads",
                "capsule ref history is cyclic or does not extend the compacted root",
            )
        })?;
        let capsule = pending
            .remove(&id)
            .ok_or_else(|| internal("ready capsule control disappeared"))?;
        apply_control_refs(&capsule, &mut refs, &mut peeled)?;
        visibility.apply(&capsule).await?;
        applied.insert(capsule.transaction_id().to_owned());
        ordered.push(capsule);
    }
    Ok(ordered)
}

/// Apply expected-old-checked ref edits to captured ref and peeled-ref maps.
pub fn apply_control_refs(
    capsule: &CapsuleControl,
    refs: &mut BTreeMap<String, String>,
    peeled_refs: &mut BTreeMap<String, String>,
) -> Result<()> {
    for edit in capsule.transaction().edits() {
        if refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old() {
            return Err(corrupt_path(
                "capsule-protocol ref heads",
                "capsule expected-old ref does not match its parent state",
            ));
        }
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
                match edit.peeled_oid() {
                    Some(peeled) => {
                        peeled_refs.insert(edit.ref_name().to_owned(), peeled.to_owned());
                    }
                    None => {
                        peeled_refs.remove(edit.ref_name());
                    }
                }
            }
            None => {
                refs.remove(edit.ref_name());
                peeled_refs.remove(edit.ref_name());
            }
        }
    }
    Ok(())
}

fn internal(reason: impl Into<String>) -> MetadataError {
    MetadataError::Internal(reason.into())
}
fn corrupt_path(path: impl Into<String>, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.into(),
        reason: reason.into(),
    }
}

/// Selected root controls and ref-frontier dependencies before visibility reconciliation.
pub struct RefControlFrontiers {
    pub root_controls: Vec<CapsuleControl>,
    pub required: BTreeSet<String>,
    pub predecessors: BTreeMap<String, BTreeSet<String>>,
    pub counts: BTreeMap<String, u32>,
}

/// Resolve captured frontier offsets into verified physical run occurrences.
pub fn select_ref_control_frontiers(
    root: &RepositoryRoot,
    ref_frontiers: &BTreeMap<String, (usize, Vec<CapsulePointer>)>,
    runs: &BTreeMap<String, (CapsuleRunControl, Vec<CapsuleControl>)>,
) -> Result<RefControlFrontiers> {
    let mut required_transactions = BTreeSet::new();
    let mut transaction_predecessors = BTreeMap::<String, BTreeSet<String>>::new();
    let mut ref_capsule_counts = BTreeMap::new();
    for (ref_name, (start, frontier)) in ref_frontiers {
        let transaction_ids = frontier
            .iter()
            .map(|pointer| {
                runs.get(pointer.hash())
                    .ok_or_else(|| internal("loaded capsule run control disappeared"))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|(_, capsules)| capsules.iter().map(|capsule| capsule.transaction_id()))
            .collect::<Vec<_>>();
        let required = transaction_ids.get(*start..).ok_or_else(|| {
            corrupt_path(
                "capsule-protocol ref heads",
                "captured frontier offset exceeds its validated transaction count",
            )
        })?;
        ref_capsule_counts.insert(
            ref_name.clone(),
            u32::try_from(required.len()).map_err(|_| internal("ref capsule count overflowed"))?,
        );
        required_transactions.extend(
            required
                .iter()
                .map(|transaction_id| (*transaction_id).to_owned()),
        );
        for pair in required.windows(2) {
            transaction_predecessors
                .entry(pair[1].to_owned())
                .or_default()
                .insert(pair[0].to_owned());
        }
    }
    let mut root_capsules = Vec::new();
    let mut root_capsule_identities = BTreeMap::new();
    for pointer in root.capsule_frontier() {
        let (_, capsules) = runs
            .get(pointer.hash())
            .ok_or_else(|| internal("loaded root capsule run control disappeared"))?;
        for capsule in capsules {
            match root_capsule_identities.get(capsule.transaction_id()) {
                Some(existing) if existing != capsule => {
                    return Err(corrupt_path(
                        "capsule-protocol root frontier",
                        "transaction identity names conflicting capsules",
                    ));
                }
                Some(_) => {}
                None => {
                    root_capsule_identities
                        .insert(capsule.transaction_id().to_owned(), capsule.clone());
                    root_capsules.push(capsule.clone());
                }
            }
        }
    }

    Ok(RefControlFrontiers {
        root_controls: root_capsules,
        required: required_transactions,
        predecessors: transaction_predecessors,
        counts: ref_capsule_counts,
    })
}

/// Deduplicate only the journal transactions required by the captured ref frontiers.
pub fn select_ref_journal_controls(
    root: &RepositoryRoot,
    required: &BTreeSet<String>,
    runs: &BTreeMap<String, (CapsuleRunControl, Vec<CapsuleControl>)>,
) -> Result<BTreeMap<String, CapsuleControl>> {
    let root_transactions = root
        .capsule_frontier()
        .iter()
        .flat_map(|pointer| pointer.transaction_ids().iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut journal_capsules = BTreeMap::new();
    for (_, capsules) in runs.values() {
        for capsule in capsules {
            if root_transactions.contains(capsule.transaction_id())
                || !required.contains(capsule.transaction_id())
            {
                continue;
            }
            match journal_capsules.get(capsule.transaction_id()) {
                Some(existing) if existing != capsule => {
                    return Err(corrupt_path(
                        "capsule-protocol ref heads",
                        "transaction identity names conflicting capsules",
                    ));
                }
                Some(_) => {}
                None => {
                    journal_capsules.insert(capsule.transaction_id().to_owned(), capsule.clone());
                }
            }
        }
    }

    Ok(journal_capsules)
}

fn frontier_contract(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "stored root",
        reason: reason.into(),
    }
}
