use std::sync::Arc;

use object_store::memory::InMemory;

use super::*;

fn fixture() -> StoreLayout<Store> {
    StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "browse-retention".into(),
    )
}

fn indexes(byte: char) -> BrowseIndexes {
    BrowseIndexes::new(byte.to_string().repeat(64), "b".repeat(64), None).unwrap()
}

#[tokio::test]
async fn unchanged_capture_does_not_grow_retention_or_rewrite_its_head() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    let before = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    let after = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn replacement_retains_prior_coverage_even_without_query_readiness() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    retain_browse_indexes(&layout, &indexes('c'), 2, None, &[])
        .await
        .unwrap();
    let roots = load_retained_browse_indexes(&layout).await.unwrap();
    assert_eq!(
        roots
            .iter()
            .map(|root| root.indexes().state_digest())
            .collect::<Vec<_>>(),
        vec!["c".repeat(64), "a".repeat(64)]
    );
}

#[tokio::test]
async fn explicit_pruning_keeps_current_readiness_and_relinks_retained_generations() {
    let layout = fixture();
    for (generation, byte) in [(1, 'a'), (2, 'c'), (3, 'd'), (4, 'e')] {
        retain_browse_indexes(&layout, &indexes(byte), generation, None, &[])
            .await
            .unwrap();
    }
    // A lifetime publication may precede a failed readiness CAS. Preserve the
    // older advertised root even when it is below the explicit pruning floor.
    layout
        .store()
        .create_strict_with_etag(
            &layout.capsule_browse_indexes_path(),
            indexes('a').encode().unwrap(),
        )
        .await
        .unwrap();
    prune_retained_browse_indexes(&layout, 3).await.unwrap();
    let roots = load_retained_browse_indexes(&layout).await.unwrap();
    assert_eq!(
        roots
            .iter()
            .map(RetainedBrowseIndexes::generation)
            .collect::<Vec<_>>(),
        vec![4, 3, 1]
    );
}

#[tokio::test]
async fn corrupt_or_missing_retained_roots_fail_closed() {
    for missing in [false, true] {
        let layout = fixture();
        retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
            .await
            .unwrap();
        let roots = load_retained_browse_indexes(&layout).await.unwrap();
        let path = layout.browse_retained_root_path(roots[0].hash());
        if missing {
            layout.store().delete(&path).await.unwrap();
        } else {
            layout
                .store()
                .put_overwrite(&path, Bytes::from_static(b"corrupt"))
                .await
                .unwrap();
        }
        assert!(load_retained_browse_indexes(&layout).await.is_err());
    }
}

#[tokio::test]
async fn backwards_publication_preserves_the_lifetime_head() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 2, None, &[])
        .await
        .unwrap();
    let before = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    assert!(
        retain_browse_indexes(&layout, &indexes('c'), 1, None, &[])
            .await
            .is_err()
    );
    assert_eq!(
        layout
            .store()
            .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
            .await
            .unwrap(),
        before
    );
}

#[test]
fn retention_head_bounds_are_admitted_before_publishing() {
    for (roots, bytes) in [
        (MAX_HISTORY_CHAIN_SEGMENTS, 1),
        (1, MAX_HISTORY_CHAIN_BYTES),
    ] {
        let old = Head {
            version: 1,
            hash: "a".repeat(64),
            roots,
            bytes,
        };
        assert!(Head::append(Some(&old), &Bytes::from_static(b"new root")).is_err());
    }
}

#[tokio::test]
async fn an_expired_publisher_cannot_replace_a_successor_lifetime_head() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    let (old, etag) = load_head(&layout).await.unwrap().unwrap();
    retain_browse_indexes(&layout, &indexes('c'), 2, None, &[])
        .await
        .unwrap();
    let result = publish_head(&layout, &old, Some(etag)).await;
    assert!(matches!(
        result,
        Err(MetadataError::Storage {
            source: crab_storage::StorageError::StateConflict { .. }
        })
    ));
}

#[tokio::test]
async fn captures_keep_ref_only_visibility_sources_independently_of_current_packaging() {
    use crate::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleRun, CapsuleTransaction};
    let layout = fixture();
    let transaction = CapsuleTransaction::new(
        &"a".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("1".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
    let run = CapsuleRun::leaf(capsule).unwrap();
    let source = CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.control_offset(),
        run.control_size(),
        run.footer_hash(),
        run.level(),
        run.transaction_ids(),
        run.newest_base_root_digest(),
    )
    .unwrap();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[source])
        .await
        .unwrap();
    retain_browse_indexes(&layout, &indexes('c'), 2, None, &[])
        .await
        .unwrap();
    let roots = load_retained_browse_indexes(&layout).await.unwrap();
    assert_eq!(
        roots[1].source_paths(&layout),
        vec![layout.capsule_path(run.hash())]
    );
}

#[tokio::test]
async fn retrying_completed_pruning_does_not_rewrite_the_lifetime_head() {
    let layout = fixture();
    for (generation, byte) in [(1, 'a'), (2, 'c'), (3, 'd')] {
        retain_browse_indexes(&layout, &indexes(byte), generation, None, &[])
            .await
            .unwrap();
    }
    prune_retained_browse_indexes(&layout, 2).await.unwrap();
    let before = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    prune_retained_browse_indexes(&layout, 2).await.unwrap();
    assert_eq!(
        layout
            .store()
            .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
            .await
            .unwrap(),
        before
    );
}
