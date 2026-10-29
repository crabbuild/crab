//! Real Capsule publication and hydration across linked Git worktrees.

use crate::{
    cmd::{
        dehydrate::{DehydrateArgs, run_dehydrate_in},
        hydrate::{HydrateArgs, HydrationRuntime, run_hydrate_in},
    },
    core::{
        config::{CacheConfig, Config},
        context::AppContext,
        output::OutputMode,
    },
    git::{
        clean::{CleanSession, StagingChunkStager},
        push::{PushConfig, PushResult, RefPushOutcome},
        remote_helper::PushSpec,
    },
    storage::{StoreLayout, store::Store},
};
use async_trait::async_trait;
use bytes::Bytes;
use crab_staging::{
    StagingArea, StagingAreaReadOnly,
    recipe::{ChunkingPolicyId, FileRecipe},
};
use crab_types::pointer::Pointer;
use crab_xet::{chunker::GearChunker, xorb::format::MerkleHash};
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory,
    path::Path as ObjectPath,
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

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
        self.puts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, path: &ObjectPath) {
        self.puts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

async fn commit_linked_pointer(linked: &Path, name: &str, content: &[u8]) -> PathBuf {
    let ctx = crate::git::worktree::WorktreeContext::resolve_from(linked).expect("linked ctx");
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
    let ctx = crate::git::worktree::WorktreeContext::resolve_from(root).expect("worktree ctx");
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
        Err(shared) => panic!(
            "shared staging still has {} owners after clean",
            Arc::strong_count(&shared)
        ),
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
    let cache = Arc::new(crate::cache::LocalCache::new(cache_root.to_path_buf()));
    let caching_store = crab_cache_store::CachingStore::new_with_local_cache(
        store.clone(),
        &CacheConfig::default(),
        cache,
    )
    .expect("build caching store");
    crate::read::build_cli_hydrator(
        caching_store,
        StoreLayout::new(store, prefix.to_owned()),
        &crate::core::config::Config::default(),
    )
    .expect("build shard hydrator")
}

fn assert_pointer_file(path: &Path) -> Pointer {
    let bytes = std::fs::read(path).expect("read pointer file");
    Pointer::parse(&bytes).expect("file must be a crab pointer")
}

fn store_from_recording(recording: &RecordingStore) -> Store {
    Store::new(Arc::new(recording.clone()) as Arc<dyn ObjectStore>)
}

async fn initialize_remote(store: &Store, router: &StoreLayout) {
    crate::cmd::init::initialize_remote_repository_store(store, router, "refs/heads/main")
        .await
        .expect("initialize Capsule repository");
}

async fn publish_ref(
    store: &Store,
    router: &StoreLayout,
    git_dir: &Path,
    staging: &Arc<StagingAreaReadOnly>,
    reference: &str,
) -> crate::core::error::Result<PushResult> {
    let config = PushConfig {
        git_dir: Some(git_dir.to_path_buf()),
        ..Default::default()
    };
    super::capsule_push::run(
        &config,
        &[PushSpec {
            force: false,
            src: reference.to_owned(),
            dst: reference.to_owned(),
        }],
        store,
        router,
        None,
        &[],
        Some(staging),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .map(|(result, _)| result)
}

async fn push_ref_from_git_dir(
    git_dir: &Path,
    staging_root: &Path,
    store: Store,
    prefix: &str,
    reference: &str,
) {
    let staging = Arc::new(
        StagingAreaReadOnly::open(staging_root.to_path_buf())
            .await
            .unwrap(),
    );
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    initialize_remote(&store, &router).await;
    let result = publish_ref(&store, &router, git_dir, &staging, reference)
        .await
        .unwrap();
    assert_eq!(result.outcomes.get(reference), Some(&RefPushOutcome::Ok));
}

#[tokio::test]
async fn linked_worktree_push_flushes_xorbs_before_capsule_visibility() {
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    let cache_root = tempfile::tempdir().unwrap();
    let _cache = crate::test::git_repo::CacheDirGuard::new(cache_root.path());
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

    let recording = RecordingStore::new();
    let inner: Arc<dyn ObjectStore> = Arc::new(recording.clone());
    let store = Store::new(inner);
    let router = StoreLayout::new(store.clone(), "linked-push".to_owned());
    initialize_remote(&store, &router).await;
    let result = publish_ref(
        &store,
        &router,
        &linked_git_dir,
        &Arc::new(staging),
        "refs/heads/linked",
    )
    .await
    .expect("publish linked Capsule");

    assert_eq!(
        result.outcomes.get("refs/heads/linked"),
        Some(&RefPushOutcome::Ok)
    );
    let puts = recording.puts();
    let first_xorb = puts
        .iter()
        .position(|path| path.contains("/xorbs/") || path.contains("xorbs/"))
        .expect("push must upload staged xorb before refs move");
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), "linked-push".to_owned());
    let head_prefix = format!("{}/", layout.capsule_ref_heads_prefix());
    let visible = puts
        .iter()
        .position(|path| path.starts_with(&head_prefix))
        .expect("publish per-ref Capsule head");
    assert!(
        first_xorb < visible,
        "payload must be durable before visibility: {puts:?}"
    );
    let linked_tip = GitFixture::run_git_output(&linked, &["rev-parse", "refs/heads/linked"]);
    let view = crab_read::capsule_protocol::open_view(
        &layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        view.refs().get("refs/heads/linked").map(String::as_str),
        Some(linked_tip.trim())
    );
}

