use super::*;
use crate::capsule_protocol::pointer::object::{CatalogPages, Descriptor};
use crab_storage::{StorageObservation, StorageObserver};
use std::sync::{Arc, Mutex};

fn binding(file: u32, shard: u32, size: u64) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_file(
            format!("{file:064x}"),
            FileCatalogEntry::new(size, format!("{shard:064x}")),
        )
        .unwrap();
    catalog
}

fn shard(identity: u32, xorbs: Vec<String>) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_shard(format!("{identity:064x}"), ShardCatalogEntry::new(7, xorbs))
        .unwrap();
    catalog
}

async fn inspect(deltas: Vec<PointerCatalog>) -> Result<Vec<String>> {
    let mut inspector = Inspector::new(16 * 1024 * 1024);
    let checked = async {
        for (rank, delta) in deltas.into_iter().enumerate() {
            inspector.delta(delta, rank as u64).await?;
        }
        Ok(())
    }
    .await;
    inspector.finish(checked).await?.into_shard_hashes().await
}

#[tokio::test]
async fn spilled_capture_matches_assembly_with_late_and_shadowed_dependencies() {
    let mut initial = PointerCatalog::new();
    // Keep the missing old binding in the first run; a later rank must shadow it.
    initial.merge_delta(&binding(0, 99, 3)).unwrap();
    for index in 1..13_000 {
        initial.merge_delta(&binding(index, 1, 3)).unwrap();
    }
    let mut xorb = PointerCatalog::new();
    xorb.insert_xorb(
        "a".repeat(64),
        XorbCatalogEntry::new(
            8,
            "b".repeat(64),
            vec![XorbChunkEntry::new("c".repeat(64), 3)],
        ),
    )
    .unwrap();
    let deltas = vec![
        initial,
        binding(0, 2, 3),
        shard(1, vec!["a".repeat(64)]),
        shard(2, Vec::new()),
        xorb,
    ];
    let mut expected = PointerCatalogAssembly::new(PointerCatalog::new());
    for delta in &deltas {
        expected.apply(delta).unwrap();
    }
    let expected = expected
        .finish()
        .unwrap()
        .shards
        .into_keys()
        .collect::<Vec<_>>();
    assert_eq!(inspect(deltas).await.unwrap(), expected);
}

#[tokio::test]
async fn captured_merge_rejects_conflicts_and_incomplete_final_closures() {
    for scenario in [
        "file-size",
        "shard-descriptor",
        "xorb-descriptor",
        "missing-shard",
        "missing-xorb",
    ] {
        let mut deltas = vec![binding(0, 1, 3), shard(1, Vec::new())];
        match scenario {
            "file-size" => deltas.push(binding(0, 99, 4)),
            "shard-descriptor" => {
                let mut conflicting = shard(1, Vec::new());
                conflicting
                    .shards
                    .get_mut(&format!("{:064x}", 1))
                    .unwrap()
                    .encoded_size = 9;
                deltas.push(conflicting);
            }
            "xorb-descriptor" => {
                for size in [8, 9] {
                    let mut catalog = PointerCatalog::new();
                    catalog
                        .insert_xorb(
                            "a".repeat(64),
                            XorbCatalogEntry::new(
                                size,
                                "b".repeat(64),
                                vec![XorbChunkEntry::new("c".repeat(64), 3)],
                            ),
                        )
                        .unwrap();
                    deltas.push(catalog);
                }
            }
            "missing-shard" => deltas.push(binding(0, 99, 3)),
            "missing-xorb" => deltas.push(shard(2, vec!["a".repeat(64)])),
            _ => unreachable!(),
        }
        let mut oracle = PointerCatalogAssembly::new(PointerCatalog::new());
        let expected = deltas
            .iter()
            .try_for_each(|delta| oracle.apply(delta))
            .and_then(|()| oracle.finish());
        assert!(expected.is_err(), "oracle: {scenario}");
        assert!(inspect(deltas).await.is_err(), "spill: {scenario}");
    }
}

