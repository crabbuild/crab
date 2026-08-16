use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, MutexGuard};

use crab::cache::LocalCache;
use crab::cmd::hydrate::ShardHydrator;
use crab::core::config::CacheConfig;
use crab::git::push::{PushConfig, RefPushOutcome, run_push_batch};
use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
use crab::git::push_state::PushState;
use crab::git::remote_helper::PushSpec;
use crab::metadata::manifest::Manifest;
use crab::storage::{Store, StoreLayout};
use crab_cache_store::CachingStore;
use crab_staging::StagingAreaReadOnly;
use crab_types::pointer::Pointer;
use crab_xet::xorb::format::MerkleHash;
use object_store::memory::InMemory;
use tokio_util::sync::CancellationToken;

fn crab_bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

struct CacheEnvGuard {
    _guard: MutexGuard<'static, ()>,
    previous: Option<OsString>,
}

impl CacheEnvGuard {
    fn new(path: &Path) -> Self {
        let guard = CACHE_ENV_LOCK.lock().expect("cache env lock");
        let previous = std::env::var_os("CRAB_CACHE_DIR");
        unsafe { std::env::set_var("CRAB_CACHE_DIR", path) };
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for CacheEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var("CRAB_CACHE_DIR", value) },
            None => unsafe { std::env::remove_var("CRAB_CACHE_DIR") },
        }
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("spawn git")
}

fn require_git_ok(repo: &Path, args: &[&str]) -> Option<Output> {
    let output = run_git(repo, args);
    if !output.status.success() {
        eprintln!(
            "SKIP: git {:?} failed\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some(output)
}

fn init_repo(repo: &Path) -> Option<()> {
    std::fs::create_dir_all(repo).expect("create repo dir");
    require_git_ok(repo, &["init", "-q"])?;
    require_git_ok(repo, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    require_git_ok(repo, &["config", "user.email", "add-push-smoke@crab.local"])?;
    require_git_ok(repo, &["config", "user.name", "Crab Add Push Smoke"])?;
    Some(())
}

fn configure_crab_filter(repo: &Path) -> Option<()> {
    let process = format!("{} filter-process", crab_bin());
    require_git_ok(repo, &["config", "filter.crab.process", &process])?;
    require_git_ok(repo, &["config", "filter.crab.clean", &process])?;
    require_git_ok(repo, &["config", "filter.crab.smudge", &process])?;
    require_git_ok(repo, &["config", "filter.crab.required", "true"])?;
    Some(())
}

fn commit_crab_attributes(repo: &Path) {
    std::fs::write(
        repo.join(".gitattributes"),
        b"*.bin filter=crab diff=crab merge=crab -text\n",
    )
    .expect("write attributes");
    require_git_ok(repo, &["add", ".gitattributes"]).expect("git add attributes");
    require_git_ok(repo, &["commit", "-qm", "track crab files"]).expect("commit attrs");
}

fn indexed_crab_pointer(repo: &Path, path: &str) -> (Pointer, Vec<u8>) {
    let indexed =
        require_git_ok(repo, &["show", &format!(":{path}")]).expect("show indexed pointer");
    let pointer = Pointer::parse(&indexed.stdout).expect("indexed blob should be a crab pointer");
    (pointer, indexed.stdout)
}

fn deterministic_content(size: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut data = Vec::with_capacity(size);
    for _ in 0..size {
        state ^= state << 7;
        state ^= state >> 9;
        state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        data.push((state >> 32) as u8);
    }
    data
}

async fn push_main_to_memory(repo: &Path, scratch: &Path) -> (Store, StoreLayout) {
    let staging = StagingAreaReadOnly::open(repo.join(".crab/staging"))
        .await
        .expect("open staging readonly");
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "smoke/add-commit-push".to_owned());
    let mut config = PushConfig {
        git_dir: Some(repo.join(".git")),
        ..PushConfig::default()
    };
    config.metadb.chunk_index.local_path = Some(scratch.join("metadb-cache/chunk-index.sqlite"));
    let specs = vec![PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    }];

    let result = run_push_batch(
        &specs,
        &config,
        Some(store.clone()),
        None,
        Some(Arc::new(staging)),
        router.clone(),
        None,
        CancellationToken::new(),
        None,
    )
    .await;

    let outcome = result
        .outcomes
        .get("refs/heads/main")
        .expect("main outcome");
    assert!(matches!(outcome, RefPushOutcome::Ok), "{outcome:?}");

    let (manifest_bytes, _) = store
        .get_with_etag(&router.manifest_path())
        .await
        .expect("manifest uploaded");
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).expect("manifest json");
    assert_eq!(manifest.generation, 1);
    assert!(manifest.refs.contains_key("refs/heads/main"));
    assert!(!manifest.shard_index_hash.is_empty());

    let xorbs = store
        .list_prefix(&router.global_path("xorbs", ""))
        .await
        .expect("list xorbs");
    assert!(!xorbs.is_empty(), "push should upload at least one xorb");

    let shards = store
        .list_prefix(&router.global_path("shards", ""))
        .await
        .expect("list shards");
    assert!(!shards.is_empty(), "push should upload at least one shard");

    (store, router)
}

