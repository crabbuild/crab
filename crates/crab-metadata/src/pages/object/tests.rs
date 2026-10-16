use super::*;
#[cfg(feature = "storage")]
use crate::pages::PageWork;

fn rows(count: u32) -> Rows {
    (0..count)
        .map(|index| (index.to_be_bytes().to_vec(), vec![7; 96]))
        .collect()
}

#[test]
fn object_tree_authenticates_internal_routing_and_complete_order() {
    let expected = (0..100_000u32)
        .map(|index| {
            let mut key = vec![0; 32];
            key[28..].copy_from_slice(&index.to_be_bytes());
            (key, vec![7; 96])
        })
        .collect::<Rows>();
    let mut builder = Builder::default();
    let root = builder
        .tree(Kind::CatalogFiles, expected.clone())
        .unwrap()
        .unwrap();
    assert!(root.height >= 2);
    let mut pending = vec![root];
    let mut actual = Vec::new();
    while let Some(reference) = pending.pop() {
        let body = &builder.objects[&reference.hash];
        match decode(body, &reference, Kind::CatalogFiles).unwrap() {
            Node::Leaf(rows) => actual.extend(rows),
            Node::Branch(children) => pending.extend(children.into_iter().rev()),
        }
    }
    assert_eq!(actual, expected);
}

#[test]
fn object_page_rejects_corrupt_truncated_or_resigned_invalid_rows() {
    let page = encode(Kind::CatalogXorbChunks, Node::Leaf(rows(3))).unwrap();
    for mode in ["corrupt", "truncated", "count", "order"] {
        let mut body = page.bytes.to_vec();
        let mut reference = page.reference.clone();
        match mode {
            "corrupt" => body[HEADER] ^= 1,
            "truncated" => {
                body.pop();
            }
            "count" => body[10..14].copy_from_slice(&u32::MAX.to_le_bytes()),
            "order" => body[HEADER..HEADER + 4].copy_from_slice(&9u32.to_be_bytes()),
            _ => unreachable!(),
        }
        if mode != "corrupt" {
            reference.hash = *blake3::hash(&body).as_bytes();
            reference.bytes = body.len() as u32;
        }
        assert!(
            decode(&body, &reference, Kind::CatalogXorbChunks).is_err(),
            "{mode}"
        );
    }
}

#[test]
fn object_directory_rejects_overlapping_children_and_false_cardinality() {
    let mut builder = Builder::default();
    let root = builder
        .tree(Kind::CatalogXorbChunks, rows(1000))
        .unwrap()
        .unwrap();
    let Node::Branch(mut children) =
        decode(&builder.objects[&root.hash], &root, Kind::CatalogXorbChunks).unwrap()
    else {
        panic!("fixture must contain an internal directory");
    };
    children[1].first = children[0].last.clone();
    assert!(encode(Kind::CatalogXorbChunks, Node::Branch(children)).is_err());
    let mut wrong = root.clone();
    wrong.count += 1;
    wrong.last = (wrong.count - 1).to_be_bytes().to_vec();
    assert!(
        decode(
            &builder.objects[&root.hash],
            &wrong,
            Kind::CatalogXorbChunks
        )
        .is_err()
    );
}

#[cfg(feature = "storage")]
mod storage {
    use super::*;
    use crab_storage::{ImmutableWriteVerification, StorageObservation, StorageObserver, Store};
    use futures_util::TryStreamExt as _;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Observations(Mutex<Vec<StorageObservation>>);
    impl StorageObserver for Observations {
        fn started(&self, _: crab_storage::StorageOperation) {}
        fn finished(&self, observation: StorageObservation) {
            self.0.lock().unwrap().push(observation);
        }
    }
    fn fixture() -> (Store, ObjectPages, Arc<Observations>) {
        let observations = Arc::new(Observations::default());
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_immutable_write_verification(ImmutableWriteVerification::ReadbackRequired)
            .with_storage_observer(observations.clone());
        (
            store.clone(),
            ObjectPages::new(object_store::path::Path::from("catalog")),
            observations,
        )
    }

