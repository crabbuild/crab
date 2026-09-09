#![cfg(feature = "publication")]

use std::{fmt, sync::Arc, time::Duration};

use crab_coordination::{CoordinationError, GcFenceLease, PushLock};
use crab_remote::publication::{Error, with_internal_lease, with_leases, with_plan};
use crab_storage::{Store, StoreLayout};
use futures_util::stream::{BoxStream, StreamExt};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, path::Path,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct FlatFileStore {
    inner: object_store::local::LocalFileSystem,
}

impl FlatFileStore {
    fn new(root: &std::path::Path) -> Self {
        Self {
            inner: object_store::local::LocalFileSystem::new_with_prefix(root).unwrap(),
        }
    }

    fn encode(path: &Path) -> Path {
        let mut encoded = String::with_capacity(path.as_ref().len() * 2);
        for byte in path.as_ref().as_bytes() {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").unwrap();
        }
        Path::from(encoded)
    }

    fn decode(path: &Path) -> object_store::Result<Path> {
        let value = path.as_ref().as_bytes();
        if !value.len().is_multiple_of(2) {
            return Err(Self::decode_error(path));
        }
        let mut decoded = Vec::with_capacity(value.len() / 2);
        for pair in value.chunks_exact(2) {
            let digits = std::str::from_utf8(pair).map_err(|_| Self::decode_error(path))?;
            decoded.push(u8::from_str_radix(digits, 16).map_err(|_| Self::decode_error(path))?);
        }
        let decoded = String::from_utf8(decoded).map_err(|_| Self::decode_error(path))?;
        Ok(Path::from(decoded))
    }

    fn decode_error(path: &Path) -> object_store::Error {
        object_store::Error::Generic {
            store: "flat-file-test",
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid encoded object path {path}"),
            )),
        }
    }
}

impl fmt::Display for FlatFileStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FlatFileStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for FlatFileStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        mut options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let encoded = Self::encode(location);
        if let PutMode::Update(expected) = &options.mode {
            let current = self.inner.head(&encoded).await?;
            let etag_mismatch = expected
                .e_tag
                .as_ref()
                .is_some_and(|etag| current.e_tag.as_ref() != Some(etag));
            let version_mismatch = expected
                .version
                .as_ref()
                .is_some_and(|version| current.version.as_ref() != Some(version));
            if etag_mismatch || version_mismatch {
                return Err(object_store::Error::Precondition {
                    path: location.to_string(),
                    source: Box::new(std::io::Error::other("flat-file test CAS mismatch")),
                });
            }
            // The crash fixture has no concurrent writers after its child exits.
            // LocalFileSystem lacks update mode, so exact readback supplies its CAS proof.
            options.mode = PutMode::Overwrite;
        }
        self.inner.put_opts(&encoded, payload, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let mut result = self
            .inner
            .get_opts(&Self::encode(location), options)
            .await?;
        result.meta.location = location.clone();
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner
            .put_multipart_opts(&Self::encode(location), options)
            .await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let encoded = locations
            .map(|location| location.map(|path| Self::encode(&path)))
            .boxed();
        self.inner
            .delete_stream(encoded)
            .map(|result| result.and_then(|path| Self::decode(&path)))
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let prefix = prefix.map(ToString::to_string);
        self.inner
            .list(None)
            .filter_map(move |result| {
                let prefix = prefix.clone();
                async move {
                    match result {
                        Ok(mut meta) => match Self::decode(&meta.location) {
                            Ok(location)
                                if prefix
                                    .as_ref()
                                    .is_none_or(|prefix| location.as_ref().starts_with(prefix)) =>
                            {
                                meta.location = location;
                                Some(Ok(meta))
                            }
                            Ok(_) => None,
                            Err(error) => Some(Err(error)),
                        },
                        Err(error) => Some(Err(error)),
                    }
                }
            })
            .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let objects = self.list(prefix).collect::<Vec<_>>().await;
        Ok(ListResult {
            common_prefixes: vec![],
            objects: objects.into_iter().collect::<object_store::Result<_>>()?,
            extensions: Default::default(),
        })
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner
            .copy_opts(&Self::encode(from), &Self::encode(to), options)
            .await
    }
}

