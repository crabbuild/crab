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
mod tests;
