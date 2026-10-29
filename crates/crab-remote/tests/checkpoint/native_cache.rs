use super::*;
use crab_cache::LocalCache;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_metadata::capsule_protocol::{PackMemberDescriptor, PackSourceKind};
use crab_read::capsule_protocol::install_git_packs_from_store;

const BLOBS: &[(&str, &[u8])] = &[("first", b"first"), ("other", b"other")];

async fn verify_cached_clone(
    layout: &StoreLayout<Store>,
    view: &CapsuleRepositoryView,
    cache: &CachingStore,
) {
    let directory = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();
    let installed = {
        let view = view.clone();
        let layout = layout.clone();
        let cache = cache.clone();
        let destination = directory.path().to_owned();
        // Real product callers spawn installation; cache routing must remain Send.
        tokio::spawn(async move {
            install_git_packs_from_store(
                &view,
                &layout,
                &destination,
                LIMIT,
                Some(&cache),
                &CancellationToken::new(),
            )
            .await
        })
        .await
        .unwrap()
        .unwrap()
    };
    assert!(installed.complete_visibility);
    verify_installed_git_blobs(view, directory.path(), &installed, BLOBS);
}

fn sidecar_bytes(member: &PackMemberDescriptor) -> u64 {
    let sections = [member.index(), member.reverse_index(), member.locator()];
    let start = sections.iter().map(|range| range.offset()).min().unwrap();
    let end = sections
        .iter()
        .map(|range| range.offset() + range.length())
        .max()
        .unwrap();
    end - start
}

fn bytes_read(observations: &Observations) -> u64 {
    observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|read| read.bytes_read)
        .sum()
}

#[tokio::test]
async fn native_pack_cache_reuses_bytes_and_repairs_corruption() {
    for repack in [false, true] {
        let (layout, observations) = fixture().await;
        let checkpointed = checkpoint(&layout).await;
        if repack {
            assert!(
                crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
                    &layout,
                    checkpointed.root_snapshot().clone(),
                    LIMIT,
                    &CancellationToken::new(),
                )
                .await
                .unwrap()
                .published
            );
        }
        let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
            &layout,
            crab_write::capsule_protocol::open_root(&layout)
                .await
                .unwrap(),
            LIMITS,
        )
        .await
        .unwrap();
        let sources = view.layered_checkpoint().unwrap().sources();
        let expected_kind = if repack {
            PackSourceKind::PackLayer
        } else {
            PackSourceKind::CapsuleRun
        };
        assert!(sources.iter().all(|source| source.kind() == expected_kind));
        let members = sources
            .iter()
            .flat_map(|source| source.members())
            .collect::<Vec<_>>();
        let pack_bytes = members
            .iter()
            .map(|member| member.pack().length())
            .sum::<u64>();
        let sidecars = members
            .iter()
            .map(|member| sidecar_bytes(member))
            .sum::<u64>();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let cache = CachingStore::new_with_local_cache(
            layout.store().clone(),
            CacheConfig::default(),
            Arc::new(LocalCache::with_limits(root.clone(), Some(LIMIT), None)),
        )
        .unwrap();
        for (name, expected) in [
            ("cold", sidecars + pack_bytes),
            ("warm", sidecars),
            ("repaired", sidecars + pack_bytes),
            ("rewarmed", sidecars),
        ] {
            if name == "repaired" {
                for member in &members {
                    let hash = member.pack().blake3();
                    std::fs::write(
                        root.join("git-packs").join(&hash[..2]).join(hash),
                        vec![b'X'; member.pack().length() as usize],
                    )
                    .unwrap();
                }
            }
            observations.0.lock().unwrap().clear();
            verify_cached_clone(&layout, &view, &cache).await;
            assert_eq!(
                bytes_read(&observations),
                expected,
                "repack={repack}, {name}"
            );
        }
        // A cached source still counts against the aggregate reader intake budget.
        observations.0.lock().unwrap().clear();
        let rejected = directory.path().join("over-budget");
        let result = install_git_packs_from_store(
            &view,
            &layout,
            &rejected,
            pack_bytes,
            Some(&cache),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(crab_read::ReadError::CapsuleReadLimit { .. })
        ));
        assert_eq!(bytes_read(&observations), 0);
        assert!(!rejected.join("objects/pack").exists());
    }
}

