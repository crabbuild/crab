use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, MutexGuard};

use crab::cache::LocalCache;
use crab::core::config::CacheConfig;
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
        let guard = CACHE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

async fn push_refs_to_memory(repo: &Path, refs: &[&str]) -> (Store, StoreLayout) {
    let staging = StagingAreaReadOnly::open(repo.join(".crab/staging"))
        .await
        .expect("open staging readonly");
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "smoke/add-commit-push".to_owned());
    let staging = Arc::new(staging);
    for reference in refs {
        let stats = publish_memory_ref(repo, &store, &router, &staging, reference)
            .await
            .expect("publish Capsule ref");
        assert_eq!((stats.refs_pushed, stats.refs_failed), (1, 0));
    }

    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let view = crab_read::capsule_protocol::open_view(
        &layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .expect("open published Capsule view");
    for reference in refs {
        let output = require_git_ok(repo, &["rev-parse", reference]).expect("local ref");
        let tip = String::from_utf8(output.stdout).expect("ref OID");
        assert_eq!(
            view.refs().get(*reference).map(String::as_str),
            Some(tip.trim())
        );
    }
    let catalog = view
        .pointer_catalog()
        .await
        .expect("published pointer catalog");
    assert!(!catalog.files().is_empty());

    let xorbs = store
        .list_prefix(&crab_storage::global_content_prefix(
            router.global_prefix(),
            "xorbs",
        ))
        .await
        .expect("list xorbs");
    assert!(!xorbs.is_empty(), "push should upload at least one xorb");

    let shards = store
        .list_prefix(&crab_storage::global_content_prefix(
            router.global_prefix(),
            "shards",
        ))
        .await
        .expect("list shards");
    assert!(!shards.is_empty(), "push should upload at least one shard");

    (store, router)
}

fn assert_main_push_uploads(repo: &Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let _ = push_refs_to_memory(repo, &["refs/heads/main"]).await;
    });
}

fn assert_shallow_first_push_rejects_without_publication(repo: &Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "smoke/shallow-boundary".to_owned());
        let layout = crab_storage::StoreLayout::with_global_prefix(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
            router.global_prefix().to_owned(),
        );
        let repository_id = blake3::hash(router.repo_prefix().as_bytes())
            .to_hex()
            .to_string();
        let before =
            crab_write::capsule_protocol::initialize(&layout, &repository_id, "refs/heads/main")
                .await
                .expect("initialize unborn Capsule root");
        let staging_dir = repo.join(".crab/staging");
        crab_staging::StagingArea::open(staging_dir.clone())
            .await
            .expect("create empty staging")
            .close()
            .await
            .expect("close empty staging");
        let staging = Arc::new(StagingAreaReadOnly::open(staging_dir).await.unwrap());
        let boundary = std::fs::read(repo.join(".git/shallow")).expect("shallow boundary");
        let result = publish_memory_ref(repo, &store, &router, &staging, "refs/heads/main").await;
        assert!(
            result.is_err(),
            "incomplete history must be rejected: {result:?}"
        );
        let after = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .unwrap();
        assert_eq!(before.record().digest(), after.record().digest());
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 16 * 1024 * 1024,
                max_frontier_bytes: 16 * 1024 * 1024,
            },
        )
        .await
        .expect("unchanged Capsule view");
        assert!(view.refs().is_empty());
        assert_eq!(std::fs::read(repo.join(".git/shallow")).unwrap(), boundary);
    });
}