async fn native_push_main_follow_tags_to_memory(
    repo: &Path,
    scratch: &Path,
    tag_ref: &str,
    tag_sha: &str,
) {
    let staging = StagingAreaReadOnly::open(repo.join(".crab/staging"))
        .await
        .expect("open staging readonly");
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "smoke/follow-tags".to_owned());
    let mut push_config = PushConfig {
        git_dir: Some(repo.join(".git")),
        ..PushConfig::default()
    };
    push_config.metadb.chunk_index.local_path =
        Some(scratch.join("follow-tags-metadb/chunk-index.sqlite"));
    let mut native_config = NativePushConfig::new(push_config);
    native_config.followtags = true;
    native_config.progress = false;
    native_config.emit_summary = false;

    let specs = vec![PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    }];
    let mut push_state = PushState::default();
    let result = run_native_push(
        &native_config,
        &specs,
        NativePushInputs::new(
            Some(store.clone()),
            None,
            Some(Arc::new(staging)),
            router.clone(),
            &mut push_state,
            "origin",
            "crab://bucket/repo",
            None,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("native follow-tags push");

    assert!(matches!(
        result.outcomes.get("refs/heads/main"),
        Some(RefPushOutcome::Ok)
    ));
    assert!(matches!(
        result.outcomes.get(tag_ref),
        Some(RefPushOutcome::Ok)
    ));

    let (manifest_bytes, _) = store
        .get_with_etag(&router.manifest_path())
        .await
        .expect("manifest uploaded");
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).expect("manifest json");
    assert_eq!(
        manifest.generation, 1,
        "branch and synthesized tag must publish through one first-generation CAS"
    );
    assert_eq!(
        manifest.refs.get(tag_ref).map(String::as_str),
        Some(tag_sha)
    );
}

fn assert_main_push_uploads(repo: &Path, scratch: &Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let _ = push_main_to_memory(repo, scratch).await;
    });
}

fn assert_native_follow_tags_pushes_annotated_tag(
    repo: &Path,
    scratch: &Path,
    tag_ref: &str,
    tag_sha: &str,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        native_push_main_follow_tags_to_memory(repo, scratch, tag_ref, tag_sha).await;
    });
}

fn assert_shallow_first_push_rejects_with_boundary(repo: &Path, scratch: &Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "smoke/shallow-boundary".to_owned());
        let mut config = PushConfig {
            git_dir: Some(repo.join(".git")),
            ..PushConfig::default()
        };
        config.metadb.chunk_index.local_path =
            Some(scratch.join("shallow-metadb/chunk-index.sqlite"));
        let specs = vec![PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }];

        let result = run_push_batch(
            &specs,
            &config,
            Some(store),
            None,
            None,
            router,
            None,
            CancellationToken::new(),
            None,
        )
        .await;

        let outcome = result
            .outcomes
            .get("refs/heads/main")
            .expect("main outcome");
        assert!(
            matches!(
                outcome,
                RefPushOutcome::Rejected(crab::git::push::PushRejectReason::ShallowBoundary { .. })
            ),
            "expected shallow-boundary rejection, got {outcome:?}"
        );
    });
}

fn assert_staging_chunk_count_at_least(repo: &Path, pointer: &Pointer, min_chunks: usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let staging = StagingAreaReadOnly::open(repo.join(".crab/staging"))
            .await
            .expect("open staging readonly");
        let chunks = staging
            .chunks_for_file(&MerkleHash::from(pointer.file_hash))
            .expect("chunks for file");
        assert!(
            chunks.len() >= min_chunks,
            "expected at least {min_chunks} staged chunks, got {}",
            chunks.len()
        );
    });
}

fn assert_main_push_hydrates(repo: &Path, scratch: &Path, pointer_bytes: &[u8], expected: &[u8]) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let (store, router) = push_main_to_memory(repo, scratch).await;
        let hydrate_cache = Arc::new(LocalCache::new(scratch.join("hydrate-cache")));
        let caching_store = CachingStore::new_with_local_cache(
            store.clone(),
            &CacheConfig::default(),
            hydrate_cache,
        )
        .expect("caching store");
        let hydrator =
            ShardHydrator::new_from_cli_layout(caching_store, router).expect("shard hydrator");
        let hydrated = hydrator
            .reconstruct_from_pointer(pointer_bytes)
            .await
            .expect("hydrate pushed pointer");
        assert_eq!(hydrated, expected);
    });
}

