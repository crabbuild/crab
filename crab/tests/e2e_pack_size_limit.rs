//! Regression tests for the P0-4 pack-intake size cap.
//!
//! Exercise four behaviours:
//!   * An aggregate closure over the limit is atomically split into bounded
//!     packs; derived locator acceleration remains repairable by the owner/read path.
//!   * A pack that exceeds `receive.maxInputSize` is rejected quickly,
//!     well under the 5-minute `INDEX_PACK_TIMEOUT`, and surfaces as
//!     `RefPushOutcome::Rejected(PushRejectReason::PackTooLarge)`.
//!   * A pack whose size equals the limit exactly is accepted.
//!   * `receive_max_input_size = 0` disables the cap entirely.

#![recursion_limit = "256"]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crab::core::error::CrabError;
use crab::git::pack::{PushPackConfig, generate_push_pack, install_pack_locally};
use crab::git::push::{PushConfig, PushRejectReason, RefUpdate, run_push_batch};
use crab::git::remote_helper::PushSpec;
use crab::metadata::manifest::{read_bulk_pack_list, read_manifest};
use crab::storage::StoreLayout;
use crab::storage::store::Store;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use tokio_util::sync::CancellationToken;

/// Fresh git repo with a single commit. Unlike the lib-internal
/// `TestGitRepo` this is re-created per test so each test can safely
/// mutate `GIT_DIR` without racing siblings.
struct Repo {
    _dir: tempfile::TempDir,
    git_dir: PathBuf,
    commit_sha: String,
}

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path();

        run_git(work, &["init", "--initial-branch=main"]);
        run_git(work, &["config", "user.email", "test@example.com"]);
        run_git(work, &["config", "user.name", "Test"]);
        std::fs::write(work.join("a.txt"), b"hello\n").expect("write file");
        run_git(work, &["add", "a.txt"]);
        run_git(work, &["commit", "-m", "init"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(work)
            .env_remove("GIT_DIR")
            .output()
            .expect("rev-parse");
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_owned();

        let git_dir = work.join(".git");
        Self {
            _dir: tmp,
            git_dir,
            commit_sha: sha,
        }
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Serialise `GIT_DIR` mutation across tests within this binary.
static GIT_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct GitDirScope {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl GitDirScope {
    fn new(git_dir: &Path) -> Self {
        let lock = GIT_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GIT_DIR").ok();
        // SAFETY: access is serialised by GIT_DIR_LOCK for the duration
        // of the returned guard.
        unsafe { std::env::set_var("GIT_DIR", git_dir) };
        Self { _lock: lock, prev }
    }
}

impl Drop for GitDirScope {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
            None => unsafe { std::env::remove_var("GIT_DIR") },
        }
    }
}

fn ref_for(commit_sha: &str) -> Vec<RefUpdate> {
    vec![RefUpdate {
        ref_name: "refs/heads/main".into(),
        old_sha: None,
        new_sha: commit_sha.to_owned(),
        force: false,
    }]
}

fn incompressible_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ seed;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_aggregate_is_atomically_published_as_bounded_packs() {
    let source = tempfile::tempdir().expect("source tempdir");
    run_git(source.path(), &["init", "--initial-branch=main"]);
    run_git(source.path(), &["config", "user.email", "test@example.com"]);
    run_git(source.path(), &["config", "user.name", "Test"]);
    for index in 0..4 {
        std::fs::write(
            source.path().join(format!("blob-{index}.bin")),
            incompressible_bytes(index, 700 * 1024),
        )
        .expect("write fixture blob");
    }
    run_git(source.path(), &["add", "."]);
    run_git(source.path(), &["commit", "-m", "large fixture"]);
    let source_head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(source.path())
            .env_remove("GIT_DIR")
            .output()
            .expect("source rev-parse")
            .stdout,
    )
    .expect("source HEAD UTF-8")
    .trim()
    .to_owned();

    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let store = Store::new(Arc::clone(&inner));
    let router = StoreLayout::new(store.clone(), "bounded-pack-set".to_owned());
    let mut config = PushConfig::default();
    config.git_dir = Some(source.path().join(".git"));
    config.receive_max_input_size = 1024 * 1024;
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
        None,
        router.clone(),
        None,
        CancellationToken::new(),
        None,
    )
    .await;
    assert!(result.all_ok(), "split pack-set push failed: {result:?}");

    let (manifest, _) = read_manifest(&store, &router)
        .await
        .expect("read committed manifest");
    assert_eq!(manifest.generation, 1, "push must commit exactly once");
    assert_eq!(
        manifest.refs.get("refs/heads/main"),
        Some(&source_head),
        "the only manifest CAS must publish the requested ref"
    );
    let packs = read_bulk_pack_list(&store, &router, &manifest.pack_index_hash)
        .await
        .expect("read committed pack set");
    assert!(packs.len() > 1, "fixture must publish multiple packs");
    assert!(
        packs
            .iter()
            .all(|pack| pack.size <= config.receive_max_input_size),
        "every committed pack must respect receive.maxInputSize"
    );

    let reconstructed = tempfile::tempdir().expect("reconstructed tempdir");
    run_git(reconstructed.path(), &["init", "--bare"]);
    let pack_dir = reconstructed.path().join("objects/pack");
    for pack in &packs {
        let pack_bytes = inner
            .get(&router.pack_path(&pack.pack_id))
            .await
            .expect("read committed pack")
            .bytes()
            .await
            .expect("read committed pack bytes");
        let index_bytes = inner
            .get(&router.pack_index_path(&pack.pack_id))
            .await
            .expect("read committed pack index")
            .bytes()
            .await
            .expect("read committed pack index bytes");
        assert_eq!(pack_bytes.len() as u64, pack.size);
        std::fs::write(
            pack_dir.join(format!("pack-{}.pack", pack.pack_id)),
            pack_bytes,
        )
        .expect("write reconstructed pack");
        std::fs::write(
            pack_dir.join(format!("pack-{}.idx", pack.pack_id)),
            index_bytes,
        )
        .expect("write reconstructed pack index");
        assert!(
            inner
                .head(&ObjectPath::from(format!(
                    "bounded-pack-set/packs/pack-{}.meta",
                    pack.pack_id
                )))
                .await
                .is_ok(),
            "every committed pack must have metadata"
        );
    }
    run_git(
        reconstructed.path(),
        &["update-ref", "refs/heads/main", &source_head],
    );
    run_git(
        reconstructed.path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    run_git(reconstructed.path(), &["fsck", "--full"]);
}

