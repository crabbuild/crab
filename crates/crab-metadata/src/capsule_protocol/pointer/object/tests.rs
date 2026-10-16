use super::*;
use crate::pages::PageWork;
use crab_storage::{StorageObservation, StorageObserver, Store};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observations(Mutex<Vec<StorageObservation>>);
impl StorageObserver for Observations {
    fn started(&self, _: crab_storage::StorageOperation) {}
    fn finished(&self, observation: StorageObservation) {
        self.0.lock().unwrap().push(observation);
    }
}

async fn fixture(catalog: &PointerCatalog) -> (CatalogPages, Store, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observations.clone());
    let prefix = object_store::path::Path::from("catalog");
    let (descriptor, objects) = Descriptor::build(catalog).unwrap();
    let root = descriptor.encode().unwrap();
    let identity = *blake3::hash(&root).as_bytes();
    let pages = CatalogPages::new(prefix, descriptor, identity, root.len() as u64).unwrap();
    for (hash, body) in objects {
        store
            .put_if_absent_verified(&pages.pages.path(&hash), body)
            .await
            .unwrap();
    }
    observations.0.lock().unwrap().clear();
    (pages, store, observations)
}

fn catalog(files: u32) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_shard("a".repeat(64), ShardCatalogEntry::new(1, Vec::new()))
        .unwrap();
    for index in 0..files {
        catalog
            .insert_file(
                format!("{index:064x}"),
                FileCatalogEntry::new(1, "a".repeat(64)),
            )
            .unwrap();
    }
    catalog
}

