#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::*;
use bytes::Bytes;

mod checkpoint_cache;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectStore, ObjectStoreExt,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug)]
enum Identity {
    ETag,
    WeakETag,
    FixedETag,
    Version,
    NullVersion,
    EmptyVersion,
    Absent,
}

#[derive(Debug)]
struct WriteBurst {
    path: object_store::path::Path,
    until: tokio::time::Instant,
    bodies: [Bytes; 2],
    next: usize,
}

#[derive(Debug)]
struct ChangingHeads {
    inner: object_store::memory::InMemory,
    identity: Identity,
    replacements: Mutex<BTreeMap<object_store::path::Path, Option<Bytes>>>,
    reads: Mutex<BTreeMap<String, usize>>,
    lists: AtomicUsize,
    always_changing: bool,
    burst: Mutex<Option<WriteBurst>>,
}

impl std::fmt::Display for ChangingHeads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("changing ref heads")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ChangingHeads {
    async fn put_opts(
        &self,
        path: &object_store::path::Path,
        body: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, body, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &object_store::path::Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &object_store::path::Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        *self
            .reads
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default() += 1;
        let mut result = self.inner.get_opts(path, options).await?;
        if matches!(self.identity, Identity::WeakETag) {
            result.meta.e_tag = result.meta.e_tag.map(|tag| format!("W/{tag}"));
        }
        if matches!(self.identity, Identity::FixedETag) {
            result.meta.e_tag = Some("opaque".to_owned());
        }
        if matches!(self.identity, Identity::Version) {
            result.meta.version = result.meta.e_tag.take();
        }
        if matches!(
            self.identity,
            Identity::NullVersion | Identity::EmptyVersion
        ) {
            result.meta.version = Some(
                if matches!(self.identity, Identity::NullVersion) {
                    "null"
                } else {
                    ""
                }
                .to_owned(),
            );
            result.meta.e_tag = None;
        }
        let replacement = self.replacements.lock().unwrap().remove(path);
        let burst = {
            let mut burst = self.burst.lock().unwrap();
            burst
                .as_mut()
                .filter(|burst| burst.path == *path && tokio::time::Instant::now() < burst.until)
                .map(|burst| {
                    let body = burst.bodies[burst.next % 2].clone();
                    burst.next += 1;
                    Some(body)
                })
        };
        let replacement = burst.or(replacement);
        if let Some(body) = replacement {
            // Publish between the first listing/body read and the second listing.
            match body {
                Some(body) => {
                    self.inner.put(path, body.into()).await?;
                }
                None => {
                    self.inner.delete(path).await?;
                }
            }
        }
        Ok(result)
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<object_store::path::Path>>,
    ) -> BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let identity = self.identity;
        let sequence = self.lists.fetch_add(1, Ordering::SeqCst);
        let changing = self.always_changing;
        self.inner
            .list(prefix)
            .map(move |item| {
                item.map(|mut meta| {
                    match identity {
                        Identity::ETag => {}
                        Identity::WeakETag => {
                            meta.e_tag = meta.e_tag.map(|tag| format!("W/{tag}"));
                        }
                        Identity::FixedETag => {
                            meta.e_tag = Some("opaque".to_owned());
                        }
                        Identity::Version => {
                            meta.version = meta.e_tag.take();
                        }
                        Identity::NullVersion | Identity::EmptyVersion => {
                            meta.version = Some(
                                if matches!(identity, Identity::NullVersion) {
                                    "null"
                                } else {
                                    ""
                                }
                                .to_owned(),
                            );
                            meta.e_tag = None;
                        }
                        Identity::Absent => {
                            meta.e_tag = None;
                            meta.version = None;
                        }
                    }
                    if changing {
                        meta.size += sequence as u64;
                    }
                    meta
                })
            })
            .boxed()
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

async fn fixture(
    identity: Identity,
    deleted: bool,
    always_changing: bool,
) -> (
    StoreLayout<Store>,
    super::super::RepositoryRoot,
    Arc<ChangingHeads>,
) {
    let store = Arc::new(ChangingHeads {
        inner: object_store::memory::InMemory::new(),
        identity,
        replacements: Mutex::new(BTreeMap::new()),
        reads: Mutex::new(BTreeMap::new()),
        lists: AtomicUsize::new(0),
        always_changing,
        burst: Mutex::new(None),
    });
    let layout = StoreLayout::new(
        Store::new(store.clone()),
        "repositories/capture-retries".to_owned(),
    );
    let root = super::super::RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap();
    for name in ["refs/heads/changed", "refs/heads/unchanged"] {
        let head =
            CapsuleRefHead::from_root(name, root.ref_epoch().to_owned(), None, None).unwrap();
        layout
            .store()
            .create_strict(
                &layout.capsule_ref_head_path(&capsule_ref_name_key(name)),
                head.encode().unwrap(),
            )
            .await
            .unwrap();
    }
    let retired = CapsuleRefHead::from_root(
        "refs/heads/changed",
        "2".repeat(64),
        Some("3".repeat(40)),
        None,
    )
    .unwrap();
    store.replacements.lock().unwrap().insert(
        layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/changed")),
        (!deleted).then(|| retired.encode().unwrap()),
    );
    (layout, root, store)
}

#[tokio::test]
async fn ref_capture_reuses_only_unchanged_provider_versions_during_retry() {
    for identity in [
        Identity::ETag,
        Identity::WeakETag,
        Identity::Version,
        Identity::NullVersion,
        Identity::EmptyVersion,
        Identity::Absent,
    ] {
        for deleted in [false, true] {
            let (layout, root, store) = fixture(identity, deleted, false).await;
            let (heads, active) = capture_ref_heads(&layout, &root).await.unwrap();
            assert!(active.is_empty());
            assert_eq!(heads.len(), 1);
            assert_eq!(heads[0].ref_name(), "refs/heads/unchanged");
            assert_eq!(store.lists.load(Ordering::SeqCst), 3);
            let reads = store.reads.lock().unwrap();
            let unchanged =
                layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/unchanged"));
            assert_eq!(
                reads[&unchanged.to_string()],
                if matches!(
                    identity,
                    Identity::Absent
                        | Identity::NullVersion
                        | Identity::EmptyVersion
                        | Identity::WeakETag
                ) {
                    2
                } else {
                    1
                },
                "{identity:?}, deleted={deleted}"
            );
            let changed = layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/changed"));
            assert_eq!(reads[&changed.to_string()], if deleted { 1 } else { 2 });
        }
    }
}

#[tokio::test(start_paused = true)]
async fn ref_capture_reuse_preserves_the_bounded_stability_check() {
    let (layout, root, store) = fixture(Identity::ETag, false, true).await;
    assert!(capture_ref_heads(&layout, &root).await.is_err());
    assert_eq!(store.lists.load(Ordering::SeqCst), 33);
}

#[tokio::test(start_paused = true)]
async fn ref_capture_waits_for_a_finite_write_burst() {
    for burst_ms in [40, 900] {
        let (layout, root, store) = fixture(Identity::ETag, false, false).await;
        store.replacements.lock().unwrap().clear();
        let name = "refs/heads/changed";
        let path = layout.capsule_ref_head_path(&capsule_ref_name_key(name));
        let current = CapsuleRefHead::from_root(name, root.ref_epoch().to_owned(), None, None)
            .unwrap()
            .encode()
            .unwrap();
        let retired = CapsuleRefHead::from_root(name, "2".repeat(64), None, None)
            .unwrap()
            .encode()
            .unwrap();
        *store.burst.lock().unwrap() = Some(WriteBurst {
            path: path.clone(),
            until: tokio::time::Instant::now() + std::time::Duration::from_millis(burst_ms),
            bodies: [retired, current],
            next: 0,
        });

        let captured = capture_ref_heads(&layout, &root).await.unwrap();
        let mut expected = Vec::new();
        for name in ["refs/heads/changed", "refs/heads/unchanged"] {
            let path = layout.capsule_ref_head_path(&capsule_ref_name_key(name));
            let body = store.inner.get(&path).await.unwrap().bytes().await.unwrap();
            let head = CapsuleRefHead::decode(&body).unwrap();
            if head.ref_epoch() == root.ref_epoch() {
                expected.push(head);
            }
        }
        assert_eq!(captured, (expected, BTreeSet::new()));
    }
}

async fn prepared_head_fixture() -> (
    StoreLayout<Store>,
    super::super::RepositoryRoot,
    Arc<ChangingHeads>,
    super::super::CapsuleTransactionRecord,
    String,
) {
    use super::super::{
        Capsule, CapsulePointer, CapsuleRefEdit, CapsuleTransaction, CapsuleTransactionRecord,
    };
    let (layout, root, store) = fixture(Identity::ETag, false, false).await;
    let base = RootRecord::encode(root.clone()).unwrap();
    let name = "refs/heads/unchanged";
    let oid = "4".repeat(40);
    let transaction = CapsuleTransaction::new(
        base.digest(),
        vec![CapsuleRefEdit::new(name, None, Some(oid.clone()), None)],
    )
    .unwrap();
    let run = CapsuleRun::leaf(Capsule::build(&transaction, vec![], vec![]).unwrap()).unwrap();
    let pointer = CapsulePointer::new(
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
    let transaction_id = transaction.id().unwrap();
    let activation = "5".repeat(64);
    let head = CapsuleRefHead::from_root(name, root.ref_epoch().to_owned(), None, None).unwrap();
    let visible = head.visible(&BTreeSet::new()).clone();
    let next = visible
        .successor(
            Some(oid.clone()),
            None,
            transaction_id.clone(),
            vec![pointer],
        )
        .unwrap();
    let prepared = head.prepare(visible, activation.clone(), next).unwrap();
    let head_path = layout.capsule_ref_head_path(&capsule_ref_name_key(name));
    layout
        .store()
        .put_overwrite(&head_path, prepared.encode().unwrap())
        .await
        .unwrap();
    let preparing =
        CapsuleTransactionRecord::preparing(activation.clone(), transaction_id).unwrap();
    let record_path = layout.capsule_transaction_path(&activation);
    layout
        .store()
        .create_strict(&record_path, preparing.encode().unwrap())
        .await
        .unwrap();
    (layout, root, store, preparing, oid)
}

#[tokio::test]
async fn ref_capture_rechecks_activation_when_a_prepared_head_is_reused() {
    let (layout, root, store, preparing, oid) = prepared_head_fixture().await;
    let activation = preparing.activation_id().to_owned();
    let record_path = layout.capsule_transaction_path(&activation);
    let head_path = layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/unchanged"));
    store.replacements.lock().unwrap().insert(
        record_path.clone(),
        Some(preparing.commit().unwrap().encode().unwrap()),
    );

    let (heads, active) = capture_ref_heads(&layout, &root).await.unwrap();
    assert_eq!(active, BTreeSet::from([activation]));
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].visible(&active).oid(), Some(oid.as_str()));
    let reads = store.reads.lock().unwrap();
    assert_eq!(reads[&head_path.to_string()], 1);
    assert_eq!(reads[&record_path.to_string()], 2);
}