async fn publish_memory_ref(
    repo: &Path,
    store: &Store,
    router: &StoreLayout,
    staging: &Arc<StagingAreaReadOnly>,
    reference: &str,
) -> crab::core::error::Result<crab::import::publish::PublishStats> {
    let branch = reference
        .strip_prefix("refs/heads/")
        .expect("branch ref")
        .to_owned();
    let output = require_git_ok(repo, &["rev-parse", reference]).expect("local ref");
    let head_commit_oid = String::from_utf8(output.stdout)
        .expect("ref OID")
        .trim()
        .to_owned();
    let target = crab::import::ingest::ResolvedStore {
        store: store.clone(),
        bucket: store.bucket_identity(),
        prefix: router.repo_prefix().to_owned(),
    };
    let inputs = crab::import::publish::PublishInputs {
        target,
        repo_prefix: router.repo_prefix().to_owned(),
        staging: Arc::clone(staging),
        branch,
        head_commit_oid,
        git_dir: repo.join(".git"),
        metrics: None,
        cancel: CancellationToken::new(),
    };
    // This public stage shares Capsule preparation with CLI/helper pushes;
    // external fixtures need no exported internal push orchestrator.
    crab::import::publish::run_publish(inputs).await
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

fn assert_prepared_only_recipe_state(
    repo: &Path,
    pointer: Option<&Pointer>,
    expected: Option<&[u8]>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let staging = StagingAreaReadOnly::open(repo.join(".crab/staging"))
            .await
            .expect("open staging readonly");
        let Some(pointer) = pointer else {
            let published = staging
                .list_files()
                .expect("list open prepared files")
                .into_iter()
                .any(|file| {
                    staging
                        .published_recipe_for_file(&MerkleHash::from(file.file_hash))
                        .expect("inspect published recipe")
                        .is_some()
                });
            assert!(
                !published,
                "skip-only recipes must remain invisible to push"
            );
            return;
        };

        let file_hash = MerkleHash::from(pointer.file_hash);
        // Git clean delegates verified payload ownership; only the outer Git
        // writer knows index success. Push selects that lease by this pointer.
        assert!(
            staging
                .published_recipe_for_file(&file_hash)
                .unwrap()
                .is_none(),
            "external Git clean must not claim a completed native index transaction"
        );
        let recipe = staging
            .external_recipes_for_files(&[file_hash])
            .expect("load delegated prepared recipe")
            .remove(&file_hash)
            .flatten()
            .expect("Git-selected pointer must retain a delegated prepared recipe");
        assert!(
            !staging
                .has_complete_segment_authority_for_recipe(&recipe)
                .expect("inspect segment authority"),
            "ordinary git add must reuse prepared authority without writing segments"
        );
        let hashes = staging
            .chunks_for_file(&file_hash)
            .expect("load prepared recipe chunks");
        let reconstructed = staging
            .get_chunks_batch(&hashes)
            .await
            .expect("read prepared-only chunks")
            .into_iter()
            .flat_map(|(_, bytes)| bytes)
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, expected.expect("expected prepared bytes"));
    });
}

fn assert_partial_overlap_has_canonical_prepared_authority(
    repo: &Path,
    first: &Pointer,
    second: &Pointer,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let staging = StagingAreaReadOnly::open(repo.join(".crab/staging"))
            .await
            .expect("open staging readonly");
        let first_hash = MerkleHash::from(first.file_hash);
        let second_hash = MerkleHash::from(second.file_hash);
        let first_chunks = staging
            .chunks_for_file(&first_hash)
            .expect("first recipe chunks")
            .into_iter()
            .collect::<HashSet<_>>();
        let second_chunks = staging
            .chunks_for_file(&second_hash)
            .expect("second recipe chunks")
            .into_iter()
            .collect::<HashSet<_>>();
        let shared = first_chunks
            .intersection(&second_chunks)
            .copied()
            .collect::<HashSet<_>>();
        assert!(shared.len() > 8, "fixture must share multiple CDC chunks");

        let first_plan = staging
            .load_file_push_plan(&first_hash)
            .await
            .expect("first plan")
            .expect("first canonical authority");
        let second_plan = staging
            .load_file_push_plan(&second_hash)
            .await
            .expect("second plan")
            .expect("second canonical authority");
        let placements = |plan: &crab_staging::push_plan::FilePushPlan| {
            plan.prepared_xorbs
                .iter()
                .flat_map(|xorb| {
                    xorb.placements
                        .iter()
                        .map(|placement| (placement.chunk_hash.clone(), xorb.hash.clone()))
                })
                .collect::<HashMap<_, _>>()
        };
        let first_placements = placements(&first_plan);
        let second_placements = placements(&second_plan);
        for chunk_hash in shared {
            let chunk_hash = chunk_hash.hex();
            assert_eq!(
                first_placements.get(&chunk_hash),
                second_placements.get(&chunk_hash),
                "shared chunk must have one canonical prepared placement"
            );
        }

        let unique_payloads = first_plan
            .prepared_xorbs
            .iter()
            .chain(&second_plan.prepared_xorbs)
            .map(|xorb| xorb.hash.clone())
            .collect::<HashSet<_>>();
        let payload_root = repo.join(".crab/staging/push-plans/payloads");
        let payload_files = std::fs::read_dir(payload_root)
            .expect("payload shards")
            .flat_map(|shard| {
                std::fs::read_dir(shard.expect("payload shard").path())
                    .expect("payload files")
                    .map(|entry| entry.expect("payload file").path())
            })
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "xorb")
            })
            .count();
        assert_eq!(payload_files, unique_payloads.len());
    });
}

fn assert_push_hydrates(repo: &Path, scratch: &Path, refs: &[&str], files: &[(&[u8], &[u8])]) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let (store, router) = push_refs_to_memory(repo, refs).await;
        let hydrate_cache = Arc::new(LocalCache::new(scratch.join("hydrate-cache")));
        let caching_store = CachingStore::new_with_local_cache(
            store.clone(),
            &CacheConfig::default(),
            hydrate_cache,
        )
        .expect("caching store");
        let hydrator = crab::read::build_cli_hydrator(
            caching_store,
            router,
            &crab::core::config::Config::default(),
        )
        .expect("shard hydrator");
        for (pointer_bytes, expected) in files {
            let hydrated = hydrator
                .reconstruct_from_pointer(pointer_bytes)
                .await
                .expect("hydrate pushed pointer");
            assert_eq!(hydrated.as_slice(), *expected);
        }
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
    assert_main_push_uploads(&repo);
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
    assert_main_push_uploads(&repo);
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
    assert_shallow_first_push_rejects_without_publication(&shallow);
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
    assert_push_hydrates(
        &repo,
        tmp.path(),
        &["refs/heads/main"],
        &[(&pointer_bytes, &content)],
    );
}

