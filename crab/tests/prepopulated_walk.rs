//! Integration tests for Commit 1 (wire incremental walk) of the
//! `crab-push-perf-tier2` spec.
//!
//! Exercises three scenarios against a real on-disk git repository plus
//! an `InMemory` object store:
//!
//! 1. Second `run_native_push` reuses the pre-populated walk
//!    (`incremental = true, source = "native"`).
//! 2. Stale `PushState` — old SHA not present in the ODB — falls back
//!    to a full walk via the `reason = "unresolvable_old_sha"` branch
//!    of `phase_discover`.
//! 3. Non-native `run_push_batch` path walks the full graph
//!    (`incremental = false, source = "walk_reachable"`) when no
//!    pre-populated walk is installed.
//!
//! Tracing capture uses a dependency-free `tracing_subscriber::fmt`
//! subscriber backed by a shared `Vec<u8>` buffer. The subscriber is
//! attached to each test future via `WithSubscriber` so the capture
//! survives `.await` points even when tokio's current-thread scheduler
//! rotates across worker threads.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

use object_store::memory::InMemory;
use tokio_util::sync::CancellationToken;
use tracing::dispatcher::Dispatch;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;

use crab::core::metrics::Metrics;
use crab::git::push::{PushConfig, run_push_batch};
use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
use crab::git::push_state::PushState;
use crab::git::remote_helper::PushSpec;
use crab::storage::StoreLayout;
use crab::storage::store::Store;
use crab_types::pointer::Pointer;

// ---------------------------------------------------------------------------
// Tracing capture helpers
// ---------------------------------------------------------------------------

/// A `MakeWriter` that tees every span / event rendering into a shared
/// `Vec<u8>`. Tests read the buffer after the traced future resolves.
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

/// Render the captured buffer as UTF-8, replacing invalid bytes so the
/// test output is always readable when an assertion fires.
fn captured_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&guard).into_owned()
}

/// Build a dispatcher that renders every span close + event into the
/// provided buffer. Uses `FmtSpan::CLOSE` so late-recorded span fields
/// (e.g. `span.record("incremental", true)`) appear in the output.
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
// Git repository fixture
// ---------------------------------------------------------------------------

/// Process-wide mutex that serialises `GIT_DIR` env-var manipulation so
/// parallel integration tests in this file don't clobber each other.
static GIT_DIR_MUTEX: Mutex<()> = Mutex::new(());

/// RAII guard that points `GIT_DIR` at the given directory for the
/// lifetime of the guard, restoring any previous value on drop.
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

