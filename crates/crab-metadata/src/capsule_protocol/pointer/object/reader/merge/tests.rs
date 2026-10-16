use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn immutable_recipe_equality_ignores_valid_page_partitioning() {
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
    let prefix = Path::from("catalog");
    let mut catalog = PointerCatalog::new();
    let ids = (1..=2000)
        .map(|id| format!("{id:064x}"))
        .collect::<Vec<_>>();
    for identity in &ids {
        catalog
            .insert_xorb(
                identity.clone(),
                XorbCatalogEntry::new(
                    70,
                    "d".repeat(64),
                    vec![XorbChunkEntry::new("c".repeat(64), 7)],
                ),
            )
            .unwrap();
    }
    catalog
        .insert_shard("a".repeat(64), ShardCatalogEntry::new(40, ids.clone()))
        .unwrap();
    catalog
        .insert_file("b".repeat(64), FileCatalogEntry::new(7, "a".repeat(64)))
        .unwrap();
    let (descriptor, objects) = Descriptor::build(&catalog).unwrap();
    let body = descriptor.encode().unwrap();
    let older = CatalogPages::new(
        prefix.clone(),
        descriptor.clone(),
        *blake3::hash(&body).as_bytes(),
        body.len() as u64,
    )
    .unwrap();
    for (identity, body) in objects {
        store
            .put_if_absent_verified(&older.pages.path(&identity), body)
            .await
            .unwrap();
    }
    let mut work = PageWork::new(16 * 1024 * 1024, 0);
    let rows = |keys: &[String]| {
        keys.iter()
            .map(|key| (hash(key).unwrap().to_vec(), Vec::new()))
            .collect::<Rows>()
    };
    // Insert the lower range last. The edited tree remains canonical but its
    // partial leaf boundary differs from a single initial build of all rows.
    let suffix = older
        .pages
        .edit(
            &store,
            None,
            Kind::CatalogShardXorbs,
            rows(&ids[1000..]),
            &mut work,
        )
        .await
        .unwrap();
    let alternate = older
        .pages
        .edit(
            &store,
            suffix.as_ref(),
            Kind::CatalogShardXorbs,
            rows(&ids[..1000]),
            &mut work,
        )
        .await
        .unwrap();
    let mut row: ShardRow = from_json(
        older
            .rows(
                &store,
                Kind::CatalogShards,
                &[hash(&"a".repeat(64)).unwrap().to_vec()],
                &mut work,
            )
            .await
            .unwrap()[0]
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    assert_ne!(row.xorbs, alternate);
    let recipe_roots = (row.xorbs.clone(), alternate.clone());
    row.xorbs = alternate;
    let mut descriptor = descriptor;
    descriptor.shards = older
        .pages
        .edit(
            &store,
            descriptor.shards.as_ref(),
            Kind::CatalogShards,
            vec![(hash(&"a".repeat(64)).unwrap().to_vec(), json(&row).unwrap())],
            &mut work,
        )
        .await
        .unwrap();
    let body = descriptor.encode().unwrap();
    let identity = *blake3::hash(&body).as_bytes();
    store
        .put_if_absent_verified(&older.pages.path(&identity), body.clone())
        .await
        .unwrap();
    let newer = CatalogPages::new(prefix, descriptor.clone(), identity, body.len() as u64).unwrap();
    let mut work = PageWork::new(16 * 1024 * 1024, 0);
    older.verify(&store, &mut work).await.unwrap();
    newer.verify(&store, &mut work).await.unwrap();
    assert_eq!(
        older.merge(&store, &newer, &mut work).await.unwrap(),
        descriptor
    );
    let mut recipes = RecipeDigests::new();
    assert!(
        newer
            .same_recipe(
                &store,
                recipe_roots.clone(),
                Kind::CatalogShardXorbs,
                &mut recipes,
                &mut work
            )
            .await
            .unwrap()
    );
    let body = descriptor.encode().unwrap();
    let cold = CatalogPages::new(
        Path::from("catalog"),
        descriptor,
        *blake3::hash(&body).as_bytes(),
        body.len() as u64,
    )
    .unwrap();
    reads.store(0, Ordering::Relaxed);
    assert!(
        cold.same_recipe(
            &store,
            recipe_roots,
            Kind::CatalogShardXorbs,
            &mut recipes,
            &mut work
        )
        .await
        .unwrap()
    );
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "shared recipe comparison repeated origin reads with a cold parsed-page cache"
    );
}
