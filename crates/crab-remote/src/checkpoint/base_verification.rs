//! Verify committed external bases without inflating their historical object graph.
use super::*;
use crab_metadata::capsule_protocol::PackSourceKind;

// Keep source verification independent of filesystem capacity and source size.
const VERIFY_CHUNK_BYTES: u64 = 1024 * 1024;

/// A source/member join binds every base OID to bytes admitted at publication.
/// Re-hashing those immutable bytes preserves that proof without reconstructing
/// unused object bodies. Older checkpoints without a join use the normal reader.
pub(super) async fn verify_sources(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    bases: &BTreeSet<gix_hash::ObjectId>,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Option<u64>, CheckpointError> {
    if bases.is_empty() {
        return Ok(Some(0));
    }
    let Some(checkpoint) = view.layered_checkpoint() else {
        return Ok(None);
    };
    let Some(snapshot) = checkpoint.visibility_ordinal_snapshot()? else {
        return Ok(None);
    };
    let Some(admission) = snapshot.member_admission() else {
        return Ok(None);
    };
    let mut source_indexes = BTreeSet::new();
    let mut pending = bases.clone();
    let mut seen = BTreeSet::new();
    while let Some(oid) = pending.pop_first() {
        if !seen.insert(oid) {
            continue;
        }
        check_cancelled(cancel)?;
        let oid: [u8; 20] = oid
            .as_bytes()
            .try_into()
            .map_err(|_| layered_admission_error("external base is not a SHA-1 object identity"))?;
        let position = snapshot.objects().binary_search(&oid).map_err(|_| {
            layered_admission_error("external base is absent from the admitted dictionary")
        })?;
        let member = admission
            .get(position)
            .ok_or_else(|| layered_admission_error("external base has no admitted pack member"))?;
        let source_index = usize::from(member.source_index());
        let source = checkpoint.sources().get(source_index).ok_or_else(|| {
            layered_admission_error("external base source admission is out of bounds")
        })?;
        let member = source
            .members()
            .get(usize::from(member.member_index()))
            .ok_or_else(|| {
                layered_admission_error("external base member admission is out of bounds")
            })?;
        // A referenced base can itself use an external REF_DELTA. Validate the
        // complete admitted member dependency closure before trusting its bytes.
        for dependency in member.external_delta_bases() {
            let oid = gix_hash::ObjectId::from_hex(dependency.as_bytes()).map_err(|_| {
                layered_admission_error("external base dependency identity is invalid")
            })?;
            if !seen.contains(&oid) {
                pending.insert(oid);
            }
        }
        source_indexes.insert(source_index);
    }
    // Admit the complete encoded read before starting it; no prefix body or
    // decoded base escapes the fixed-size read buffer into the repack workspace.
    let total = source_indexes.iter().try_fold(0_u64, |sum, index| {
        sum.checked_add(checkpoint.sources()[*index].object_size())
            .ok_or(CheckpointError::AccountingOverflow)
    })?;
    if total > maximum_bytes {
        return Err(CheckpointError::OutputLimit {
            maximum: maximum_bytes,
        });
    }
    for index in source_indexes {
        let source = &checkpoint.sources()[index];
        let path = match source.kind() {
            PackSourceKind::CapsuleRun => layout.capsule_path(source.object_hash()),
            PackSourceKind::PackLayer => layout.capsule_pack_layer_path(source.object_hash()),
        };
        let metadata = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CheckpointError::Cancelled),
            metadata = layout.store().head(&path) => metadata?,
        };
        check_cancelled(cancel)?;
        if metadata.size != source.object_size() {
            return Err(layered_admission_error(
                "external base source size does not match",
            ));
        }
        let mut hash = blake3::Hasher::new();
        let mut offset = 0;
        while offset < source.object_size() {
            let end = offset
                .saturating_add(VERIFY_CHUNK_BYTES)
                .min(source.object_size());
            let bytes = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CheckpointError::Cancelled),
                bytes = layout.store().range_get(&path, offset..end) => bytes?,
            };
            check_cancelled(cancel)?;
            if bytes.len() as u64 != end - offset {
                return Err(layered_admission_error(
                    "external base source range is truncated",
                ));
            }
            hash.update(&bytes);
            offset = end;
        }
        if hash.finalize().to_hex().as_str() != source.object_hash() {
            return Err(layered_admission_error(
                "external base source content hash does not match",
            ));
        }
    }
    Ok(Some(total))
}
