use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_object_locator::{GitPackInventoryEntry, PagedGitObjectIndex};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_read::capsule_protocol::{CapsuleReadLimits, CapsuleRepositoryView};
use crab_remote_git::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::StoreLayout;
use crab_xet::hash::MerkleHash;
use futures_util::{TryStreamExt, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};
use tokio_util::sync::CancellationToken;

use super::{GcArgs, sweep_capsule_objects};
use crate::storage::store::Store;

const LIMIT: u64 = 8 * 1024 * 1024;

// Immutable bodies can predate their first reader by years. Alter only their
// provider timestamps; native pack validation and conditional publication stay real.
#[derive(Debug)]
struct AgedObjects {
    inner: InMemory,
    paths: Mutex<HashSet<Path>>,
    modified: SystemTime,
    list_calls: Mutex<Vec<Path>>,
}

impl std::fmt::Display for AgedObjects {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("aged index fixture")
    }
}

#[async_trait::async_trait]
impl ObjectStore for AgedObjects {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        let mut result = self.inner.get_opts(path, options).await?;
        if self.paths.lock().unwrap().contains(path) {
            result.meta.last_modified = self.modified.into();
        }
        Ok(result)
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        if let Some(prefix) = prefix {
            self.list_calls.lock().unwrap().push(prefix.clone());
        }
        let paths = self.paths.lock().unwrap().clone();
        let modified = self.modified;
        Box::pin(self.inner.list(prefix).map_ok(move |mut meta| {
            if paths.contains(&meta.location) {
                meta.last_modified = modified.into();
            }
            meta
        }))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let mut result = self.inner.list_with_delimiter(prefix).await?;
        let paths = self.paths.lock().unwrap();
        for meta in &mut result.objects {
            if paths.contains(&meta.location) {
                meta.last_modified = self.modified.into();
            }
        }
        Ok(result)
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

async fn publish_blob(layout: &StoreLayout<crab_storage::Store>, name: &str) -> String {
    let payload = name.as_bytes();
    let oid = crab_remote::objects::object_id(gix_object::Kind::Blob, payload)
        .unwrap()
        .to_string();
    let mut bytes = Vec::new();
    crab_git::pack_writer::write_pack(
        &mut bytes,
        std::iter::once(Ok((gix_object::Kind::Blob, payload.len() as u64, payload))),
        LIMIT,
        || false,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("incoming.pack");
    std::fs::write(&path, &bytes).unwrap();
    let installed = crab_git::pack::install_pack_file_from_path(
        &directory.path().join("indexed"),
        &path,
        blake3::hash(&bytes).to_hex().as_ref(),
        LIMIT,
        true,
    )
    .unwrap();
    let checksum = gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).unwrap();
    let kinds =
        crab_git::pack_locator::encode_pack_kind_metadata(checksum, &[gix_object::Kind::Blob])
            .unwrap();
    let pack = CapsuleGitPack::new(
        bytes.into(),
        std::fs::read(installed.idx_path).unwrap().into(),
        std::fs::read(installed.rev_path).unwrap().into(),
        Bytes::from(kinds),
        installed.git_sha1,
        1,
    )
    .unwrap();
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    let name = format!("refs/tags/{name}");
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        name,
        GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid.clone()]),
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
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap();
    oid
}

async fn view(layout: &StoreLayout<crab_storage::Store>) -> CapsuleRepositoryView {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    crab_read::capsule_protocol::open_git_view_from_root(
        layout,
        root,
        CapsuleReadLimits {
            max_capsule_bytes: LIMIT,
            max_frontier_bytes: LIMIT,
        },
    )
    .await
    .unwrap()
}

