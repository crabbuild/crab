use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use crab_storage::Store;
use object_store::{
    memory::InMemory,
    path::Path,
    throttle::{ThrottleConfig, ThrottledStore},
};

use super::*;

#[test]
fn radix_tree_round_trips_multi_level_aggregates() {
    let entries = (1..=70_000)
        .map(|page| {
            (
                page,
                DirectoryEntry {
                    page,
                    object: [1; 32],
                    offset: u64::from(page) * 100,
                    length: 100,
                    frame_hash: [2; 32],
                    checksum: u64::from(page) | crate::CHECKSUM_FLAG,
                },
            )
        })
        .collect();
    let tree = DirectoryTree::build(entries, 4096, 70_000).unwrap();
    assert_eq!(tree.height(), 2);
    assert_eq!(tree.root.aggregate.live_pages, 70_000);
    assert!(tree.objects.len() > 256);
}

#[tokio::test(start_paused = true)]
async fn streamed_tree_matches_canonical_root_without_retaining_objects() {
    let entries = (1..=70_000)
        .map(|page| DirectoryEntry {
            page,
            object: [1; 32],
            offset: u64::from(page) * 100,
            length: 100,
            frame_hash: [2; 32],
            checksum: u64::from(page) | crate::CHECKSUM_FLAG,
        })
        .collect::<Vec<_>>();
    let canonical = DirectoryTree::build(
        entries
            .iter()
            .cloned()
            .map(|entry| (entry.page, entry))
            .collect(),
        4096,
        70_000,
    )
    .unwrap();
    let delay = std::time::Duration::from_millis(10);
    let store = Store::new(Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: delay,
            ..ThrottleConfig::default()
        },
    )));
    let layout = CellStorageLayout::new(store.clone(), Path::from("streaming"), [3; 16]);
    let replica =
        super::super::CellReplica::new(layout, [1; 32], [2; 16], crate::Limits::default()).unwrap();
    let started = tokio::time::Instant::now();
    let streamed = build_initial_and_upload(entries.into_iter().map(Ok), 4096, 70_000, &replica)
        .await
        .unwrap();
    let mut level_nodes = 70_000_usize.div_ceil(FANOUT);
    let mut upload_intervals = 0_usize;
    loop {
        upload_intervals += level_nodes.div_ceil(super::super::OBJECT_UPLOAD_CONCURRENCY);
        if level_nodes == 1 {
            break;
        }
        level_nodes = level_nodes.div_ceil(FANOUT);
    }
    assert_eq!(
        started.elapsed(),
        delay * u32::try_from(upload_intervals).unwrap()
    );

    assert_eq!(streamed.root_digest(), canonical.root_digest());
    assert_eq!(streamed.height(), canonical.height());
    assert_eq!(streamed.checksum(), canonical.checksum());
    assert!(streamed.objects().is_empty());
    assert_eq!(
        store
            .list_prefix(&Path::from("streaming/cells/v1"))
            .await
            .unwrap()
            .len(),
        canonical.objects().len()
    );
}

#[tokio::test]
async fn incremental_update_rebuilds_the_last_height_two_branch() {
    let page_size = 4096;
    let base_pages = 401_938;
    let final_pages = 403_220;
    let lock = crate::ltx::lock_pgno(page_size);
    let cell = [8; 32];
    let incarnation = [9; 16];
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store.clone(), Path::from("update"), incarnation);
    let replica =
        super::super::CellReplica::new(layout.clone(), cell, incarnation, crate::Limits::default())
            .unwrap();
    let entry = |page: u32, object: [u8; 32]| DirectoryEntry {
        page,
        object,
        offset: u64::from(page) * 100,
        length: 100,
        frame_hash: [page as u8; 32],
        checksum: crate::CHECKSUM_FLAG | u64::from(page),
    };
    let pages = |end: u32, object| {
        (1..=end)
            .filter(|page| *page != lock)
            .map(|page| (page, entry(page, object)))
            .collect::<BTreeMap<_, _>>()
    };
    let base_entries = pages(base_pages, [1; 32]);
    let final_entries = pages(final_pages, [1; 32])
        .into_iter()
        .map(|(page, mut entry)| {
            if page <= 2 || page > base_pages {
                entry.object = [2; 32];
            }
            (page, entry)
        })
        .collect::<BTreeMap<_, _>>();
    let base_tree = DirectoryTree::build(base_entries, page_size, base_pages).unwrap();
    for object in &base_tree.objects {
        let path = layout.incarnation_object_path(
            &cell,
            &incarnation,
            &object.digest,
            CellObjectKind::Directory,
        );
        store
            .put(&path, Bytes::from(object.bytes.clone()))
            .await
            .unwrap();
    }
    let final_tree = DirectoryTree::build(final_entries.clone(), page_size, final_pages).unwrap();
    let changes = final_entries
        .into_iter()
        .filter(|(page, _)| *page <= 2 || *page > base_pages)
        .collect::<BTreeMap<_, _>>();
    let base_extents = BTreeMap::from([(
        [1; 32],
        ObjectExtent {
            kind: CellObjectKind::Ltx,
            ranges: std::iter::once(0..50_000_000).collect(),
        },
    )]);
    let final_extents = BTreeMap::from([
        (
            [1; 32],
            ObjectExtent {
                kind: CellObjectKind::Ltx,
                ranges: std::iter::once(0..50_000_000).collect(),
            },
        ),
        (
            [2; 32],
            ObjectExtent {
                kind: CellObjectKind::Ltx,
                ranges: std::iter::once(0..50_000_000).collect(),
            },
        ),
    ]);
    let updated = DirectoryTree::update(
        Verification {
            layout: &layout,
            cell: &cell,
            incarnation: &incarnation,
            page_size,
            database_pages: base_pages,
            extents: &base_extents,
            host: &replica.host,
            origin: crate::LtxReadOrigin::Cold,
        },
        base_tree.root_digest(),
        base_tree.height(),
        base_tree.root.aggregate,
        changes,
        base_pages,
        Verification {
            layout: &layout,
            cell: &cell,
            incarnation: &incarnation,
            page_size,
            database_pages: final_pages,
            extents: &final_extents,
            host: &replica.host,
            origin: crate::LtxReadOrigin::Cold,
        },
        final_tree.checksum(),
    )
    .await
    .unwrap();
    assert_eq!(updated.root_digest(), final_tree.root_digest());
    assert_eq!(updated.height(), final_tree.height());
}

#[test]
fn radix_tree_rejects_missing_allocated_page() {
    let entries = [1, 3]
        .into_iter()
        .map(|page| {
            (
                page,
                DirectoryEntry {
                    page,
                    object: [1; 32],
                    offset: u64::from(page) * 100,
                    length: 100,
                    frame_hash: [2; 32],
                    checksum: u64::from(page) | crate::CHECKSUM_FLAG,
                },
            )
        })
        .collect();
    assert!(DirectoryTree::build(entries, 4096, 3).is_err());
}
