//! Foreground run selection, bounded by bytes and the persisted frontier ceiling.
use std::ops::Range;

use crab_metadata::capsule_protocol::{
    CAPSULE_REF_COMPACTION_FAN_IN, CAPSULE_REF_ROLLUP_WINDOW, CapsulePointer, CapsuleRun,
    MAX_CAPSULE_REF_FRONTIER, MAX_CAPSULES_PER_RUN,
};
use crab_storage::{Store, StoreLayout};
use futures_util::future::try_join_all;

use crate::{Result, WriteError};

// A large historical run remains a stable source. Rewriting it to append small
// commits amplifies both network traffic and peak memory with repository size.
const COMPACTION_INPUT_BYTES: u128 = 64 * 1024 * 1024;

fn window(frontier: &[CapsulePointer]) -> Result<Option<Range<usize>>> {
    if frontier.len() > MAX_CAPSULE_REF_FRONTIER {
        // The persisted ref head cannot exceed 64 segments. At that boundary
        // merge the cheapest adjacent pair, even above the normal byte budget;
        // checkpoints retire this pressure without changing stored run formats.
        return frontier
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| {
                pair.iter()
                    .map(|p| p.capsule_count() as usize)
                    .sum::<usize>()
                    <= MAX_CAPSULES_PER_RUN
            })
            .min_by_key(|(_, pair)| pair.iter().map(|p| u128::from(p.size())).sum::<u128>())
            .map(|(index, _)| Some(index..index + 2))
            .ok_or_else(|| {
                WriteError::Internal(
                    "capsule frontier requires a checkpoint before further publication".to_owned(),
                )
            });
    }
    let start = frontier
        .iter()
        .rposition(|pointer| {
            pointer.capsule_count() as usize >= CAPSULE_REF_ROLLUP_WINDOW
                || u128::from(pointer.size()) >= COMPACTION_INPUT_BYTES
        })
        .map_or(0, |index| index + 1);
    let suffix = &frontier[start..];
    if suffix.len() < 2 {
        return Ok(None);
    }
    let bytes: u128 = suffix.iter().map(|p| u128::from(p.size())).sum();
    let count: usize = suffix.iter().map(|p| p.capsule_count() as usize).sum();
    if bytes >= COMPACTION_INPUT_BYTES || count >= CAPSULE_REF_ROLLUP_WINDOW {
        let Some(first) = (start..frontier.len() - 1).find(|&index| {
            frontier[index].capsule_count() as usize + frontier[index + 1].capsule_count() as usize
                <= MAX_CAPSULES_PER_RUN
        }) else {
            return Ok(None);
        };
        // Two sub-budget runs may together exceed the soft byte ceiling. Keep
        // that smallest useful merge, then admit no additional oversized input.
        let mut end = first + 2;
        let mut bytes = u128::from(frontier[first].size()) + u128::from(frontier[first + 1].size());
        let mut count =
            frontier[first].capsule_count() as usize + frontier[first + 1].capsule_count() as usize;
        while let Some(next) = frontier.get(end) {
            if bytes + u128::from(next.size()) > COMPACTION_INPUT_BYTES
                || count + next.capsule_count() as usize > MAX_CAPSULES_PER_RUN
            {
                break;
            }
            bytes += u128::from(next.size());
            count += next.capsule_count() as usize;
            end += 1;
        }
        return Ok(Some(first..end));
    }
    if !CAPSULE_REF_COMPACTION_FAN_IN.is_power_of_two() {
        return Err(WriteError::Internal(
            "capsule compaction fan-in is not a power of two".to_owned(),
        ));
    }
    let level = suffix[suffix.len() - 1].level();
    if suffix
        .iter()
        .rev()
        .take_while(|p| p.level() == level)
        .count()
        < CAPSULE_REF_COMPACTION_FAN_IN
    {
        return Ok(None);
    }
    let mut first = frontier.len() - CAPSULE_REF_COMPACTION_FAN_IN;
    let mut count: usize = frontier[first..]
        .iter()
        .map(|p| p.capsule_count() as usize)
        .sum();
    if count > MAX_CAPSULES_PER_RUN {
        return Ok(None);
    }
    let mut next_level = level + CAPSULE_REF_COMPACTION_FAN_IN.ilog2() as u8;
    while first > start
        && frontier[first - 1].level() == next_level
        && count + frontier[first - 1].capsule_count() as usize <= MAX_CAPSULES_PER_RUN
    {
        first -= 1;
        count += frontier[first].capsule_count() as usize;
        next_level += 1;
    }
    Ok(Some(first..frontier.len()))
}

