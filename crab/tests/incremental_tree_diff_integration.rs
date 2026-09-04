//! Integration tests for the incremental tree-diff pointer discovery
//! within the push pipeline.
//!
//! Exercises two scenarios against a real on-disk git repository plus
//! an `InMemory` object store:
//!
//! 1. End-to-end: create a repo with 100 pointer files, push, modify 3
//!    files, push again. Assert the second push discovers exactly 3
//!    pointers via the incremental walk.
//! 2. Verify that `PrePopulatedWalk` correctly receives the tree-diff-
//!    based pointer set by inspecting tracing output from
//!    `install_prepopulated_walk` and `enumerate_pointers`.
//!
//! Follows the same tracing-capture pattern as `prepopulated_walk.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio_util::sync::CancellationToken;
use tracing::dispatcher::Dispatch;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;

use crab::cmd::dehydrate::{DehydrateArgs, run_dehydrate_in};
use crab::cmd::hydrate::{HydrateArgs, HydrationRuntime, run_hydrate_in};
use crab::core::config::{CacheConfig, Config};
use crab::core::context::AppContext;
use crab::core::error::CrabError;
use crab::core::metrics::Metrics;
use crab::core::output::OutputMode;
use crab::git::clean::{CleanSession, StagingChunkStager};
use crab::git::push::{PushConfig, PushRejectReason, RefPushOutcome};
use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
use crab::git::push_state::PushState;
use crab::git::remote_helper::PushSpec;
use crab::storage::StoreLayout;
use crab::storage::store::Store;
use crab_coordination::PushLock;
use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_staging::{StagingArea, StagingAreaReadOnly};
use crab_types::pointer::Pointer;
use crab_xet::chunker::GearChunker;
use crab_xet::xorb::format::MerkleHash;

// ---------------------------------------------------------------------------
// Tracing capture helpers (mirrors prepopulated_walk.rs)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BufferMaker {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl BufferMaker {
    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let inner = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: Arc::clone(&inner),
            },
            inner,
        )
    }
}

struct BufferWriter {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufferMaker {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter {
            inner: Arc::clone(&self.inner),
        }
    }
}

fn captured_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&guard).into_owned()
}

fn capture_dispatch(buf: BufferMaker) -> Dispatch {
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_writer(buf)
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_target(true)
        .with_span_events(FmtSpan::CLOSE)
        .finish();
    Dispatch::new(subscriber)
}

// ---------------------------------------------------------------------------
// Recording object store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RecordingStore {
    inner: Arc<InMemory>,
    puts: Arc<Mutex<Vec<String>>>,
}

impl RecordingStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(InMemory::new()),
            puts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn puts(&self) -> Vec<String> {
        self.puts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self, path: &ObjectPath) {
        self.puts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.as_ref().to_owned());
    }
}

impl std::fmt::Debug for RecordingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingStore").finish()
    }
}

impl std::fmt::Display for RecordingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RecordingStore")
    }
}

#[async_trait]
impl ObjectStore for RecordingStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let result = self.inner.put_opts(location, payload, opts).await?;
        self.record(location);
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.record(location);
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

// ---------------------------------------------------------------------------
// Git repository fixture
// ---------------------------------------------------------------------------

static GIT_DIR_MUTEX: Mutex<()> = Mutex::new(());

struct ScopedGitDir {
    _lock: MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl ScopedGitDir {
    fn new(git_dir: &Path) -> Self {
        let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GIT_DIR").ok();
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe { std::env::set_var("GIT_DIR", git_dir) };
        Self { _lock: lock, prev }
    }
}

impl Drop for ScopedGitDir {
    fn drop(&mut self) {
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
            None => unsafe { std::env::remove_var("GIT_DIR") },
        }
    }
}

struct ScopedGitWorktreeEnv {
    _lock: MutexGuard<'static, ()>,
    prev_git_dir: Option<String>,
    prev_git_work_tree: Option<String>,
    prev_git_common_dir: Option<String>,
    prev_cwd: Option<PathBuf>,
}

impl ScopedGitWorktreeEnv {
    fn new(cwd: &Path, git_dir: &Path, work_tree: &Path) -> Self {
        let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_git_dir = std::env::var("GIT_DIR").ok();
        let prev_git_work_tree = std::env::var("GIT_WORK_TREE").ok();
        let prev_git_common_dir = std::env::var("GIT_COMMON_DIR").ok();
        let prev_cwd = std::env::current_dir().ok();
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe {
            std::env::set_var("GIT_DIR", git_dir);
            std::env::set_var("GIT_WORK_TREE", work_tree);
            std::env::remove_var("GIT_COMMON_DIR");
        }
        std::env::set_current_dir(cwd).expect("set git cwd");
        Self {
            _lock: lock,
            prev_git_dir,
            prev_git_work_tree,
            prev_git_common_dir,
            prev_cwd,
        }
    }
}

impl Drop for ScopedGitWorktreeEnv {
    fn drop(&mut self) {
        if let Some(cwd) = &self.prev_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe {
            match &self.prev_git_dir {
                Some(value) => std::env::set_var("GIT_DIR", value),
                None => std::env::remove_var("GIT_DIR"),
            }
            match &self.prev_git_work_tree {
                Some(value) => std::env::set_var("GIT_WORK_TREE", value),
                None => std::env::remove_var("GIT_WORK_TREE"),
            }
            match &self.prev_git_common_dir {
                Some(value) => std::env::set_var("GIT_COMMON_DIR", value),
                None => std::env::remove_var("GIT_COMMON_DIR"),
            }
        }
    }
}

