use super::*;
use bytes::Bytes;
use crab_lfs::LfsObjectStore;
use crab_storage::test_support::CountingObjectStore;
use object_store::{
    chunked::ChunkedStore,
    memory::InMemory,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

type SlowStore = ThrottledStore<ChunkedStore>;

async fn fixture() -> (
    SnapshotReader,
    Arc<CountingObjectStore>,
    Arc<CountingObjectStore>,
    Arc<SlowStore>,
    tempfile::TempDir,
) {
    let directory = tempfile::tempdir().unwrap();
    let git_dir = directory.path().join("git");
    std::fs::create_dir(&git_dir).unwrap();
    let git = |args: &[&str], input: &[u8]| {
        let mut child = Command::new("git")
            .args([
                "-c",
                "user.name=Read test",
                "-c",
                "user.email=read@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&git_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--bare", "--quiet"], b"");
    let content = Bytes::from(vec![0x5a; 512 * 1024]);
    use sha2::Digest as _;
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
        hex_encode(&oid),
        content.len()
    );
    let blob = git(&["hash-object", "-w", "--stdin"], pointer.as_bytes());
    let tree = git(
        &["mktree"],
        format!("100644 blob {blob}\tfile.lfs\n").as_bytes(),
    );
    let commit = git(&["commit-tree", &tree], b"LFS read fixture\n");
    let headers = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
    let slow = Arc::new(ThrottledStore::new(
        ChunkedStore::new(headers.clone(), 64 * 1024),
        ThrottleConfig::default(),
    ));
    let counted = Arc::new(CountingObjectStore::new(slow.clone()));
    let store = crate::storage::Store::new(counted.clone());
    let router = crate::storage::StoreLayout::new(store.clone(), "lfs-read-test".into());
    LfsObjectStore::new(store.clone().into(), router.repo_prefix())
        .put(&oid, content)
        .await
        .unwrap();
    counted.reset();
    headers.reset();
    let config = Config::default();
    let cache = Arc::new(LocalCache::new(directory.path().join("cache")));
    let caching_store =
        CachingStore::new_with_local_cache(store, &config.cache, cache.clone()).unwrap();
    let hydrator = build_cli_hydrator(caching_store.clone(), router.clone(), &config).unwrap();
    let repo = RepositoryReader {
        inner: Arc::new(Inner {
            url: "local-lfs-fixture".into(),
            config,
            cache,
            git_dir: Some(git_dir.clone()),
            work_dir: None,
            cancel: CancellationToken::new(),
            remote: OnceCell::new_with(Some(Arc::new(RemoteContext {
                caching_store,
                router,
                hydrator: Arc::new(hydrator),
                git_runtime: Arc::new(crab_remote_git::RemoteGitRuntime::default()),
                lfs_reads: LfsReadSession::default(),
            }))),
        }),
    };
    let snapshot = SnapshotReader {
        repo,
        requested_revision: "HEAD".into(),
        resolved_revision: commit,
        source: SnapshotSource::Local(git_dir),
        file_index_lookup: None,
    };
    (snapshot, counted, headers, slow, directory)
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_lfs_materialization_before_headers_and_during_body() {
    for to_file in [true, false] {
        for before_headers in [true, false] {
            let (snapshot, counted, headers, slow, directory) = fixture().await;
            slow.config_mut(|config| {
                if before_headers {
                    config.wait_get_per_call = Duration::from_secs(30);
                } else {
                    config.wait_get_per_byte = Duration::from_micros(100);
                }
            });
            let destination = directory.path().join("result");
            std::fs::write(&destination, b"original").unwrap();
            let read = async {
                if to_file {
                    snapshot.download_to_path("file.lfs", &destination).await
                } else {
                    snapshot.write_to_writer("file.lfs", std::io::sink()).await
                }
            };
            let cancel = async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    // The inner counter runs after the header throttle; InMemory
                    // metadata returns immediately before the delayed body is polled.
                    let entered = if before_headers { &counted } else { &headers };
                    while entered.counts().full == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                snapshot.repo.inner.cancel.cancel();
            };
            let (result, ()) =
                tokio::join!(tokio::time::timeout(Duration::from_secs(3), read), cancel);
            snapshot.repo.shutdown().await;
            assert!(
                matches!(result, Ok(Err(CrabError::Cancelled))),
                "to_file={to_file}, before_headers={before_headers}: {result:?}"
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lfs_path_streams_privately_before_verified_publication() {
    let (snapshot, counted, _, slow, directory) = fixture().await;
    slow.config_mut(|config| config.wait_get_per_byte = Duration::from_micros(1));
    let destination = directory.path().join("result");
    std::fs::write(&destination, b"original").unwrap();
    let progress = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let streaming = std::fs::read_dir(directory.path())
                    .unwrap()
                    .filter_map(|entry| entry.ok())
                    .any(|entry| {
                        let path = entry.path();
                        path != destination
                            && path.is_file()
                            && entry
                                .metadata()
                                .is_ok_and(|meta| meta.len() > 0 && meta.len() < 512 * 1024)
                    });
                if streaming {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
    };
    let (result, ()) = tokio::join!(
        snapshot.download_to_path("file.lfs", &destination),
        progress
    );
    snapshot.repo.shutdown().await;
    assert_eq!(result.unwrap(), 512 * 1024);
    assert_eq!(std::fs::read(&destination).unwrap(), vec![0x5a; 512 * 1024]);
    assert_eq!(counted.counts().full, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_lfs_preserves_existing_destination_and_integrity_diagnostic() {
    for to_file in [true, false] {
        let (snapshot, _, _, _, directory) = fixture().await;
        let remote = snapshot.repo.inner.remote.get().unwrap();
        let mut content = vec![0x5a; 512 * 1024];
        use sha2::Digest as _;
        let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
        *content.last_mut().unwrap() ^= 1;
        let lfs = LfsObjectStore::new(
            remote.caching_store.origin().clone(),
            remote.router.repo_prefix(),
        );
        remote
            .caching_store
            .origin()
            .put_overwrite(&lfs.object_path_for(&oid), Bytes::from(content))
            .await
            .unwrap();
        let destination = directory.path().join("result");
        std::fs::write(&destination, b"original").unwrap();
        let result = if to_file {
            snapshot.download_to_path("file.lfs", &destination).await
        } else {
            snapshot.write_to_writer("file.lfs", std::io::sink()).await
        };
        snapshot.repo.shutdown().await;
        assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_file())
                .count(),
            1
        );
    }
}