#[tokio::test]
async fn fixed_binding_update_reuses_unrelated_catalog_pages() {
    for count in [100, 10_000, 100_000] {
        let expected = catalog(count);
        let (pages, store, observations) = fixture(&expected).await;
        let mut delta = PointerCatalog::new();
        delta
            .insert_file(
                format!("{count:064x}"),
                FileCatalogEntry::new(1, "a".repeat(64)),
            )
            .unwrap();
        let descriptor = pages
            .apply(&store, &[delta.clone()], &mut PageWork::new(256 * 1024, 0))
            .await
            .unwrap();
        let io = observations.0.lock().unwrap().clone();
        assert!(io.iter().map(|op| op.bytes_read).sum::<u64>() <= 256 * 1024);
        assert!(io.iter().map(|op| op.bytes_written).sum::<u64>() <= 256 * 1024);
        assert_eq!(
            pages
                .full(&store, &mut PageWork::new(64 * 1024 * 1024, 0))
                .await
                .unwrap(),
            expected
        );
        let root = descriptor.encode().unwrap();
        let identity = *blake3::hash(&root).as_bytes();
        store
            .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
            .await
            .unwrap();
        let next = CatalogPages::new(
            object_store::path::Path::from("catalog"),
            descriptor,
            identity,
            root.len() as u64,
        )
        .unwrap();
        let mut expected = expected;
        expected.apply(&delta).unwrap();
        assert_eq!(
            next.full(&store, &mut PageWork::new(64 * 1024 * 1024, 0))
                .await
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn ordered_deltas_resolve_later_dependencies_and_preserve_immutable_contracts() {
    let (pages, store, _) = fixture(&catalog(1)).await;
    let mut file = PointerCatalog::new();
    file.insert_file("1".repeat(64), FileCatalogEntry::new(7, "b".repeat(64)))
        .unwrap();
    let mut shard = PointerCatalog::new();
    shard
        .insert_shard("b".repeat(64), ShardCatalogEntry::new(2, Vec::new()))
        .unwrap();
    assert!(
        pages
            .apply(&store, &[file.clone()], &mut PageWork::new(1024 * 1024, 0))
            .await
            .is_err()
    );
    let next = pages
        .apply(&store, &[file, shard], &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap();
    assert!(next.files.is_some() && next.shards.is_some());
    let mut conflict = PointerCatalog::new();
    conflict
        .insert_shard("a".repeat(64), ShardCatalogEntry::new(99, Vec::new()))
        .unwrap();
    assert!(
        pages
            .apply(&store, &[conflict], &mut PageWork::new(1024 * 1024, 0))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn new_lower_xorb_updates_shared_chunk_precedence() {
    let chunk = "c".repeat(64);
    let mut base = PointerCatalog::new();
    base.insert_xorb(
        "9".repeat(64),
        XorbCatalogEntry::new(
            80,
            "d".repeat(64),
            vec![
                XorbChunkEntry::new(chunk.clone(), 7),
                XorbChunkEntry::new(chunk.clone(), 7),
            ],
        ),
    )
    .unwrap();
    let (pages, store, _) = fixture(&base).await;
    let mut delta = PointerCatalog::new();
    delta
        .insert_xorb(
            "1".repeat(64),
            XorbCatalogEntry::new(
                90,
                "e".repeat(64),
                vec![XorbChunkEntry::new(chunk.clone(), 7)],
            ),
        )
        .unwrap();
    let descriptor = pages
        .apply(&store, &[delta.clone()], &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap();
    let root = descriptor.encode().unwrap();
    let identity = *blake3::hash(&root).as_bytes();
    store
        .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
        .await
        .unwrap();
    let next = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        descriptor,
        identity,
        root.len() as u64,
    )
    .unwrap();
    base.apply(&delta).unwrap();
    assert_eq!(
        next.full(&store, &mut PageWork::new(1024 * 1024, 0))
            .await
            .unwrap(),
        base
    );
}

#[tokio::test]
async fn reused_nested_pages_cannot_amplify_materialization_past_budget() {
    let mut catalog = PointerCatalog::new();
    for identity in 1..=16 {
        catalog
            .insert_xorb(
                format!("{identity:064x}"),
                XorbCatalogEntry::new(
                    80,
                    "d".repeat(64),
                    (0..1000)
                        .map(|_| XorbChunkEntry::new("c".repeat(64), 7))
                        .collect(),
                ),
            )
            .unwrap();
    }
    let (pages, store, _) = fixture(&catalog).await;
    let mut work = PageWork::new(256 * 1024, 0);
    let result = pages.full(&store, &mut work).await;
    assert!(
        result.is_err() && work.bytes <= 256 * 1024,
        "shared physical pages must not grant unbounded materialization"
    );
}

#[tokio::test]
async fn delta_row_admission_precedes_origin_reads_and_recipe_clones() {
    let (pages, store, observations) = fixture(&catalog(1)).await;
    let mut delta = PointerCatalog::new();
    for index in 0..1000 {
        delta
            .insert_shard(format!("{index:064x}"), ShardCatalogEntry::new(1, vec![]))
            .unwrap();
    }
    let result = pages
        .apply(&store, &[delta], &mut PageWork::new(4096, 0))
        .await;
    assert!(result.is_err() && observations.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn superseded_reverse_placement_still_validates_prior_dependency() {
    let chunk = "c".repeat(64);
    let mut base = PointerCatalog::new();
    base.insert_xorb(
        "9".repeat(64),
        XorbCatalogEntry::new(
            80,
            "d".repeat(64),
            vec![XorbChunkEntry::new(chunk.clone(), 7)],
        ),
    )
    .unwrap();
    let (pages, store, _) = fixture(&base).await;
    let mut descriptor = pages.descriptor.clone();
    let mut placement = hash(&"f".repeat(64)).unwrap().to_vec();
    placement.extend_from_slice(&0u32.to_be_bytes());
    placement.extend_from_slice(&7u32.to_be_bytes());
    descriptor.chunks = pages
        .pages
        .edit(
            &store,
            descriptor.chunks.as_ref(),
            Kind::CatalogChunks,
            vec![(hash(&chunk).unwrap().to_vec(), placement)],
            &mut PageWork::new(1024 * 1024, 0),
        )
        .await
        .unwrap();
    let root = descriptor.encode().unwrap();
    let identity = *blake3::hash(&root).as_bytes();
    store
        .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
        .await
        .unwrap();
    let forged = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        descriptor,
        identity,
        root.len() as u64,
    )
    .unwrap();
    let mut delta = PointerCatalog::new();
    delta
        .insert_xorb(
            "1".repeat(64),
            XorbCatalogEntry::new(90, "e".repeat(64), vec![XorbChunkEntry::new(chunk, 7)]),
        )
        .unwrap();
    assert!(
        forged
            .apply(&store, &[delta], &mut PageWork::new(1024 * 1024, 0))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn maintenance_streams_shared_recipes_without_materializing_the_catalog() {
    let mut catalog = PointerCatalog::new();
    for identity in 1..=64 {
        catalog
            .insert_xorb(
                format!("{identity:064x}"),
                XorbCatalogEntry::new(
                    80,
                    "d".repeat(64),
                    (0..2048)
                        .map(|_| XorbChunkEntry::new("c".repeat(64), 7))
                        .collect(),
                ),
            )
            .unwrap();
    }
    let (_, expected) = Descriptor::build(&catalog).unwrap();
    let (pages, store, _) = fixture(&catalog).await;
    drop(catalog);
    let maximum = 2 * 1024 * 1024;
    assert!(
        pages
            .full(&store, &mut PageWork::new(maximum, 0))
            .await
            .is_err()
    );
    let body = pages.descriptor.encode().unwrap();
    let pages = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        pages.descriptor.clone(),
        *blake3::hash(&body).as_bytes(),
        body.len() as u64,
    )
    .unwrap();
    let actual = pages.inspected_objects(&store, maximum).await.unwrap();
    let expected = expected
        .into_iter()
        .map(|(hash, body)| (pages.pages.path(&hash), body.len() as u64))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual.into_iter().collect::<BTreeMap<_, _>>(), expected);
}

#[tokio::test]
async fn streamed_inspection_rejects_resigned_broken_dependency_edges() {
    for scenario in [
        "file-shard",
        "shard-xorb",
        "shard-data",
        "empty-shard",
        "extra-placement",
        "absent-placement-xorb",
        "later-occurrence",
        "placement-size",
        "late-recipe",
        "empty-xorb",
        "empty-chunk",
        "missing-recipe-page",
        "shared-routing",
    ] {
        let mut catalog = PointerCatalog::new();
        catalog
            .insert_xorb(
                "9".repeat(64),
                XorbCatalogEntry::new(
                    80,
                    "d".repeat(64),
                    vec![
                        XorbChunkEntry::new("c".repeat(64), 7),
                        XorbChunkEntry::new("c".repeat(64), 7),
                    ],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(
                "a".repeat(64),
                ShardCatalogEntry::new(1, vec!["9".repeat(64)]),
            )
            .unwrap();
        catalog
            .insert_file("f".repeat(64), FileCatalogEntry::new(14, "a".repeat(64)))
            .unwrap();
        if scenario == "shared-routing" {
            catalog
                .insert_xorb("1".repeat(64), catalog.xorbs()[&"9".repeat(64)].clone())
                .unwrap();
        }
        let (pages, store, _) = fixture(&catalog).await;
        let mut descriptor = pages.descriptor.clone();
        let mut work = PageWork::new(1024 * 1024, 0);
        let xorb_values = pages
            .rows(
                &store,
                Kind::CatalogXorbs,
                &[hash(&"9".repeat(64)).unwrap().to_vec()],
                &mut work,
            )
            .await
            .unwrap();
        let mut xorb: XorbRow = from_json(xorb_values[0].as_ref().unwrap()).unwrap();
        let recipe_path = pages.pages.path(&xorb.chunks.hash);
        let mut kind = Kind::CatalogChunks;
        let mut key = hash(&"c".repeat(64)).unwrap().to_vec();
        let mut value = hash(&"9".repeat(64)).unwrap().to_vec();
        value.extend_from_slice(&0u32.to_be_bytes());
        value.extend_from_slice(&7u32.to_be_bytes());
        match scenario {
            "file-shard" => {
                kind = Kind::CatalogFiles;
                key = hash(&"f".repeat(64)).unwrap().to_vec();
                value = 14u64.to_be_bytes().to_vec();
                value.extend_from_slice(&hash(&"b".repeat(64)).unwrap());
            }
            "shard-xorb" | "shard-data" | "empty-shard" => {
                let nested_key = (if scenario == "shard-xorb" { "b" } else { "9" }).repeat(64);
                let nested_value = if scenario == "shard-data" {
                    vec![1]
                } else {
                    Vec::new()
                };
                let nested = pages
                    .pages
                    .edit(
                        &store,
                        None,
                        Kind::CatalogShardXorbs,
                        vec![(hash(&nested_key).unwrap().to_vec(), nested_value)],
                        &mut work,
                    )
                    .await
                    .unwrap();
                kind = Kind::CatalogShards;
                key = hash(&"a".repeat(64)).unwrap().to_vec();
                value = json(&ShardRow {
                    encoded_size: u64::from(scenario != "empty-shard"),
                    xorbs: nested,
                })
                .unwrap();
            }
            "extra-placement" => key = hash(&"b".repeat(64)).unwrap().to_vec(),
            "absent-placement-xorb" => value[..32].copy_from_slice(&hash(&"1".repeat(64)).unwrap()),
            "later-occurrence" => value[32..36].copy_from_slice(&1u32.to_be_bytes()),
            "placement-size" => value[36..].copy_from_slice(&9u32.to_be_bytes()),
            "late-recipe" | "empty-xorb" | "empty-chunk" | "shared-routing" => {
                if scenario == "empty-xorb" {
                    xorb.encoded_size = 0;
                } else if scenario == "shared-routing" {
                    xorb.chunks.count = 3;
                    xorb.chunks.last = 2u32.to_be_bytes().to_vec();
                } else {
                    let mut chunk = hash(&"c".repeat(64)).unwrap().to_vec();
                    chunk.extend_from_slice(&u32::from(scenario != "empty-chunk").to_be_bytes());
                    xorb.chunks = pages
                        .pages
                        .edit(
                            &store,
                            None,
                            Kind::CatalogXorbChunks,
                            vec![(
                                u32::from(scenario == "late-recipe").to_be_bytes().to_vec(),
                                chunk,
                            )],
                            &mut work,
                        )
                        .await
                        .unwrap()
                        .unwrap();
                }
                kind = Kind::CatalogXorbs;
                key = hash(&"9".repeat(64)).unwrap().to_vec();
                value = json(&xorb).unwrap();
            }
            "missing-recipe-page" => (),
            _ => unreachable!(),
        }
        if scenario != "missing-recipe-page" {
            let previous = match kind {
                Kind::CatalogFiles => descriptor.files.as_ref(),
                Kind::CatalogShards => descriptor.shards.as_ref(),
                Kind::CatalogXorbs => descriptor.xorbs.as_ref(),
                Kind::CatalogChunks => descriptor.chunks.as_ref(),
                _ => unreachable!(),
            };
            let next = pages
                .pages
                .edit(&store, previous, kind, vec![(key, value)], &mut work)
                .await
                .unwrap();
            match kind {
                Kind::CatalogFiles => descriptor.files = next,
                Kind::CatalogShards => descriptor.shards = next,
                Kind::CatalogXorbs => descriptor.xorbs = next,
                Kind::CatalogChunks => descriptor.chunks = next,
                _ => unreachable!(),
            }
        }
        let root = descriptor.encode().unwrap();
        let identity = *blake3::hash(&root).as_bytes();
        store
            .put_if_absent_verified(&pages.pages.path(&identity), root.clone())
            .await
            .unwrap();
        if scenario == "missing-recipe-page" {
            store.delete(&recipe_path).await.unwrap();
        }
        // A fresh reader must authenticate the forged closure, not reuse fixture nodes.
        let forged = CatalogPages::new(
            object_store::path::Path::from("catalog"),
            descriptor,
            identity,
            root.len() as u64,
        )
        .unwrap();
        assert!(
            forged
                .full(&store, &mut PageWork::new(1024 * 1024, 0))
                .await
                .is_err(),
            "full: {scenario}"
        );
        assert!(
            forged.inspected_objects(&store, 1024 * 1024).await.is_err(),
            "stream: {scenario}"
        );
    }
}

#[tokio::test]
async fn streamed_inspection_bounds_io_for_distinct_recipes_beyond_the_page_cache() {
    let mut catalog = PointerCatalog::new();
    for identity in 0..1000u32 {
        let chunks = (0..128u32)
            .map(|index| {
                let mut input = identity.to_be_bytes().to_vec();
                input.extend_from_slice(&index.to_be_bytes());
                XorbChunkEntry::new(blake3::hash(&input).to_hex().to_string(), 7)
            })
            .collect();
        catalog
            .insert_xorb(
                format!("{identity:064x}"),
                XorbCatalogEntry::new(80, "d".repeat(64), chunks),
            )
            .unwrap();
    }
    let (pages, store, observations) = fixture(&catalog).await;
    drop(catalog);
    let maximum = 64 * 1024 * 1024;
    drop(
        pages
            .full(&store, &mut PageWork::new(maximum, 0))
            .await
            .unwrap(),
    );
    let full = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|op| op.bytes_read)
        .sum::<u64>();
    observations.0.lock().unwrap().clear();
    let body = pages.descriptor.encode().unwrap();
    let cold = CatalogPages::new(
        object_store::path::Path::from("catalog"),
        pages.descriptor.clone(),
        *blake3::hash(&body).as_bytes(),
        body.len() as u64,
    )
    .unwrap();
    cold.inspected_objects(&store, maximum).await.unwrap();
    let streamed = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|op| op.bytes_read)
        .sum::<u64>();
    assert!(
        observations
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|op| op.bytes_written == 0)
    );
    assert!(
        streamed <= 4 * full,
        "streaming must not trade catalog-sized memory for repeated cold joins: {streamed} versus {full}"
    );
}
