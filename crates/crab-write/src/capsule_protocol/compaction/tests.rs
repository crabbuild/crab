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
    let retained = crab_metadata::capsule_protocol::load_retained_capsule_sources(&router, &heads)
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