#[test]
fn crab_add_partial_overlap_pushes_and_hydrates_byte_identically() {
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

    let shared = deterministic_content(20 * 1024 * 1024);
    let first_prefix = deterministic_content(4 * 1024 * 1024);
    let mut second_prefix = first_prefix.clone();
    second_prefix.reverse();
    let first = [first_prefix, shared.clone()].concat();
    let second = [second_prefix, shared].concat();
    std::fs::write(repo.join("first.bin"), &first).expect("write first overlap file");
    std::fs::write(repo.join("second.bin"), &second).expect("write second overlap file");

    let add = Command::new(crab_bin())
        .args(["add", "--jobs", "2", "first.bin", "second.bin"])
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
    let (first_pointer, first_pointer_bytes) = indexed_crab_pointer(&repo, "first.bin");
    let (second_pointer, second_pointer_bytes) = indexed_crab_pointer(&repo, "second.bin");
    assert_partial_overlap_has_canonical_prepared_authority(&repo, &first_pointer, &second_pointer);

    require_git_ok(&repo, &["commit", "-qm", "add overlapping pointers"]).expect("commit pointers");
    assert_push_hydrates(
        &repo,
        tmp.path(),
        &["refs/heads/main"],
        &[
            (&first_pointer_bytes, &first),
            (&second_pointer_bytes, &second),
        ],
    );
}

#[test]
fn skip_git_add_then_git_add_reuses_prepared_authority_and_hydrates() {
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

    let content = deterministic_content(2 * 1024 * 1024);
    std::fs::write(repo.join("prepared.bin"), &content).expect("write prepared content");
    let add = Command::new(crab_bin())
        .args(["add", "--jobs", "0", "--skip-git-add", "prepared.bin"])
        .current_dir(&repo)
        .env("CRAB_CACHE_DIR", &cache_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("spawn crab add --skip-git-add");
    assert!(
        add.status.success(),
        "crab add --skip-git-add failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    assert!(
        !run_git(&repo, &["ls-files", "--error-unmatch", "prepared.bin"])
            .status
            .success(),
        "skip-git-add must not mutate Git's index"
    );
    assert_prepared_only_recipe_state(&repo, None, None);

    require_git_ok(&repo, &["add", "prepared.bin"]).expect("git add prepared content");
    let (pointer, pointer_bytes) = indexed_crab_pointer(&repo, "prepared.bin");
    assert_eq!(pointer.size, content.len() as u64);
    assert_prepared_only_recipe_state(&repo, Some(&pointer), Some(&content));

    require_git_ok(&repo, &["commit", "-qm", "add prepared pointer"])
        .expect("commit prepared pointer");
    assert_push_hydrates(
        &repo,
        tmp.path(),
        &["refs/heads/main"],
        &[(&pointer_bytes, &content)],
    );
}

#[test]
fn committed_recipe_on_another_branch_survives_skip_then_git_add_until_first_push() {
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

    let first = deterministic_content(2 * 1024 * 1024);
    std::fs::write(repo.join("history.bin"), &first).expect("write first version");
    require_git_ok(&repo, &["add", "history.bin"]).expect("git add first version");
    let (_, first_pointer) = indexed_crab_pointer(&repo, "history.bin");
    require_git_ok(&repo, &["commit", "-qm", "add first version"]).expect("commit first version");
    require_git_ok(&repo, &["checkout", "-qb", "other", "HEAD~1"])
        .expect("switch to branch without first version");

    let mut second = deterministic_content(2 * 1024 * 1024);
    second.reverse();
    std::fs::write(repo.join("history.bin"), &second).expect("write second version");
    let add = Command::new(crab_bin())
        .args(["add", "--skip-git-add", "history.bin"])
        .current_dir(&repo)
        .env("CRAB_CACHE_DIR", &cache_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("spawn crab add --skip-git-add");
    assert!(
        add.status.success(),
        "crab add --skip-git-add failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    require_git_ok(&repo, &["add", "history.bin"]).expect("git add second version");
    let (_, second_pointer) = indexed_crab_pointer(&repo, "history.bin");
    require_git_ok(&repo, &["commit", "-qm", "add second version"]).expect("commit second version");

    assert_push_hydrates(
        &repo,
        tmp.path(),
        &["refs/heads/main", "refs/heads/other"],
        &[(&first_pointer, &first), (&second_pointer, &second)],
    );
}
