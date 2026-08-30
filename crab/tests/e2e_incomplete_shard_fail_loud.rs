//! Bug-exploration test: large-file cold-prefix push must either
//! produce reconstructible terms or abort loudly before publishing.
//!
//! This test is EXPECTED TO FAIL on unfixed code for chunk counts above
//! the observed boundary (5k / 22k / 27k). Failure confirms the bug;
//! a pass on those cases means either the fix already landed or the
//! reproducer drifted.
//!
//! The in-memory harness drives the real 14-step [`run_push_batch`]
//! pipeline end-to-end: a temp git repo with a staged pointer, a
//! populated on-disk staging area, and a recording wrapper around
//! `InMemory` that captures every PUT path so we can assert no durable
//! shard / metadata DB / manifest writes escape on the fail-loud path.

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
use proptest::prelude::*;
use rand::RngCore;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tokio_util::sync::CancellationToken;

use crab::cmd::hydrate::{Hydrator, ShardHydrator};
use crab::git::push::{PushConfig, run_push_batch};
use crab::git::remote_helper::PushSpec;
use crab::storage::StoreLayout;
use crab::storage::store::Store;
use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_staging::{StagingArea, StagingAreaReadOnly};
use crab_types::pointer::Pointer;
use crab_xet::chunker::GearChunker;
use crab_xet::xorb::format::MerkleHash;

// ---------------------------------------------------------------------------
// Recording object store
// ---------------------------------------------------------------------------