struct ScopedCleanGitEnv {
    _lock: MutexGuard<'static, ()>,
    prev_git_dir: Option<String>,
    prev_git_work_tree: Option<String>,
    prev_git_common_dir: Option<String>,
}

impl ScopedCleanGitEnv {
    fn new() -> Self {
        let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev_git_dir = std::env::var("GIT_DIR").ok();
        let prev_git_work_tree = std::env::var("GIT_WORK_TREE").ok();
        let prev_git_common_dir = std::env::var("GIT_COMMON_DIR").ok();
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe {
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
            std::env::remove_var("GIT_COMMON_DIR");
        }
        Self {
            _lock: lock,
            prev_git_dir,
            prev_git_work_tree,
            prev_git_common_dir,
        }
    }
}

impl Drop for ScopedCleanGitEnv {
    fn drop(&mut self) {
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe {
            match &self.prev_git_dir {
                Some(value) => std::env::set_var("GIT_DIR", value),
                None => std::env::remove_var("GIT_DIR"),
            }
            match &self.prev_git_work_tree {
                Some(value) => std::env::set_var("GIT_WORK_TREE", value),
                None => std::env::remove_var("GIT_WORK_TREE"),
            }
            match &self.prev_git_common_dir {
                Some(value) => std::env::set_var("GIT_COMMON_DIR", value),
                None => std::env::remove_var("GIT_COMMON_DIR"),
            }
        }
    }
}

struct GitFixture {
    dir: tempfile::TempDir,
}

impl GitFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir for git fixture");
        Self::run_git(dir.path(), &["init", "--initial-branch=main"]);
        Self::run_git(dir.path(), &["config", "user.email", "test@test.com"]);
        Self::run_git(dir.path(), &["config", "user.name", "Test"]);
        Self::run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
        Self { dir }
    }

    fn work_tree(&self) -> &Path {
        self.dir.path()
    }

    fn git_dir(&self) -> std::path::PathBuf {
        self.dir.path().join(".git")
    }

    fn add_linked_worktree(&self, branch: &str) -> PathBuf {
        let linked = self.dir.path().join(format!("{branch}-worktree"));
        Self::run_git(
            self.work_tree(),
            &[
                "worktree",
                "add",
                "-b",
                branch,
                linked.to_str().expect("linked path utf8"),
                "HEAD",
            ],
        );
        linked
    }

    fn linked_git_dir(linked: &Path) -> PathBuf {
        let git_file = std::fs::read_to_string(linked.join(".git")).expect("linked .git file");
        let gitdir = git_file
            .trim()
            .strip_prefix("gitdir: ")
            .expect("gitdir prefix");
        let path = PathBuf::from(gitdir);
        let path = if path.is_absolute() {
            path
        } else {
            linked.join(path)
        };
        path.canonicalize().expect("linked git dir")
    }

    fn head_sha(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(self.work_tree())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .expect("git rev-parse HEAD");
        assert!(
            out.status.success(),
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("rev-parse output is utf8")
            .trim()
            .to_owned()
    }

    /// Write a crab pointer blob with a deterministic file-hash derived
    /// from `seed`.
    fn write_pointer(&self, name: &str, seed: u8) {
        let mut file_hash = [0u8; 32];
        for (i, byte) in file_hash.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        let pointer = Pointer {
            file_hash,
            size: 1024 + u64::from(seed),
            shard_hint: None,
        };
        std::fs::write(self.work_tree().join(name), pointer.serialize())
            .expect("write pointer blob");
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn run_git_output(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("git stdout utf8")
    }
}

// ---------------------------------------------------------------------------
// Pipeline plumbing helpers
// ---------------------------------------------------------------------------

fn make_store() -> Store {
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    Store::new(inner)
}

fn make_router(store: Store, prefix: &str) -> StoreLayout {
    StoreLayout::new(store, prefix.to_owned())
}

async fn initialize_remote(store: &Store, router: &StoreLayout) {
    crab::core::remote_layout::initialize(store, router)
        .await
        .expect("initialize canonical remote layout");
    match crab::cmd::init::create_initial_manifest(store, router, "refs/heads/main").await {
        Ok(()) => {}
        Err(CrabError::CasConflict { .. }) => {
            crab::metadata::manifest::read_manifest(store, router)
                .await
                .expect("read concurrently initialized manifest");
        }
        Err(error) => panic!("initialize canonical remote manifest: {error}"),
    }
}

fn make_specs() -> Vec<PushSpec> {
    vec![PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    }]
}

fn make_specs_for_ref(ref_name: &str) -> Vec<PushSpec> {
    vec![PushSpec {
        force: false,
        src: ref_name.to_owned(),
        dst: ref_name.to_owned(),
    }]
}

fn deterministic_content(seed: u64, size: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..size)
        .map(|_| {
            state ^= state << 7;
            state ^= state >> 9;
            state ^= state << 8;
            state as u8
        })
        .collect()
}