#[tokio::test]
async fn linked_worktree_hydrate_edit_push_reclone_hydrates_byte_identical_content() {
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    let cache_root = tempfile::tempdir().unwrap();
    let _cache = crate::test::git_repo::CacheDirGuard::new(cache_root.path());
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
        crate::git::worktree::WorktreeContext::resolve_from(fixture.work_tree()).expect("main ctx");
    push_ref_from_git_dir(
        &fixture.git_dir(),
        &ctx.shared_staging_dir(),
        store.clone(),
        prefix,
        "refs/heads/main",
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
    let linked_ctx = crate::git::worktree::WorktreeContext::resolve_from(&linked)
        .expect("linked ctx after edit");
    push_ref_from_git_dir(
        &linked_git_dir,
        &linked_ctx.shared_staging_dir(),
        store.clone(),
        prefix,
        "refs/heads/e2e-linked",
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
            .any(|path| path.starts_with(&format!("{prefix}/v2/capsules/"))),
        "linked push must publish a Capsule: {puts:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sibling_worktrees_concurrently_share_staging_without_missing_chunks() {
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    let cache_root = tempfile::tempdir().unwrap();
    let _cache = crate::test::git_repo::CacheDirGuard::new(cache_root.path());
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
        crate::git::worktree::WorktreeContext::resolve_from(fixture.work_tree()).expect("main ctx");
    push_ref_from_git_dir(
        &fixture.git_dir(),
        &main_ctx.shared_staging_dir(),
        store.clone(),
        prefix,
        "refs/heads/main",
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
        crate::git::worktree::WorktreeContext::resolve_from(&push_wt).expect("push worker ctx");

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
            crate::cmd::status::run_status_in(&hydrate_wt, false, OutputMode::Json)
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
        )
        .await;
        pushed
    };

    let status_task = async {
        for _ in 0..3 {
            crate::cmd::status::run_status_in(&hydrate_wt, false, OutputMode::Json)
                .expect("hydrate worker status");
            crate::cmd::status::run_status_in(&push_wt, false, OutputMode::Json)
                .expect("push worker status");
            tokio::task::yield_now().await;
        }
    };

    let ((), pushed, ()) = tokio::join!(hydrate_task, push_task, status_task);

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

#[tokio::test]
async fn namespace_contention_retains_linked_recipe_until_successful_retry() {
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    let cache_root = tempfile::tempdir().unwrap();
    let _cache = crate::test::git_repo::CacheDirGuard::new(cache_root.path());
    let fixture = GitFixture::new();
    std::fs::write(fixture.work_tree().join("README.md"), b"init").unwrap();
    GitFixture::run_git(fixture.work_tree(), &["add", "README.md"]);
    GitFixture::run_git(fixture.work_tree(), &["commit", "-m", "initial"]);
    let linked = fixture.add_linked_worktree("linked");
    let git_dir = GitFixture::linked_git_dir(&linked);
    let content = deterministic_content(11, 96 * 1024);
    let root = commit_linked_pointer(&linked, "locked.bin", &content).await;
    let staging = Arc::new(StagingAreaReadOnly::open(root).await.unwrap());
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "linked-contention".to_owned());
    initialize_remote(&store, &router).await;
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let cancel = CancellationToken::new();
    // Current publication may leave immutable orphans after preparation. The
    // namespace lease must still prevent visibility and preserve retry authority.
    crab_write::with_ref_namespaces_wait(
        layout.store(),
        &layout,
        &["refs/heads/linked".to_owned()],
        std::time::Duration::from_mins(1),
        std::time::Duration::ZERO,
        &cancel,
        |_| async {
            let error = publish_ref(&store, &router, &git_dir, &staging, "refs/heads/linked")
                .await
                .unwrap_err();
            assert!(matches!(
                crate::git::push::PushRejectReason::from_error(&error),
                crate::git::push::PushRejectReason::LockContention { .. }
            ));
            let view = crab_read::capsule_protocol::open_view(
                &layout,
                crab_read::capsule_protocol::CapsuleReadLimits {
                    max_capsule_bytes: 16 * 1024 * 1024,
                    max_frontier_bytes: 16 * 1024 * 1024,
                },
            )
            .await
            .unwrap();
            assert!(!view.refs().contains_key("refs/heads/linked"));
            assert!(staging.lifecycle_health().unwrap().path_leases > 0);
            Ok::<(), crate::core::error::CrabError>(())
        },
    )
    .await
    .unwrap();
    let result = publish_ref(&store, &router, &git_dir, &staging, "refs/heads/linked")
        .await
        .unwrap();
    assert!(result.all_ok());
    let cache = tempfile::tempdir().unwrap();
    let hydrator = shard_hydrator(store, router.repo_prefix(), cache.path());
    let pointer = std::fs::read(linked.join("locked.bin")).unwrap();
    assert_eq!(
        hydrator.reconstruct_from_pointer(&pointer).await.unwrap(),
        content
    );
}
