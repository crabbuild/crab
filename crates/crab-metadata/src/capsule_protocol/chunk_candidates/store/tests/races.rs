use super::*;
use futures_util::{StreamExt, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    let old_page = old_index.pages.path(&old_root.records.hash);
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

#[tokio::test]
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