/// Pack exceeding the limit is rejected as `PackTooLarge`, fast.
///
/// "Fast" here means well under the 5-minute `INDEX_PACK_TIMEOUT`:
/// generation of a trivial pack takes a handful of milliseconds, so
/// setting the limit to 1 byte forces rejection on the first real
/// pack regardless of repo content.
#[tokio::test]
async fn pack_exceeding_limit_rejected_before_index_pack() {
    let repo = Repo::new();
    let _git_dir = GitDirScope::new(&repo.git_dir);

    let refs = ref_for(&repo.commit_sha);
    let config = PushPackConfig {
        thin_packs: false,
        // 1 byte cap: any real pack exceeds this.
        max_input_size: 1,
        git_dir: None,
    };

    let start = Instant::now();
    let result = generate_push_pack(&refs, None, &config).await;
    let elapsed = start.elapsed();

    let err = result.expect_err("oversized pack must be rejected");
    match err {
        CrabError::PackTooLarge { size, limit } => {
            assert_eq!(limit, 1, "limit must round-trip");
            assert!(size > 1, "reported size must exceed the limit");
        }
        other => panic!("expected PackTooLarge, got {other:?}"),
    }

    // Rejection must be measured in milliseconds, not minutes.
    // `INDEX_PACK_TIMEOUT` is 300s; budget 5s here so a loaded CI
    // machine still exits well under that bound.
    assert!(
        elapsed < Duration::from_secs(5),
        "rejection took {elapsed:?}; must be well under INDEX_PACK_TIMEOUT"
    );
}

/// `PushRejectReason::from_error` maps `PackTooLarge` to the structured
/// reject reason with both size and limit preserved. This is what the
/// remote helper surfaces to every ref in the batch.
#[test]
fn pack_too_large_error_maps_to_structured_reject_reason() {
    let err = CrabError::PackTooLarge {
        size: 3 * 1024 * 1024 * 1024,
        limit: 2 * 1024 * 1024 * 1024,
    };
    match PushRejectReason::from_error(&err) {
        PushRejectReason::PackTooLarge {
            size_bytes,
            limit_bytes,
        } => {
            assert_eq!(size_bytes, 3 * 1024 * 1024 * 1024);
            assert_eq!(limit_bytes, 2 * 1024 * 1024 * 1024);
        }
        other => panic!("expected PackTooLarge, got {other:?}"),
    }
}