/// Owns a tempdir containing a real git working tree plus a `.git`
/// subdirectory. Methods mutate the working tree and commit via the
/// user's `git` binary — the same tool `phase_discover` and `run_push_batch`
/// rely on for `rev-parse` and ODB reads.
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

    /// Write a crab pointer blob to `name` and commit it with
    /// `message`. Returns the resulting commit SHA.
    fn commit_pointer(&self, name: &str, seed: u8, message: &str) -> String {
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

        Self::run_git(self.work_tree(), &["add", name]);
        Self::run_git(self.work_tree(), &["commit", "-m", message]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(self.work_tree())
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

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
    crab::cmd::init::create_initial_manifest(store, router, "refs/heads/main")
        .await
        .expect("initialize canonical remote manifest");
}

fn make_specs() -> Vec<PushSpec> {
    vec![PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    }]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Task 1.9 — a second native push with a pre-populated walk reports
/// `incremental = true, source = "native"` and hands a non-empty pointer
/// set to the delegated pipeline.
#[tokio::test]
async fn second_native_push_reuses_prepopulated_walk() {
    let fixture = GitFixture::new();
    let _git = ScopedGitDir::new(&fixture.git_dir());

    // First commit seeds the baseline for `PushState`.
    let first_sha = fixture.commit_pointer("a.bin", 0x10, "first pointer");
    // Second commit is what the incremental walk must discover.
    let second_sha = fixture.commit_pointer("b.bin", 0x20, "second pointer");
    assert_ne!(first_sha, second_sha, "second commit must advance HEAD");

    // Seed `PushState` so `phase_discover` picks the incremental branch
    // on its very first invocation here — equivalent to the "second
    // push" scenario described in the spec but without needing the
    // first push to complete end-to-end.
    let mut push_state = PushState::default();
    push_state.set("crab://bucket/repo", "refs/heads/main", &first_sha);

    let store = make_store();
    let router = make_router(store.clone(), "repo-prefix");
    initialize_remote(&store, &router).await;
    let config = NativePushConfig::new(PushConfig::default());
    let cancel = CancellationToken::new();
    let metrics: Option<Arc<Metrics>> = None;

    let (buf_maker, buf) = BufferMaker::new();
    let dispatch = capture_dispatch(buf_maker);

    let _ = async {
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

    let trace = captured_text(&buf);

    // `phase_discover` must record `reason = "incremental_ok"` for the
    // ref because the pre-seeded SHA is present in the ODB.
    assert!(
        trace.contains("reason=\"incremental_ok\""),
        "phase_discover should take the incremental branch; trace was:\n{trace}"
    );

    // The delegated pipeline's `push.enumerate_pointers` span must fire
    // with the pre-populated source and a non-empty pointer count.
    assert!(
        trace.contains("push.enumerate_pointers"),
        "push.enumerate_pointers span missing; trace was:\n{trace}"
    );
    assert!(
        trace.contains("incremental=true"),
        "span must record incremental=true; trace was:\n{trace}"
    );
    assert!(
        trace.contains("source=\"native\""),
        "span must record source=\"native\"; trace was:\n{trace}"
    );

    // `install_prepopulated_walk` logs `pointers = N` at debug level.
    // The value must be non-zero — proving the incremental walk
    // discovered the second commit's pointer blob.
    let install_pointers = extract_field_after(&trace, "installing pre-populated walk", "pointers")
        .expect("install_prepopulated_walk debug event missing");
    assert!(
        install_pointers >= 1,
        "pre-populated walk must carry the new commit's pointer; got pointers={install_pointers}\nTrace:\n{trace}"
    );

    // The step-1 "using pre-computed walk" event mirrors the installed
    // counts once `enumerate_pointers` adopts them.
    let step1_pointers = extract_field_after(&trace, "using pre-computed walk", "pointers")
        .expect("step 1 pre-computed walk event missing");
    assert_eq!(
        install_pointers, step1_pointers,
        "step 1 must adopt exactly the installed pointer count"
    );
}

/// Task 1.10 — an unresolvable old SHA in `PushState` must flip
/// `phase_discover` into the fallback path and emit
/// `reason = "unresolvable_old_sha"`.
#[tokio::test]
async fn stale_push_state_falls_back_to_full_walk() {
    let fixture = GitFixture::new();
    let _git = ScopedGitDir::new(&fixture.git_dir());

    let _commit_sha = fixture.commit_pointer("a.bin", 0x30, "only pointer");

    // Seed a SHA that is syntactically valid but guaranteed not to
    // exist in the local ODB. `gix-odb` treats the all-zero SHA as
    // the empty/unknown object; combined with `odb.exists` returning
    // false, this exercises the fallback branch.
    let mut push_state = PushState::default();
    push_state.set(
        "crab://bucket/repo",
        "refs/heads/main",
        "0000000000000000000000000000000000000000",
    );

    let store = make_store();
    let router = make_router(store.clone(), "repo-prefix");
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

    // The call must complete without surfacing an error up the stack —
    // the individual ref may or may not succeed end-to-end, but the
    // orchestrator itself returns `Ok(PushResult)`.
    result.expect("run_native_push must succeed end-to-end on InMemory store");

    let trace = captured_text(&buf);

    assert!(
        trace.contains("reason=\"unresolvable_old_sha\""),
        "phase_discover must emit reason=\"unresolvable_old_sha\"; trace was:\n{trace}"
    );
}

/// Task 1.11 — `run_push_batch` (non-native path) has no pre-populated
/// walk, so `push.enumerate_pointers` must walk the full graph and
/// record `incremental = false, source = "walk_reachable"`.
#[tokio::test]
async fn non_native_push_batch_walks_full_graph() {
    let fixture = GitFixture::new();
    let _git = ScopedGitDir::new(&fixture.git_dir());

    let _commit_sha = fixture.commit_pointer("a.bin", 0x40, "first pointer");

    let store = make_store();
    let router = make_router(store.clone(), "repo-prefix");
    initialize_remote(&store, &router).await;
    let config = PushConfig::default();
    let cancel = CancellationToken::new();

    let (buf_maker, buf) = BufferMaker::new();
    let dispatch = capture_dispatch(buf_maker);

    let _ = async {
        run_push_batch(
            &make_specs(),
            &config,
            Some(store),
            None,
            None,
            router,
            None,
            cancel,
            None,
        )
        .await
    }
    .with_subscriber(dispatch)
    .await;

    let trace = captured_text(&buf);

    assert!(
        trace.contains("push.enumerate_pointers"),
        "push.enumerate_pointers span missing; trace was:\n{trace}"
    );
    assert!(
        trace.contains("incremental=false"),
        "span must record incremental=false on the non-native path; trace was:\n{trace}"
    );
    assert!(
        trace.contains("source=\"walk_reachable\""),
        "span must record source=\"walk_reachable\"; trace was:\n{trace}"
    );
}

// ---------------------------------------------------------------------------
// Trace parsing helpers
// ---------------------------------------------------------------------------

/// Find the first line of `trace` that contains `anchor`, then return
/// the integer value of `field=...` inside that line. The default
/// `tracing_subscriber::fmt` writer renders structured fields as
/// `key=value` tokens separated by spaces, so a simple string search
/// is sufficient.
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