fn fixture() -> (Store, StoreLayout<Store>) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::with_global_prefix(
        store.clone(),
        "repositories/one".to_owned(),
        "shared".to_owned(),
    );
    (store, layout)
}

async fn initialized_fixture() -> (Store, StoreLayout<Store>) {
    let (store, layout) = fixture();
    crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &layout)
        .await
        .unwrap();
    crab_metadata::manifest_store::create_manifest(
        &store,
        &layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    (store, layout)
}

#[test]
fn lost_marker_reply_preserves_outcome() {
    let transaction_id = "a".repeat(64);
    let transport = crab_storage::StorageError::NetworkTransient {
        source: object_store::Error::Generic {
            store: "publication-test",
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "marker reply lost",
            )),
        },
    };
    let result = crab_remote::publication::journal_outcome(Err(crab_write::WriteError::Metadata(
        crab_metadata::error::MetadataError::RefJournalCommitUncertain {
            transaction_id: transaction_id.clone(),
            source: Box::new(transport),
            verification: None,
        },
    )))
    .unwrap();

    assert!(matches!(
        result,
        crab_remote::publication::CommitOutcome::Indeterminate {
            transaction_id: actual,
            ..
        } if actual == transaction_id
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_conflict_rejects_batch() {
    use crab_metadata::ref_journal::RefJournalEdit;
    type TestError = Box<dyn std::error::Error + Send + Sync>;

    let (store, layout) = initialized_fixture().await;
    let names = ["refs/heads/topic", "refs/heads/topic/child"];
    let (store, layout) = (&store, &layout);
    let result = with_leases(
        store,
        layout,
        names.map(str::to_owned),
        Duration::from_secs(60),
        &CancellationToken::new(),
        |holders, cancel| async move {
            let snapshot =
                crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
            let edits = names
                .into_iter()
                .map(|name| RefJournalEdit {
                    ref_name: name.to_owned(),
                    old_oid: None,
                    new_oid: Some("b".repeat(40)),
                    peeled_oid: None,
                    lock_holder: holders.get(name).cloned(),
                    visibility_evidence_hash: None,
                })
                .collect();
            crab_write::journal::commit_edits(
                store,
                layout,
                &snapshot,
                edits,
                None,
                vec![],
                vec![],
                crab_write::journal::CommitOptions::new(Duration::from_secs(60), &cancel),
            )
            .await?;
            Ok::<_, TestError>(())
        },
    )
    .await;

    assert!(matches!(
        result.unwrap_err().downcast_ref::<crab_write::WriteError>(),
        Some(crab_write::WriteError::Namespace(_))
    ));
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(store, layout)
        .await
        .unwrap();
    assert!(snapshot.journal.refs.is_empty());
    assert!(snapshot.journal.transactions.is_empty());
    assert_released(store, layout).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_and_gc_preserve_receipt_proof() {
    use crab_metadata::{plan_receipt, ref_journal::RefJournalEdit};
    use crab_remote::publication::with_plan;
    type TestError = Box<dyn std::error::Error + Send + Sync>;

    let (store, layout) = initialized_fixture().await;
    let name = "refs/heads/main";
    let plan_id = "c".repeat(64);
    let ttl = Duration::from_secs(60);
    let cancel = CancellationToken::new();
    let (store, layout, plan_id) = (&store, &layout, plan_id.as_str());
    let committed = with_plan(store, layout, plan_id, ttl, &cancel, |cancel| async move {
        with_leases(
            store,
            layout,
            [name.to_owned()],
            ttl,
            &cancel,
            |holders, cancel| async move {
                let snapshot =
                    crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
                let committed = crab_write::journal::commit_edits(
                    store,
                    layout,
                    &snapshot,
                    vec![RefJournalEdit {
                        ref_name: name.to_owned(),
                        old_oid: None,
                        new_oid: Some("d".repeat(40)),
                        peeled_oid: None,
                        lock_holder: holders.get(name).cloned(),
                        visibility_evidence_hash: None,
                    }],
                    None,
                    vec![],
                    vec![],
                    crab_write::journal::CommitOptions::new(ttl, &cancel).with_plan(plan_id),
                )
                .await?;
                Ok::<_, TestError>(committed)
            },
        )
        .await
    })
    .await
    .unwrap();
    store
        .delete(&layout.ref_journal_plan_receipt_path(plan_id))
        .await
        .unwrap();
    assert!(
        crab_write::journal::compact_for_owner(store, layout, ttl, None, &cancel)
            .await
            .unwrap()
    );

    with_leases(
        store,
        layout,
        [name.to_owned()],
        ttl,
        &cancel,
        |holders, cancel| async move {
            let snapshot =
                crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
            crab_write::journal::commit_edits(
                store,
                layout,
                &snapshot,
                vec![RefJournalEdit {
                    ref_name: name.to_owned(),
                    old_oid: Some("d".repeat(40)),
                    new_oid: Some("e".repeat(40)),
                    peeled_oid: None,
                    lock_holder: holders.get(name).cloned(),
                    visibility_evidence_hash: None,
                }],
                None,
                vec![],
                vec![],
                crab_write::journal::CommitOptions::new(ttl, &cancel),
            )
            .await?;
            Ok::<_, TestError>(())
        },
    )
    .await
    .unwrap();
    assert!(
        crab_write::journal::compact_for_owner(store, layout, ttl, None, &cancel)
            .await
            .unwrap()
    );

    let garbage = layout.repo_path("packs/unreferenced.pack");
    store
        .put_exact(&garbage, bytes::Bytes::from_static(b"garbage"))
        .await
        .unwrap();
    store.delete(&garbage).await.unwrap();
    let receipt = plan_receipt::read_plan_receipt(store, layout, plan_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        receipt.commit,
        plan_receipt::PlanCommit::RefJournal { transaction_id, .. }
            if transaction_id == committed.transaction_id
    ));
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(store, layout)
        .await
        .unwrap();
    assert_eq!(snapshot.journal.refs.get(name), Some(&"e".repeat(40)));
    assert_released(store, layout).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_termination_recovers_committed_plan() {
    const ROOT: &str = "CRAB_TEST_PUBLICATION_PROCESS_ROOT";
    const NAME: &str = "refs/heads/main";
    type TestError = Box<dyn std::error::Error + Send + Sync>;

    if let Some(root) = std::env::var_os(ROOT) {
        let store = Store::new(Arc::new(FlatFileStore::new(std::path::Path::new(&root))));
        let layout = StoreLayout::with_global_prefix(
            store.clone(),
            "repositories/one".to_owned(),
            "shared".to_owned(),
        );
        let plan_id = "f".repeat(64);
        let ttl = Duration::from_secs(2);
        let cancel = CancellationToken::new();
        let (store, layout, plan_id) = (&store, &layout, plan_id.as_str());
        let result: Result<(), TestError> =
            with_plan(store, layout, plan_id, ttl, &cancel, |cancel| async move {
                let leases = crab_remote::publication::acquire_ref_leases(
                    store,
                    layout,
                    [NAME.to_owned()],
                    crab_remote::publication::LeaseOptions::renewing(ttl),
                    &cancel,
                )
                .await?;
                let snapshot =
                    crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
                crab_write::journal::commit_edits(
                    store,
                    layout,
                    &snapshot,
                    vec![crab_metadata::ref_journal::RefJournalEdit {
                        ref_name: NAME.to_owned(),
                        old_oid: None,
                        new_oid: Some("a".repeat(40)),
                        peeled_oid: None,
                        lock_holder: leases.holders().get(NAME).cloned(),
                        visibility_evidence_hash: None,
                    }],
                    None,
                    vec![],
                    vec![],
                    crab_write::journal::CommitOptions::new(ttl, &cancel).with_plan(plan_id),
                )
                .await?;
                std::process::exit(0);
            })
            .await;
        panic!("child publication returned instead of terminating: {result:?}");
    }

    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(Arc::new(FlatFileStore::new(directory.path())));
    let layout = StoreLayout::with_global_prefix(
        store.clone(),
        "repositories/one".to_owned(),
        "shared".to_owned(),
    );
    crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &layout)
        .await
        .unwrap();
    crab_metadata::manifest_store::create_manifest(
        &store,
        &layout,
        &crab_metadata::manifests::Manifest::default_for_repo(NAME),
    )
    .await
    .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "process_termination_recovers_committed_plan",
            "--nocapture",
        ])
        .env(ROOT, directory.path());
    let output = tokio::task::spawn_blocking(move || child.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let plan_id = "f".repeat(64);
    let receipt = crab_metadata::plan_receipt::read_plan_receipt(&store, &layout, &plan_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        receipt.commit,
        crab_metadata::plan_receipt::PlanCommit::RefJournal { .. }
    ));
    let lease = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match PushLock::acquire_ref(
                store.inner(),
                layout.repo_prefix(),
                NAME,
                Duration::from_secs(60),
            )
            .await
            {
                Ok(lease) => break lease,
                Err(CoordinationError::PushLockHeld { .. }) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("unexpected ref lease recovery failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    lease.release().await.unwrap();
    let operation = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        &format!("publication-plan-{plan_id}"),
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    operation.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_same_plan_commits_once() {
    use crab_metadata::{manifest_store, manifests::Manifest, plan_receipt, ref_journal};
    use crab_remote::publication::with_plan;
    type TestError = Box<dyn std::error::Error + Send + Sync>;

    let (store, layout) = fixture();
    crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &layout)
        .await
        .unwrap();
    let name = "refs/heads/main";
    manifest_store::create_manifest(&store, &layout, &Manifest::default_for_repo(name))
        .await
        .unwrap();
    let plan_id = "a".repeat(64);
    let cancel = CancellationToken::new();
    let ttl = Duration::from_secs(60);
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, proceed) = tokio::sync::oneshot::channel();
    let (store, layout, plan_id) = (&store, &layout, plan_id.as_str());
    let first = with_plan(store, layout, plan_id, ttl, &cancel, |cancel| async move {
        entered.send(()).unwrap();
        proceed.await.unwrap();
        with_leases(
            store,
            layout,
            [name.to_owned()],
            ttl,
            &cancel,
            |holders, cancel| async move {
                let snapshot = manifest_store::read_repository_snapshot(store, layout).await?;
                let committed = crab_write::journal::commit_edits(
                    store,
                    layout,
                    &snapshot,
                    vec![ref_journal::RefJournalEdit {
                        ref_name: name.to_owned(),
                        old_oid: None,
                        new_oid: Some("b".repeat(40)),
                        peeled_oid: None,
                        lock_holder: holders.get(name).cloned(),
                        visibility_evidence_hash: None,
                    }],
                    None,
                    vec![],
                    vec![],
                    crab_write::journal::CommitOptions::new(ttl, &cancel).with_plan(plan_id),
                )
                .await?;
                Ok::<_, TestError>(committed)
            },
        )
        .await
    });
    let mut competitor_entered = false;
    let second = async {
        ready.await.unwrap();
        let result = with_plan(store, layout, plan_id, ttl, &cancel, |_| async {
            competitor_entered = true;
            Ok::<_, TestError>(())
        })
        .await;
        release.send(()).unwrap();
        result
    };
    let (committed, competing) = tokio::join!(first, second);
    let committed = committed.unwrap();
    assert!(matches!(
        competing.unwrap_err().downcast_ref::<Error>(),
        Some(Error::Coordination(CoordinationError::PushLockHeld { .. }))
    ));
    assert!(!competitor_entered);

    let retry = with_plan(store, layout, plan_id, ttl, &cancel, |_| async {
        competitor_entered = true;
        Ok::<_, TestError>(())
    })
    .await;
    assert!(matches!(
        retry
            .unwrap_err()
            .downcast_ref::<crab_metadata::error::MetadataError>(),
        Some(crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. })
    ));
    assert!(!competitor_entered);
    let receipt = plan_receipt::read_plan_receipt(store, layout, plan_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(receipt.commit, plan_receipt::PlanCommit::RefJournal { transaction_id, .. }
        if transaction_id == committed.transaction_id)
    );
    let snapshot = manifest_store::read_repository_snapshot(store, layout)
        .await
        .unwrap();
    assert_eq!(snapshot.journal.refs.get(name), Some(&"b".repeat(40)));
    assert_eq!(snapshot.journal.transactions.len(), 1);
    assert_released(store, layout).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unresolved_plan_blocks_reexecution_and_releases_admission() {
    type TestError = Box<dyn std::error::Error + Send + Sync>;
    let (store, layout) = fixture();
    let plan_id = "a".repeat(64);
    let cancel = CancellationToken::new();
    let ttl = Duration::from_secs(60);
    let intent = format!(
        r#"{{"version":1,"repo_prefix":"repositories/one","plan_id":"{plan_id}","attempt":1,"commit":{{"kind":"ref_journal","transaction_id":"{}","dependency_digest":"{}"}}}}"#,
        "b".repeat(64),
        "c".repeat(64)
    );
    crab_remote::publication::with_plan(&store, &layout, &plan_id, ttl, &cancel, |_| async {
        store
            .put_exact(
                &layout.ref_journal_plan_intent_path(&plan_id, 1),
                intent.into(),
            )
            .await?;
        Ok::<_, TestError>(())
    })
    .await
    .unwrap();
    let mut executed = false;
    let result =
        crab_remote::publication::with_plan(&store, &layout, &plan_id, ttl, &cancel, |_| async {
            executed = true;
            Ok::<_, TestError>(())
        })
        .await;
    assert!(matches!(
        result.unwrap_err().downcast_ref::<crab_metadata::error::MetadataError>(),
        Some(crab_metadata::error::MetadataError::PlanAlreadyAttempted { plan_id: actual })
            if actual == &plan_id
    ));
    assert!(!executed);
    let successor = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        &format!("publication-plan-{plan_id}"),
        ttl,
    )
    .await
    .unwrap();
    successor.release().await.unwrap();
}

