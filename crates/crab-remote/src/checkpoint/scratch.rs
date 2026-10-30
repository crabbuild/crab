//! Optional physical maintenance must fit without borrowing foreground scratch.
use super::*;

const FIXED_SCRATCH_BYTES: u64 = 32 * 1024 * 1024;
const SCRATCH_COPIES: u64 = 3;
// Include allocation rounding and per-member pack/index/reverse/kind files;
// compressed byte counts alone undercharge namespaces full of tiny packs.
const MEMBER_SCRATCH_OVERHEAD: u64 = 64 * 1024;

/// Repack a captured checkpoint using a conservative compressed-copy budget.
///
/// The budget covers source fetch/install, replacement body/sidecars and fixed
/// workspace overhead. A smaller recent suffix may fit when the geometric cut
/// would copy a large stable prefix. Native rewrites and footer fallback are
/// deferred; the foreground admission path retains its existing authority.
pub async fn repack_capsule_checkpoint_from_root_with_scratch_limit(
    layout: &StoreLayout<Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    maximum_bytes: u64,
    maximum_scratch_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    if maximum_scratch_bytes <= FIXED_SCRATCH_BYTES || root.record().root().checkpoint().is_none() {
        return Ok(CheckpointOutcome::default());
    }
    let view = crab_read::capsule_protocol::open_compacted_view_from_root(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: maximum_bytes,
            max_frontier_bytes: maximum_bytes,
        },
    )
    .await?;
    let checkpoint = view.layered_checkpoint().ok_or_else(|| {
        layered_admission_error("bounded physical maintenance requires a checkpoint")
    })?;
    let Some(start) = layered_suffix_start(checkpoint.sources())? else {
        return Ok(CheckpointOutcome::default());
    };
    let sizes = checkpoint
        .sources()
        .iter()
        .map(PackSourceDescriptor::object_size)
        .collect::<Vec<_>>();
    let members = checkpoint
        .sources()
        .iter()
        .map(|source| source.members().len())
        .collect::<Vec<_>>();
    let Some(start) = fitting_suffix(&sizes, &members, start, maximum_scratch_bytes) else {
        return Ok(CheckpointOutcome::default());
    };
    let consolidation = match consolidate_layered_suffix_with_policy(
        layout,
        &view,
        checkpoint.sources().to_vec(),
        start,
        ConsolidationPolicy {
            maximum_bytes,
            cancel,
            compressed_only: true,
        },
    )
    .await
    {
        Ok(work) => work,
        Err(CheckpointError::MaintenanceRequiresRewrite) => return Ok(CheckpointOutcome::default()),
        Err(error) => return Err(error),
    };
    let publication = publish_layered_sources_with_policy(
        layout,
        &view,
        consolidation.sources,
        consolidation.member_oids,
        CatalogWrite::Durable(checkpoint.catalog().cloned().map(Box::new)),
        PublicationPolicy {
            maximum_bytes,
            cancel,
            allow_footer_consolidation: false,
        },
    )
    .await?;
    consolidation.work.combine(publication.work)
}

fn fitting_suffix(sizes: &[u64], members: &[usize], start: usize, budget: u64) -> Option<usize> {
    for start in start..sizes.len() {
        let bytes = sizes[start..]
            .iter()
            .try_fold(0_u64, |sum, bytes| sum.checked_add(*bytes))?;
        let members = members[start..]
            .iter()
            .try_fold(0_usize, |sum, count| sum.checked_add(*count))?;
        let overhead = u64::try_from(members)
            .ok()?
            .checked_mul(MEMBER_SCRATCH_OVERHEAD)?;
        let required = bytes
            .checked_mul(SCRATCH_COPIES)?
            .checked_add(FIXED_SCRATCH_BYTES)?
            .checked_add(overhead)?;
        if required <= budget && members > 1 {
            return Some(start);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_cut_preserves_large_prefix_and_bounds_all_copies() {
        let mib = 1024 * 1024;
        let sizes = [400 * mib, 20 * mib, 20 * mib];
        assert_eq!(
            fitting_suffix(&sizes, &[1, 500, 500], 0, 256 * mib),
            Some(1)
        );
        assert_eq!(
            fitting_suffix(&sizes, &[1, 500, 500], 0, 192 * mib),
            Some(2)
        );
        assert_eq!(fitting_suffix(&sizes, &[1, 500, 500], 0, 80 * mib), None);
        assert_eq!(fitting_suffix(&[20 * mib], &[1], 0, 192 * mib), None);
        assert_eq!(fitting_suffix(&[1], &[32_768], 0, 256 * mib), None);
        assert_eq!(fitting_suffix(&[u64::MAX], &[500], 0, u64::MAX), None);
    }
}