pub(super) async fn compact_ref_frontier(
    router: &StoreLayout<Store>,
    frontier: &mut Vec<CapsulePointer>,
    known_leaf: &CapsuleRun,
) -> Result<Option<CapsuleRun>> {
    let Some(selected) = window(frontier)? else {
        return Ok(None);
    };
    let pointers = frontier[selected.clone()].to_vec();
    let runs = try_join_all(pointers.iter().map(|pointer| async move {
        // Byte pressure may select an older prefix. Its final source is not
        // necessarily the new leaf, so reuse local bytes only by exact identity.
        if pointer.hash() == known_leaf.hash() {
            return Ok::<_, WriteError>(known_leaf.clone());
        }
        Ok::<_, WriteError>(
            crab_metadata::capsule_protocol::load_capsule_run(router, pointer).await?,
        )
    }))
    .await?;
    // Only the final run is published. Encode and authenticate it once rather
    // than copying every capsule through each discarded binary merge level.
    let compacted = tokio::task::spawn_blocking(move || CapsuleRun::compact(runs)).await??;
    let pointer = CapsulePointer::new(
        compacted.hash(),
        compacted.bytes().len() as u64,
        compacted.control_offset(),
        compacted.control_size(),
        compacted.footer_hash(),
        compacted.level(),
        compacted.transaction_ids(),
        compacted.newest_base_root_digest(),
    )?;
    frontier.splice(selected, [pointer]);
    Ok(Some(compacted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn pointer(index: usize, size: u64, count: usize) -> CapsulePointer {
        CapsulePointer::new(
            format!("{:064x}", index + 1),
            size,
            size - 1,
            1,
            "f".repeat(64),
            (usize::BITS - (count - 1).leading_zeros()) as u8,
            (0..count)
                .map(|n| format!("{:064x}", index * 1000 + n + 1))
                .collect(),
            "0".repeat(64),
        )
        .unwrap()
    }

    #[test]
    fn large_seed_stays_outside_small_suffix_compaction() {
        let mut frontier = vec![pointer(0, 1024 * 1024 * 1024, 1)];
        frontier.extend((1..=32).map(|index| pointer(index, 1024, 1)));
        assert_eq!(window(&frontier).unwrap(), Some(1..33));
    }

    #[test]
    fn byte_pressure_compacts_a_bounded_prefix_without_the_newest_leaf() {
        let frontier = (0..6)
            .map(|index| pointer(index, 12 * 1024 * 1024, 1))
            .collect::<Vec<_>>();
        assert_eq!(window(&frontier).unwrap(), Some(0..5));
    }

    #[test]
    fn large_new_leaf_remains_a_stable_segment() {
        let mut frontier = (0..32)
            .map(|index| pointer(index, 1024, 1))
            .collect::<Vec<_>>();
        frontier.push(pointer(32, 1024 * 1024 * 1024, 1));
        assert_eq!(window(&frontier).unwrap(), None);
    }

    #[test]
    fn hard_frontier_pressure_merges_the_cheapest_eligible_adjacent_pair() {
        let mut frontier = vec![pointer(0, 1024 * 1024 * 1024, 1)];
        frontier.extend((1..=64).map(|index| pointer(index, 1024, 1)));
        assert_eq!(window(&frontier).unwrap(), Some(1..3));
    }

    #[test]
    fn hard_frontier_pressure_keeps_run_count_within_the_wire_bound() {
        let frontier = (0..65)
            .map(|index| pointer(index, 1024, MAX_CAPSULES_PER_RUN))
            .collect::<Vec<_>>();
        assert!(window(&frontier).is_err());
    }

    #[tokio::test]
    async fn compaction_preserves_an_unread_large_source() {
        let router = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        let large = pointer(1000, 1024 * 1024 * 1024, 1);
        let mut frontier = vec![large.clone()];
        let mut leaf = None;
        for sequence in 1..=32 {
            let transaction = CapsuleTransaction::new(
                &"0".repeat(64),
                vec![CapsuleRefEdit::new(
                    "refs/heads/main",
                    None,
                    Some(format!("{sequence:040x}")),
                    None,
                )],
            )
            .unwrap();
            let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
            let run = crate::capsule_protocol::capsule_leaf_run(&capsule).unwrap();
            frontier.push(
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
                .unwrap(),
            );
            if sequence != 32 {
                router
                    .store()
                    .put(&router.capsule_path(run.hash()), run.bytes().clone())
                    .await
                    .unwrap();
            }
            leaf = Some(run);
        }
        // Neither the giant historical body nor the known local leaf exists remotely.
        let compacted = compact_ref_frontier(&router, &mut frontier, &leaf.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frontier[0], large);
        assert_eq!(frontier.len(), 2);
        assert_eq!(compacted.capsules().len(), 32);
    }

    #[tokio::test]
    async fn byte_pressure_retains_selected_sources_and_preserves_the_newest_leaf() {
        use crate::capsule_protocol::{initialize, open_root, publish, read_ref_head};
        use bytes::Bytes;
        use crab_metadata::capsule_protocol::{CapsuleSection, CapsuleSectionKind};
        let router = StoreLayout::new(
            Store::new(Arc::new(InMemory::new())),
            "repositories/test".to_owned(),
        );
        initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let mut transactions = Vec::new();
        let mut leaves = Vec::new();
        for sequence in 1..=6 {
            let base = open_root(&router).await.unwrap();
            let previous = (sequence > 1).then(|| format!("{:040x}", sequence - 1));
            let transaction = CapsuleTransaction::new(
                base.record().digest(),
                vec![CapsuleRefEdit::new(
                    "refs/heads/main",
                    previous,
                    Some(format!("{sequence:040x}")),
                    None,
                )],
            )
            .unwrap();
            transactions.push(transaction.id().unwrap());
            let capsule = Capsule::build(
                &transaction,
                Vec::new(),
                vec![CapsuleSection::new(
                    CapsuleSectionKind::FileData,
                    Bytes::from(vec![sequence; 12 * 1024 * 1024]),
                )],
            )
            .unwrap();
            leaves.push(
                crate::capsule_protocol::capsule_leaf_run(&capsule)
                    .unwrap()
                    .hash()
                    .to_owned(),
            );
            publish(&router, base, &transaction, &capsule)
                .await
                .unwrap();
        }
        let base = open_root(&router).await.unwrap();
        let head = read_ref_head(&router, base.record().root(), "refs/heads/main")
            .await
            .unwrap();
        let frontier = head.visible.frontier();
        assert_eq!(frontier.len(), 2);
        assert_eq!(frontier[1].hash(), leaves[5]);
        let compacted = crab_metadata::capsule_protocol::load_capsule_run(&router, &frontier[0])
            .await
            .unwrap();
        assert_eq!(compacted.transaction_ids(), transactions[..5]);
        let heads = router
            .store()
            .list_prefix(&router.capsule_source_heads_prefix())
            .await
            .unwrap()
            .into_iter()
            .map(|object| object.location)
            .collect::<Vec<_>>();
        let retained =
            crab_metadata::capsule_protocol::load_retained_capsule_sources(&router, &heads)
                .await
                .unwrap();
        let mut actual = retained
            .runs()
            .iter()
            .map(|run| run.hash().to_owned())
            .collect::<Vec<_>>();
        let mut expected = leaves[..5].to_vec();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }
}