#[tokio::test]
async fn cancelled_plan_evidence_read_releases_operation_admission() {
    type TestError = Box<dyn std::error::Error + Send + Sync>;
    let origin = Arc::new(object_store::memory::InMemory::new());
    let delayed = Arc::new(object_store::throttle::ThrottledStore::new(
        origin.clone(),
        object_store::throttle::ThrottleConfig {
            wait_get_per_call: Duration::from_secs(60),
            ..Default::default()
        },
    ));
    let store = Store::new(delayed.clone());
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    let plan_id = "a".repeat(64);
    let resource = format!("publication-plan-{plan_id}");
    let lock_path = object_store::path::Path::from(
        crab_coordination::internal_lock_path(layout.repo_prefix(), &resource).unwrap(),
    );
    let cancel = CancellationToken::new();
    let (worker_store, worker_layout, worker_cancel) =
        (store.clone(), layout.clone(), cancel.clone());
    let worker = tokio::spawn(async move {
        crab_remote::publication::with_plan(
            &worker_store,
            &worker_layout,
            &plan_id,
            Duration::from_secs(60),
            &worker_cancel,
            |_| async {
                Err::<(), TestError>(
                    std::io::Error::other("publication entered after cancellation").into(),
                )
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while origin.head(&lock_path).await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The in-flight evidence read retains its original delay. Subsequent lease
    // cleanup reads can finish, so the test isolates cancellation of that read.
    delayed.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), worker).await;
    assert!(
        matches!(result, Ok(Ok(Err(error))) if matches!(error.downcast_ref::<Error>(), Some(Error::Cancelled)))
    );
    let successor = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        &resource,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    successor.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_ref_owner_stops_renewal_and_releases_lease() {
    let (store, layout) = fixture();
    let cancel = CancellationToken::new();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let worker_store = store.clone();
    let worker_layout = layout.clone();
    let worker_cancel = cancel.clone();
    let worker = tokio::spawn(async move {
        with_leases(
            &worker_store,
            &worker_layout,
            ["refs/heads/main".to_owned()],
            Duration::from_secs(60),
            &worker_cancel,
            |_, _| async move {
                entered.send(()).unwrap();
                std::future::pending::<Result<(), Error>>().await
            },
        )
        .await
    });
    ready.await.unwrap();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    let successor = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match PushLock::acquire_ref(
                store.inner(),
                layout.repo_prefix(),
                "refs/heads/main",
                Duration::from_secs(60),
            )
            .await
            {
                Ok(lease) => break lease,
                Err(CoordinationError::PushLockHeld { .. }) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected acquisition error: {error}"),
            }
        }
    })
    .await
    .unwrap();
    successor.release().await.unwrap();
    assert!(!cancel.is_cancelled());
}