/// The protocol tag and Display shape match what clients parse.
#[test]
fn pack_too_large_reject_reason_protocol_shape() {
    let reason = PushRejectReason::PackTooLarge {
        size_bytes: 100,
        limit_bytes: 50,
    };
    assert_eq!(reason.protocol_tag(), "pack-too-large");
    assert_eq!(
        reason.to_string(),
        "pack size 100 bytes exceeds 50 byte limit"
    );
}

/// A pack whose bytes fit under the limit is accepted. The spec also
/// requires that a pack exactly at the limit works — since `size
/// <= limit` is the check (`> limit` rejects), exact-equality lands.
#[tokio::test]
async fn pack_at_limit_succeeds() {
    let repo = Repo::new();
    let _git_dir = GitDirScope::new(&repo.git_dir);

    let refs = ref_for(&repo.commit_sha);

    // First, compute the pack size with the cap off. That value is
    // exactly the "at the limit" figure for the second run.
    let probe = generate_push_pack(
        &refs,
        None,
        &PushPackConfig {
            thin_packs: false,
            max_input_size: 0,
            git_dir: None,
        },
    )
    .await
    .expect("probe pack generation must succeed");
    let pack_size = probe.pack.len() as u64;
    assert!(pack_size > 0, "probe pack must be non-empty");

    let at_limit = generate_push_pack(
        &refs,
        None,
        &PushPackConfig {
            thin_packs: false,
            max_input_size: pack_size, // equal — not greater — so accepted
            git_dir: None,
        },
    )
    .await
    .expect("pack at exact limit must be accepted");
    assert_eq!(at_limit.pack.len() as u64, pack_size);
}

/// `max_input_size = 0` disables the cap. The fetch side passes `0`
/// for this reason; trusted internal repos with oversized legitimate
/// packs opt in by setting the config knob to zero.
///
/// Gated on the test runner being able to allocate a few hundred KB,
/// which is trivially below any practical CI budget but well above
/// the pack size from a single-commit repo.
#[tokio::test]
async fn zero_limit_means_unlimited() {
    let repo = Repo::new();
    let _git_dir = GitDirScope::new(&repo.git_dir);

    let refs = ref_for(&repo.commit_sha);
    let config = PushPackConfig {
        thin_packs: false,
        max_input_size: 0,
        git_dir: None,
    };

    let packed = generate_push_pack(&refs, None, &config).await;
    let ok = packed.expect("zero limit must disable the cap");
    assert!(!ok.pack.is_empty(), "pack must contain data");
}

/// `install_pack_locally` also enforces the size cap, preventing a
/// hostile client from burning the `INDEX_PACK_TIMEOUT` budget with
/// pre-generated oversized bytes. Symmetric to the generator-side
/// check in `generate_push_pack`.
#[tokio::test]
async fn install_pack_locally_rejects_oversized_bytes() {
    let dir = tempfile::tempdir().unwrap();
    // Any buffer over the limit triggers rejection before the invalid
    // pack header would be noticed by index-pack. Well-formedness
    // of the bytes is deliberately not tested here — the guard
    // rejects based on length alone.
    let oversized = vec![0u8; 128];

    let start = Instant::now();
    let result = install_pack_locally(dir.path(), &oversized, "bogus", 64).await;
    let elapsed = start.elapsed();

    match result {
        Err(CrabError::PackTooLarge { size, limit }) => {
            assert_eq!(size, 128);
            assert_eq!(limit, 64);
        }
        Ok(_) => panic!("install must reject oversized bytes"),
        Err(other) => panic!("expected PackTooLarge, got {other:?}"),
    }
    // The guard runs before any filesystem work, so it's effectively
    // instantaneous. 1s keeps the test stable on loaded CI.
    assert!(
        elapsed < Duration::from_secs(1),
        "install-side size check should be near-instant; took {elapsed:?}"
    );
}