fn chunk_and_hash(content: &[u8]) -> (Vec<(MerkleHash, Bytes)>, [u8; 32]) {
    let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();
    let mut chunker = GearChunker::new();
    let mut chunks = Vec::new();
    for block in content.chunks(128 * 1024) {
        for chunk in chunker.feed(block) {
            chunks.push((chunk.hash, chunk.data));
        }
    }
    if let Some(last) = chunker.finalize() {
        chunks.push((last.hash, last.data));
    }
    (chunks, file_hash)
}

async fn stage_content(staging_root: PathBuf, path: &Path, content: &[u8]) -> ([u8; 32], u64) {
    let (chunks, file_hash) = chunk_and_hash(content);
    assert!(!chunks.is_empty(), "test content must produce chunks");
    let file_merkle = MerkleHash::from(file_hash);
    let staging = StagingArea::open(staging_root).await.expect("open staging");
    staging
        .pre_register_file(&file_merkle, content.len() as u64)
        .expect("pre-register staged file");
    let refs: Vec<(&MerkleHash, &[u8])> = chunks
        .iter()
        .map(|(hash, bytes)| (hash, bytes.as_ref()))
        .collect();
    staging
        .stage_chunks_batch(&refs, &file_merkle, 0)
        .await
        .expect("stage chunks");
    staging.flush_pending().await.expect("flush staging");
    let recipe_chunks = chunks
        .iter()
        .map(|(hash, bytes)| (*hash, bytes.len() as u64))
        .collect::<Vec<_>>();
    let recipe = FileRecipe::from_staged_chunks(
        ChunkingPolicyId::XetGearV1_64KiB,
        file_merkle,
        content.len() as u64,
        &recipe_chunks,
    )
    .expect("build staged recipe");
    staging
        .publish_verified_recipe_lease(path, &recipe)
        .expect("publish staged recipe");
    staging.close().await.expect("close staging");
    (file_hash, content.len() as u64)
}

async fn write_staged_pointer(root: &Path, name: &str, content: &[u8]) {
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(root).expect("worktree ctx");
    let staging_dir = ctx.shared_staging_dir();
    std::fs::create_dir_all(&staging_dir).expect("create shared staging dir");
    let (file_hash, size) = stage_content(staging_dir, Path::new(name), content).await;
    let pointer = Pointer {
        file_hash,
        size,
        shard_hint: None,
    };
    std::fs::write(root.join(name), pointer.serialize()).expect("write staged pointer");
}

async fn commit_linked_pointer(linked: &Path, name: &str, content: &[u8]) -> PathBuf {
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(linked).expect("linked ctx");
    let (file_hash, size) = stage_content(ctx.shared_staging_dir(), Path::new(name), content).await;
    let pointer = Pointer {
        file_hash,
        size,
        shard_hint: None,
    };
    std::fs::write(linked.join(name), pointer.serialize()).expect("write linked pointer");
    GitFixture::run_git(linked, &["add", name]);
    GitFixture::run_git(linked, &["commit", "-m", "linked pointer"]);
    ctx.shared_staging_dir()
}

async fn clean_content_to_pointer(root: &Path, rel_path: &str, content: Vec<u8>) -> Vec<u8> {
    let ctx = crab::git::worktree::WorktreeContext::resolve_from(root).expect("worktree ctx");
    let staging_dir = ctx.shared_staging_dir();
    std::fs::create_dir_all(&staging_dir).expect("create shared staging dir");
    let staging = Arc::new(
        StagingArea::open(staging_dir)
            .await
            .expect("open shared staging"),
    );
    let mut session = CleanSession::new(AppContext::default());
    session.set_repo_root(root.to_path_buf());
    session.set_chunk_stager(Box::new(StagingChunkStager::new(
        Arc::clone(&staging),
        tokio::runtime::Handle::current(),
    )));
    let rel_path = rel_path.to_owned();
    let pointer_bytes = tokio::task::spawn_blocking(move || {
        session
            .clean_file(&rel_path, content)
            .expect("clean content to pointer")
    })
    .await
    .expect("clean task joins");
    match Arc::try_unwrap(staging) {
        Ok(staging) => staging.close().await.expect("close shared staging"),
        Err(_) => panic!("shared staging still referenced after clean"),
    }
    pointer_bytes
}

fn hydrate_args(patterns: &[&str]) -> HydrateArgs {
    HydrateArgs {
        patterns: patterns
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
        include: Vec::new(),
        exclude: Vec::new(),
        all: false,
        mode: OutputMode::Json,
        manifest: None,
        manifest_ref: None,
        profile: None,
        ignore_sparse: false,
        recover_from: None,
    }
}

fn dehydrate_args(patterns: &[&str]) -> DehydrateArgs {
    DehydrateArgs {
        patterns: patterns
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
        all: false,
        ignore_profiles: true,
        mode: OutputMode::Json,
    }
}

fn shard_hydrator(store: Store, prefix: &str, cache_root: &Path) -> HydrationRuntime {
    let cache = Arc::new(crab::cache::LocalCache::new(cache_root.to_path_buf()));
    let caching_store = crab_cache_store::CachingStore::new_with_local_cache(
        store.clone(),
        &CacheConfig::default(),
        cache,
    )
    .expect("build caching store");
    crab::read::build_cli_hydrator(
        caching_store,
        StoreLayout::new(store, prefix.to_owned()),
        &crab::core::config::Config::default(),
    )
    .expect("build shard hydrator")
}