#[tokio::test]
async fn warm_native_pack_cannot_hide_corrupt_sidecars() {
    let (layout, observations) = fixture().await;
    let view = checkpoint(&layout).await;
    let directory = tempfile::tempdir().unwrap();
    let cache = CachingStore::new_with_local_cache(
        layout.store().clone(),
        CacheConfig::default(),
        Arc::new(LocalCache::new(directory.path().join("cache"))),
    )
    .unwrap();
    verify_cached_clone(&layout, &view, &cache).await;
    let sources = view.layered_checkpoint().unwrap().sources();
    let last = sources.last().unwrap();
    assert_eq!(last.kind(), PackSourceKind::CapsuleRun);
    let path = layout.capsule_path(last.object_hash());
    let (body, etag) = layout.store().get_with_etag(&path).await.unwrap();
    let mut corrupt = body.to_vec();
    corrupt[last.members()[0].index().offset() as usize] ^= 1;
    layout
        .store()
        .update(&path, Bytes::from(corrupt), etag)
        .await
        .unwrap();
    let rejected = directory.path().join("rejected");
    observations.0.lock().unwrap().clear();
    let result = install_git_packs_from_store(
        &view,
        &layout,
        &rejected,
        LIMIT,
        Some(&cache),
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(crab_read::ReadError::CorruptObject { .. })
    ));
    let sidecars = sources
        .iter()
        .flat_map(|source| source.members())
        .map(sidecar_bytes)
        .sum::<u64>();
    assert_eq!(bytes_read(&observations), sidecars);
    assert!(
        std::fs::read_dir(rejected.join("objects/pack"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[derive(Default)]
struct PendingRead {
    entered: tokio::sync::Notify,
    unrelated_token: CancellationToken,
}

#[async_trait::async_trait]
impl crab_storage::ReadAdmission for PendingRead {
    fn cancellation(&self) -> &CancellationToken {
        &self.unrelated_token
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.entered.notify_one();
        std::future::pending().await
    }

    async fn bytes(&self, _: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

#[tokio::test]
async fn cancelled_native_install_removes_private_files_without_publishing_git_packs() {
    for repack in [false, true] {
        let (layout, _) = fixture().await;
        let checkpointed = checkpoint(&layout).await;
        if repack {
            crab_remote::checkpoint::repack_capsule_checkpoint_from_root(
                &layout,
                checkpointed.root_snapshot().clone(),
                LIMIT,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        }
        let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
            &layout,
            crab_write::capsule_protocol::open_root(&layout)
                .await
                .unwrap(),
            LIMITS,
        )
        .await
        .unwrap();
        for warm in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let cache = CachingStore::new_with_local_cache(
                layout.store().clone(),
                CacheConfig::default(),
                Arc::new(LocalCache::new(directory.path().join("cache"))),
            )
            .unwrap();
            if warm {
                verify_cached_clone(&layout, &view, &cache).await;
            }
            let gate = Arc::new(PendingRead::default());
            let origin = layout.store().clone().with_read_admission(gate.clone());
            let selected_layout = StoreLayout::with_global_prefix(
                origin,
                layout.repo_prefix().to_owned(),
                layout.global_prefix().to_owned(),
            );
            let git_dir = directory.path().join("cancelled.git");
            crab_git::initialize_bare_git_dir(&git_dir).unwrap();
            let cancel = CancellationToken::new();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(
                    install_git_packs_from_store(
                        &view,
                        &selected_layout,
                        &git_dir,
                        LIMIT,
                        Some(&cache),
                        &cancel,
                    ),
                    async {
                        gate.entered.notified().await;
                        cancel.cancel();
                    },
                )
                .0
            })
            .await;
            assert!(
                matches!(result, Ok(Err(crab_read::ReadError::Cancelled))),
                "repack={repack}, warm={warm}: {result:?}"
            );
            assert!(
                std::fs::read_dir(git_dir.join("objects/pack"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            // Retry with an independent database and uncancelled selected origin.
            verify_cached_clone(&layout, &view, &cache).await;
        }
    }
}

#[derive(Debug)]
struct InterruptedRead(Arc<dyn object_store::ObjectStore>);

impl std::fmt::Display for InterruptedRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InterruptedRead")
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for InterruptedRead {
    async fn put_opts(
        &self,
        path: &object_store::path::Path,
        body: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.0.put_opts(path, body, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &object_store::path::Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.0.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if options.head {
            return self.0.get_opts(path, options).await;
        }
        let response = self.0.get_opts(path, options).await?;
        let meta = response.meta.clone();
        let range = response.range.clone();
        let attributes = response.attributes.clone();
        let extensions = response.extensions.clone();
        let bytes = response.bytes().await?;
        let first = bytes.slice(..bytes.len().min(1));
        let failure = object_store::Error::Generic {
            store: "interrupted fixture",
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "fixture body interrupted",
            )),
        };
        Ok(object_store::GetResult {
            payload: object_store::GetResultPayload::Stream(Box::pin(futures_util::stream::iter(
                [Ok(first), Err(failure)],
            ))),
            meta,
            range,
            attributes,
            extensions,
        })
    }

    fn delete_stream(
        &self,
        paths: futures_util::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
    {
        self.0.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn interrupted_checkpoint_body_preserves_its_cause_and_drains_before_retry() {
    let (layout, _) = fixture().await;
    let captured = checkpoint(&layout).await;
    let checkpoint = captured.layered_checkpoint().unwrap();
    let broken = StoreLayout::new(
        Store::new(Arc::new(InterruptedRead(layout.store().inner().clone()))),
        layout.repo_prefix().to_owned(),
    );
    let directory = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();

    let error = crab_read::capsule_protocol::install_layered_checkpoint(
        checkpoint,
        &broken,
        directory.path(),
        LIMIT,
    )
    .await
    .unwrap_err();

    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut connection_reset = false;
    while let Some(source) = cause {
        connection_reset |= source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset);
        cause = source.source();
    }
    assert!(connection_reset, "{error:?}");
    assert_eq!(
        std::fs::read_dir(directory.path().join("objects/pack"))
            .unwrap()
            .count(),
        0
    );
    crab_read::capsule_protocol::install_layered_checkpoint(
        checkpoint,
        &layout,
        directory.path(),
        LIMIT,
    )
    .await
    .unwrap();
    for (name, expected) in BLOBS {
        let oid = crab_remote::objects::object_id(gix_object::Kind::Blob, expected).unwrap();
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(directory.path())
            .args(["cat-file", "blob", &oid.to_string()])
            .output()
            .unwrap();
        assert!(output.status.success(), "{name}");
        assert_eq!(output.stdout, *expected);
    }
}
