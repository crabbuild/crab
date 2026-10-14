use super::*;

pub(super) async fn ordered(
    router: &StoreLayout<Store>,
    checkpoint: Option<&LayeredCheckpoint>,
    base: &RootRecord,
    root_capsules: &[CapsuleControl],
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    pending: BTreeMap<String, CapsuleControl>,
    limits: CapsuleReadLimits,
) -> Result<Vec<CapsuleControl>> {
    let reader = checkpoint
        .as_ref()
        .map(|checkpoint| {
            let remaining = limits
                .max_capsule_bytes
                .checked_sub(checkpoint.captured_metadata_bytes())
                .filter(|remaining| *remaining > 0)
                .ok_or(ReadError::CapsuleReadLimit {
                    resource: "captured checkpoint metadata",
                    maximum: limits.max_capsule_bytes,
                })?;
            checkpoint
                .visibility_reader(router.capsule_checkpoint_path(checkpoint.hash()), remaining)
                .map_err(ReadError::from)
        })
        .transpose()?
        .flatten();
    let mut visibility =
        crab_metadata::capsule_protocol::PagedGitVisibility::new(reader, router.store().clone())
            .await?;
    if let Some(checkpoint) = checkpoint {
        for (name, transitions) in checkpoint.visibility_transitions() {
            for transition in transitions {
                let objects = std::iter::once(&transition.from_oid)
                    .chain(std::iter::once(&transition.to_oid))
                    .chain(transition.objects.iter())
                    .map(|oid| {
                        ObjectId::from_hex(oid.as_bytes())
                            .map_err(|error| corrupt_path("visibility history", error.to_string()))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let objects = objects
                    .into_iter()
                    .map(|oid| {
                        <[u8; 20]>::try_from(oid.as_bytes())
                            .map_err(|_| corrupt_path("visibility history", "OID width is invalid"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                if visibility
                    .contains_in_ref(name, &objects)
                    .await?
                    .into_iter()
                    .any(|member| !member)
                {
                    return Err(corrupt_path(
                        "visibility history",
                        "checkpoint transition escapes its ref closure",
                    ));
                }
            }
        }
    }
    for capsule in root_capsules {
        if capsule.visibility_delta().is_some() {
            apply_paged_control_visibility(capsule, &mut visibility).await?;
        }
    }
    order_paged_ref_controls(
        base.root().refs(),
        base.root().peeled_refs(),
        &mut visibility,
        predecessors,
        pending,
    )
    .await
}

async fn apply_paged_control_visibility(
    capsule: &CapsuleControl,
    visibility: &mut crab_metadata::capsule_protocol::PagedGitVisibility,
) -> Result<()> {
    for (name, edit) in control_visibility_edits(capsule)? {
        match edit {
            Some(edit) => visibility.apply_ref_edit(name.to_owned(), edit).await?,
            None => visibility.remove_ref(name),
        }
    }
    Ok(())
}

async fn order_paged_ref_controls(
    base_refs: &BTreeMap<String, String>,
    base_peeled: &BTreeMap<String, String>,
    visibility: &mut crab_metadata::capsule_protocol::PagedGitVisibility,
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
            let Some(bases) =
                control_readiness_bases(capsule, &refs, |name| visibility.contains_ref(name))?
            else {
                continue;
            };
            let mut available = true;
            for base in bases {
                if !visibility.contains_hex_in_any_ref(base).await? {
                    available = false;
                    break;
                }
            }
            if available {
                ready = Some(id.clone());
                break;
            }
        }
        let ready = ready.ok_or_else(|| {
            corrupt_path(
                "capsule-protocol ref heads",
                "capsule ref history is cyclic or does not extend the compacted root",
            )
        })?;
        let capsule = pending
            .remove(&ready)
            .ok_or_else(|| ReadError::internal("ready capsule control disappeared"))?;
        apply_control_refs(&capsule, &mut refs, &mut peeled)?;
        if capsule.visibility_delta().is_some() {
            apply_paged_control_visibility(&capsule, visibility).await?;
        }
        applied.insert(capsule.transaction_id().to_owned());
        ordered.push(capsule);
    }
    Ok(ordered)
}
