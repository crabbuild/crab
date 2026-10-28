use super::*;
use bytes::Bytes;
use crab_storage::StoreLayout;
use object_store::memory::InMemory;
use std::sync::Arc;

use crab_metadata::ref_registry::ActiveActiveCoordinatorRegistration;

fn memory_store() -> Store {
    Store::new(Arc::new(InMemory::new()))
}

struct TestListOutcome {
    objects: Vec<ListedObject>,
    requests: u64,
    parallelism: usize,
}

#[derive(Default)]
struct TestObjectCollector(Vec<ListedObject>);

#[async_trait(?Send)]
impl GlobalObjectConsumer for TestObjectCollector {
    async fn consume(&mut self, object: ListedObject) -> Result<()> {
        self.0.push(object);
        Ok(())
    }
}

async fn list_global_objects(
    store: &Store,
    kind: &str,
    profile: GcListProfile,
    concurrency: usize,
    permits: Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<TestListOutcome> {
    let mut collector = TestObjectCollector::default();
    let stats = scan_global_objects(
        store,
        kind,
        profile,
        concurrency,
        permits,
        cancel,
        &mut collector,
    )
    .await?;
    Ok(TestListOutcome {
        objects: collector.0,
        requests: stats.requests,
        parallelism: stats.parallelism,
    })
}

async fn seed_registry(store: &Store, registry: &RefRegistry) {
    let storage = store.as_storage().clone();
    let bucket_router = crab_storage::StoreLayout::new(storage.clone(), String::new());
    let mut repos = registry.repos.clone();
    for repo in registry
        .workflow_stage_hashes
        .keys()
        .chain(registry.workflow_experiment_ids.keys())
        .chain(registry.active_active_coordinators.keys())
    {
        repos.entry(repo.clone()).or_default();
    }
    crab_metadata::ref_registry::replace_ref_registry_from_repository_roots(
        &storage,
        &bucket_router,
        repos,
    )
    .await
    .unwrap();
    for repo in registry.repos.keys().chain(
        registry
            .workflow_stage_hashes
            .keys()
            .chain(registry.workflow_experiment_ids.keys()),
    ) {
        let router = crab_storage::StoreLayout::new(storage.clone(), repo.clone());
        crab_metadata::ref_registry::register_workflow_roots_exact(
            &storage,
            &router,
            registry
                .workflow_stage_hashes
                .get(repo)
                .cloned()
                .unwrap_or_default(),
            registry
                .workflow_experiment_ids
                .get(repo)
                .cloned()
                .unwrap_or_default(),
        )
        .await
        .unwrap();
    }
    for (repo, coordinator) in &registry.active_active_coordinators {
        let router = crab_storage::StoreLayout::new(storage.clone(), repo.clone());
        crab_metadata::ref_registry::register_active_active_coordinator_for_repo(
            &storage,
            &router,
            coordinator.clone(),
        )
        .await
        .unwrap();
    }
}

async fn publish_capsule_pointer_catalog(store: &Store, repo: &str) -> (String, String) {
    use crab_metadata::capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
        PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
    };

    let router = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    let base =
        crab_write::capsule_protocol::initialize(&router, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let xorb_hash = "a".repeat(64);
    let shard_hash = "b".repeat(64);
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb_hash.clone(),
            XorbCatalogEntry::new(
                1,
                "c".repeat(64),
                vec![XorbChunkEntry::new("d".repeat(64), 1)],
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard_hash.clone(),
            ShardCatalogEntry::new(1, vec![xorb_hash.clone()]),
        )
        .unwrap();
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        Vec::new(),
        vec![CapsuleSection::new(
            CapsuleSectionKind::CatalogDelta,
            catalog.encode_delta().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&router, base, &transaction, &capsule)
        .await
        .unwrap();
    (shard_hash, xorb_hash)
}

#[test]
fn extract_hash_from_key_works() {
    assert_eq!(
        extract_hash_from_key(&format!(".crab/shards/ab/{}", "ab".repeat(32))),
        "ab".repeat(32)
    );
    assert_eq!(
        extract_hash_from_key(&format!(".crab/xorbs/de/{}", "de".repeat(32))),
        "de".repeat(32)
    );
    assert_eq!(extract_hash_from_key(""), "");
}

#[tokio::test]
async fn adaptive_global_listing_keeps_small_namespace_on_one_stream() {
    let store = memory_store();
    let hashes = [
        "aa".repeat(32),
        "ff".repeat(32),
        format!("aa{}", "1".repeat(62)),
    ];
    for hash in &hashes {
        store
            .put(
                &crab_storage::canonical_global_content_path("xorbs", hash),
                Bytes::from(hash.clone()),
            )
            .await
            .unwrap();
    }

    let mut outcome = list_global_objects(
        &store,
        "xorbs",
        GcListProfile::Adaptive,
        8,
        Arc::new(Semaphore::new(8)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    outcome
        .objects
        .sort_by(|left, right| left.location.cmp(&right.location));

    assert_eq!(outcome.objects.len(), 3);
    assert_eq!(outcome.requests, 1);
    assert_eq!(outcome.parallelism, 1);
    assert!(
        outcome
            .objects
            .iter()
            .all(|object| { content_hash_from_path(&object.location, "xorbs").is_some() })
    );
}

#[tokio::test]
async fn latency_global_listing_scans_only_populated_hash_partitions() {
    let store = memory_store();
    for hash in ["aa".repeat(32), "ff".repeat(32)] {
        store
            .put(
                &crab_storage::canonical_global_content_path("xorbs", &hash),
                Bytes::from(hash),
            )
            .await
            .unwrap();
    }

    let outcome = list_global_objects(
        &store,
        "xorbs",
        GcListProfile::Latency,
        8,
        Arc::new(Semaphore::new(8)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.requests, 3);
    assert_eq!(outcome.parallelism, 2);
}

#[tokio::test]
async fn adaptive_global_listing_partitions_large_namespace_after_bounded_probe() {
    let store = memory_store().with_bucket_identity(crab_storage::BucketIdentity::new(
        StorageProviderKind::S3,
        "bucket",
        "bucket",
    ));
    for index in 0..=256_000_u64 {
        let hash = format!("{:02x}{index:062x}", index % 256);
        store
            .put(
                &crab_storage::canonical_global_content_path("xorbs", &hash),
                Bytes::new(),
            )
            .await
            .unwrap();
    }

    let outcome = list_global_objects(
        &store,
        "xorbs",
        GcListProfile::Adaptive,
        16,
        Arc::new(Semaphore::new(16)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.objects.len(), 256_001);
    assert_eq!(outcome.requests, 258);
}

#[tokio::test]
async fn adaptive_global_listing_keeps_local_and_serial_stores_recursive() {
    let local = memory_store();
    let s3 = memory_store().with_bucket_identity(crab_storage::BucketIdentity::new(
        StorageProviderKind::S3,
        "bucket",
        "bucket",
    ));
    for index in 0..=2_000_u64 {
        let hash = format!("{index:064x}");
        let path = crab_storage::canonical_global_content_path("xorbs", &hash);
        local.put(&path, Bytes::new()).await.unwrap();
        s3.put(&path, Bytes::new()).await.unwrap();
    }

    let local_outcome = list_global_objects(
        &local,
        "xorbs",
        GcListProfile::Adaptive,
        8,
        Arc::new(Semaphore::new(8)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let serial_outcome = list_global_objects(
        &s3,
        "xorbs",
        GcListProfile::Adaptive,
        1,
        Arc::new(Semaphore::new(1)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(local_outcome.requests, 1);
    assert_eq!(serial_outcome.requests, 1);
}

#[test]
fn adaptive_probe_uses_provider_page_capacity() {
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    let s3 = Store::new(Arc::clone(&inner)).with_bucket_identity(
        crab_storage::BucketIdentity::new(StorageProviderKind::S3, "bucket", "bucket"),
    );
    let azure = Store::new(Arc::clone(&inner)).with_bucket_identity(
        crab_storage::BucketIdentity::new(StorageProviderKind::Azure, "account", "container"),
    );
    let local = Store::new(inner);

    assert_eq!(adaptive_probe_limit(&s3), 256_000);
    assert_eq!(adaptive_probe_limit(&azure), 1_280_000);
    assert_eq!(adaptive_probe_limit(&local), usize::MAX);
}

#[tokio::test]
async fn global_listing_honors_cancellation_during_enumeration() {
    let store = memory_store();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = list_global_objects(
        &store,
        "xorbs",
        GcListProfile::Cost,
        8,
        Arc::new(Semaphore::new(8)),
        &cancel,
    )
    .await;

    assert!(matches!(result, Err(CrabError::Cancelled)));
}

#[tokio::test]
async fn global_listing_rejects_flat_legacy_objects() {
    let store = memory_store();
    let hash = "ab".repeat(32);
    store
        .put(
            &ObjectPath::from(format!(".crab/xorbs/{hash}")),
            Bytes::from_static(b"legacy"),
        )
        .await
        .unwrap();

    let result = list_global_objects(
        &store,
        "xorbs",
        GcListProfile::Adaptive,
        8,
        Arc::new(Semaphore::new(8)),
        &CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
}

#[tokio::test]
async fn destructive_plan_collects_orphan_closure_segments() {
    let store = memory_store();
    let hash = "a".repeat(64);
    let path = super::super::closure::segment_path(GLOBAL_PREFIX, &hash, 7);
    store
        .put(&path, Bytes::from_static(b"orphan"))
        .await
        .unwrap();
    let mut journal = super::super::journal::GcRunJournal::start(
        store.clone(),
        GLOBAL_PREFIX,
        "bucket",
        GLOBAL_PREFIX,
        SystemTime::now(),
        Duration::from_secs(3600),
        true,
    )
    .await
    .unwrap();
    journal.set_root_identity("root").await.unwrap();
    let protected = HashSet::new();
    let mut planner = ImmutableObjectStreamingPlanner::new(
        DurableMarkReader::new_keys(
            store.clone(),
            journal.marks_prefix(),
            "live-closure-segments",
        ),
        &protected,
        SystemTime::now(),
        true,
        &mut journal,
    );

    scan_closure_segment_objects(
        &store,
        Arc::new(Semaphore::new(1)),
        &CancellationToken::new(),
        &mut planner,
    )
    .await
    .unwrap();
    planner.finish().await.unwrap();
    journal.finish_plan().await.unwrap();

    let batch = journal.next_batch().await.unwrap().unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].key, path.to_string());
}

#[tokio::test]
async fn streaming_plan_retains_coordinator_protected_xorb() {
    let store = memory_store();
    let hash = "a".repeat(64);
    let path = crab_storage::canonical_global_content_path("xorbs", &hash).to_string();
    let mut journal = super::super::journal::GcRunJournal::start(
        store.clone(),
        GLOBAL_PREFIX,
        "bucket",
        GLOBAL_PREFIX,
        SystemTime::now(),
        Duration::from_secs(3600),
        true,
    )
    .await
    .unwrap();
    journal.set_root_identity("root").await.unwrap();
    let protected = [path.clone()].into_iter().collect();
    let mut planner = XorbStreamingPlanner::new(
        DurableMarkReader::new(store.clone(), journal.marks_prefix(), "referenced-xorbs"),
        &protected,
        SystemTime::now(),
        true,
        &mut journal,
    );

    planner
        .consume(ListedObject {
            location: path,
            size: 7,
            last_modified: SystemTime::UNIX_EPOCH,
            e_tag: Some("planned".to_owned()),
            version: None,
        })
        .await
        .unwrap();
    planner.finish().await.unwrap();
    journal.finish_plan().await.unwrap();

    assert!(journal.next_batch().await.unwrap().is_none());
}

#[tokio::test]
async fn load_registry_missing_without_force_errors() {
    let store = memory_store();
    let result = load_ref_registry(&store, false).await;
    assert!(matches!(result, Err(CrabError::NotFound { .. })));
}

#[tokio::test]
async fn load_registry_missing_with_force_returns_empty() {
    let store = memory_store();
    let reg = load_ref_registry(&store, true).await.unwrap();
    assert!(reg.repos.is_empty());
    assert_eq!(reg.generation, 0);
    assert_eq!(reg.schema_version, 0);
    assert!(!reg.is_complete_for_destructive_gc());
}

#[tokio::test]
async fn load_registry_valid_json() {
    let store = memory_store();
    let reg = RefRegistry {
        generation: 5,
        repos: [("org/models".to_string(), vec!["aaa".to_string()])]
            .into_iter()
            .collect(),
        ..RefRegistry::default()
    };
    seed_registry(&store, &reg).await;

    let loaded = load_ref_registry(&store, false).await.unwrap();
    assert_eq!(loaded.repos.len(), 1);
}

#[tokio::test]
async fn deregister_missing_repo_is_a_noop() {
    let store = memory_store();
    deregister_repo(&store, "org/old-repo").await.unwrap();

    assert!(matches!(
        load_ref_registry(&store, false).await,
        Err(CrabError::NotFound { .. })
    ));
}

#[tokio::test]
async fn deregister_removes_existing_repo() {
    let store = memory_store();
    let reg = RefRegistry {
        generation: 3,
        repos: [
            ("org/models".to_string(), vec!["aaa".to_string()]),
            ("org/datasets".to_string(), vec!["bbb".to_string()]),
        ]
        .into_iter()
        .collect(),
        ..RefRegistry::default()
    };
    seed_registry(&store, &reg).await;

    deregister_repo(&store, "org/models").await.unwrap();

    let updated = load_ref_registry(&store, false).await.unwrap();
    assert_eq!(updated.repos.len(), 1);
    assert!(!updated.repos.contains_key("org/models"));
    assert!(updated.repos.contains_key("org/datasets"));
}

async fn legacy_manifest(store: &Store, repo: &str) {
    let router = StoreLayout::new(store.clone(), repo.to_owned());
    crate::core::remote_layout::initialize(store, &router)
        .await
        .unwrap();
    crate::metadata::manifest::create_manifest(
        store,
        &router,
        &crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn capsule_authority_missing_root_rejects_manifest_fallback() {
    let store = memory_store();
    legacy_manifest(&store, "org/legacy").await;
    let result = capsule_repository_roots(&store, "org/legacy").await;
    assert!(
        matches!(result, Err(CrabError::NotFound { ref path }) if path == "org/legacy/v2/root")
    );
}

#[tokio::test]
async fn capsule_authority_snapshot_rejects_manifest_fallback() {
    let store = memory_store();
    legacy_manifest(&store, "org/legacy").await;
    let mut registry = RefRegistry::default();
    registry.register("org/legacy", Vec::new());
    let result = bucket_root_snapshot_streaming(&store, &registry, &HashSet::new()).await;
    assert!(
        matches!(result, Err(CrabError::NotFound { ref path }) if path == "org/legacy/v2/root")
    );
}

#[tokio::test]
async fn capsule_authority_closure_repair_rejects_manifest_fallback() {
    let store = memory_store();
    legacy_manifest(&store, "org/legacy").await;
    let mut registry = RefRegistry::default();
    registry.register("org/legacy", Vec::new());
    let result = repository_referenced_shards(&store, &registry, 4).await;
    assert!(
        matches!(result, Err(CrabError::NotFound { ref path }) if path == "org/legacy/v2/root")
    );
}

#[tokio::test]
async fn capsule_authority_registry_repair_does_not_certify_manifest_repository() {
    let store = memory_store();
    initialize_capsule_repository(&store, "org/current").await;
    legacy_manifest(&store, "org/legacy").await;
    let mut registry = RefRegistry::default();
    registry.register("org/current", Vec::new());
    seed_registry(&store, &registry).await;
    let storage = store.as_storage();
    let layout = crab_storage::StoreLayout::new(storage.clone(), String::new());
    let before = crab_metadata::ref_registry::load_ref_registry_summary(storage, &layout)
        .await
        .unwrap();
    let result = repair_ref_registry(&store).await;
    assert!(
        matches!(result, Err(CrabError::NotFound { ref path }) if path == "org/legacy/v2/root")
    );
    let after = crab_metadata::ref_registry::load_ref_registry_summary(storage, &layout)
        .await
        .unwrap();
    assert_eq!(
        (before.generation, before.coverage_complete, before.repos),
        (after.generation, after.coverage_complete, after.repos)
    );
}

async fn initialize_capsule_repository(store: &Store, repo: &str) {
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
}

#[tokio::test]
async fn registry_repair_keeps_registered_roots_when_repository_marker_disappears() {
    let store = memory_store();
    let repo = "org/registered";
    let (shard, _) = publish_capsule_pointer_catalog(&store, repo).await;
    repair_ref_registry(&store).await.unwrap();
    let before = load_ref_registry(&store, false).await.unwrap();
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    layout
        .store()
        .delete(&layout.capsule_root_path())
        .await
        .unwrap();

    let result = repair_ref_registry(&store).await;
    assert!(
        matches!(result, Err(CrabError::NotFound { ref path }) if path == "org/registered/v2/root")
    );
    let after = load_ref_registry(&store, false).await.unwrap();
    assert_eq!(
        (
            after.generation,
            after.coverage_complete,
            after.repos[repo].clone()
        ),
        (before.generation, before.coverage_complete, vec![shard])
    );
}

#[tokio::test]
async fn registry_repair_respects_explicit_deregistration_after_marker_removal() {
    let store = memory_store();
    let repo = "org/removed";
    publish_capsule_pointer_catalog(&store, repo).await;
    repair_ref_registry(&store).await.unwrap();
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    layout
        .store()
        .delete(&layout.capsule_root_path())
        .await
        .unwrap();
    deregister_repo(&store, repo).await.unwrap();

    assert_eq!(repair_ref_registry(&store).await.unwrap(), (0, 0));
}

#[tokio::test]
async fn registry_repair_replaces_a_corrupt_coverage_marker() {
    let store = memory_store();
    publish_capsule_pointer_catalog(&store, "org/current").await;
    repair_ref_registry(&store).await.unwrap();
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), String::new());
    let coverage = ObjectPath::from(format!("{}/coverage.json", layout.ref_registry_path()));
    layout
        .store()
        .put_overwrite(&coverage, Bytes::from_static(b"corrupt coverage"))
        .await
        .unwrap();

    assert_eq!(repair_ref_registry(&store).await.unwrap(), (1, 1));
    assert!(
        load_ref_registry(&store, false)
            .await
            .unwrap()
            .is_complete_for_destructive_gc()
    );
}

#[tokio::test]
async fn registry_repair_discovers_capsule_roots_and_marks_coverage_complete() {
    let store = memory_store();
    for repo in ["org/a", "org/b"] {
        initialize_capsule_repository(&store, repo).await;
    }

    let (repos, shards) = repair_ref_registry(&store).await.unwrap();

    assert_eq!((repos, shards), (2, 0));
    let registry = load_ref_registry(&store, false).await.unwrap();
    assert!(registry.is_complete_for_destructive_gc());
    assert!(registry.complete_repos.contains("org/a"));
    assert!(registry.complete_repos.contains("org/b"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires CRAB_E2E_FLAT_BUCKET and explicit scratch S3 credentials"]
async fn live_capsule_shard_inspection_reads_published_catalog_without_gc() {
    let bucket = std::env::var("CRAB_E2E_FLAT_BUCKET").unwrap();
    let storage =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let store = Store::from_storage(storage);
    let repo = format!("catalog-shard-inspection/{}", uuid::Uuid::now_v7());
    let (expected, _) = publish_capsule_pointer_catalog(&store, &repo).await;
    let roots = capsule_repository_roots(&store, &repo).await.unwrap();
    assert_eq!(roots.shard_hashes, vec![expected]);
    // Root capture only: this fixture must never invoke a bucket sweep.
    let again = capsule_repository_roots(&store, &repo).await.unwrap();
    assert_eq!(again.root_digest, roots.root_digest);
}

#[tokio::test]
async fn registry_repair_and_bucket_marks_preserve_capsule_catalog_shards() {
    use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};

    let store = memory_store();
    let repo = "org/v2";
    let (shard_hash, _) = publish_capsule_pointer_catalog(&store, repo).await;

    let (repos, shards) = repair_ref_registry(&store).await.unwrap();

    assert_eq!((repos, shards), (1, 1));
    let registry = load_ref_registry(&store, false).await.unwrap();
    assert_eq!(registry.repos[repo], vec![shard_hash.clone()]);
    assert!(registry.is_complete_for_destructive_gc());
    let roots = bucket_root_snapshot_streaming(&store, &registry, &HashSet::new())
        .await
        .unwrap();
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    let base = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            Some("2".repeat(40)),
            Some("3".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
    crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
        .await
        .unwrap();
    let moved_roots = bucket_root_snapshot_streaming(&store, &registry, &HashSet::new())
        .await
        .unwrap();
    assert_ne!(roots.root_identity, moved_roots.root_identity);

    let journal = super::super::journal::GcRunJournal::start(
        store.clone(),
        GLOBAL_PREFIX,
        "bucket",
        GLOBAL_PREFIX,
        SystemTime::now(),
        Duration::from_secs(3600),
        true,
    )
    .await
    .unwrap();
    write_bucket_root_marks(&store, &registry, &roots.capsule_shards, &journal)
        .await
        .unwrap();
    let mut reader =
        DurableMarkReader::new_keys(store, journal.marks_prefix(), "referenced-shards");
    assert!(reader.contains(&shard_hash).await.unwrap());
}

#[tokio::test]
async fn dry_run_bucket_gc_with_empty_store() {
    let store = memory_store();
    let reg = RefRegistry::default();
    seed_registry(&store, &reg).await;

    let args = BucketGcArgs {
        bucket: "test-bucket".to_string(),
        dry_run: true,
        grace_period: Duration::from_secs(3600),
        force: false,
        yes: false,
        list_concurrency: 16,
        list_profile: GcListProfile::Adaptive,
        delete_concurrency: 64,
        resume_run_id: None,
    };

    let protected = HashSet::new();
    let protected_repos = HashSet::new();
    let outcome = run_bucket_gc(
        &args,
        &store,
        &protected,
        &protected_repos,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(outcome.dry_run);
    assert_eq!(outcome.shards_deleted, 0);
    assert_eq!(outcome.xorbs_deleted, 0);
    assert_eq!(outcome.file_index_deleted, 0);

    let mut scratch = store
        .inner()
        .list(Some(&ObjectPath::from(".crab/gc/runs/")));
    assert!(scratch.try_next().await.unwrap().is_none());
}

#[tokio::test]
async fn destructive_bucket_gc_preserves_recent_xorb_without_force() {
    let store = memory_store();
    let mut registry = RefRegistry::default();
    registry.mark_coverage_complete();
    seed_registry(&store, &registry).await;
    let xorb_path = canonical_global_content_path("xorbs", &"a".repeat(64));
    store
        .put(&xorb_path, Bytes::from_static(b"recent orphan"))
        .await
        .unwrap();

    let outcome = run_bucket_gc(
        &BucketGcArgs {
            bucket: "test-bucket".to_owned(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        },
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.xorbs_deleted, 0);
    store.get_with_etag(&xorb_path).await.unwrap();
}

#[tokio::test]
async fn destructive_bucket_gc_force_deletes_recent_xorb() {
    let store = memory_store();
    let mut registry = RefRegistry::default();
    registry.mark_coverage_complete();
    seed_registry(&store, &registry).await;
    let xorb_path = canonical_global_content_path("xorbs", &"b".repeat(64));
    let xorb = Bytes::from_static(b"recent orphan");
    store.put(&xorb_path, xorb.clone()).await.unwrap();

    let outcome = run_bucket_gc(
        &BucketGcArgs {
            bucket: "test-bucket".to_owned(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: true,
            yes: true,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        },
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.xorbs_deleted, 1);
    assert_eq!(outcome.bytes_reclaimed, xorb.len() as u64);
    assert!(matches!(
        store.get_with_etag(&xorb_path).await,
        Err(CrabError::NotFound { .. })
    ));
    let summary = outcome.to_summary();
    assert_eq!(summary.packs_deleted, 0);
    assert_eq!(summary.xorbs_deleted, 1);
    assert_eq!(summary.shards_deleted, 0);
    assert_eq!(summary.bytes_reclaimed, xorb.len() as u64);
    assert!(!summary.dry_run);
    assert!(!summary.cancelled);
    assert!(!summary.partial_enumeration);
}

#[tokio::test]
async fn bucket_gc_tombstones_file_rows_outside_each_repo_closure() {
    use crab_metadata::value_codec::CommittedFileRecord;

    let store = memory_store();
    let repo = "org/models";
    let retained = MerkleHash::from([1, 2, 3, 4]);
    let stale = MerkleHash::from([5, 6, 7, 8]);
    let shard = MerkleHash::from([9, 10, 11, 12]);
    let config = crate::metadata::MetaDbConfig::for_repo(repo);
    let guard = crate::metadata::MetaDbGuard::new(crate::metadata::MetaDb::new(
        Arc::clone(store.inner()),
        repo.to_owned(),
        config.clone(),
    ));
    let file_index = guard.file_index().await.unwrap();
    let mut transaction = guard.new_transaction().unwrap();
    file_index.save_committed_batch(
        &mut transaction,
        &[
            (
                retained,
                CommittedFileRecord {
                    recipe_hash: [1; 32],
                    shard_hash: shard,
                    committed_generation: 1,
                    shard_index_hash: shard,
                },
            ),
            (
                stale,
                CommittedFileRecord {
                    recipe_hash: [2; 32],
                    shard_hash: shard,
                    committed_generation: 1,
                    shard_index_hash: shard,
                },
            ),
        ],
    );
    guard.commit(transaction).await.unwrap();
    guard.close().await.unwrap();

    let marks_prefix = ".crab/gc/runs/test/marks".to_owned();
    let mut writer = DurableMarkWriter::new_hash_width(
        store.clone(),
        marks_prefix.clone(),
        "referenced-files",
        4,
    );
    writer.add(&retained.hex()).await.unwrap();
    writer.finish().await.unwrap();
    let mut reader =
        DurableMarkReader::new_hash_width(store.clone(), marks_prefix, "referenced-files", 4);
    let removed = gc_file_indexes_partitioned(
        &store,
        &[repo.to_owned()],
        &mut reader,
        false,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(removed, 1);
    let guard = crate::metadata::MetaDbGuard::new(crate::metadata::MetaDb::new(
        Arc::clone(store.inner()),
        repo.to_owned(),
        config.with_read_only(true),
    ));
    let values = guard
        .file_index()
        .await
        .unwrap()
        .get_committed_batch(&[retained, stale])
        .await
        .unwrap();
    assert!(values[0].is_some());
    assert!(values[1].is_none());
    guard.close().await.unwrap();
}

#[tokio::test]
async fn gc_writer_fence_blocks_destructive_bucket_gc_but_not_preview() {
    let store = memory_store();
    initialize_capsule_repository(&store, "org/models").await;
    let mut registry = RefRegistry::default();
    registry.register("org/models", Vec::new());
    registry.mark_coverage_complete();
    seed_registry(&store, &registry).await;
    let held = crab_coordination::GcFenceLease::acquire_writer(
        store.inner(),
        GLOBAL_PREFIX,
        crab_coordination::DEFAULT_GC_FENCE_TTL,
    )
    .await
    .unwrap();

    let destructive = run_bucket_gc(
        &BucketGcArgs {
            bucket: "test-bucket".to_owned(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        },
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(destructive, Err(CrabError::PushLockHeld { .. })));

    let preview = run_bucket_gc(
        &BucketGcArgs {
            bucket: "test-bucket".to_owned(),
            dry_run: true,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        },
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(preview.dry_run);

    held.release().await.unwrap();
}

#[tokio::test]
async fn destructive_gc_aborts_when_referenced_shard_is_corrupt() {
    let store = memory_store();
    initialize_capsule_repository(&store, "org/models").await;
    let corrupt_shard = Bytes::from_static(b"not a shard");
    let shard_hash = crab_xet::hash::compute_data_hash(&corrupt_shard).hex();
    store
        .put(
            &canonical_global_content_path("shards", &shard_hash),
            corrupt_shard,
        )
        .await
        .unwrap();

    let mut registry = RefRegistry::default();
    registry.register("org/models", vec![shard_hash]);
    registry.mark_coverage_complete();
    seed_registry(&store, &registry).await;

    let error = run_bucket_gc(
        &BucketGcArgs {
            bucket: "test-bucket".to_owned(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        },
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, CrabError::CorruptObject { .. }));
}

#[tokio::test]
async fn destructive_gc_aborts_when_registry_root_is_missing() {
    let store = memory_store();
    initialize_capsule_repository(&store, "org/models").await;
    let mut registry = RefRegistry::default();
    registry.register("org/models", vec!["a".repeat(64)]);
    registry.mark_coverage_complete();
    seed_registry(&store, &registry).await;

    let error = run_bucket_gc(
        &BucketGcArgs {
            bucket: "test-bucket".to_owned(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
            yes: false,
            list_concurrency: 16,
            list_profile: GcListProfile::Adaptive,
            delete_concurrency: 64,
            resume_run_id: None,
        },
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, CrabError::CorruptObject { .. }));
}

#[tokio::test]
async fn bucket_gc_destructive_run_requires_proof_for_registered_active_active_repos() {
    let store = memory_store();
    let mut reg = RefRegistry::default();
    reg.mark_coverage_complete();
    reg.register_active_active_coordinator(
        "org/models",
        ActiveActiveCoordinatorRegistration {
            provider: "dynamodb".to_owned(),
            url: "dynamodb://crab-coordinator".to_owned(),
            region: "us-east-1".to_owned(),
            failover_regions: vec!["us-west-2".to_owned()],
        },
    );
    seed_registry(&store, &reg).await;
    let args = BucketGcArgs {
        bucket: "test-bucket".to_string(),
        dry_run: false,
        grace_period: Duration::from_secs(3600),
        force: false,
        yes: false,
        list_concurrency: 16,
        list_profile: GcListProfile::Adaptive,
        delete_concurrency: 64,
        resume_run_id: None,
    };
    let protected = HashSet::new();
    let protected_repos = HashSet::new();

    let err = run_bucket_gc(
        &args,
        &store,
        &protected,
        &protected_repos,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, CrabError::Configuration { .. }));
    assert!(err.to_string().contains("org/models"));
}

#[tokio::test]
async fn bucket_gc_destructive_run_accepts_all_repo_coordinator_proof() {
    let store = memory_store();
    initialize_capsule_repository(&store, "org/models").await;
    let mut reg = RefRegistry::default();
    reg.mark_coverage_complete();
    reg.register_active_active_coordinator(
        "org/models",
        ActiveActiveCoordinatorRegistration {
            provider: "dynamodb".to_owned(),
            url: "dynamodb://crab-coordinator".to_owned(),
            region: "us-east-1".to_owned(),
            failover_regions: Vec::new(),
        },
    );
    seed_registry(&store, &reg).await;
    let args = BucketGcArgs {
        bucket: "test-bucket".to_string(),
        dry_run: false,
        grace_period: Duration::from_secs(3600),
        force: false,
        yes: false,
        list_concurrency: 16,
        list_profile: GcListProfile::Adaptive,
        delete_concurrency: 64,
        resume_run_id: None,
    };
    let protected = HashSet::new();
    let protected_repos = ["org/models".to_owned()].into_iter().collect();

    let outcome = run_bucket_gc(
        &args,
        &store,
        &protected,
        &protected_repos,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.shards_deleted, 0);
    assert_eq!(outcome.xorbs_deleted, 0);
}

#[tokio::test]
async fn bucket_gc_destructive_run_rejects_incomplete_legacy_registry() {
    let store = memory_store();
    let legacy = br#"{"generation":1,"repos":{"org/models":["shard-a"]}}"#;
    let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
    store
        .put(&registry_path, Bytes::from_static(legacy))
        .await
        .unwrap();
    let args = BucketGcArgs {
        bucket: "test-bucket".to_string(),
        dry_run: false,
        grace_period: Duration::from_secs(3600),
        force: true,
        yes: true,
        list_concurrency: 16,
        list_profile: GcListProfile::Adaptive,
        delete_concurrency: 64,
        resume_run_id: None,
    };

    let err = run_bucket_gc(
        &args,
        &store,
        &HashSet::new(),
        &HashSet::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, CrabError::Configuration { .. }));
    assert!(err.to_string().contains("ref-registry"));
}

/// Regression: the bucket GC walker MUST NOT enumerate or delete
/// objects under `.crab/workflow/**`.
///
/// The workflow push path (task 4.8) ships a fresh object
/// namespace (`workflow/stages/…`, `workflow/exp/…`) that the
/// current walker is intentionally blind to. This test seeds
/// representative objects in that namespace, runs the GC with
/// force=true (so a missing workflow entry in the registry
/// doesn't error out) and dry_run=true (so we observe what
/// *would* be deleted without needing to backdate
/// `last_modified`), and asserts the workflow objects remain
/// intact regardless of registry contents.
///
/// If someone later adds a generic `list_global_objects(store, "*")`
/// call to the walker without filtering, this test fails
/// loudly: the workflow objects would show up in
/// `bytes_reclaimed` / deletion counters.
#[tokio::test]
async fn bucket_gc_does_not_touch_workflow_objects() {
    let store = memory_store();

    // Empty ref-registry — worst case for the walker: every
    // workflow object is unreferenced by construction.
    let reg = RefRegistry::default();
    seed_registry(&store, &reg).await;

    // Seed representative workflow objects. Synthetic bytes
    // are fine — the walker never inspects object content,
    // only keys, so the test exercises key-matching behavior.
    let stage_key = ObjectPath::from(format!("{GLOBAL_PREFIX}/workflow/stages/ab/abcdef.json"));
    let meta_key = ObjectPath::from(format!(
        "{GLOBAL_PREFIX}/workflow/exp/01931b9e-4b3c-7b2a-b9f0-0123456789ab/meta.json"
    ));
    let stage_refs_key = ObjectPath::from(format!(
        "{GLOBAL_PREFIX}/workflow/exp/01931b9e-4b3c-7b2a-b9f0-0123456789ab/stage-refs.json"
    ));
    store
        .put(&stage_key, Bytes::from_static(b"{\"stage\": \"payload\"}"))
        .await
        .unwrap();
    store
        .put(&meta_key, Bytes::from_static(b"{\"exp\": \"payload\"}"))
        .await
        .unwrap();
    store
        .put(&stage_refs_key, Bytes::from_static(b"[\"deadbeef\"]"))
        .await
        .unwrap();

    // Run GC in dry-run mode. Nothing should be deleted (dry
    // run), and regardless of what *would* be deleted, the
    // workflow keys must not appear in any deletion counter.
    let args = BucketGcArgs {
        bucket: "test-bucket".to_string(),
        dry_run: true,
        grace_period: Duration::from_secs(3600),
        force: true,
        yes: false,
        list_concurrency: 16,
        list_profile: GcListProfile::Adaptive,
        delete_concurrency: 64,
        resume_run_id: None,
    };
    let protected = HashSet::new();
    let protected_repos = HashSet::new();
    let outcome = run_bucket_gc(
        &args,
        &store,
        &protected,
        &protected_repos,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(outcome.dry_run);
    assert_eq!(
        outcome.shards_deleted, 0,
        "workflow stage entries must not be counted as shard deletions",
    );
    assert_eq!(
        outcome.xorbs_deleted, 0,
        "workflow stage entries must not be counted as xorb deletions",
    );
    assert_eq!(
        outcome.file_index_deleted, 0,
        "workflow stage entries must not be counted as file-index deletions",
    );
    assert_eq!(outcome.bytes_reclaimed, 0);

    // All three workflow objects survive — the walker never
    // saw them.
    assert!(
        store.get_with_etag(&stage_key).await.is_ok(),
        "workflow stage entry was touched by bucket GC",
    );
    assert!(
        store.get_with_etag(&meta_key).await.is_ok(),
        "workflow experiment meta was touched by bucket GC",
    );
    assert!(
        store.get_with_etag(&stage_refs_key).await.is_ok(),
        "workflow experiment stage-refs blob was touched by bucket GC",
    );
}
#[path = "bucket_hint_tests.rs"]
mod hint_tests;

struct RetainedReaderFixture {
    store: Store,
    layout: crab_storage::StoreLayout<crab_storage::Store>,
    current: crab_metadata::capsule_protocol::RootSnapshot,
    lease: crab_coordination::read_retention::ReadRetentionLease,
    retained_objects: Vec<object_store::path::Path>,
}

async fn retained_reader_fixture() -> RetainedReaderFixture {
    use crab_coordination::read_retention::{ReadRetentionLease, RetainedReadRoot};
    use crab_metadata::capsule_protocol::{
        FileCatalogEntry, GcFence, HistorySegment, HistorySegmentState, LayeredCheckpoint,
        PointerCatalog, ShardCatalogEntry, store_read_capture,
    };
    use crab_write::capsule_protocol::{
        begin_gc, end_gc, initialize, publish_ref_layered_checkpoint, replace_history,
    };
    let store = memory_store();
    let layout = StoreLayout::new(store.as_storage().clone(), "retained-reader".to_owned());
    let initial = initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_file("a".repeat(64), FileCatalogEntry::new(7, "b".repeat(64)))
        .unwrap();
    catalog
        .insert_shard("b".repeat(64), ShardCatalogEntry::new(8, Vec::new()))
        .unwrap();
    let layer = crab_metadata::capsule_protocol::PackLayer::build(
        &crab_metadata::capsule_protocol::CapsuleGitPack::new(
            Bytes::from_static(b"PACK"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "3".repeat(40),
            1,
        )
        .unwrap(),
    )
    .unwrap();
    layout
        .store()
        .put_if_absent_verified(
            &layout.capsule_pack_layer_path(layer.hash()),
            layer.bytes().clone(),
        )
        .await
        .unwrap();
    let source = layer.source_descriptor().unwrap();
    let old = LayeredCheckpoint::build(
        0,
        initial.record().digest(),
        vec![source.clone()],
        catalog,
        None,
    )
    .unwrap();
    let captured = publish_ref_layered_checkpoint(
        &layout,
        initial,
        &old,
        Default::default(),
        Default::default(),
        Default::default(),
        Vec::new(),
    )
    .await
    .unwrap();
    let view = crab_read::capsule_protocol::open_view_from_root_with_control(
        &layout,
        captured.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 8 * 1024 * 1024,
            max_frontier_bytes: 8 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    let lookup = view
        .pointer_catalog_lookup(&layout, 8 * 1024 * 1024)
        .unwrap();
    let owner = "c".repeat(64);
    let (hash, size) = store_read_capture(
        &layout,
        &owner,
        view.root(),
        view.capsule_run_pointers(),
        &lookup,
    )
    .await
    .unwrap();
    let lease = ReadRetentionLease::create(
        store.inner().clone(),
        layout.read_retention_claim_path(&owner),
        RetainedReadRoot::new(hash, size).unwrap(),
    )
    .await
    .unwrap();
    let mut retained_objects = old
        .catalog_objects(&layout, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    retained_objects.push(layout.capsule_checkpoint_path(old.hash()));
    retained_objects.extend(
        store
            .list_prefix(&layout.read_retention_prefix())
            .await
            .unwrap()
            .into_iter()
            .map(|object| object.location),
    );
    let empty = LayeredCheckpoint::build(
        captured.record().root().generation(),
        captured.record().digest(),
        vec![source],
        PointerCatalog::new(),
        None,
    )
    .unwrap();
    let current = publish_ref_layered_checkpoint(
        &layout,
        captured,
        &empty,
        Default::default(),
        Default::default(),
        Default::default(),
        Vec::new(),
    )
    .await
    .unwrap();
    let root = current.record().root();
    let history = HistorySegment::build(
        root.checkpoint().unwrap().clone(),
        None,
        HistorySegmentState::new(
            root.refs().clone(),
            root.peeled_refs().clone(),
            root.head().to_owned(),
            root.compacted_ref_transactions().clone(),
            Vec::new(),
        ),
    )
    .unwrap();
    let fence = "d".repeat(64);
    let current = begin_gc(&layout, current, GcFence::new(&fence, u64::MAX).unwrap())
        .await
        .unwrap();
    let current = replace_history(&layout, current, &[history]).await.unwrap();
    let current = end_gc(&layout, current, &fence).await.unwrap();
    RetainedReaderFixture {
        store,
        layout,
        current,
        lease,
        retained_objects,
    }
}

#[tokio::test]
async fn retained_reader_roots_survive_history_pruning_and_renewal_without_reusing_released_marks()
{
    let mut fixture = retained_reader_fixture().await;
    let before = capsule_repository_roots(&fixture.store, fixture.layout.repo_prefix())
        .await
        .unwrap();
    assert_eq!(before.shard_hashes, vec!["b".repeat(64)]);
    fixture.lease.renew().await.unwrap();
    let renewed = capsule_repository_roots(&fixture.store, fixture.layout.repo_prefix())
        .await
        .unwrap();
    assert_eq!(before.root_digest, renewed.root_digest);
    fixture.lease.release().await.unwrap();
    let released = capsule_repository_roots(&fixture.store, fixture.layout.repo_prefix())
        .await
        .unwrap();
    assert!(released.shard_hashes.is_empty());
    assert_ne!(before.root_digest, released.root_digest);
}

#[tokio::test]
async fn repository_gc_retains_reader_metadata_until_release_then_reclaims_aged_objects() {
    let fixture = retained_reader_fixture().await;
    let current = crab_read::capsule_protocol::open_view_from_root_with_control(
        &fixture.layout,
        fixture.current.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 8 * 1024 * 1024,
            max_frontier_bytes: 8 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    let snapshot = current.git_snapshot().unwrap();
    let args = super::super::GcArgs::default();
    let protected = HashSet::new();
    let cancel = CancellationToken::new();
    let collect = || {
        super::super::sweep_capsule_objects(
            &args,
            &fixture.store,
            &fixture.layout,
            fixture.current.record().root(),
            current.capsule_run_pointers(),
            &snapshot.manifest,
            &protected,
            &cancel,
            std::time::SystemTime::now() + Duration::from_secs(7200),
            Duration::from_secs(3600),
            std::time::Instant::now(),
            None,
        )
    };
    collect().await.unwrap();
    for path in &fixture.retained_objects {
        assert!(
            fixture.store.head(path).await.is_ok(),
            "retained metadata disappeared at {path}"
        );
    }
    fixture.lease.release().await.unwrap();
    collect().await.unwrap();
    for path in &fixture.retained_objects {
        assert!(
            matches!(
                fixture.store.head(path).await,
                Err(CrabError::NotFound { .. })
            ),
            "released metadata survived at {path}"
        );
    }
}

#[tokio::test]
async fn missing_reader_capture_aborts_both_gc_marks_before_deleting_orphans() {
    let fixture = retained_reader_fixture().await;
    let descriptor = fixture
        .retained_objects
        .iter()
        .find(|path| path.as_ref().contains("/readers/") && path.as_ref().contains("/objects/"))
        .unwrap();
    fixture.store.delete(descriptor).await.unwrap();
    let orphan = fixture.layout.capsule_path(&"f".repeat(64));
    fixture
        .store
        .put(&orphan, Bytes::from_static(b"orphan"))
        .await
        .unwrap();
    assert!(
        capsule_repository_roots(&fixture.store, fixture.layout.repo_prefix())
            .await
            .is_err()
    );
    let current = crab_read::capsule_protocol::open_view_from_root_with_control(
        &fixture.layout,
        fixture.current.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 8 * 1024 * 1024,
            max_frontier_bytes: 8 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    let snapshot = current.git_snapshot().unwrap();
    let result = super::super::sweep_capsule_objects(
        &super::super::GcArgs::default(),
        &fixture.store,
        &fixture.layout,
        fixture.current.record().root(),
        current.capsule_run_pointers(),
        &snapshot.manifest,
        &HashSet::new(),
        &CancellationToken::new(),
        std::time::SystemTime::now() + Duration::from_secs(7200),
        Duration::from_secs(3600),
        std::time::Instant::now(),
        None,
    )
    .await;
    assert!(result.is_err());
    assert!(fixture.store.head(&orphan).await.is_ok());
}