/// Wraps [`InMemory`] and records every PUT path into a shared log.
/// Tests use the log to assert that the fail-loud path leaves no
/// durable writes under the prefixes the bug would otherwise corrupt
/// (`.crab/shards/`, `file_index_db/`, `chunk_index_db/`, manifests, metadata).
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

    /// Snapshot the recorded PUT paths.
    fn puts(&self) -> Vec<String> {
        self.puts.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn clear_puts(&self) {
        self.puts.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
        // Multipart uploads complete when the upload object is `complete`d;
        // record the path here since there is no practical interception
        // point on the returned handle without reimplementing the trait.
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
// Git repo fixture
// ---------------------------------------------------------------------------

/// Process-wide mutex serialising `GIT_DIR` env-var manipulation.
/// Every test in this file runs under a `ScopedGitDir` guard.
static GIT_DIR_MUTEX: Mutex<()> = Mutex::new(());

/// RAII guard holding [`GIT_DIR_MUTEX`] and restoring `GIT_DIR` on
/// drop. Creation clears the ambient value so `git init` cannot be
/// redirected into a sibling test's fixture; call [`Self::set_git_dir`]
/// after the fixture's `.git` exists.
struct ScopedGitDir {
    _lock: MutexGuard<'static, ()>,
    prev: Option<String>,
}

impl ScopedGitDir {
    fn acquire() -> Self {
        let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GIT_DIR").ok();
        // SAFETY: access is serialised by GIT_DIR_MUTEX.
        unsafe { std::env::remove_var("GIT_DIR") };
        Self { _lock: lock, prev }
    }

    fn set_git_dir(&self, git_dir: &Path) {
        // SAFETY: access is serialised by GIT_DIR_MUTEX (held by self).
        unsafe { std::env::set_var("GIT_DIR", git_dir) };
    }
}

impl Drop for ScopedGitDir {
    fn drop(&mut self) {
        match &self.prev {
            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
            None => unsafe { std::env::remove_var("GIT_DIR") },
        }
    }
}

/// Temp git repo with a working tree ready to accept pointer commits.
struct GitFixture {
    dir: tempfile::TempDir,
}

impl GitFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        run_git(dir.path(), &["init", "--initial-branch=main"]);
        run_git(dir.path(), &["config", "user.email", "test@test.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
        Self { dir }
    }

    fn work_tree(&self) -> &Path {
        self.dir.path()
    }

    fn git_dir(&self) -> PathBuf {
        self.dir.path().join(".git")
    }

    /// Write a pointer-blob file under the working tree and commit it.
    fn commit_pointer(&self, name: &str, pointer: &Pointer) {
        std::fs::write(self.work_tree().join(name), pointer.serialize())
            .expect("write pointer blob");
        run_git(self.work_tree(), &["add", name]);
        run_git(self.work_tree(), &["commit", "-m", name]);
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    if !out.status.success() {
        let _ = writeln!(
            io::stderr(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        panic!("git command failed");
    }
}

// ---------------------------------------------------------------------------
// Content + staging helpers
// ---------------------------------------------------------------------------

/// Deterministic random content of the given byte length keyed by `seed`.
/// Uses a seeded RNG so every `(target_chunks, seed)` pair produces
/// the same sequence across runs.
fn generate_random_content(seed: u64, size_bytes: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size_bytes];
    rng.fill_bytes(&mut data);
    data
}

/// Estimate the source byte count needed to produce roughly `target`
/// CDC chunks at xet-core's default 64 KiB target chunk size. The
/// `1.03x` fudge compensates for CDC's variable-length chunks skewing
/// the count slightly above `size / 64 KiB`.
fn bytes_for_target_chunks(target_chunks: usize) -> usize {
    // 64 KiB average × target; extra 3% so realised chunk count lands
    // at or above `target` after CDC boundary variance.
    (target_chunks * 64 * 1024 * 103) / 100
}

/// Run the real CDC chunker over `content` and return the chunks plus
/// the file-hash the clean filter would have emitted. Identical to
/// what [`CleanSession::clean_file`](crab::git::clean::CleanSession)
/// computes for the same bytes.
fn chunk_and_hash(content: &[u8]) -> (Vec<(MerkleHash, Bytes)>, [u8; 32]) {
    let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();

    let mut chunker = GearChunker::new();
    let mut chunks: Vec<(MerkleHash, Bytes)> = Vec::new();
    // Feed in 128 KiB blocks to match what the clean filter does; CDC
    // boundaries are independent of feed granularity but this keeps
    // per-call allocation bounded.
    for block in content.chunks(128 * 1024) {
        for c in chunker.feed(block) {
            chunks.push((c.hash, c.data));
        }
    }
    if let Some(last) = chunker.finalize() {
        chunks.push((last.hash, last.data));
    }
    (chunks, file_hash)
}

fn publish_staged_recipe(
    staging: &StagingArea,
    path: &Path,
    file_hash: MerkleHash,
    file_size: u64,
    chunks: &[(MerkleHash, Bytes)],
) {
    let recipe_chunks = chunks
        .iter()
        .map(|(hash, data)| (*hash, data.len() as u64))
        .collect::<Vec<_>>();
    let recipe = FileRecipe::from_staged_chunks(
        ChunkingPolicyId::XetGearV1_64KiB,
        file_hash,
        file_size,
        &recipe_chunks,
    )
    .expect("build staged recipe");
    staging
        .publish_verified_recipe_lease(path, &recipe)
        .expect("publish staged recipe");
}

/// Populate a fresh on-disk staging area with `(file_hash, chunks)`
/// and reopen it read-only. Mirrors the batch-staging path the real
/// clean filter uses via `BackgroundChunkStager::checkpoint`.
async fn populate_staging(
    staging_root: &Path,
    file_hash: [u8; 32],
    chunks: &[(MerkleHash, Bytes)],
    file_size: u64,
) -> StagingAreaReadOnly {
    {
        let staging = StagingArea::open(staging_root.to_path_buf())
            .await
            .expect("open staging rw");

        let file_merkle = MerkleHash::from(file_hash);
        staging
            .pre_register_file(&file_merkle, file_size)
            .expect("pre-register");

        // Stage the chunks in small batches to match what the clean
        // filter does; `stage_chunks_batch`'s chunk_index_offset keeps
        // consecutive batches from colliding on the
        // `(file_hash, chunk_index)` UNIQUE constraint.
        const BATCH: usize = 512;
        let mut offset: u64 = 0;
        for group in chunks.chunks(BATCH) {
            let refs: Vec<(&MerkleHash, &[u8])> =
                group.iter().map(|(h, d)| (h, d.as_ref())).collect();
            staging
                .stage_chunks_batch(&refs, &file_merkle, offset)
                .await
                .expect("stage chunks batch");
            offset += group.len() as u64;
        }

        staging.flush_pending().await.expect("flush pending");
        publish_staged_recipe(
            &staging,
            Path::new("large.bin"),
            file_merkle,
            file_size,
            chunks,
        );
        staging.close().await.expect("close staging");
    }

    StagingAreaReadOnly::open(staging_root.to_path_buf())
        .await
        .expect("reopen staging read-only")
}

// ---------------------------------------------------------------------------
// Full push+hydrate harness
// ---------------------------------------------------------------------------

/// Result of driving a single file through push + hydrate.
struct PushOutcome {
    /// Original content bytes (for byte-identical comparison on Ok pushes).
    content: Vec<u8>,
    /// File hash the pointer blob committed to git.
    file_hash: [u8; 32],
    /// `None` on ok, `Some(message)` when the push reported an error
    /// for the ref. The message is the stringified [`CrabError`].
    push_error_message: Option<String>,
    /// Every PUT path recorded against the remote during the push.
    recorded_puts: Vec<String>,
}

/// Drive the full 14-step push pipeline for a single random file of
/// approximately `target_chunks` CDC chunks. Returns everything a caller
/// needs to assert on success (hydrate and compare bytes) or on
/// fail-loud (inspect the error and confirm no durable shard/metadata
/// writes happened).
async fn push_single_file(
    target_chunks: usize,
    seed: u64,
) -> (PushOutcome, RecordingStore, tempfile::TempDir) {
    let git_guard = ScopedGitDir::acquire();
    let fixture = GitFixture::new();
    git_guard.set_git_dir(&fixture.git_dir());

    // Generate content sized for the requested chunk count, CDC-chunk
    // it the way the real clean filter would, and build the pointer
    // the clean filter would have emitted.
    let content = generate_random_content(seed, bytes_for_target_chunks(target_chunks));
    let (chunks, file_hash) = chunk_and_hash(&content);
    assert!(
        !chunks.is_empty(),
        "CDC produced no chunks for {} bytes",
        content.len()
    );
    let pointer = Pointer {
        file_hash,
        size: content.len() as u64,
        shard_hint: None,
    };

    // Stage the chunks on disk under `.crab/staging` relative to the
    // work tree — the same layout `crab add` uses.
    let staging_root = fixture.work_tree().join(".crab").join("staging");
    let staging = populate_staging(&staging_root, file_hash, &chunks, content.len() as u64).await;

    // Commit the pointer blob so step 1's `walk_reachable` has
    // something to discover.
    fixture.commit_pointer("large.bin", &pointer);

    // In-memory remote with a PUT recorder.
    let recording = RecordingStore::new();
    let inner: Arc<dyn ObjectStore> = Arc::new(recording.clone());
    let store = Store::new(inner);

    let router = StoreLayout::new(store.clone(), SINGLE_PREFIX.to_owned());
    crab::core::remote_layout::initialize(&store, &router)
        .await
        .expect("initialize canonical layout");
    crab::cmd::init::create_initial_manifest(&store, &router, "refs/heads/main")
        .await
        .expect("initialize canonical manifest");
    recording.clear_puts();

    let specs = vec![PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    }];

    let result = run_push_batch(
        &specs,
        &PushConfig::default(),
        Some(store),
        None,
        Some(Arc::new(staging)),
        router,
        None,
        CancellationToken::new(),
        None,
    )
    .await;

    // `run_push_batch` reports failures per ref. Extract the structured
    // rejection for the only ref so the caller can branch on success
    // versus a loud error.
    let push_error_message =
        result
            .outcomes
            .get("refs/heads/main")
            .and_then(|outcome| match outcome {
                crab::git::push::RefPushOutcome::Ok => None,
                crab::git::push::RefPushOutcome::Rejected(reason) => Some(reason.to_string()),
            });

    let outcome = PushOutcome {
        content,
        file_hash,
        push_error_message,
        recorded_puts: recording.puts(),
    };

    // Keep the git fixture alive until the caller drops the tuple; the
    // staging SQLite file is under `fixture.dir`, and the `StoreLayout`
    // holds a clone of the store.
    (outcome, recording, fixture.dir)
}

/// Attempt to hydrate the pushed file from the same store. Returns
/// `Ok(bytes)` only if reconstruction succeeded end-to-end and the
/// blake3 hash of the reconstructed bytes matches the pointer; returns
/// `Err(description)` when hydrate refused to reconstruct (e.g. the
/// shard omits the file, so the output materializes as a pointer stub)
/// or the final hash verification failed.
async fn hydrate_pushed_file(
    recording: &RecordingStore,
    prefix: &str,
    file_hash: [u8; 32],
    expected_size: u64,
) -> std::result::Result<Vec<u8>, String> {
    // Rebuild a plain `Store` over the same in-memory backend — hydrate
    // must see every xorb / shard / metadata entry the push wrote,
    // so we reuse `recording.inner` instead of spinning up a new one.
    let store = Store::new(recording.inner.clone() as Arc<dyn ObjectStore>);

    let cache_dir = tempfile::tempdir().expect("tempdir for hydrate cache");
    let cache = Arc::new(crab::cache::LocalCache::new(cache_dir.path().to_path_buf()));
    let caching_store = crab_cache_store::CachingStore::new_with_local_cache(
        store.clone(),
        &crab::core::config::CacheConfig::default(),
        cache,
    )
    .expect("build caching store");

    let hydrator = ShardHydrator::new_from_cli_layout(
        caching_store,
        StoreLayout::new(store, prefix.to_owned()),
    )
    .expect("build shard hydrator");

    let ptr = Pointer {
        file_hash,
        size: expected_size,
        shard_hint: None,
    };

    let cancel = CancellationToken::new();
    let out_path = cache_dir.path().join("reconstructed.bin");
    let items = vec![(out_path.clone(), ptr)];

    let summary = hydrator
        .hydrate_batch(&items, &cancel, None)
        .await
        .map_err(|e| format!("hydrate_batch error: {e}"))?;

    if summary.failed > 0 || summary.hydrated != 1 {
        return Err(format!(
            "hydrate summary reports failure: hydrated={}, failed={}",
            summary.hydrated, summary.failed
        ));
    }

    let bytes = std::fs::read(&out_path).map_err(|e| format!("read output: {e}"))?;

    // A pointer stub (the bug's CLion-case symptom) parses as a
    // Pointer; flag it as an explicit failure separate from bytewise
    // hash-mismatch so the test can report what kind of short-circuit
    // hydrate took.
    if Pointer::parse(&bytes).is_ok() {
        return Err(format!(
            "hydrate produced a pointer stub of {} bytes instead of full content",
            bytes.len()
        ));
    }

    if bytes.len() != expected_size as usize {
        return Err(format!(
            "reconstructed length {} != expected {}",
            bytes.len(),
            expected_size
        ));
    }

    let got_hash = *blake3::hash(&bytes).as_bytes();
    if got_hash != file_hash {
        return Err("blake3 hash mismatch between reconstructed and original".to_owned());
    }

    Ok(bytes)
}

const SINGLE_PREFIX: &str = "bug-repro";
const BATCH_PREFIX: &str = "bug-repro-batch";

fn repo_prefix(prefix: &str) -> String {
    format!("{}/", prefix.trim_end_matches('/'))
}

/// Durable metadata paths that must stay untouched when
/// `IncompleteShardReconstruction` aborts before the commit boundary.
fn durable_writes(prefix: &str, puts: &[String]) -> Vec<String> {
    let repo = repo_prefix(prefix);
    let file_index_db = format!("{repo}file_index_db/");
    let repo_manifest = format!("{repo}manifest");
    let repo_manifests = format!("{repo}manifests/");
    let repo_metadata = format!("{repo}metadata/");

    puts.iter()
        .filter(|p| {
            p.starts_with(".crab/shards/")
                || p.starts_with(".crab/chunk_index_db/")
                || p.starts_with(&file_index_db)
                || p.as_str() == repo_manifest
                || p.starts_with(&repo_manifests)
                || p.starts_with(&repo_metadata)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Shared assertion logic
// ---------------------------------------------------------------------------

/// Core post-fix assertion: for any file (small or large), the push
/// either publishes reconstructible terms or aborts loudly before
/// writing anything durable.
///
/// Run this against the UNFIXED pipeline and it will fail for large
/// chunk counts — that failure is the bug-exploration evidence.
/// Against the FIXED pipeline it passes for every chunk count.
async fn assert_push_is_reconstructible_or_fails_loud(target_chunks: usize, seed: u64) {
    let (outcome, recording, _keepalive) = push_single_file(target_chunks, seed).await;

    match outcome.push_error_message {
        None => {
            // Push reported success. The fix check requires that
            // hydrate then reconstruct the file byte-identically.
            match hydrate_pushed_file(
                &recording,
                SINGLE_PREFIX,
                outcome.file_hash,
                outcome.content.len() as u64,
            )
            .await
            {
                Ok(bytes) => {
                    assert_eq!(
                        bytes, outcome.content,
                        "target_chunks={target_chunks}: hydrate returned non-identical bytes",
                    );
                }
                Err(err) => {
                    panic!(
                        "target_chunks={target_chunks}: push returned Ok but hydrate failed: {err}\n\
                         This is the data-loss path the fail-loud fix must close."
                    );
                }
            }
        }
        Some(msg) => {
            // Push aborted. Structured check: the only acceptable error
            // is IncompleteShardReconstruction, with at least one
            // uncovered chunk and a non-empty example chunk hash.
            assert!(
                msg.contains("CRAB-E0085"),
                "target_chunks={target_chunks}: push error is not IncompleteShardReconstruction: {msg}",
            );

            // Parse the structured fields out of the Display message.
            let uncovered = extract_uncovered_chunks(&msg);
            let example = extract_example_chunk_hash(&msg);
            assert!(
                uncovered.map(|n| n >= 1).unwrap_or(false),
                "target_chunks={target_chunks}: IncompleteShardReconstruction must report \
                 uncovered_chunks >= 1; message: {msg}",
            );
            assert!(
                example.map(|s| !s.is_empty()).unwrap_or(false),
                "target_chunks={target_chunks}: IncompleteShardReconstruction must report a \
                 non-empty example_chunk_hash; message: {msg}",
            );

            let leaks = durable_writes(SINGLE_PREFIX, &outcome.recorded_puts);
            assert!(
                leaks.is_empty(),
                "target_chunks={target_chunks}: push aborted with IncompleteShardReconstruction \
                 but left durable writes on the remote: {leaks:?}",
            );
        }
    }
}

/// Extract `has {N} unresolved chunk(s)` from the error's Display.
fn extract_uncovered_chunks(msg: &str) -> Option<usize> {
    let marker = " has ";
    let start = msg.find(marker)? + marker.len();
    let rest = &msg[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// Extract `(chunk {hex})` from the error's Display; returns the hex body.
fn extract_example_chunk_hash(msg: &str) -> Option<String> {
    let marker = "(chunk ";
    let start = msg.find(marker)? + marker.len();
    let rest = &msg[start..];
    let end = rest.find(')')?;
    Some(rest[..end].to_owned())
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

/// Env gate for the multi-GiB chunk-count cases. Pushing a 1.4 GiB+
/// random file through the full pipeline takes minutes even in memory;
/// default CI runs skip these and only the fast 2k / 5k cases execute.
fn run_large_chunk_cases() -> bool {
    std::env::var("CRAB_RUN_LARGE_CHUNK_TESTS").map_or(false, |v| v == "1")
}

/// Counter-case from the verify-ml-clean-room matrix: a file whose CDC
/// chunk count lands well below the observed bug boundary must push
/// and hydrate byte-identically. Today this already passes; it stays
/// here as a regression guard when the fail-loud fix lands.
#[tokio::test(flavor = "multi_thread")]
async fn push_reconstructible_at_2k_chunks() {
    assert_push_is_reconstructible_or_fails_loud(2_000, 0x2c0_0000).await;
}

/// Just past the observed ok/fail boundary (~4,000 chunks ok, ~22,000
/// chunks fail). On unfixed code this reproduces the silent-drop bug
/// and the test fails. On fixed code the push either succeeds with
/// byte-identical hydrate or aborts loudly with E0085.
#[tokio::test(flavor = "multi_thread")]
async fn push_reconstructible_at_5k_chunks() {
    assert_push_is_reconstructible_or_fails_loud(5_000, 0x5c0_0000).await;
}

/// Matches the observed idea-2025.3.4-aarch64.dmg field case.
/// Gated behind `CRAB_RUN_LARGE_CHUNK_TESTS=1` because generating
/// ~1.4 GiB of random content in-process is too slow for default CI.
#[tokio::test(flavor = "multi_thread")]
async fn push_reconstructible_at_22k_chunks() {
    if !run_large_chunk_cases() {
        eprintln!("skipping 22k-chunk repro; set CRAB_RUN_LARGE_CHUNK_TESTS=1 to enable");
        return;
    }
    assert_push_is_reconstructible_or_fails_loud(22_000, 0x22c_0000).await;
}

/// Matches the observed CLion-2026.1-aarch64.dmg field case, where
/// hydrate materialized only a pointer stub — the shard omitted the
/// file's MDBFileInfo entirely. Gated behind
/// `CRAB_RUN_LARGE_CHUNK_TESTS=1`.
#[tokio::test(flavor = "multi_thread")]
async fn push_reconstructible_at_27k_chunks() {
    if !run_large_chunk_cases() {
        eprintln!("skipping 27k-chunk repro; set CRAB_RUN_LARGE_CHUNK_TESTS=1 to enable");
        return;
    }
    assert_push_is_reconstructible_or_fails_loud(27_000, 0x27c_0000).await;
}

// ---------------------------------------------------------------------------
// Multi-file regression
// ---------------------------------------------------------------------------

/// Mirrors the verify-ml-clean-room batch: five files with chunk
/// counts spanning below and above the bug boundary, pushed in one
/// operation. The per-file boundary is independent, so a fixed pipeline
/// must either reconstruct all five or abort loudly without leaving
/// durable writes. Gated — the two largest files dominate the runtime.
#[tokio::test(flavor = "multi_thread")]
async fn push_batch_of_five_files_is_reconstructible_or_fails_loud() {
    if !run_large_chunk_cases() {
        eprintln!("skipping 5-file batch repro; set CRAB_RUN_LARGE_CHUNK_TESTS=1 to enable");
        return;
    }

    let git_guard = ScopedGitDir::acquire();
    let fixture = GitFixture::new();
    git_guard.set_git_dir(&fixture.git_dir());

    // Five sizes spanning below and above the observed boundary.
    let targets: &[(usize, u64, &str)] = &[
        (100, 0x100_0000, "tiny.bin"),
        (2_000, 0x200_0000, "small.bin"),
        (5_000, 0x500_0000, "medium.bin"),
        (22_000, 0x220_0000, "large.bin"),
        (27_000, 0x270_0000, "xlarge.bin"),
    ];

    // Generate, chunk, and stage each file up front; staging is shared
    // across all five pointers so a single push picks them all up.
    let staging_root = fixture.work_tree().join(".crab").join("staging");
    let mut files: Vec<(String, [u8; 32], Vec<u8>)> = Vec::new();

    {
        let staging = StagingArea::open(staging_root.clone())
            .await
            .expect("open staging rw");

        for (target_chunks, seed, name) in targets {
            let content = generate_random_content(*seed, bytes_for_target_chunks(*target_chunks));
            let (chunks, file_hash) = chunk_and_hash(&content);
            let file_merkle = MerkleHash::from(file_hash);

            staging
                .pre_register_file(&file_merkle, content.len() as u64)
                .expect("pre-register");

            const BATCH: usize = 512;
            let mut offset: u64 = 0;
            for group in chunks.chunks(BATCH) {
                let refs: Vec<(&MerkleHash, &[u8])> =
                    group.iter().map(|(h, d)| (h, d.as_ref())).collect();
                staging
                    .stage_chunks_batch(&refs, &file_merkle, offset)
                    .await
                    .expect("stage chunks batch");
                offset += group.len() as u64;
            }

            publish_staged_recipe(
                &staging,
                Path::new(name),
                file_merkle,
                content.len() as u64,
                &chunks,
            );

            files.push(((*name).to_owned(), file_hash, content));
        }

        staging.flush_pending().await.expect("flush pending");
        staging.close().await.expect("close staging");
    }

    let staging = StagingAreaReadOnly::open(staging_root)
        .await
        .expect("reopen staging ro");

    // Commit all five pointer blobs in one commit.
    for (name, file_hash, content) in &files {
        let pointer = Pointer {
            file_hash: *file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };
        std::fs::write(fixture.work_tree().join(name), pointer.serialize()).expect("write pointer");
        run_git(fixture.work_tree(), &["add", name]);
    }
    run_git(fixture.work_tree(), &["commit", "-m", "batch"]);

    let recording = RecordingStore::new();
    let inner: Arc<dyn ObjectStore> = Arc::new(recording.clone());
    let store = Store::new(inner);
    let router = StoreLayout::new(store.clone(), BATCH_PREFIX.to_owned());
    crab::core::remote_layout::initialize(&store, &router)
        .await
        .expect("initialize canonical layout");
    crab::cmd::init::create_initial_manifest(&store, &router, "refs/heads/main")
        .await
        .expect("initialize canonical manifest");
    recording.clear_puts();

    let specs = vec![PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    }];

    let result = run_push_batch(
        &specs,
        &PushConfig::default(),
        Some(store),
        None,
        Some(Arc::new(staging)),
        router,
        None,
        CancellationToken::new(),
        None,
    )
    .await;

    let outcome_msg = result
        .outcomes
        .get("refs/heads/main")
        .and_then(|o| match o {
            crab::git::push::RefPushOutcome::Ok => None,
            crab::git::push::RefPushOutcome::Rejected(reason) => Some(reason.to_string()),
        });

    match outcome_msg {
        None => {
            // Push ok — every file must hydrate byte-identically.
            for (name, file_hash, content) in &files {
                match hydrate_pushed_file(
                    &recording,
                    BATCH_PREFIX,
                    *file_hash,
                    content.len() as u64,
                )
                .await
                {
                    Ok(bytes) => {
                        assert_eq!(&bytes, content, "file {name} hydrated non-identical");
                    }
                    Err(err) => {
                        panic!(
                            "file {name}: push ok but hydrate failed: {err}\n\
                             This is the data-loss path the fail-loud fix must close."
                        );
                    }
                }
            }
        }
        Some(msg) => {
            assert!(
                msg.contains("CRAB-E0085"),
                "batch push error is not IncompleteShardReconstruction: {msg}",
            );
            let leaks = durable_writes(BATCH_PREFIX, &recording.puts());
            assert!(
                leaks.is_empty(),
                "batch push aborted with IncompleteShardReconstruction but left \
                 durable writes: {leaks:?}",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Scoped property test
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        // One case per chunk-count sample by default; proptest's shrinker
        // can't meaningfully reduce a random-content file, so the value
        // is in coverage across sample points rather than shrink depth.
        cases: 4,
        // Don't persist regressions under `tests/` — the cases here are
        // already deterministic via the seed.
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    /// Scoped PBT over `(target_chunk_count, seed)`. The chunk counts
    /// are the same sample points the verify-ml-clean-room run stress
    /// tested in the field (2k below boundary, 5k/22k/27k above).
    /// Large-chunk cases are skipped unless
    /// `CRAB_RUN_LARGE_CHUNK_TESTS=1`.
    #[test]
    fn pbt_push_reconstructible_or_fails_loud(
        (target_chunks, seed) in (
            prop::sample::select(vec![2_000usize, 5_000, 22_000, 27_000]),
            any::<u64>(),
        ),
    ) {
        // Gate expensive cases. `prop_assume!` is not available on the
        // stable macro without the feature; a plain early-return keeps
        // the test cheap when the env var is unset.
        if target_chunks >= 10_000 && !run_large_chunk_cases() {
            return Ok(());
        }

        // proptest requires a sync harness; spin up a private runtime.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        rt.block_on(async move {
            assert_push_is_reconstructible_or_fails_loud(target_chunks, seed).await;
        });
    }
}
