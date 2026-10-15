use std::sync::Arc;

use bytes::Bytes;
use object_store::memory::InMemory;

use super::*;
use crate::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleRun, CapsuleTransaction};

fn fixture() -> StoreLayout<Store> {
    StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "source-lifetime".into(),
    )
}

async fn source(layout: &StoreLayout<Store>, sequence: u64) -> CapsulePointer {
    let transaction = CapsuleTransaction::new(
        &"a".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(format!("{sequence:040x}")),
            None,
        )],
    )
    .unwrap();
    let run =
        CapsuleRun::leaf(Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap()).unwrap();
    layout
        .store()
        .put(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
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
    .unwrap()
}

async fn heads(layout: &StoreLayout<Store>) -> Vec<Path> {
    layout
        .store()
        .list_prefix(&layout.capsule_source_heads_prefix())
        .await
        .unwrap()
        .into_iter()
        .map(|meta| meta.location)
        .collect()
}

#[tokio::test]
async fn repeated_retirement_does_not_append_or_rewrite_lifetime() {
    let layout = fixture();
    let pointer = source(&layout, 1).await;
    retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        std::slice::from_ref(&pointer),
    )
    .await
    .unwrap();
    let path = heads(&layout).await.remove(0);
    let before = layout.store().get_with_etag(&path).await.unwrap();
    retain_capsule_sources(&layout, "refs/heads/main", 0, &[pointer])
        .await
        .unwrap();
    assert_eq!(layout.store().get_with_etag(&path).await.unwrap(), before);
}

#[tokio::test]
async fn independent_ref_heads_share_source_identity_without_sharing_cas() {
    let layout = fixture();
    let pointer = source(&layout, 1).await;
    for name in ["refs/heads/main", "refs/heads/other"] {
        retain_capsule_sources(&layout, name, 0, std::slice::from_ref(&pointer))
            .await
            .unwrap();
    }
    let paths = heads(&layout).await;
    let retained = load_retained_capsule_sources(&layout, &paths)
        .await
        .unwrap();
    assert_eq!((paths.len(), retained.runs()), (2, [pointer].as_slice()));
}

