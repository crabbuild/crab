use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use crab_storage::test_support::CountingObjectStore;
use crab_types::pointer::Pointer;
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use object_store::memory::InMemory;

use crate::cmd::hydrate::HydrationRuntime;
use crate::core::config::Config;
use crate::core::error::Result;

pub(crate) async fn stored_file(
    cache_root: &Path,
    content: Bytes,
    corrupt_origin: bool,
) -> Result<(HydrationRuntime, Pointer, Arc<CountingObjectStore>)> {
    let file_hash = *blake3::hash(&content).as_bytes();
    let size = content.len();
    let chunk = crab_xet::xorb::format::Chunk::new(content);
    let mut builder = XorbBuilder::new();
    builder.push(&chunk, RunId(0))?;
    let xorb = builder.finalize()?.remove(0);
    let mut shard = ShardWriter::new();
    shard.add_xorb(Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, size),
        chunks: vec![XorbChunkSequenceEntry::new(chunk.hash, size as u32, 0)],
    }))?;
    shard.add_file(MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash.into(), 1, false, false),
        segments: vec![FileDataSequenceEntry::new(xorb.hash, size as u32, 0, 1)],
        verification: vec![],
        metadata_ext: None,
    })?;
    let (shard_bytes, shard_hash) = shard.finalize()?;
    let counted = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
    let store = crate::storage::Store::new(counted.clone());
    let layout = crab_storage::StoreLayout::new(store.clone(), "read-test".into());
    store
        .put(&layout.shard_path(&shard_hash), shard_bytes.into())
        .await?;
    let mut xorb_bytes = xorb.bytes.to_vec();
    if corrupt_origin {
        *xorb_bytes.last_mut().unwrap() ^= 0xff;
    }
    store
        .put(&layout.xorb_path(&xorb.hash), xorb_bytes.into())
        .await?;
    let local = Arc::new(crate::cache::LocalCache::new(cache_root.to_owned()));
    let config = Config::default();
    let caching =
        crab_cache_store::CachingStore::new_with_local_cache(store, &config.cache, local)?;
    let hydrator = super::build_cli_hydrator(caching, layout, &config)?;
    let pointer = Pointer {
        file_hash,
        size: size as u64,
        shard_hint: Some(shard_hash.into()),
    };
    Ok((hydrator, pointer, counted))
}

use super::{
    Inner, LfsReadSession, RemoteContext, RepositoryReader, SnapshotReader, SnapshotSource,
    build_cli_hydrator, hex_encode,
};
use crate::cache::LocalCache;
use crab_cache_store::CachingStore;
use crab_lfs::LfsObjectStore;
use object_store::{
    chunked::ChunkedStore,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::io::Write as _;
use std::process::{Command, Stdio};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

pub(crate) type SlowStore = ThrottledStore<ChunkedStore>;

pub(crate) async fn lfs_snapshot(
    declared_size: u64,
) -> (
    RepositoryReader,
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
        declared_size
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
        repo: repo.clone(),
        requested_revision: "HEAD".into(),
        resolved_revision: commit,
        source: SnapshotSource::Local(git_dir),
        file_index_lookup: None,
    };
    (repo, snapshot, counted, headers, slow, directory)
}