async fn push_ref_from_git_dir(
    git_dir: &Path,
    staging_root: &Path,
    store: Store,
    prefix: &str,
    ref_name: &str,
    push_state: &mut PushState,
) {
    let staging = StagingAreaReadOnly::open(staging_root.to_path_buf())
        .await
        .expect("open shared staging ro");
    let mut config = NativePushConfig::new(PushConfig {
        git_dir: Some(git_dir.to_path_buf()),
        ..PushConfig::default()
    });
    config.progress = false;
    config.color = false;
    config.incremental = false;
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    initialize_remote(&store, &router).await;
    let result = run_native_push(
        &config,
        &make_specs_for_ref(ref_name),
        NativePushInputs::new(
            Some(store),
            None,
            crab::git::push_staging::PushStaging::Ready(Arc::new(staging)),
            router,
            push_state,
            "origin",
            "crab://bucket/repo",
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("native push");
    assert_eq!(result.outcomes.get(ref_name), Some(&RefPushOutcome::Ok));
}

fn assert_pointer_file(path: &Path) -> Pointer {
    let bytes = std::fs::read(path).expect("read pointer file");
    Pointer::parse(&bytes).expect("file must be a crab pointer")
}

fn store_from_recording(recording: &RecordingStore) -> Store {
    Store::new(Arc::new(recording.clone()) as Arc<dyn ObjectStore>)
}

/// Extract the integer value of `field=N` from the first line of `trace`
/// that contains `anchor`.
fn extract_field_after(trace: &str, anchor: &str, field: &str) -> Option<u64> {
    let needle = format!("{field}=");
    for line in trace.lines() {
        if !line.contains(anchor) {
            continue;
        }
        if let Some(start) = line.find(&needle) {
            let after = &line[start + needle.len()..];
            let end = after
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after.len());
            if end == 0 {
                continue;
            }
            return after[..end].parse().ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linked_worktree_push_uploads_staged_xorbs_before_pack() {
    let fixture = GitFixture::new();
    std::fs::write(fixture.work_tree().join("README.md"), b"init\n").expect("readme");
    GitFixture::run_git(fixture.work_tree(), &["add", "README.md"]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "init"]);
    let linked = fixture.add_linked_worktree("linked");
    let linked_git_dir = GitFixture::linked_git_dir(&linked);
    let content = deterministic_content(7, 96 * 1024);
    let staging_root = commit_linked_pointer(&linked, "linked.bin", &content).await;
    let staging = StagingAreaReadOnly::open(staging_root)
        .await
        .expect("open shared staging ro");

    let _env = ScopedGitWorktreeEnv::new(&linked, &linked_git_dir, &linked);
    let recording = RecordingStore::new();
    let inner: Arc<dyn ObjectStore> = Arc::new(recording.clone());
    let store = Store::new(inner);
    let router = StoreLayout::new(store.clone(), "linked-push".to_owned());
    initialize_remote(&store, &router).await;
    let mut config = NativePushConfig::new(PushConfig::default());
    config.progress = false;
    config.color = false;
    config.incremental = false;
    let mut push_state = PushState::default();

    let result = run_native_push(
        &config,
        &make_specs_for_ref("refs/heads/linked"),
        NativePushInputs::new(
            Some(store),
            None,
            crab::git::push_staging::PushStaging::Ready(Arc::new(staging)),
            router,
            &mut push_state,
            "origin",
            "crab://bucket/repo",
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("linked native push");

    assert_eq!(
        result.outcomes.get("refs/heads/linked"),
        Some(&RefPushOutcome::Ok)
    );
    let puts = recording.puts();
    let first_xorb = puts
        .iter()
        .position(|path| path.contains("/xorbs/") || path.contains("xorbs/"))
        .expect("push must upload staged xorb before refs move");
    let first_pack = puts
        .iter()
        .position(|path| path.contains("/packs/pack-") || path.contains("packs/pack-"))
        .expect("push must upload git pack");
    assert!(
        first_xorb < first_pack,
        "staged xorb must upload before pack; puts={puts:?}"
    );
    let linked_tip = GitFixture::run_git_output(&linked, &["rev-parse", "refs/heads/linked"]);
    assert_eq!(
        push_state.last_pushed("crab://bucket/repo", "refs/heads/linked"),
        Some(linked_tip.trim())
    );
}

#[tokio::test]
async fn linked_worktree_push_lock_contention_is_per_destination_ref() {
    let fixture = GitFixture::new();
    std::fs::write(fixture.work_tree().join("README.md"), b"init\n").expect("readme");
    GitFixture::run_git(fixture.work_tree(), &["add", "README.md"]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "init"]);
    let linked = fixture.add_linked_worktree("linked");
    let linked_git_dir = GitFixture::linked_git_dir(&linked);
    let content = deterministic_content(11, 96 * 1024);
    let staging_root = commit_linked_pointer(&linked, "locked.bin", &content).await;
    let staging = StagingAreaReadOnly::open(staging_root)
        .await
        .expect("open shared staging ro");

    let _env = ScopedGitWorktreeEnv::new(&linked, &linked_git_dir, &linked);
    let recording = RecordingStore::new();
    let inner: Arc<dyn ObjectStore> = Arc::new(recording.clone());
    let store = Store::new(inner);
    let router = StoreLayout::new(store.clone(), "linked-lock".to_owned());
    initialize_remote(&store, &router).await;
    let held_lock = PushLock::acquire_ref(
        store.inner(),
        router.repo_prefix(),
        "refs/heads/linked",
        std::time::Duration::from_secs(60),
    )
    .await
    .expect("pre-acquire linked ref lock");
    let mut config = NativePushConfig::new(PushConfig::default());
    config.progress = false;
    config.color = false;
    config.incremental = false;
    let mut push_state = PushState::default();

    let result = run_native_push(
        &config,
        &make_specs_for_ref("refs/heads/linked"),
        NativePushInputs::new(
            Some(store.clone()),
            None,
            crab::git::push_staging::PushStaging::Ready(Arc::new(staging)),
            router,
            &mut push_state,
            "origin",
            "crab://bucket/repo",
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("lock-contention push should return per-ref outcome");

    match result.outcomes.get("refs/heads/linked") {
        Some(RefPushOutcome::Rejected(PushRejectReason::LockContention { .. })) => {}
        other => panic!("expected lock-contention rejection, got {other:?}"),
    }
    let puts = recording.puts();
    assert!(
        !puts
            .iter()
            .any(|path| path.contains("/xorbs/") || path.contains("xorbs/")),
        "lock-contention push must not upload xorbs; puts={puts:?}"
    );
    assert!(
        !puts
            .iter()
            .any(|path| path.contains("/packs/pack-") || path.contains("packs/pack-")),
        "lock-contention push must not upload packs; puts={puts:?}"
    );

    held_lock.release().await.expect("release held lock");
}

#[tokio::test]
async fn linked_worktree_hydrate_edit_push_reclone_hydrates_byte_identical_content() {
    let _git_env = ScopedCleanGitEnv::new();
    let fixture = GitFixture::new();
    std::fs::write(
        fixture.work_tree().join(".gitattributes"),
        "*.bin filter=crab diff=crab merge=crab -text\n",
    )
    .expect("write attrs");
    GitFixture::run_git(fixture.work_tree(), &["add", ".gitattributes"]);
    GitFixture::run_git(
        fixture.work_tree(),
        &["commit", "-m", "track crab binaries"],
    );

    let original = deterministic_content(21, 128 * 1024);
    let skipped = deterministic_content(22, 96 * 1024);
    let model_pointer =
        clean_content_to_pointer(fixture.work_tree(), "model.bin", original.clone()).await;
    let skip_pointer =
        clean_content_to_pointer(fixture.work_tree(), "skip.bin", skipped.clone()).await;
    std::fs::write(fixture.work_tree().join("model.bin"), model_pointer).expect("write model ptr");
    std::fs::write(fixture.work_tree().join("skip.bin"), skip_pointer).expect("write skip ptr");
    GitFixture::run_git(fixture.work_tree(), &["add", "model.bin", "skip.bin"]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "add pointer files"]);

    let recording = RecordingStore::new();
    let store = store_from_recording(&recording);
    let prefix = "linked-e2e";
    let ctx =
        crab::git::worktree::WorktreeContext::resolve_from(fixture.work_tree()).expect("main ctx");
    let mut push_state = PushState::default();
    push_ref_from_git_dir(
        &fixture.git_dir(),
        &ctx.shared_staging_dir(),
        store.clone(),
        prefix,
        "refs/heads/main",
        &mut push_state,
    )
    .await;

    let main_cache = tempfile::tempdir().expect("main hydrate cache");
    let main_hydrator = shard_hydrator(store.clone(), prefix, main_cache.path());
    let cancel = CancellationToken::new();
    run_hydrate_in(
        fixture.work_tree(),
        &hydrate_args(&["model.bin"]),
        &Config::default(),
        &main_hydrator,
        &cancel,
    )
    .await
    .expect("hydrate main sibling candidate");
    assert_eq!(
        std::fs::read(fixture.work_tree().join("model.bin")).expect("read main hydrated model"),
        original
    );

    let linked = fixture.add_linked_worktree("e2e-linked");
    let cache_dir = tempfile::tempdir().expect("hydrate cache");
    let hydrator = shard_hydrator(store.clone(), prefix, cache_dir.path());
    run_hydrate_in(
        &linked,
        &hydrate_args(&["model.bin"]),
        &Config::default(),
        &hydrator,
        &cancel,
    )
    .await
    .expect("hydrate selected linked file");
    assert_eq!(
        std::fs::read(linked.join("model.bin")).expect("read hydrated model"),
        original
    );
    assert_pointer_file(&linked.join("skip.bin"));

    run_dehydrate_in(
        &linked,
        &dehydrate_args(&["model.bin"]),
        &CancellationToken::new(),
    )
    .expect("dehydrate CoW hydrated linked file");
    assert_pointer_file(&linked.join("model.bin"));
    run_hydrate_in(
        &linked,
        &hydrate_args(&["model.bin"]),
        &Config::default(),
        &hydrator,
        &cancel,
    )
    .await
    .expect("rehydrate linked file from sibling candidate");
    assert_eq!(
        std::fs::read(linked.join("model.bin")).expect("read rehydrated model"),
        original
    );

    let edited = deterministic_content(23, 160 * 1024);
    std::fs::write(linked.join("model.bin"), &edited).expect("edit hydrated model");
    let edited_pointer = clean_content_to_pointer(&linked, "model.bin", edited.clone()).await;
    let parsed_edited_pointer = Pointer::parse(&edited_pointer).expect("edited pointer parses");
    assert_eq!(
        parsed_edited_pointer.file_hash,
        *blake3::hash(&edited).as_bytes()
    );
    std::fs::write(linked.join("model.bin"), edited_pointer).expect("write edited pointer");
    GitFixture::run_git(&linked, &["add", "model.bin"]);
    GitFixture::run_git(&linked, &["commit", "-m", "edit linked model"]);

    let linked_git_dir = GitFixture::linked_git_dir(&linked);
    let linked_ctx =
        crab::git::worktree::WorktreeContext::resolve_from(&linked).expect("linked ctx after edit");
    push_ref_from_git_dir(
        &linked_git_dir,
        &linked_ctx.shared_staging_dir(),
        store.clone(),
        prefix,
        "refs/heads/e2e-linked",
        &mut push_state,
    )
    .await;

    let reclone_parent = tempfile::tempdir().expect("reclone parent");
    let reclone = reclone_parent.path().join("reclone");
    let source = fixture.work_tree().to_str().expect("source path utf8");
    let dest = reclone.to_str().expect("reclone path utf8");
    GitFixture::run_git(
        fixture.work_tree(),
        &["clone", "--branch", "e2e-linked", source, dest],
    );
    assert_pointer_file(&reclone.join("model.bin"));

    let reclone_cache = tempfile::tempdir().expect("reclone hydrate cache");
    let reclone_hydrator = shard_hydrator(store, prefix, reclone_cache.path());
    run_hydrate_in(
        &reclone,
        &hydrate_args(&["model.bin"]),
        &Config::default(),
        &reclone_hydrator,
        &cancel,
    )
    .await
    .expect("hydrate recloned file");
    assert_eq!(
        std::fs::read(reclone.join("model.bin")).expect("read reclone hydrated model"),
        edited
    );

    let puts = recording.puts();
    assert!(
        puts.iter()
            .any(|path| path.contains("/xorbs/") || path.contains("xorbs/")),
        "linked E2E push must upload xorbs; puts={puts:?}"
    );
    assert!(
        puts.iter()
            .any(|path| path.contains("/packs/pack-") || path.contains("packs/pack-")),
        "linked E2E push must upload git packs; puts={puts:?}"
    );
}

#[tokio::test]
async fn sibling_worktrees_concurrently_share_staging_without_missing_chunks() {
    let _git_env = ScopedCleanGitEnv::new();
    let fixture = GitFixture::new();
    std::fs::write(
        fixture.work_tree().join(".gitattributes"),
        "*.bin filter=crab diff=crab merge=crab -text\n",
    )
    .expect("write attrs");
    GitFixture::run_git(fixture.work_tree(), &["add", ".gitattributes"]);
    GitFixture::run_git(
        fixture.work_tree(),
        &["commit", "-m", "track crab binaries"],
    );

    let baseline = deterministic_content(31, 128 * 1024);
    let baseline_pointer =
        clean_content_to_pointer(fixture.work_tree(), "model.bin", baseline.clone()).await;
    std::fs::write(fixture.work_tree().join("model.bin"), baseline_pointer)
        .expect("write baseline pointer");
    GitFixture::run_git(fixture.work_tree(), &["add", "model.bin"]);
    GitFixture::run_git(
        fixture.work_tree(),
        &["commit", "-m", "add baseline pointer"],
    );

    let recording = RecordingStore::new();
    let store = store_from_recording(&recording);
    let prefix = "sibling-concurrent";
    let main_ctx =
        crab::git::worktree::WorktreeContext::resolve_from(fixture.work_tree()).expect("main ctx");
    let mut push_state = PushState::default();
    push_ref_from_git_dir(
        &fixture.git_dir(),
        &main_ctx.shared_staging_dir(),
        store.clone(),
        prefix,
        "refs/heads/main",
        &mut push_state,
    )
    .await;

    let main_cache = tempfile::tempdir().expect("concurrent main hydrate cache");
    let main_hydrator = shard_hydrator(store.clone(), prefix, main_cache.path());
    run_hydrate_in(
        fixture.work_tree(),
        &hydrate_args(&["model.bin"]),
        &Config::default(),
        &main_hydrator,
        &CancellationToken::new(),
    )
    .await
    .expect("hydrate concurrent sibling candidate");
    assert_eq!(
        std::fs::read(fixture.work_tree().join("model.bin"))
            .expect("read concurrent sibling candidate"),
        baseline
    );

    let hydrate_wt = fixture.add_linked_worktree("hydrate-worker");
    let push_wt = fixture.add_linked_worktree("push-worker");
    let push_git_dir = GitFixture::linked_git_dir(&push_wt);
    let push_ctx =
        crab::git::worktree::WorktreeContext::resolve_from(&push_wt).expect("push worker ctx");

    let hydrate_store = store.clone();
    let hydrate_task = {
        let hydrate_wt = hydrate_wt.clone();
        async move {
            let cache_dir = tempfile::tempdir().expect("hydrate cache");
            let hydrator = shard_hydrator(hydrate_store, prefix, cache_dir.path());
            let cancel = CancellationToken::new();
            run_hydrate_in(
                &hydrate_wt,
                &hydrate_args(&["model.bin"]),
                &Config::default(),
                &hydrator,
                &cancel,
            )
            .await
            .expect("hydrate sibling file");
            assert_eq!(
                std::fs::read(hydrate_wt.join("model.bin")).expect("read sibling hydrated"),
                baseline
            );
            crab::cmd::status::run_status_in(&hydrate_wt, false, OutputMode::Json)
                .expect("status after hydrate");
            run_dehydrate_in(
                &hydrate_wt,
                &dehydrate_args(&["model.bin"]),
                &CancellationToken::new(),
            )
            .expect("dehydrate sibling file");
            assert_pointer_file(&hydrate_wt.join("model.bin"));
        }
    };

    let push_store = store.clone();
    let push_task = async {
        let pushed = deterministic_content(32, 144 * 1024);
        let pushed_pointer =
            clean_content_to_pointer(&push_wt, "concurrent.bin", pushed.clone()).await;
        std::fs::write(push_wt.join("concurrent.bin"), pushed_pointer)
            .expect("write concurrent pointer");
        GitFixture::run_git(&push_wt, &["add", "concurrent.bin"]);
        GitFixture::run_git(&push_wt, &["commit", "-m", "add concurrent pointer"]);
        push_ref_from_git_dir(
            &push_git_dir,
            &push_ctx.shared_staging_dir(),
            push_store,
            prefix,
            "refs/heads/push-worker",
            &mut push_state,
        )
        .await;
        pushed
    };

    let status_task = async {
        for _ in 0..3 {
            crab::cmd::status::run_status_in(&hydrate_wt, false, OutputMode::Json)
                .expect("hydrate worker status");
            crab::cmd::status::run_status_in(&push_wt, false, OutputMode::Json)
                .expect("push worker status");
            tokio::task::yield_now().await;
        }
    };

    let (_, pushed, _) = tokio::join!(hydrate_task, push_task, status_task);

    let verify_cache = tempfile::tempdir().expect("verify hydrate cache");
    let verify_hydrator = shard_hydrator(store, prefix, verify_cache.path());
    run_hydrate_in(
        &push_wt,
        &hydrate_args(&["concurrent.bin"]),
        &Config::default(),
        &verify_hydrator,
        &CancellationToken::new(),
    )
    .await
    .expect("hydrate concurrently pushed pointer");
    assert_eq!(
        std::fs::read(push_wt.join("concurrent.bin")).expect("read concurrently pushed file"),
        pushed
    );
}

/// End-to-end: create a repo with 100 pointer files, simulate a first push
/// by seeding `PushState`, modify 3 files, push again. Assert the second
/// push discovers exactly 3 pointers via the incremental tree-diff walk
/// and the push succeeds.
#[tokio::test]
async fn second_push_discovers_only_modified_pointers() {
    let fixture = GitFixture::new();
    let _git = ScopedGitDir::new(&fixture.git_dir());

    // First commit: 100 pointer files.
    for i in 0u8..100 {
        let name = format!("file_{i:03}.bin");
        fixture.write_pointer(&name, i);
    }
    GitFixture::run_git(fixture.work_tree(), &["add", "."]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "initial 100 files"]);
    let first_sha = fixture.head_sha();

    // Second commit: modify exactly 3 files with backed pointer content.
    write_staged_pointer(
        fixture.work_tree(),
        "file_010.bin",
        &deterministic_content(200, 48 * 1024),
    )
    .await;
    write_staged_pointer(
        fixture.work_tree(),
        "file_050.bin",
        &deterministic_content(201, 48 * 1024),
    )
    .await;
    write_staged_pointer(
        fixture.work_tree(),
        "file_090.bin",
        &deterministic_content(202, 48 * 1024),
    )
    .await;
    GitFixture::run_git(
        fixture.work_tree(),
        &["add", "file_010.bin", "file_050.bin", "file_090.bin"],
    );
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "modify 3 files"]);
    let second_sha = fixture.head_sha();
    assert_ne!(first_sha, second_sha, "second commit must advance HEAD");

    // Seed PushState with the first commit SHA so phase_discover takes
    // the incremental branch — equivalent to "second push" after a
    // successful first push.
    let mut push_state = PushState::default();
    push_state.set("crab://bucket/repo", "refs/heads/main", &first_sha);

    let store = make_store();
    let router = make_router(store.clone(), "test-repo");
    initialize_remote(&store, &router).await;
    let config = NativePushConfig::new(PushConfig::default());
    let cancel = CancellationToken::new();
    let metrics: Option<Arc<Metrics>> = None;

    let (buf_maker, buf) = BufferMaker::new();
    let dispatch = capture_dispatch(buf_maker);

    let result = async {
        run_native_push(
            &config,
            &make_specs(),
            NativePushInputs::new(
                Some(store),
                None,
                crab::git::push_staging::PushStaging::Missing,
                router,
                &mut push_state,
                "origin",
                "crab://bucket/repo",
                metrics,
                cancel,
            ),
        )
        .await
    }
    .with_subscriber(dispatch)
    .await;

    // The pipeline must complete without error on an InMemory store.
    result.expect("run_native_push must succeed on InMemory store");

    let trace = captured_text(&buf);

    // phase_discover must take the incremental branch.
    assert!(
        trace.contains("reason=\"incremental_ok\""),
        "phase_discover should use incremental walk; trace was:\n{trace}"
    );

    // The incremental walk must discover exactly 3 pointers (the 3
    // modified files), not all 100.
    let phase1_pointers = extract_field_after(&trace, "phase 1: ref walk complete", "pointers")
        .expect("phase 1 ref walk complete event missing from trace");
    assert_eq!(
        phase1_pointers, 3,
        "incremental walk should discover exactly 3 modified pointers, got {phase1_pointers}\nTrace:\n{trace}"
    );

    // The pre-populated walk must carry those 3 pointers into the
    // delegated pipeline.
    let install_pointers = extract_field_after(&trace, "installing pre-populated walk", "pointers")
        .expect("install_prepopulated_walk debug event missing from trace");
    assert_eq!(
        install_pointers, 3,
        "pre-populated walk must carry exactly 3 pointers, got {install_pointers}\nTrace:\n{trace}"
    );

    // Step 1 (enumerate_pointers) must adopt the pre-computed walk.
    assert!(
        trace.contains("incremental=true"),
        "enumerate_pointers must record incremental=true; trace was:\n{trace}"
    );
    assert!(
        trace.contains("source=\"native\""),
        "enumerate_pointers must record source=\"native\"; trace was:\n{trace}"
    );

    let step1_pointers = extract_field_after(&trace, "using pre-computed walk", "pointers")
        .expect("step 1 pre-computed walk event missing from trace");
    assert_eq!(
        step1_pointers, 3,
        "step 1 must adopt exactly 3 pointers, got {step1_pointers}\nTrace:\n{trace}"
    );
}

/// Verify that `PrePopulatedWalk` receives the tree-diff-based pointer set
/// by checking that the installed pointer count matches the incremental
/// walk output and that `enumerate_pointers` adopts it without re-walking.
///
/// This is a focused test for the PrePopulatedWalk wiring: a small repo
/// with a single modified file, verifying the exact data flow from
/// `walk_incremental` → `PrePopulatedWalk` → `enumerate_pointers`.
#[tokio::test]
async fn prepopulated_walk_receives_tree_diff_pointer_set() {
    let fixture = GitFixture::new();
    let _git = ScopedGitDir::new(&fixture.git_dir());

    // First commit: 5 pointer files (small repo for fast test).
    for i in 0u8..5 {
        let name = format!("ptr_{i}.bin");
        fixture.write_pointer(&name, i);
    }
    GitFixture::run_git(fixture.work_tree(), &["add", "."]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "initial"]);
    let first_sha = fixture.head_sha();

    // Second commit: modify 1 file with backed pointer content.
    write_staged_pointer(
        fixture.work_tree(),
        "ptr_2.bin",
        &deterministic_content(100, 48 * 1024),
    )
    .await;
    GitFixture::run_git(fixture.work_tree(), &["add", "ptr_2.bin"]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "modify one"]);
    let second_sha = fixture.head_sha();
    assert_ne!(first_sha, second_sha);

    let mut push_state = PushState::default();
    push_state.set("crab://bucket/repo", "refs/heads/main", &first_sha);

    let store = make_store();
    let router = make_router(store.clone(), "test-repo");
    initialize_remote(&store, &router).await;
    let config = NativePushConfig::new(PushConfig::default());
    let cancel = CancellationToken::new();

    let (buf_maker, buf) = BufferMaker::new();
    let dispatch = capture_dispatch(buf_maker);

    let result = async {
        run_native_push(
            &config,
            &make_specs(),
            NativePushInputs::new(
                Some(store),
                None,
                crab::git::push_staging::PushStaging::Missing,
                router,
                &mut push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
    }
    .with_subscriber(dispatch)
    .await;

    result.expect("run_native_push must succeed");

    let trace = captured_text(&buf);

    // The incremental walk must find exactly 1 pointer.
    let walk_pointers = extract_field_after(&trace, "phase 1: ref walk complete", "pointers")
        .expect("phase 1 ref walk complete event missing");
    assert_eq!(
        walk_pointers, 1,
        "walk_incremental should find 1 pointer, got {walk_pointers}"
    );

    // PrePopulatedWalk must receive the same count.
    let installed = extract_field_after(&trace, "installing pre-populated walk", "pointers")
        .expect("install_prepopulated_walk event missing");
    assert_eq!(
        installed, walk_pointers,
        "PrePopulatedWalk must receive the same pointer count as walk_incremental"
    );

    // enumerate_pointers must adopt the pre-populated walk (not re-walk).
    let adopted = extract_field_after(&trace, "using pre-computed walk", "pointers")
        .expect("step 1 pre-computed walk event missing");
    assert_eq!(
        adopted, installed,
        "enumerate_pointers must adopt the installed pointer count"
    );

    // Commit entries must also flow through.
    let walk_commits = extract_field_after(&trace, "phase 1: ref walk complete", "commits")
        .expect("commits field missing from phase 1 event");
    let installed_commits =
        extract_field_after(&trace, "installing pre-populated walk", "commit_entries")
            .expect("commit_entries field missing from install event");
    assert_eq!(
        walk_commits, installed_commits,
        "commit entries must flow from walk_incremental through PrePopulatedWalk"
    );
}