#[tokio::test]
async fn pruning_relinks_kept_generations_and_rejects_late_old_publishers() {
    let layout = fixture();
    let mut sources = Vec::new();
    for generation in 0..3 {
        let pointer = source(&layout, generation + 1).await;
        retain_capsule_sources(
            &layout,
            "refs/heads/main",
            generation,
            std::slice::from_ref(&pointer),
        )
        .await
        .unwrap();
        sources.push(pointer);
    }
    prune_retained_capsule_sources(&layout, 2).await.unwrap();
    let paths = heads(&layout).await;
    let retained = load_retained_capsule_sources(&layout, &paths)
        .await
        .unwrap();
    assert_eq!(retained.runs(), &sources[2..]);
    assert!(
        retain_capsule_sources(&layout, "refs/heads/main", 0, &sources[..1])
            .await
            .is_err()
    );
    let before = layout.store().get_with_etag(&paths[0]).await.unwrap();
    prune_retained_capsule_sources(&layout, 2).await.unwrap();
    assert_eq!(
        layout.store().get_with_etag(&paths[0]).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn empty_pruned_history_keeps_its_generation_floor() {
    let layout = fixture();
    let pointer = source(&layout, 1).await;
    retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        std::slice::from_ref(&pointer),
    )
    .await
    .unwrap();
    prune_retained_capsule_sources(&layout, 1).await.unwrap();
    let paths = heads(&layout).await;
    let retained = load_retained_capsule_sources(&layout, &paths)
        .await
        .unwrap();
    let mut expected = paths.clone();
    expected.push(layout.capsule_source_floor_path());
    expected.sort();
    assert_eq!(
        (retained.paths(), retained.runs()),
        (expected.as_slice(), [].as_slice())
    );
    assert!(
        retain_capsule_sources(&layout, "refs/heads/main", 0, &[pointer])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pruning_rejects_old_publishers_for_previously_unseen_refs() {
    let layout = fixture();
    let pointer = source(&layout, 1).await;
    prune_retained_capsule_sources(&layout, 1).await.unwrap();
    assert!(
        retain_capsule_sources(&layout, "refs/heads/new", 0, std::slice::from_ref(&pointer))
            .await
            .is_err()
    );
    retain_capsule_sources(&layout, "refs/heads/new", 1, std::slice::from_ref(&pointer))
        .await
        .unwrap();
    let retained = load_retained_capsule_sources(&layout, &heads(&layout).await)
        .await
        .unwrap();
    assert_eq!(retained.runs(), &[pointer]);
}

#[tokio::test]
async fn late_first_head_cannot_restore_sources_below_the_repository_floor() {
    let layout = fixture();
    let pointer = source(&layout, 1).await;
    retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        std::slice::from_ref(&pointer),
    )
    .await
    .unwrap();
    let path = heads(&layout).await.remove(0);
    let (old, _) = load_head(&layout, &path).await.unwrap().unwrap();
    layout.store().delete(&path).await.unwrap();
    prune_retained_capsule_sources(&layout, 1).await.unwrap();
    // Model an expired publisher that read the absent head before pruning,
    // then won its first-head create after the pruner's enumeration.
    publish_head(&layout, &old, None).await.unwrap();
    let retained = load_retained_capsule_sources(&layout, &[path])
        .await
        .unwrap();
    assert!(retained.runs().is_empty());
    assert!(
        !retained
            .paths()
            .contains(&layout.capsule_path(pointer.hash()))
    );
}

#[tokio::test]
async fn missing_pruning_floor_rejects_newer_per_ref_authority() {
    let layout = fixture();
    let pointer = source(&layout, 1).await;
    prune_retained_capsule_sources(&layout, 1).await.unwrap();
    retain_capsule_sources(&layout, "refs/heads/main", 1, &[pointer])
        .await
        .unwrap();
    layout
        .store()
        .delete(&layout.capsule_source_floor_path())
        .await
        .unwrap();
    assert!(
        load_retained_capsule_sources(&layout, &heads(&layout).await)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn expired_publisher_cannot_overwrite_successor_lifetime() {
    let layout = fixture();
    let first = source(&layout, 1).await;
    retain_capsule_sources(&layout, "refs/heads/main", 0, &[first])
        .await
        .unwrap();
    let path = heads(&layout).await.remove(0);
    let (old, etag) = load_head(&layout, &path).await.unwrap().unwrap();
    let second = source(&layout, 2).await;
    retain_capsule_sources(&layout, "refs/heads/main", 0, &[second])
        .await
        .unwrap();
    assert!(matches!(
        publish_head(&layout, &old, Some(etag)).await,
        Err(MetadataError::Storage {
            source: crab_storage::StorageError::StateConflict { .. }
        })
    ));
}

#[tokio::test]
async fn missing_or_corrupt_source_segments_stop_dependency_discovery() {
    for missing in [false, true] {
        let layout = fixture();
        let pointer = source(&layout, 1).await;
        retain_capsule_sources(&layout, "refs/heads/main", 0, &[pointer])
            .await
            .unwrap();
        let paths = heads(&layout).await;
        let (head, _) = load_head(&layout, &paths[0]).await.unwrap().unwrap();
        let path = layout.capsule_source_segment_path(&head.tip.unwrap().hash);
        if missing {
            layout.store().delete(&path).await.unwrap();
        } else {
            layout
                .store()
                .put_overwrite(&path, Bytes::from_static(b"corrupt"))
                .await
                .unwrap();
        }
        assert!(
            load_retained_capsule_sources(&layout, &paths)
                .await
                .is_err()
        );
    }
}

#[test]
fn segment_admission_checks_chain_bounds_before_immutable_upload() {
    for (roots, bytes) in [
        (MAX_HISTORY_CHAIN_SEGMENTS, 1),
        (1, MAX_HISTORY_CHAIN_BYTES),
    ] {
        let tip = Tip {
            hash: "a".repeat(64),
            roots,
            bytes,
        };
        assert!(Tip::append(Some(&tip), "b".repeat(64), 1).is_err());
    }
}