#[test]
fn crab_add_commit_then_push_in_memory_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let cache_dir = tmp.path().join("cache");
    let _cache_guard = CacheEnvGuard::new(&cache_dir);

    let Some(()) = init_repo(&repo) else {
        return;
    };

    commit_crab_attributes(&repo);

    std::fs::write(
        repo.join("model.bin"),
        b"hello from the add/commit/push smoke\n",
    )
    .expect("write model");
    let add = Command::new(crab_bin())
        .args(["add", "--jobs", "0", "model.bin"])
        .current_dir(&repo)
        .env("CRAB_CACHE_DIR", &cache_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("spawn crab add");
    assert!(
        add.status.success(),
        "crab add failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );

    let _ = indexed_crab_pointer(&repo, "model.bin");

    require_git_ok(&repo, &["commit", "-qm", "add model pointer"]).expect("commit pointer");
    assert_main_push_uploads(&repo, tmp.path());
}

#[test]
fn git_add_commit_then_push_in_memory_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let cache_dir = tmp.path().join("cache");
    let _cache_guard = CacheEnvGuard::new(&cache_dir);

    let Some(()) = init_repo(&repo) else {
        return;
    };
    let Some(()) = configure_crab_filter(&repo) else {
        return;
    };
    commit_crab_attributes(&repo);

    std::fs::write(
        repo.join("model.bin"),
        b"hello from the git add/commit/push smoke\n",
    )
    .expect("write model");
    require_git_ok(&repo, &["add", "model.bin"]).expect("git add model");

    let _ = indexed_crab_pointer(&repo, "model.bin");

    require_git_ok(&repo, &["commit", "-qm", "add model pointer"]).expect("commit pointer");
    assert_main_push_uploads(&repo, tmp.path());
}

#[test]
fn native_push_follow_tags_pushes_annotated_tag_in_memory_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let cache_dir = tmp.path().join("cache");
    let _cache_guard = CacheEnvGuard::new(&cache_dir);

    let Some(()) = init_repo(&repo) else {
        return;
    };
    let Some(()) = configure_crab_filter(&repo) else {
        return;
    };
    commit_crab_attributes(&repo);

    std::fs::write(repo.join("model.bin"), b"follow tags smoke\n").expect("write model");
    require_git_ok(&repo, &["add", "model.bin"]).expect("git add model");
    let _ = indexed_crab_pointer(&repo, "model.bin");
    require_git_ok(&repo, &["commit", "-qm", "add release pointer"]).expect("commit pointer");
    require_git_ok(&repo, &["tag", "-a", "v1.0", "-m", "release"]).expect("annotated tag");
    let tag_sha = require_git_ok(&repo, &["rev-parse", "refs/tags/v1.0"])
        .map(|out| String::from_utf8(out.stdout).expect("tag sha utf8"))
        .map(|sha| sha.trim().to_owned())
        .expect("tag sha");

    assert_native_follow_tags_pushes_annotated_tag(&repo, tmp.path(), "refs/tags/v1.0", &tag_sha);
}

#[test]
fn shallow_clone_first_push_rejects_with_shallow_boundary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source");
    let shallow = tmp.path().join("shallow");

    let Some(()) = init_repo(&source) else {
        return;
    };
    std::fs::write(source.join("one.txt"), b"one\n").expect("write first");
    require_git_ok(&source, &["add", "one.txt"]).expect("add first");
    require_git_ok(&source, &["commit", "-qm", "one"]).expect("commit first");
    std::fs::write(source.join("two.txt"), b"two\n").expect("write second");
    require_git_ok(&source, &["add", "two.txt"]).expect("add second");
    require_git_ok(&source, &["commit", "-qm", "two"]).expect("commit second");

    let source_url = format!("file://{}", source.display());
    let output = Command::new("git")
        .args([
            "clone",
            "--depth=1",
            &source_url,
            shallow.to_str().unwrap_or_default(),
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("spawn git clone");
    if !output.status.success() {
        eprintln!(
            "SKIP: shallow clone failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    assert!(
        shallow.join(".git/shallow").exists(),
        "test must use a real shallow clone"
    );
    assert_shallow_first_push_rejects_with_boundary(&shallow, tmp.path());
}

#[test]
fn git_add_multi_batch_file_pushes_and_hydrates_byte_identically() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let cache_dir = tmp.path().join("cache");
    let _cache_guard = CacheEnvGuard::new(&cache_dir);

    let Some(()) = init_repo(&repo) else {
        return;
    };
    let Some(()) = configure_crab_filter(&repo) else {
        return;
    };
    commit_crab_attributes(&repo);

    let content = deterministic_content(10 * 1024 * 1024);
    std::fs::write(repo.join("large.bin"), &content).expect("write large model");
    require_git_ok(&repo, &["add", "large.bin"]).expect("git add large model");

    let (pointer, pointer_bytes) = indexed_crab_pointer(&repo, "large.bin");
    assert_eq!(pointer.size, content.len() as u64);
    assert_staging_chunk_count_at_least(&repo, &pointer, 65);

    require_git_ok(&repo, &["commit", "-qm", "add large pointer"]).expect("commit pointer");
    assert_main_push_hydrates(&repo, tmp.path(), &pointer_bytes, &content);
}