async fn assert_released(store: &Store, layout: &StoreLayout<Store>) {
    let lease = PushLock::acquire_ref(
        store.inner(),
        layout.repo_prefix(),
        "refs/heads/main",
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    lease.release().await.unwrap();
    for domain in [layout.global_prefix(), layout.repo_prefix()] {
        let sweep = GcFenceLease::acquire_sweep(store.inner(), domain, Duration::from_secs(60))
            .await
            .unwrap();
        sweep.release().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn publication_excludes_competing_writers_and_gc_then_drains() {
    let (store, layout) = fixture();
    let result: Result<_, Error> = with_leases(
        &store,
        &layout,
        ["refs/heads/main".to_owned()],
        Duration::from_secs(60),
        &CancellationToken::new(),
        |holders, _| {
            let store = &store;
            let layout = &layout;
            async move {
                assert!(holders.contains_key("refs/heads/main"));
                assert!(matches!(
                    PushLock::acquire_ref(
                        store.inner(),
                        layout.repo_prefix(),
                        "refs/heads/main",
                        Duration::from_secs(60)
                    )
                    .await,
                    Err(CoordinationError::PushLockHeld { .. })
                ));
                for domain in [layout.global_prefix(), layout.repo_prefix()] {
                    assert!(matches!(
                        GcFenceLease::acquire_sweep(store.inner(), domain, Duration::from_secs(60))
                            .await,
                        Err(CoordinationError::GcFenceHeld { .. })
                    ));
                }
                Ok("committed")
            }
        },
    )
    .await;
    assert_eq!(result.unwrap(), "committed");
    assert_released(&store, &layout).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repository_fence_contention_releases_earlier_ref_and_global_admission() {
    let (store, layout) = fixture();
    let sweep =
        GcFenceLease::acquire_sweep(store.inner(), layout.repo_prefix(), Duration::from_secs(60))
            .await
            .unwrap();
    let result: Result<(), Error> = with_leases(
        &store,
        &layout,
        ["refs/heads/main".to_owned()],
        Duration::from_secs(60),
        &CancellationToken::new(),
        |_, _| async { panic!("contended publication must not start") },
    )
    .await;
    assert!(matches!(
        result,
        Err(Error::Coordination(CoordinationError::GcFenceHeld { .. }))
    ));
    sweep.release().await.unwrap();
    assert_released(&store, &layout).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn renewal_loss_drains_without_replacing_the_operation_outcome() {
    for committed in [false, true] {
        let (store, layout) = fixture();
        let parent = CancellationToken::new();
        let result: Result<_, Error> = with_leases(
            &store,
            &layout,
            ["refs/heads/main".to_owned()],
            Duration::from_secs(3),
            &parent,
            |_, cancel| {
                let store = &store;
                let layout = &layout;
                async move {
                    let path =
                        crab_coordination::push_lock_path(layout.repo_prefix(), "refs/heads/main")
                            .unwrap();
                    store
                        .inner()
                        .delete(&object_store::path::Path::from(path))
                        .await
                        .unwrap();
                    tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
                        .await
                        .unwrap();
                    if committed {
                        Ok("committed")
                    } else {
                        Err(Error::Cancelled)
                    }
                }
            },
        )
        .await;
        assert_eq!(result.is_ok(), committed);
        assert!(!parent.is_cancelled());
        assert_released(&store, &layout).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn internal_lease_loss_preserves_recorded_commit_and_drains() {
    let (store, layout) = fixture();
    let resource = crab_coordination::GIT_MANIFEST_RESOURCE;
    let result: Result<_, Error> = with_internal_lease(
        &store,
        &layout,
        resource,
        Duration::from_secs(3),
        &CancellationToken::new(),
        |cancel| {
            let store = &store;
            let layout = &layout;
            async move {
                let path =
                    crab_coordination::internal_lock_path(layout.repo_prefix(), resource).unwrap();
                store
                    .inner()
                    .delete(&object_store::path::Path::from(path))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
                    .await
                    .unwrap();
                Ok("committed")
            }
        },
    )
    .await;
    assert_eq!(result.unwrap(), "committed");
    let successor = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        resource,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    successor.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_retains_plan_ref_and_gc_admission_until_operation_drains() {
    let (store, layout) = fixture();
    let cancel = CancellationToken::new();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (draining, cancelled) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let worker_store = store.clone();
    let worker_layout = layout.clone();
    let worker_cancel = cancel.clone();
    let resource = "publication-plan-draining";
    let worker = tokio::spawn(async move {
        let store = &worker_store;
        let layout = &worker_layout;
        with_internal_lease(
            store,
            layout,
            resource,
            Duration::from_secs(60),
            &worker_cancel,
            |cancel| async move {
                with_leases(
                    store,
                    layout,
                    ["refs/heads/main".to_owned()],
                    Duration::from_secs(60),
                    &cancel,
                    |_, cancel| async move {
                        entered.send(()).unwrap();
                        cancel.cancelled().await;
                        draining.send(()).unwrap();
                        finished.await.unwrap();
                        Err::<(), _>(Error::Cancelled)
                    },
                )
                .await
            },
        )
        .await
    });
    ready.await.unwrap();
    cancel.cancel();
    cancelled.await.unwrap();
    assert!(matches!(
        PushLock::acquire_internal(
            store.inner(),
            layout.repo_prefix(),
            resource,
            Duration::from_secs(60)
        )
        .await,
        Err(CoordinationError::PushLockHeld { .. })
    ));
    assert!(matches!(
        PushLock::acquire_ref(
            store.inner(),
            layout.repo_prefix(),
            "refs/heads/main",
            Duration::from_secs(60)
        )
        .await,
        Err(CoordinationError::PushLockHeld { .. })
    ));
    for domain in [layout.global_prefix(), layout.repo_prefix()] {
        assert!(matches!(
            GcFenceLease::acquire_sweep(store.inner(), domain, Duration::from_secs(60)).await,
            Err(CoordinationError::GcFenceHeld { .. })
        ));
    }
    finish.send(()).unwrap();
    assert!(matches!(worker.await.unwrap(), Err(Error::Cancelled)));
    let successor = PushLock::acquire_internal(
        store.inner(),
        layout.repo_prefix(),
        resource,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    successor.release().await.unwrap();
    assert_released(&store, &layout).await;
}
