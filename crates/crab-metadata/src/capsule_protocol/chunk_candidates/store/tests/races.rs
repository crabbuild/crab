use super::*;
use futures_util::{StreamExt, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct RootWritePause {
    path: Path,
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

struct Replacement {
    trigger: Path,
    root: Path,
    body: Bytes,
    retired_page: Path,
}
#[derive(Default)]
struct Hooks {
    replacement: Mutex<Option<Replacement>>,
    root_barrier: Option<(Path, tokio::sync::Barrier)>,
    root_reads: AtomicUsize,
    reads: AtomicUsize,
    denied_writes: AtomicUsize,
    corrupt_run_reads: bool,
    root_write_pause: Option<RootWritePause>,
    tracked_run: Option<Path>,
    tracked_reads: AtomicUsize,
}
struct ScriptedStore {
    inner: Arc<dyn ObjectStore>,
    read_only: bool,
    hooks: Arc<Hooks>,
}
impl std::fmt::Debug for ScriptedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptedStore")
    }
}
impl std::fmt::Display for ScriptedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scripted hint store")
    }
}
fn denied(path: &Path) -> object_store::Error {
    object_store::Error::PermissionDenied {
        path: path.to_string(),
        source: Box::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
    }
}
#[async_trait::async_trait]
impl ObjectStore for ScriptedStore {
    async fn put_opts(
        &self,
        path: &Path,
        body: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.read_only {
            self.hooks.denied_writes.fetch_add(1, Ordering::Relaxed);
            return Err(denied(path));
        }
        if let Some(pause) = &self.hooks.root_write_pause
            && pause.path == *path
            && matches!(options.mode, object_store::PutMode::Update(_))
            && pause.armed.swap(false, Ordering::SeqCst)
        {
            pause.entered.notify_one();
            pause.resume.notified().await;
        }
        self.inner.put_opts(path, body, options).await
    }
    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        if self.read_only {
            self.hooks.denied_writes.fetch_add(1, Ordering::Relaxed);
            return Err(denied(path));
        }
        self.inner.put_multipart_opts(path, options).await
    }
    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        if self.hooks.tracked_run.as_ref() == Some(path) {
            self.hooks.tracked_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.hooks.reads.fetch_add(1, Ordering::Relaxed);
        let replacement = {
            let mut pending = self.hooks.replacement.lock().unwrap();
            if pending.as_ref().is_some_and(|p| p.trigger == *path) {
                pending.take()
            } else {
                None
            }
        };
        if let Some(replacement) = replacement {
            // Model an independent publisher and sweep, not writes by the reader.
            self.inner
                .put(&replacement.root, replacement.body.into())
                .await?;
            self.inner.delete(&replacement.retired_page).await?;
        }
        if self.hooks.corrupt_run_reads && path.as_ref().contains("/capsule-chunk-pages/") {
            self.inner
                .put(path, Bytes::from_static(b"damaged run").into())
                .await?;
        }
        let result = self.inner.get_opts(path, options).await;
        if let Some((root, barrier)) = &self.hooks.root_barrier
            && root == path
            && self.hooks.root_reads.fetch_add(1, Ordering::Relaxed) < 2
        {
            barrier.wait().await;
        }
        result
    }
    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        if !self.read_only {
            return self.inner.delete_stream(paths);
        }
        let hooks = self.hooks.clone();
        paths
            .map(move |path| {
                hooks.denied_writes.fetch_add(1, Ordering::Relaxed);
                path.and_then(|path| Err(denied(&path)))
            })
            .boxed()
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        if self.read_only {
            self.hooks.denied_writes.fetch_add(1, Ordering::Relaxed);
            return Err(denied(to));
        }
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn read_only_lookup_recaptures_after_retired_page_is_swept() {
    let layout = layout();
    let (catalog, origins, hashes) = fixture(2, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let old_index = CapsuleChunkIndex::new(&layout);
    let (old_body, _) = layout
        .store()
        .get_with_etag(&layout.capsule_chunk_index_path())
        .await
        .unwrap();
    let old_root: Root = serde_json::from_slice(&old_body).unwrap();
    let old_page = old_root
        .levels
        .iter()
        .flatten()
        .next()
        .unwrap()
        .path(&old_index.prefix);
    let old = old_index
        .candidates(layout.store(), &hashes, 1024 * 1024)
        .await
        .unwrap();
    let newer = StoreLayout::new(layout.store().clone(), "newer-source".to_owned());
    write_capsule_chunk_candidates(&newer, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let (new_body, _) = layout
        .store()
        .get_with_etag(&layout.capsule_chunk_index_path())
        .await
        .unwrap();
    let (_, live) = CapsuleChunkIndex::new(&layout)
        .objects(layout.store(), 1024 * 1024)
        .await
        .unwrap();
    assert!(!live.iter().any(|(path, _)| *path == old_page));
    layout
        .store()
        .put_overwrite(&layout.capsule_chunk_index_path(), old_body)
        .await
        .unwrap();
    let hooks = Arc::new(Hooks {
        replacement: Mutex::new(Some(Replacement {
            trigger: old_page.clone(),
            root: layout.capsule_chunk_index_path(),
            body: new_body,
            retired_page: old_page,
        })),
        ..Hooks::default()
    });
    let reader = Store::new(Arc::new(ScriptedStore {
        inner: layout.store().inner().clone(),
        read_only: true,
        hooks: hooks.clone(),
    }));
    let reader_layout = StoreLayout::new(reader.clone(), layout.repo_prefix().to_owned());
    let current = CapsuleChunkIndex::new(&reader_layout)
        .candidates(&reader, &hashes, 1024 * 1024)
        .await
        .unwrap();
    assert!(
        current
            .anchors
            .values()
            .all(|anchor| anchor.source_repo_prefix == "newer-source")
    );
    let ids = old
        .placements
        .values()
        .map(|p| (MerkleHash::from(p.chunk_hash), p.placement_id()))
        .collect::<Vec<_>>();
    let exact = CapsuleChunkIndex::new(&reader_layout)
        .candidates_by_id(&reader, &ids, 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(exact.placements, old.placements);
    assert_eq!(hooks.denied_writes.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_hint_publishers_merge_after_root_create_conflict() {
    let memory: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let plain = StoreLayout::new(Store::new(memory.clone()), "first".to_owned());
    let hooks = Arc::new(Hooks {
        root_barrier: Some((
            plain.capsule_chunk_index_path(),
            tokio::sync::Barrier::new(2),
        )),
        ..Hooks::default()
    });
    let store = Store::new(Arc::new(ScriptedStore {
        inner: memory,
        read_only: false,
        hooks,
    }));
    let first = StoreLayout::new(store.clone(), "first".to_owned());
    let second = StoreLayout::new(store.clone(), "second".to_owned());
    let (one, one_origins, mut hashes) = fixture(2, 3);
    let (two, two_origins, other_hashes) = fixture(2, 5);
    let (a, b) = tokio::join!(
        write_capsule_chunk_candidates(&first, &one, &one_origins, &|| false),
        write_capsule_chunk_candidates(&second, &two, &two_origins, &|| false)
    );
    a.unwrap();
    b.unwrap();
    hashes.extend(other_hashes);
    let result = CapsuleChunkIndex::new(&first)
        .candidates(&store, &hashes, 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(result.placements.len(), 4);
}

#[tokio::test]
async fn request_budget_failure_precedes_storage_access() {
    let plain = layout();
    let hooks = Arc::new(Hooks::default());
    let reader = Store::new(Arc::new(ScriptedStore {
        inner: plain.store().inner().clone(),
        read_only: true,
        hooks: hooks.clone(),
    }));
    let reader_layout = StoreLayout::new(reader.clone(), plain.repo_prefix().to_owned());
    assert!(
        CapsuleChunkIndex::new(&reader_layout)
            .candidates(&reader, &[MerkleHash::from([1; 32])], 1)
            .await
            .is_err()
    );
    assert_eq!(hooks.reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn failed_run_readback_never_publishes_a_root() {
    let memory: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let store = Store::new(Arc::new(ScriptedStore {
        inner: memory,
        read_only: false,
        hooks: Arc::new(Hooks {
            corrupt_run_reads: true,
            ..Hooks::default()
        }),
    }));
    let layout = StoreLayout::new(store.clone(), "source".to_owned());
    let (catalog, origins, _) = fixture(3, 7);
    assert!(
        write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
            .await
            .is_err()
    );
    assert!(matches!(
        store
            .get_with_etag(&layout.capsule_chunk_index_path())
            .await,
        Err(StorageError::NotFound { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_updates_preserve_both_exact_placement_ids() {
    let memory: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let plain = StoreLayout::new(Store::new(memory.clone()), "base".to_owned());
    let (base, origins, hashes) = fixture(3000, 3);
    write_capsule_chunk_candidates(&plain, &base, &origins, &|| false)
        .await
        .unwrap();
    let store = Store::new(Arc::new(ScriptedStore {
        inner: memory,
        read_only: false,
        hooks: Arc::new(Hooks {
            root_barrier: Some((
                plain.capsule_chunk_index_path(),
                tokio::sync::Barrier::new(2),
            )),
            ..Hooks::default()
        }),
    }));
    let first = StoreLayout::new(store.clone(), "first".to_owned());
    let second = StoreLayout::new(store.clone(), "second".to_owned());
    let (one, one_origins, _) = fixture_from_hashes(5, vec![hashes[0]]);
    let (two, two_origins, _) = fixture_from_hashes(7, vec![hashes[0]]);
    let (a, b) = tokio::join!(
        write_capsule_chunk_candidates(&first, &one, &one_origins, &|| false),
        write_capsule_chunk_candidates(&second, &two, &two_origins, &|| false)
    );
    a.unwrap();
    b.unwrap();
    let index = CapsuleChunkIndex::new(&first);
    for (layout, catalog, origins, seed) in [
        (&first, &one, &one_origins, 5),
        (&second, &two, &two_origins, 7),
    ] {
        let xorb = MerkleHash::from([seed; 4]);
        let rows = capsule_chunk_candidate_rows(
            layout.repo_prefix(),
            &"2".repeat(64),
            xorb,
            &catalog.xorbs()[&xorb.hex()],
            &origins[&xorb],
        )
        .unwrap();
        let placement = rows.into_iter().find(|(key, _)| key[0] == 0x08).unwrap();
        let id = CommittedChunkPlacement::decode(&placement.1)
            .unwrap()
            .placement_id();
        let exact = index
            .candidates_by_id(&store, &[(hashes[0], id)], 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            exact.anchors.values().next().unwrap().source_repo_prefix,
            layout.repo_prefix()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn root_retry_reuses_only_unchanged_run_pages() {
    for replace_existing_head in [false, true] {
        let memory: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let plain = StoreLayout::new(Store::new(memory.clone()), "base".to_owned());
        let (base, origins, hashes) = fixture(3000, 3);
        write_capsule_chunk_candidates(&plain, &base, &origins, &|| false)
            .await
            .unwrap();
        let first_seed = StoreLayout::new(plain.store().clone(), "first".to_owned());
        let racer = StoreLayout::new(plain.store().clone(), "racer".to_owned());
        let (one, one_origins, _) = fixture_from_hashes(5, vec![hashes[0]]);
        let other_hash = if replace_existing_head {
            hashes[0]
        } else {
            hashes[1]
        };
        let (two, two_origins, _) = fixture_from_hashes(7, vec![other_hash]);
        if replace_existing_head {
            write_capsule_chunk_candidates(&first_seed, &one, &one_origins, &|| false)
                .await
                .unwrap();
            write_capsule_chunk_candidates(&racer, &two, &two_origins, &|| false)
                .await
                .unwrap();
            // Reuse the captured proof: a fresh timestamp creates another
            // immutable anchor instead of isolating a mutable-head update.
            let (head, _, _) = fixture_from_hashes(3, vec![hashes[0]]);
            write_capsule_chunk_candidates(&plain, &head, &origins, &|| false)
                .await
                .unwrap();
        }
        let index = CapsuleChunkIndex::new(&plain);
        let root = index
            .load(plain.store(), &mut PageWork::new(16 * 1024 * 1024, 0))
            .await
            .unwrap()
            .unwrap()
            .0;
        let large = root
            .levels
            .iter()
            .flatten()
            .max_by_key(|run| run.bytes)
            .unwrap();
        let hooks = Arc::new(Hooks {
            root_write_pause: Some(RootWritePause {
                path: plain.capsule_chunk_index_path(),
                armed: AtomicBool::new(true),
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            }),
            tracked_run: Some(large.path(&index.prefix)),
            ..Hooks::default()
        });
        let first = StoreLayout::new(
            Store::new(Arc::new(ScriptedStore {
                inner: memory,
                read_only: false,
                hooks: hooks.clone(),
            })),
            "first".to_owned(),
        );
        let task = tokio::spawn(async move {
            write_capsule_chunk_candidates(&first, &one, &one_origins, &|| false).await
        });
        let pause = hooks.root_write_pause.as_ref().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), pause.entered.notified())
            .await
            .unwrap();
        let before = hooks.tracked_reads.load(Ordering::Relaxed);
        write_capsule_chunk_candidates(&racer, &two, &two_origins, &|| false)
            .await
            .unwrap();
        let changed = index
            .load(plain.store(), &mut PageWork::new(16 * 1024 * 1024, 0))
            .await
            .unwrap()
            .unwrap()
            .0;
        assert!(changed.levels.iter().flatten().any(|run| run == large));
        if replace_existing_head {
            assert!(
                root.levels
                    .iter()
                    .zip(&changed.levels)
                    .any(|(old, new)| old.is_some() && new.is_some() && old != new)
            );
        }
        pause.resume.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let found = index
            .candidates(plain.store(), &hashes[..2], 16 * 1024 * 1024)
            .await
            .unwrap();
        let second_source = if replace_existing_head {
            "base"
        } else {
            "racer"
        };
        for (hash, source) in [(hashes[0], "first"), (hashes[1], second_source)] {
            let placement = &found.placements[&<[u8; 32]>::from(hash)];
            assert_eq!(
                found.anchors[&placement.source_anchor_id].source_repo_prefix,
                source
            );
        }
        assert_eq!(
            (before > 0, hooks.tracked_reads.load(Ordering::Relaxed)),
            (true, before)
        );
    }
}