#[tokio::test]
async fn failed_or_abandoned_capture_retires_private_spill() {
    for abandon in [false, true] {
        let parent = tempfile::tempdir().unwrap();
        let space = ScratchSpace::create(parent.path(), 8 * 1024 * 1024)
            .await
            .unwrap();
        let mut inspector = Inspector {
            space: Some(space),
            rows: Reducer::new(),
            maximum: 1024 * 1024,
        };
        for index in 0..5000 {
            inspector.delta(binding(index, 1, 3), 0).await.unwrap();
        }
        assert_ne!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
        if abandon {
            drop(inspector);
        } else {
            assert!(
                inspector
                    .finish(Err(corrupt("source disappeared")))
                    .await
                    .is_err()
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while std::fs::read_dir(parent.path()).unwrap().count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[test]
fn logical_spill_rejects_malformed_framing_values_and_lengths() {
    let mut value = 3u64.to_be_bytes().to_vec();
    value.extend_from_slice(&[1; 32]);
    let bytes = Entry::encode(&[Entry::new(0, &"a".repeat(64), 7, value).unwrap()]).unwrap();
    for scenario in ["count", "truncated", "trailing", "kind", "length"] {
        let mut invalid = bytes.to_vec();
        match scenario {
            "count" => invalid[8..12].copy_from_slice(&u32::MAX.to_be_bytes()),
            "truncated" => {
                invalid.pop();
            }
            "trailing" => invalid.push(0),
            "kind" => invalid[12] = 9,
            "length" => invalid[53..57].copy_from_slice(&u32::MAX.to_be_bytes()),
            _ => unreachable!(),
        }
        assert!(Entry::decode(invalid.into()).is_err(), "{scenario}");
    }
}

#[derive(Default)]
struct Observations(Mutex<Vec<StorageObservation>>);
impl StorageObserver for Observations {
    fn started(&self, _: crab_storage::StorageOperation) {}
    fn finished(&self, operation: StorageObservation) {
        self.0.lock().unwrap().push(operation);
    }
}

#[tokio::test]
async fn checkpoint_rows_stream_without_per_file_dependency_joins() {
    let mut catalog = shard(1, vec!["a".repeat(64)]);
    catalog
        .insert_xorb(
            "a".repeat(64),
            XorbCatalogEntry::new(
                8,
                "b".repeat(64),
                vec![XorbChunkEntry::new("c".repeat(64), 3)],
            ),
        )
        .unwrap();
    for index in 0..100_000 {
        catalog.merge_delta(&binding(index, 1, 3)).unwrap();
    }
    let observations = Arc::new(Observations::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observations.clone());
    let prefix = object_store::path::Path::from("catalog");
    let (descriptor, objects) = Descriptor::build(&catalog).unwrap();
    let body = descriptor.encode().unwrap();
    let identity = *blake3::hash(&body).as_bytes();
    let pages = CatalogPages::new(prefix, descriptor, identity, body.len() as u64).unwrap();
    for (hash, body) in objects {
        store
            .put_if_absent_verified(&pages.pages.path(&hash), body)
            .await
            .unwrap();
    }
    observations.0.lock().unwrap().clear();
    let maximum = 64 * 1024 * 1024;
    assert_eq!(
        pages
            .full(&store, &mut crate::pages::PageWork::new(maximum, 0))
            .await
            .unwrap(),
        catalog
    );
    let full = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|op| op.bytes_read)
        .sum::<u64>();
    observations.0.lock().unwrap().clear();
    let cold = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        pages.descriptor.clone(),
        identity,
        pages.descriptor.encode().unwrap().len() as u64,
    )
    .unwrap();
    let mut inspector = Inspector::new(maximum);
    let checked = cold.inspect_into(&store, maximum, &mut inspector).await;
    assert_eq!(
        inspector
            .finish(checked)
            .await
            .unwrap()
            .into_shard_hashes()
            .await
            .unwrap(),
        catalog.shards.into_keys().collect::<Vec<_>>()
    );
    let io = observations.0.lock().unwrap();
    let streamed = io.iter().map(|op| op.bytes_read).sum::<u64>();
    tracing::debug!(
        origin_bytes = streamed,
        full_materializer_bytes = full,
        "logical catalog maintenance"
    );
    assert!(streamed <= 4 * full, "{streamed} versus {full}");
    assert!(io.iter().all(|op| op.bytes_written == 0));
}

async fn cursor_fixture(
    files: u32,
) -> (
    tempfile::TempDir,
    PointerCatalogInspection,
    Vec<PointerCatalogObject>,
) {
    let parent = tempfile::tempdir().unwrap();
    let space = ScratchSpace::create(parent.path(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let mut inspector = Inspector {
        space: Some(space),
        rows: Reducer::new(),
        maximum: 1024 * 1024,
    };
    let xorb = XorbCatalogEntry::new(
        80,
        "d".repeat(64),
        vec![
            XorbChunkEntry::new("c".repeat(64), 3),
            XorbChunkEntry::new("b".repeat(64), 9),
            XorbChunkEntry::new("c".repeat(64), 7),
        ],
    );
    let shard = ShardCatalogEntry::new(7, vec!["a".repeat(64)]);
    let expected = vec![
        PointerCatalogObject::Shard {
            hash: format!("{:064x}", 1),
            entry: shard.clone(),
        },
        PointerCatalogObject::Xorb {
            hash: "a".repeat(64),
            entry: xorb.clone(),
        },
    ];
    let mut catalog = PointerCatalog::new();
    for file in 0..files {
        catalog.merge_delta(&binding(file, 1, 19)).unwrap();
    }
    catalog.insert_shard(format!("{:064x}", 1), shard).unwrap();
    catalog.insert_xorb("a".repeat(64), xorb).unwrap();
    inspector.delta(catalog, 0).await.unwrap();
    let cursor = inspector.finish(Ok(())).await.unwrap();
    (parent, cursor, expected)
}

#[tokio::test]
async fn verified_cursor_preserves_complete_descriptors_across_spilled_file_rows() {
    let (_parent, mut cursor, expected) = cursor_fixture(5000).await;
    let before = cursor.counts();
    let result: Result<_> = async {
        let mut values = Vec::new();
        while let Some(object) = cursor.next_object().await? {
            values.push(object);
        }
        Ok(values)
    }
    .await;
    let after = cursor.counts();
    let counts = PointerCatalogCounts {
        files: 5000,
        shards: 1,
        xorbs: 1,
    };
    assert_eq!(
        (before, after, cursor.finish(result).await.unwrap()),
        (counts, counts, expected)
    );
}

#[tokio::test]
async fn verified_cursor_retires_spill_after_early_stop_error_abandonment_and_tampering() {
    for outcome in ["early", "error", "drop", "tamper"] {
        let (parent, mut cursor, _) = cursor_fixture(64).await;
        match outcome {
            "early" => {
                cursor.next_object().await.unwrap();
                cursor.finish(Ok::<_, MetadataError>(())).await.unwrap();
            }
            "error" => {
                let result: Result<()> = Err(contract_error("body check failed"));
                assert!(matches!(
                    cursor.finish(result).await,
                    Err(MetadataError::CapsuleContract { .. })
                ));
            }
            "drop" => drop(cursor),
            "tamper" => {
                let directory = std::fs::read_dir(parent.path())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                for file in std::fs::read_dir(directory).unwrap() {
                    std::fs::write(file.unwrap().path(), b"changed").unwrap();
                }
                let result = cursor.next_object().await;
                assert!(matches!(
                    result,
                    Err(MetadataError::Storage {
                        source: crab_storage::StorageError::CorruptObject { .. }
                    })
                ));
                assert!(cursor.next_object().await.is_err());
                assert!(cursor.finish(result).await.is_err());
            }
            _ => unreachable!(),
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while std::fs::read_dir(parent.path()).unwrap().count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn verified_cursor_preserves_operation_errors_when_cleanup_also_fails() {
    let (parent, cursor, _) = cursor_fixture(1).await;
    let directory = std::fs::read_dir(parent.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_dir_all(&directory).unwrap();
    std::fs::write(&directory, b"obstruct cleanup").unwrap();
    let result: Result<()> = Err(contract_error("body check failed"));
    assert!(
        matches!(cursor.finish(result).await, Err(MetadataError::CapsuleContract { reason, .. }) if reason == "body check failed")
    );
}

#[test]
fn cancelled_pending_cursor_read_resumes_at_the_same_object() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_parent, mut cursor, mut expected) = cursor_fixture(64).await;
        let (release, gate) = std::sync::mpsc::channel();
        let (started, entered) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            gate.recv().unwrap();
        });
        entered.await.unwrap();
        let mut pending = Box::pin(cursor.next_object());
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(pending.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(pending);
        release.send(()).unwrap();
        blocker.await.unwrap();
        let actual = cursor.next_object().await.unwrap();
        cursor.finish(Ok::<_, MetadataError>(())).await.unwrap();
        assert_eq!(actual, Some(expected.remove(0)));
    });
}
