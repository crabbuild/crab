use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::core::config::Config;
use crate::read::{Inner, RemoteContext, RepositoryReader};
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_storage::{StorageObservation, StorageObserver, StorageOperation};

#[derive(Default)]
struct ReadBytes(AtomicU64);
impl StorageObserver for ReadBytes {
    fn started(&self, _: StorageOperation) {}
    fn finished(&self, observation: StorageObservation) {
        self.0.fetch_add(observation.bytes_read, Ordering::Relaxed);
    }
}

async fn fixture() -> (RepositoryReader, Arc<ReadBytes>, tempfile::TempDir, String) {
    let observer = Arc::new(ReadBytes::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observer.clone());
    let layout = StoreLayout::new(store.clone(), "read-ranges".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let mut unrelated = Vec::new();
    for i in 0_u64..65_536 {
        unrelated.extend_from_slice(blake3::hash(&i.to_le_bytes()).as_bytes());
    }
    let mut objects = vec![
        (gix_object::Kind::Blob, b"wanted bytes\n".to_vec()),
        (gix_object::Kind::Blob, unrelated),
        (gix_object::Kind::Blob, b"wanted.txt".to_vec()),
    ];
    let ids = objects
        .iter()
        .map(|(kind, data)| crab_remote::objects::object_id(*kind, data).unwrap())
        .collect::<Vec<_>>();
    let mut tree = Vec::new();
    for (mode, name, oid) in [
        ("120000", "link", ids[2]),
        ("100644", "unrelated.bin", ids[1]),
        ("100644", "wanted.txt", ids[0]),
    ] {
        tree.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        tree.extend_from_slice(oid.as_bytes());
    }
    let tree_oid = crab_remote::objects::object_id(gix_object::Kind::Tree, &tree).unwrap();
    objects.push((gix_object::Kind::Tree, tree));
    let commit = |message: &str| {
        format!(
        "tree {tree_oid}\nauthor Test <test@example.invalid> 1 +0000\ncommitter Test <test@example.invalid> 1 +0000\n\n{message}\n"
    ).into_bytes()
    };
    let main = commit("visible");
    let main_oid = crab_remote::objects::object_id(gix_object::Kind::Commit, &main).unwrap();
    objects.push((gix_object::Kind::Commit, main));
    let dangling = commit("retained");
    let dangling_oid =
        crab_remote::objects::object_id(gix_object::Kind::Commit, &dangling).unwrap();
    objects.push((gix_object::Kind::Commit, dangling));
    let directory = tempfile::tempdir().unwrap();
    let incoming = crab_git::incoming_pack::IncomingPack::from_generated_objects(
        objects.clone(),
        directory.path(),
        crab_git::incoming_pack::ReceiveLimits {
            max_pack_bytes: 8 * 1024 * 1024,
            max_objects: 10,
            max_object_bytes: 4 * 1024 * 1024,
            max_inflated_bytes: 8 * 1024 * 1024,
            max_delta_depth: 8,
        },
        || false,
    )
    .unwrap();
    let prepared = incoming
        .prepare(
            directory.path(),
            8 * 1024 * 1024,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
    let kind_bytes = std::fs::read(prepared.object_metadata_path()).unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from(std::fs::read(prepared.pack_path()).unwrap()),
        Bytes::from(std::fs::read(prepared.index_path()).unwrap()),
        Bytes::from(std::fs::read(prepared.reverse_path()).unwrap()),
        Bytes::from(kind_bytes),
        prepared.git_sha1().to_string(),
        objects.len() as u64,
    )
    .unwrap();
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(main_oid.to_string()),
            None,
        )],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        "refs/heads/main".to_owned(),
        GitVisibilityEdit::from_replacement_objects(
            None,
            main_oid.to_string(),
            ids.into_iter()
                .chain([tree_oid, main_oid])
                .map(|id| id.to_string())
                .collect(),
        ),
    )]))
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![pack],
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            visibility.encode().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
        .await
        .unwrap();
    let config = Config::default();
    let cache = Arc::new(crate::cache::LocalCache::new(
        directory.path().join("cache"),
    ));
    let product_store = crate::storage::Store::from_storage(store);
    let router =
        crate::storage::StoreLayout::new(product_store.clone(), layout.repo_prefix().to_owned());
    let caching_store = crab_cache_store::CachingStore::new_with_local_cache(
        product_store,
        &config.cache,
        cache.clone(),
    )
    .unwrap();
    let hydrator =
        crate::read::build_cli_hydrator(caching_store.clone(), router.clone(), &config).unwrap();
    let reader = RepositoryReader {
        inner: Arc::new(Inner {
            url: "crab://test/read-ranges".to_owned(),
            config,
            cache,
            git_dir: None,
            work_dir: None,
            cancel: CancellationToken::new(),
            remote: tokio::sync::OnceCell::new(),
        }),
    };
    assert!(
        reader
            .inner
            .remote
            .set(Arc::new(RemoteContext {
                caching_store,
                router,
                hydrator: Arc::new(hydrator),
                git_runtime: Arc::new(RemoteGitRuntime::default()),
                lfs_reads: crab_lfs::LfsReadSession::default(),
            }))
            .is_ok()
    );
    observer.0.store(0, Ordering::Relaxed);
    (reader, observer, directory, dangling_oid.to_string())
}

#[tokio::test]
async fn selected_file_reads_ranges_without_installing_unrelated_payload() {
    let (reader, bytes, directory, _) = fixture().await;
    let snapshot = reader.snapshot(Some("main")).await.unwrap();
    let dest = directory.path().join("downloaded.txt");
    snapshot
        .download_to_path("wanted.txt", &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), b"wanted bytes\n");
    assert!(bytes.0.load(Ordering::Relaxed) < 256 * 1024);
    assert!(!reader.cache_dir().join("git").exists());
    reader.shutdown().await;
}

#[tokio::test]
async fn listing_keeps_regular_files_and_omits_symlinks() {
    let (reader, bytes, _, _) = fixture().await;
    let snapshot = reader.snapshot(None).await.unwrap();
    let entries = snapshot.list_entries().await.unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (&*entry.path, entry.size))
            .collect::<Vec<_>>(),
        vec![("unrelated.bin", 2 * 1024 * 1024), ("wanted.txt", 13)]
    );
    assert!(bytes.0.load(Ordering::Relaxed) < 256 * 1024);
    assert!(matches!(
        snapshot.entry_for_path("link").await,
        Err(CrabError::NotFound { .. })
    ));
    reader.shutdown().await;
}

#[tokio::test]
async fn retained_commit_is_not_selected_by_physical_presence() {
    let (reader, _, _, retained) = fixture().await;
    let result = reader.snapshot(Some(&retained)).await;
    let error = result.err().unwrap();
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut rejected = false;
    while let Some(source) = cause {
        rejected |= matches!(
            source.downcast_ref::<crab_remote_git::Error>(),
            Some(crab_remote_git::Error::Revision {
                reason: crab_remote_git::RevisionError::NotReachable
            })
        );
        cause = source.source();
    }
    assert!(rejected, "{error:?}");
    reader.shutdown().await;
}

#[tokio::test]
async fn cancellation_stops_selected_reads_before_storage() {
    let (reader, bytes, _, _) = fixture().await;
    let snapshot = reader.snapshot(None).await.unwrap();
    bytes.0.store(0, Ordering::Relaxed);
    reader.inner.cancel.cancel();
    assert!(matches!(
        snapshot.entry_for_path("wanted.txt").await,
        Err(CrabError::Cancelled)
    ));
    assert_eq!(bytes.0.load(Ordering::Relaxed), 0);
    reader.shutdown().await;
}