    #[tokio::test]
    async fn small_edits_retain_history_and_reuse_unmodified_subtrees() {
        for count in [1000, 100_000] {
            let (store, writer, observations) = fixture();
            let original = rows(count);
            let root = writer
                .edit(
                    &store,
                    None,
                    Kind::CatalogXorbChunks,
                    original.clone(),
                    &mut PageWork::new(64 * 1024 * 1024, 0),
                )
                .await
                .unwrap()
                .unwrap();
            let pages = ObjectPages::new(object_store::path::Path::from("catalog"));
            observations.0.lock().unwrap().clear();
            let change = vec![(17u32.to_be_bytes().to_vec(), vec![9; 96])];
            let next = pages
                .edit(
                    &store,
                    Some(&root),
                    Kind::CatalogXorbChunks,
                    change,
                    &mut PageWork::new(256 * 1024, 0),
                )
                .await
                .unwrap()
                .unwrap();
            let io = observations.0.lock().unwrap().clone();
            assert!(io.iter().map(|op| op.bytes_read).sum::<u64>() < 128 * 1024);
            assert!(io.iter().map(|op| op.bytes_written).sum::<u64>() < 128 * 1024);
            let mut expected = original.clone();
            expected[17].1 = vec![9; 96];
            assert_eq!(
                pages
                    .scan(
                        &store,
                        Some(&next),
                        Kind::CatalogXorbChunks,
                        &mut PageWork::new(64 * 1024 * 1024, 0)
                    )
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                pages
                    .scan(
                        &store,
                        Some(&root),
                        Kind::CatalogXorbChunks,
                        &mut PageWork::new(64 * 1024 * 1024, 0)
                    )
                    .await
                    .unwrap(),
                original
            );
        }
    }

    #[tokio::test]
    async fn batched_queries_charge_warm_pages_and_preserve_duplicate_order() {
        let (store, pages, observations) = fixture();
        let root = pages
            .edit(
                &store,
                None,
                Kind::CatalogXorbChunks,
                rows(1000),
                &mut PageWork::new(1024 * 1024, 0),
            )
            .await
            .unwrap()
            .unwrap();
        let keys = [17, 500, 17, 2000].map(|i: u32| i.to_be_bytes().to_vec());
        observations.0.lock().unwrap().clear();
        let expected = vec![
            Some(vec![7; 96]),
            Some(vec![7; 96]),
            Some(vec![7; 96]),
            None,
        ];
        let mut work = PageWork::new(256 * 1024, 0);
        assert_eq!(
            pages
                .lookup(
                    &store,
                    Some(&root),
                    Kind::CatalogXorbChunks,
                    &keys,
                    &mut work
                )
                .await
                .unwrap(),
            expected
        );
        assert!(observations.0.lock().unwrap().is_empty());
        assert!(
            pages
                .lookup(
                    &store,
                    Some(&root),
                    Kind::CatalogXorbChunks,
                    &keys,
                    &mut PageWork::new(100, 0)
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn missing_or_corrupt_objects_fail_before_returning_rows() {
        for corrupt_body in [false, true] {
            let (store, writer, _) = fixture();
            let root = writer
                .edit(
                    &store,
                    None,
                    Kind::CatalogXorbChunks,
                    rows(1000),
                    &mut PageWork::new(1024 * 1024, 0),
                )
                .await
                .unwrap()
                .unwrap();
            let path = writer.path(&root.hash);
            if corrupt_body {
                store
                    .put_overwrite(&path, Bytes::from(vec![0; root.bytes as usize]))
                    .await
                    .unwrap();
            } else {
                store.delete(&path).await.unwrap();
            }
            let cold = ObjectPages::new(object_store::path::Path::from("catalog"));
            assert!(
                cold.lookup(
                    &store,
                    Some(&root),
                    Kind::CatalogXorbChunks,
                    &[17u32.to_be_bytes().to_vec()],
                    &mut PageWork::new(256 * 1024, 0)
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn insertion_splits_preserve_every_key_and_streamed_dependency() {
        let (store, pages, _) = fixture();
        let mut expected = BTreeMap::new();
        let mut root = None;
        for pass in 0..8u32 {
            let changes = (0..1000u32)
                .map(|index| {
                    (
                        (index * 8 + pass).to_be_bytes().to_vec(),
                        vec![pass as u8; 100],
                    )
                })
                .collect::<Rows>();
            expected.extend(changes.clone());
            // Sparse keys exercise insertions before, inside and after old coverage.
            root = pages
                .edit(
                    &store,
                    root.as_ref(),
                    Kind::Children,
                    changes,
                    &mut PageWork::new(8 * 1024 * 1024, 0),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            pages
                .scan(
                    &store,
                    root.as_ref(),
                    Kind::Children,
                    &mut PageWork::new(8 * 1024 * 1024, 0)
                )
                .await
                .unwrap(),
            expected.into_iter().collect::<Rows>()
        );
        let mut work = PageWork::new(8 * 1024 * 1024, 0);
        let nodes = pages
            .walk(&store, root.as_ref(), Kind::Children, &mut work)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            nodes
                .iter()
                .filter_map(|(_, n)| match n.as_ref() {
                    Node::Leaf(rows) => Some(rows.len()),
                    _ => None,
                })
                .sum::<usize>(),
            8000
        );
    }
}