async fn cold_capture_after_gc(replace_readiness: bool) -> crab_metadata::error::Result<bool> {
    let backend = Arc::new(AgedObjects {
        inner: InMemory::new(),
        paths: Mutex::new(HashSet::new()),
        modified: SystemTime::now() - Duration::from_secs(7200),
        list_calls: Mutex::new(Vec::new()),
    });
    let store = Store::new(backend.clone());
    let layout = StoreLayout::new(store.as_storage().clone(), "org/captured-gc".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let oid = publish_blob(&layout, "original").await;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let identity = RepositoryIdentity::new("local", "captured-gc", 1).unwrap();
    let cancel = CancellationToken::new();
    let indexes = crab_remote::browse_indexes::ensure(
        &layout,
        &identity,
        runtime.clone(),
        RepositoryOptions::default(),
        LIMIT,
        &cancel,
    )
    .await
    .unwrap()
    .unwrap();
    let captured = view(&layout).await;
    let snapshot = captured.git_snapshot().unwrap();
    let reader = PagedGitObjectIndex::load(
        layout.store(),
        &layout,
        indexes.object_index_hash().unwrap(),
        &captured.state_digest(),
        &snapshot.manifest,
        LIMIT,
    )
    .await
    .unwrap();
    // Age only this immutable body. The capture, ref change, readiness
    // replacement and GC all happen now, within a fresh reader's lifetime.
    backend
        .paths
        .lock()
        .unwrap()
        .insert(reader.body_path().clone());
    let inventory = snapshot
        .journal
        .packs
        .iter()
        .map(|pack| {
            let id = MerkleHash::from_hex(&pack.pack_id).unwrap();
            (
                id,
                GitPackInventoryEntry {
                    pack_id: id,
                    object_count: pack.object_count,
                    pack_size: pack.size,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    publish_blob(&layout, "later").await;
    if replace_readiness {
        let replacement = crab_remote::browse_indexes::ensure(
            &layout,
            &identity,
            runtime.clone(),
            RepositoryOptions::default(),
            LIMIT,
            &cancel,
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(replacement.object_index_hash(), indexes.object_index_hash());
    }
    let current = view(&layout).await;
    let current_snapshot = current.git_snapshot().unwrap();
    sweep_capsule_objects(
        &GcArgs::default(),
        &store,
        &layout,
        current.root().root(),
        current.capsule_run_pointers(),
        &current_snapshot.manifest,
        &HashSet::new(),
        &cancel,
        SystemTime::now(),
        Duration::from_secs(3600),
        Instant::now(),
    )
    .await
    .unwrap();
    let oid = gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap();
    let oid: [u8; 20] = oid.as_bytes().try_into().unwrap();
    let result = reader.lookup(layout.store(), &[oid], &inventory).await;
    runtime.shutdown().await;
    result.map(|entries| entries[0].is_some())
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_preserves_cold_captures_after_ref_movement() {
    let result = cold_capture_after_gc(false).await;
    assert!(
        matches!(result, Ok(true)),
        "cold capture lost after ref movement: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_preserves_cold_captures_after_readiness_replacement() {
    let result = cold_capture_after_gc(true).await;
    assert!(
        matches!(result, Ok(true)),
        "cold capture lost after readiness replacement: {result:?}"
    );
}

#[tokio::test]
async fn capsule_gc_lists_each_candidate_namespace_once() {
    let backend = Arc::new(AgedObjects {
        inner: InMemory::new(),
        paths: Mutex::new(HashSet::new()),
        modified: SystemTime::now(),
        list_calls: Mutex::new(Vec::new()),
    });
    let store = Store::new(backend.clone());
    let layout = StoreLayout::new(store.as_storage().clone(), "org/gc-listing".to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
    manifest.seal_git_validation();
    backend.list_calls.lock().unwrap().clear();
    let outcome = sweep_capsule_objects(
        &GcArgs::default(),
        &store,
        &layout,
        root.record().root(),
        &[],
        &manifest,
        &HashSet::new(),
        &CancellationToken::new(),
        SystemTime::now(),
        Duration::from_secs(3600),
        Instant::now(),
    )
    .await
    .unwrap();
    let mut actual = backend.list_calls.lock().unwrap().clone();
    actual.sort();
    let mut expected = [
        "v2/capsules/",
        "v2/checkpoints/",
        "v2/pack-layers/",
        "v2/history/",
        "metadata/",
        "v2/browse-roots/",
        "manifests/",
    ]
    .map(|suffix| layout.repo_path(suffix))
    .to_vec();
    expected.sort();
    assert_eq!(
        (actual, outcome.list_requests, outcome.list_parallelism),
        (expected, 7, 7)
    );
}

#[tokio::test]
async fn capsule_gc_preserves_metadata_outside_owned_index_directories() {
    let backend = Arc::new(AgedObjects {
        inner: InMemory::new(),
        paths: Mutex::new(HashSet::new()),
        modified: SystemTime::now() - Duration::from_secs(7200),
        list_calls: Mutex::new(Vec::new()),
    });
    let store = Store::new(backend.clone());
    let layout = StoreLayout::new(store.as_storage().clone(), "org/gc-metadata".to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let unrelated = [
        "unrelated/keep",
        "path-state-backups/keep",
        "commit-graph-backups/keep",
        "git-object-index-backups/keep",
    ]
    .map(|suffix| layout.repo_path(&format!("metadata/{suffix}")));
    for path in &unrelated {
        store
            .put(path, Bytes::from_static(b"unowned metadata"))
            .await
            .unwrap();
        backend.paths.lock().unwrap().insert(path.clone());
    }
    let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
    manifest.seal_git_validation();
    sweep_capsule_objects(
        &GcArgs::default(),
        &store,
        &layout,
        root.record().root(),
        &[],
        &manifest,
        &HashSet::new(),
        &CancellationToken::new(),
        SystemTime::now(),
        Duration::from_secs(3600),
        Instant::now(),
    )
    .await
    .unwrap();
    let retained =
        futures_util::future::try_join_all(unrelated.iter().map(|path| store.head(path))).await;
    assert!(
        retained.is_ok(),
        "GC removed metadata outside its owned directories: {retained:?}"
    );
}
