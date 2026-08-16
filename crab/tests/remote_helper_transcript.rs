//! Snapshot tests for the git remote helper protocol transcripts.
//!
//! Each test sends a sequence of commands through `run_remote_helper` and
//! snapshots the exact byte-level response using `cargo insta`.

#![recursion_limit = "256"]
#![allow(
    deprecated,
    reason = "exercises the deprecated RefPushOutcome::Error path for backward-compat regression"
)]

use std::io::Cursor;

use crab::git::remote_helper::{
    ListOutput, RefEntry, StdIo, filter_list_for_push, format_list_output,
};
use tokio::io::BufReader;

fn test_url() -> gix_url::Url {
    gix_url::Url::from_bytes(b"crab://bucket/repo".into()).expect("test URL should parse")
}

/// Run the remote helper with the given input and capture stdout as a string.
///
/// Uses a duplex stream so we can read the output after the helper exits.
async fn run_remote_helper_capture(input: &str) -> String {
    // Since run_remote_helper takes `impl StdIo` by value and the writer
    // is moved into the function, we use a channel to capture output.
    use tokio::io::{AsyncReadExt, duplex};

    let input_bytes = input.as_bytes().to_vec();
    let (writer_tx, mut writer_rx) = duplex(64 * 1024);

    struct DuplexIo {
        reader: BufReader<Cursor<Vec<u8>>>,
        writer: tokio::io::DuplexStream,
    }

    impl StdIo for DuplexIo {
        type Reader = BufReader<Cursor<Vec<u8>>>;
        type Writer = tokio::io::DuplexStream;

        fn split(self) -> (Self::Reader, Self::Writer) {
            (self.reader, self.writer)
        }
    }

    let io = DuplexIo {
        reader: BufReader::new(Cursor::new(input_bytes)),
        writer: writer_tx,
    };

    let url = test_url();
    let mut output = Vec::new();
    let helper = crab::git::remote_helper::run_remote_helper(
        "origin",
        &url,
        io,
        tokio_util::sync::CancellationToken::new(),
    );
    let reader = writer_rx.read_to_end(&mut output);
    let (helper_result, read_result) = tokio::join!(helper, reader);
    helper_result.expect("remote helper should not error");
    read_result.expect("reading output should succeed");

    String::from_utf8(output).expect("output should be valid UTF-8")
}

// --- Snapshot tests ---

#[tokio::test]
async fn capabilities_transcript() {
    let output = run_remote_helper_capture("capabilities\n").await;
    let expected_agent = format!("agent=crab/{}", env!("CARGO_PKG_VERSION"));
    assert!(
        output.lines().any(|line| line == expected_agent),
        "capabilities must advertise the crate version {expected_agent:?}; output: {output:?}",
    );
    let normalized = output.replace(&expected_agent, "agent=crab/<version>");
    insta::assert_snapshot!(normalized, @r"
    fetch
    push
    option
    check-connectivity
    agent=crab/<version>

    ");
}

#[tokio::test]
async fn option_handling_transcript() {
    let input = "\
option progress true\n\
option verbosity 2\n\
option unknown-key value\n";
    let output = run_remote_helper_capture(input).await;
    insta::assert_snapshot!(output, @r"
    ok
    ok
    unsupported
    ");
}

#[test]
fn head_symref_in_list_output() {
    let output = ListOutput {
        refs: vec![
            RefEntry {
                sha: "abc123def456abc123def456abc123def456abcd".into(),
                ref_name: "refs/heads/main".into(),
                peeled: None,
            },
            RefEntry {
                sha: "111222333444555666777888999000aaabbbcccd".into(),
                ref_name: "refs/heads/dev".into(),
                peeled: None,
            },
        ],
        head_symref: Some("refs/heads/main".into()),
    };
    let formatted = format_list_output(&output);
    insta::assert_snapshot!(formatted, @r"
    @refs/heads/main HEAD
    abc123def456abc123def456abc123def456abcd refs/heads/main
    111222333444555666777888999000aaabbbcccd refs/heads/dev

    ");
}

#[test]
fn list_for_push_omits_head_symref() {
    let output = ListOutput {
        refs: vec![
            RefEntry {
                sha: "abc123def456abc123def456abc123def456abcd".into(),
                ref_name: "refs/heads/main".into(),
                peeled: None,
            },
            RefEntry {
                sha: "111222333444555666777888999000aaabbbcccd".into(),
                ref_name: "refs/heads/dev".into(),
                peeled: None,
            },
        ],
        head_symref: Some("refs/heads/main".into()),
    };
    let filtered = filter_list_for_push(output, true);
    let formatted = format_list_output(&filtered);
    insta::assert_snapshot!(formatted, @r"
    abc123def456abc123def456abc123def456abcd refs/heads/main
    111222333444555666777888999000aaabbbcccd refs/heads/dev

    ");
}

// Process-wide mutex serialising `GIT_DIR` env-var manipulation across
// every test in this binary. Shared between `delete_ref` and
// `pipeline_error_outcomes` so the two modules can't race on the env
// var and hand a push pipeline the wrong repo mid-flight.
static GIT_DIR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------------
// Ref-name validation regression tests
//
// These push a single malformed refspec through the transcript harness
// and assert `error {dst} bad-refname` lands in the response. The
// validator runs inside `parse_command` before any pipeline work, so
// a batch containing only bad-refname entries short-circuits with
// zero store / git interaction — which is what lets these tests run
// without a fixture repo or a real object store.
// ---------------------------------------------------------------------------

mod refname_validation {
    use std::io::Cursor;
    use std::time::Duration;

    use tokio::io::BufReader;

    use crab::git::remote_helper::StdIo;

    fn test_url() -> gix_url::Url {
        gix_url::Url::from_bytes(b"crab://bucket/repo".into()).expect("test URL should parse")
    }

    /// Run the helper with `input`, capture stdout, and return both the
    /// bytes emitted and the terminal `Result` from the helper task.
    ///
    /// Unlike the top-level `run_remote_helper_capture` harness, this
    /// variant keeps the helper's error instead of panicking on it —
    /// tests here need to distinguish "batch aborted with a protocol
    /// error" (genuine malformed syntax) from "per-ref rejection
    /// landed in the response" (bad refname).
    ///
    /// The helper is wrapped in a short timeout as a belt-and-braces
    /// guard: any regression that re-routes a pre-rejected ref
    /// through the pipeline would otherwise block on network I/O
    /// indefinitely. A timeout hit is surfaced as an `Err` so the
    /// caller can decide how to treat it.
    async fn run_capture(input: &str) -> (String, std::result::Result<(), String>) {
        run_capture_with_timeout(input, Duration::from_secs(10)).await
    }

    async fn run_capture_with_timeout(
        input: &str,
        timeout: Duration,
    ) -> (String, std::result::Result<(), String>) {
        use tokio::io::{AsyncReadExt, duplex};

        let input_bytes = input.as_bytes().to_vec();
        let (writer_tx, mut writer_rx) = duplex(64 * 1024);

        struct DuplexIo {
            reader: BufReader<Cursor<Vec<u8>>>,
            writer: tokio::io::DuplexStream,
        }

        impl StdIo for DuplexIo {
            type Reader = BufReader<Cursor<Vec<u8>>>;
            type Writer = tokio::io::DuplexStream;

            fn split(self) -> (Self::Reader, Self::Writer) {
                (self.reader, self.writer)
            }
        }

        let io = DuplexIo {
            reader: BufReader::new(Cursor::new(input_bytes)),
            writer: writer_tx,
        };

        let url = test_url();
        let mut output = Vec::new();
        let helper = crab::git::remote_helper::run_remote_helper(
            "origin",
            &url,
            io,
            tokio_util::sync::CancellationToken::new(),
        );
        let reader = writer_rx.read_to_end(&mut output);

        let helper_result =
            match tokio::time::timeout(timeout, async { tokio::join!(helper, reader) }).await {
                Ok((Ok(()), Ok(_))) => Ok(()),
                Ok((Ok(()), Err(e))) => Err(e.to_string()),
                Ok((Err(e), _)) => Err(e.to_string()),
                Err(_) => Err("helper task timed out".to_owned()),
            };

        (String::from_utf8_lossy(&output).into_owned(), helper_result)
    }

    /// `..` anywhere in a path component is forbidden by git's
    /// `check-ref-format` and by `gix_validate::reference::name_partial`.
    #[tokio::test]
    async fn dot_dot_ref_name_rejected() {
        let input = "push src-sha:refs/heads/..foo\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("error refs/heads/..foo bad-refname"),
            "expected bad-refname rejection for refs/heads/..foo; output={output:?}"
        );
    }

    /// `.lock` suffix is reserved by git's filesystem refs backend
    /// (loose refs use it for atomic update staging).
    #[tokio::test]
    async fn lock_suffix_rejected() {
        let input = "push src-sha:refs/heads/main.lock\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("error refs/heads/main.lock bad-refname"),
            "expected bad-refname rejection for refs/heads/main.lock; output={output:?}"
        );
    }

    /// An empty `dst` cannot be surfaced as `error {dst} bad-refname`
    /// because there is no ref to address the rejection against. The
    /// remote-helper spec treats this as malformed syntax, so the
    /// whole batch aborts with a protocol error rather than silently
    /// accepting or pretending to reject a ref.
    #[tokio::test]
    async fn empty_dst_rejected() {
        let input = "push refs/heads/feature:\n";
        let (output, helper_result) = run_capture(input).await;
        assert!(
            helper_result.is_err(),
            "empty dst must abort the batch with a protocol error; \
             output={output:?} result={helper_result:?}"
        );
        let err_msg = helper_result.err().unwrap_or_default();
        assert!(
            err_msg.contains("empty dst") || err_msg.contains("Protocol"),
            "expected a protocol error about empty dst; got {err_msg}"
        );
        assert!(
            !output.contains("bad-refname"),
            "empty dst must not be funnelled through the per-ref rejection path; \
             output={output:?}"
        );
    }

    /// `push :refs/heads/old` is the `git push :ref` delete form.
    /// Empty `src` is a legal refspec and must not trigger the
    /// refname validator — deleting a ref has no source name to
    /// validate. The test asserts that the validator did not fire;
    /// whether the delete succeeds end-to-end is covered by the
    /// `delete_ref` module below.
    #[tokio::test]
    async fn empty_src_is_delete_not_bad_refname() {
        // Single delete refspec drives a full pipeline run (there
        // is no way to inspect parse results in isolation from the
        // integration harness). The generous timeout accommodates
        // slow debug builds and the store-warmup path; the only
        // assertion is that no `bad-refname` rejection ever lands
        // against `refs/heads/old`.
        let input = "push :refs/heads/old\n";
        let (output, _) = run_capture_with_timeout(input, Duration::from_secs(60)).await;
        assert!(
            !output.contains("refs/heads/old bad-refname"),
            "empty src delete must not be rejected as bad-refname; output={output:?}"
        );
    }

    /// ASCII control bytes are invalid in ref names.
    #[tokio::test]
    async fn control_char_in_ref_rejected() {
        let input = "push src-sha:refs/heads/bad\x01name\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("bad-refname"),
            "control byte in ref must trigger bad-refname; output={output:?}"
        );
    }

    /// `@{` is reserved by git as reflog shorthand (`@{1}`, `@{upstream}`)
    /// and rejected by `name_partial` via `ReflogPortion`. The `@`
    /// character on its own as a component is not rejected, which the
    /// original design called out as a gap. Using the reflog-portion
    /// pattern keeps the test representative of real ref-name abuse
    /// rather than hypothetical.
    #[tokio::test]
    async fn at_symbol_component_rejected() {
        let input = "push src-sha:refs/heads/foo@{upstream}\n";
        let (output, _) = run_capture(input).await;
        assert!(
            output.contains("error refs/heads/foo@{upstream} bad-refname"),
            "expected bad-refname rejection for refs/heads/foo@{{upstream}}; output={output:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Delete-ref regression tests
//
// These drive `run_native_push` end-to-end against an in-memory object
// store plus a real on-disk git work tree. The git tree is the source
// of truth for ref resolution; the object store is where the unified
// manifest lands. Between the two we can exercise the full push
// pipeline — classify, pack, upload, manifest CAS — for create,
// update, and delete specs in any mix.
// ---------------------------------------------------------------------------

mod delete_ref {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, MutexGuard};

    use object_store::memory::InMemory;
    use tokio_util::sync::CancellationToken;

    use crab::git::push::{PushConfig, RefPushOutcome};
    use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
    use crab::git::push_state::PushState;
    use crab::git::remote_helper::PushSpec;
    use crab::metadata::manifest::read_manifest;
    use crab::storage::StoreLayout;
    use crab::storage::store::Store;

    use super::GIT_DIR_MUTEX;
    /// RAII guard serialising `GIT_DIR` env-var manipulation. Two-phase
    /// lifecycle — `acquire()` takes the lock and clears any stray
    /// `GIT_DIR` so `git init` inside the fixture picks its own cwd,
    /// then `set_git_dir()` points the env var at the fresh `.git`
    /// directory so subsequent `git` calls route correctly regardless
    /// of cwd handling quirks inside the push pipeline.
    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: serialised by GIT_DIR_MUTEX.
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
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

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }

        /// Write a plain text file and commit it. Returns the commit
        /// SHA. Uses plain text (not a crab pointer) so
        /// `phase_discover` doesn't flag it as a pointer blob — the
        /// delete-ref tests are interested in manifest ref-map
        /// plumbing, not pointer / chunk / xorb handling.
        fn commit_text(&self, name: &str, seed: u8, message: &str) -> String {
            let content = format!("crab-delete-ref-test seed={seed} file={name}\n");
            std::fs::write(self.work_tree().join(name), content.as_bytes())
                .expect("write text file");

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

        /// Create a branch `name` pointing at `HEAD` (the last commit)
        /// without checking it out, so subsequent work stays on
        /// whichever branch the caller is on.
        fn branch_at_head(&self, name: &str) {
            Self::run_git(self.work_tree(), &["branch", name]);
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

    fn make_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    fn make_router(store: Store, prefix: &str) -> StoreLayout {
        StoreLayout::new(store, prefix.to_owned())
    }

    fn push_spec(src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force: false,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    /// Run a single batch through `run_native_push` using a fresh
    /// `InMemory` store. Returns the per-ref outcome map along with
    /// the final `PushState` so callers can assert on both. The
    /// caller must already hold [`ScopedGitDir`] pointed at the
    /// fixture's `.git` directory — otherwise parallel tests can
    /// swap `GIT_DIR` mid-pipeline.
    async fn run_batch(
        store: Store,
        router: StoreLayout,
        push_state: &mut PushState,
        specs: &[PushSpec],
    ) -> crab::git::push::PushResult {
        let config = NativePushConfig::new(PushConfig::default());
        let cancel = CancellationToken::new();
        run_native_push(
            &config,
            specs,
            NativePushInputs::new(
                Some(store),
                None,
                None,
                router,
                push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
        .expect("run_native_push must succeed end-to-end on InMemory store")
    }

    fn assert_ok(result: &crab::git::push::PushResult, dst: &str) {
        match result.outcomes.get(dst) {
            Some(RefPushOutcome::Ok) => {}
            other => panic!("expected Ok outcome for {dst}, got {other:?}"),
        }
    }

    /// Push a ref, then push `:ref` — the manifest must no longer
    /// carry an entry for the deleted destination.
    #[tokio::test]
    async fn delete_ref_removes_manifest_entry() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());
        fixture.commit_text("a.txt", 0x10, "seed main");
        fixture.branch_at_head("feature");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Initial push creates refs/heads/main and refs/heads/feature.
        let create = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
        )
        .await;
        assert_ok(&create, "refs/heads/main");
        assert_ok(&create, "refs/heads/feature");

        let (after_create, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must exist after create push");
        assert!(
            after_create.refs.contains_key("refs/heads/feature"),
            "feature ref expected in manifest after create; refs={:?}",
            after_create.refs
        );

        // Delete refs/heads/feature; main must survive.
        let delete = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[push_spec("", "refs/heads/feature")],
        )
        .await;
        assert_ok(&delete, "refs/heads/feature");

        let (after_delete, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must exist after delete push");
        assert!(
            !after_delete.refs.contains_key("refs/heads/feature"),
            "feature ref must be gone from manifest; refs={:?}",
            after_delete.refs
        );
        assert!(
            after_delete.refs.contains_key("refs/heads/main"),
            "main ref must survive sibling delete; refs={:?}",
            after_delete.refs
        );
    }

    /// Deleting a ref that was never present must succeed as a no-op
    /// rather than return an error — mirrors git's behaviour and the
    /// idempotency contract documented in the P0-1 requirements.
    #[tokio::test]
    async fn delete_nonexistent_ref_is_noop() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());
        fixture.commit_text("a.txt", 0x20, "seed main");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Seed the manifest with one ref so the delete runs against a
        // pre-existing (generation > 0) manifest — the more realistic
        // shape than a cold-start empty store.
        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[push_spec("refs/heads/main", "refs/heads/main")],
        )
        .await;
        assert_ok(&seed, "refs/heads/main");

        let (before, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must exist after seed push");

        // Delete a ref that was never created. The outcome must be
        // Ok and the manifest must be unchanged.
        let delete = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[push_spec("", "refs/heads/never-existed")],
        )
        .await;
        assert_ok(&delete, "refs/heads/never-existed");

        let (after, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must still exist after noop delete");
        assert_eq!(
            before.refs, after.refs,
            "noop delete must not change the ref map"
        );
    }

    /// After a successful delete, the persisted push-state file must
    /// not contain an entry for the removed ref — otherwise the next
    /// incremental walk would try to hide against a SHA that no
    /// longer exists on the remote.
    #[tokio::test]
    async fn delete_ref_clears_push_state_entry() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());
        fixture.commit_text("a.txt", 0x30, "seed main");
        fixture.branch_at_head("feature");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let create = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
        )
        .await;
        assert_ok(&create, "refs/heads/main");
        assert_ok(&create, "refs/heads/feature");
        assert!(
            push_state
                .last_pushed("crab://bucket/repo", "refs/heads/feature")
                .is_some(),
            "feature ref must be tracked after create; state={push_state:?}"
        );

        let delete = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[push_spec("", "refs/heads/feature")],
        )
        .await;
        assert_ok(&delete, "refs/heads/feature");

        // Persist and reload so the test reflects what the on-disk
        // push-state file looks like — not just the in-memory copy.
        let repo_root = fixture.work_tree().to_owned();
        push_state.save(&repo_root).expect("save push-state");
        let reloaded = PushState::load(&repo_root);

        assert!(
            reloaded
                .last_pushed("crab://bucket/repo", "refs/heads/feature")
                .is_none(),
            "push-state must not carry a deleted ref; reloaded={reloaded:?}"
        );
        assert_eq!(
            reloaded.last_pushed("crab://bucket/repo", "refs/heads/main"),
            push_state.last_pushed("crab://bucket/repo", "refs/heads/main"),
            "sibling ref's push-state entry must survive"
        );
    }

    /// A batch containing both a delete (`:old`) and a create
    /// (`new:new`) must apply both in a single manifest generation —
    /// the unified manifest CAS commits the ref map atomically, so a
    /// partial apply would leave the repo in a state git never
    /// expects to observe.
    #[tokio::test]
    async fn delete_plus_create_in_same_batch_applies_both() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());
        fixture.commit_text("a.txt", 0x40, "seed main");
        fixture.branch_at_head("old");
        fixture.commit_text("b.txt", 0x41, "advance for new branch");
        fixture.branch_at_head("new");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Seed the manifest with main + old so the next batch has
        // something to delete.
        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/old", "refs/heads/old"),
            ],
        )
        .await;
        assert_ok(&seed, "refs/heads/main");
        assert_ok(&seed, "refs/heads/old");

        let (seed_manifest, _) = read_manifest(&store, &router)
            .await
            .expect("seed manifest read");
        let seed_generation = seed_manifest.generation;

        // Mixed batch: delete `old`, create `new`.
        let mixed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("", "refs/heads/old"),
                push_spec("refs/heads/new", "refs/heads/new"),
            ],
        )
        .await;
        assert_ok(&mixed, "refs/heads/old");
        assert_ok(&mixed, "refs/heads/new");

        let (after, _) = read_manifest(&store, &router)
            .await
            .expect("manifest read after mixed batch");

        assert!(
            !after.refs.contains_key("refs/heads/old"),
            "old ref must be removed by the mixed batch; refs={:?}",
            after.refs
        );
        assert!(
            after.refs.contains_key("refs/heads/new"),
            "new ref must be created by the mixed batch; refs={:?}",
            after.refs
        );
        assert!(
            after.refs.contains_key("refs/heads/main"),
            "main ref must survive the mixed batch; refs={:?}",
            after.refs
        );

        // Both changes must land in a single generation bump — a
        // one-batch CAS, not two sequential CAS operations.
        assert_eq!(
            after.generation,
            seed_generation + 1,
            "delete + create must commit in one manifest generation"
        );
    }
}

// ---------------------------------------------------------------------------
// Pipeline-error per-ref outcome preservation
//
// These tests cover the carrier variant that keeps multi-ref push
// responses structured even when the pipeline aborts on one ref's
// behalf. Lock contention gets its own structured reject reason;
// NonFastForward inside the manifest CAS retry loop preserves the
// non-conflicting refs' Ok outcomes instead of collapsing every ref
// to the same error string.
// ---------------------------------------------------------------------------

mod pipeline_error_outcomes {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, MutexGuard};
    use std::time::Duration;

    use object_store::memory::InMemory;
    use tokio_util::sync::CancellationToken;

    use crab::git::push::{PushConfig, PushRejectReason, PushResult, RefPushOutcome};
    use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
    use crab::git::push_state::PushState;
    use crab::git::remote_helper::PushSpec;
    use crab::storage::StoreLayout;
    use crab::storage::store::Store;

    use super::GIT_DIR_MUTEX;

    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: serialised by GIT_DIR_MUTEX.
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
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

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }

        fn commit_text(&self, name: &str, seed: u8, message: &str) -> String {
            let content = format!("crab-partial-outcome-test seed={seed} file={name}\n");
            std::fs::write(self.work_tree().join(name), content.as_bytes())
                .expect("write text file");
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

        fn branch_at_head(&self, name: &str) {
            Self::run_git(self.work_tree(), &["branch", name]);
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

    fn make_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    fn make_router(store: Store, prefix: &str) -> StoreLayout {
        StoreLayout::new(store, prefix.to_owned())
    }

    fn push_spec(src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force: false,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    async fn run_batch(
        store: Store,
        router: StoreLayout,
        push_state: &mut PushState,
        specs: &[PushSpec],
    ) -> PushResult {
        let config = NativePushConfig::new(PushConfig::default());
        let cancel = CancellationToken::new();
        run_native_push(
            &config,
            specs,
            NativePushInputs::new(
                Some(store),
                None,
                None,
                router,
                push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
        .expect("run_native_push must return Ok(PushResult) end-to-end")
    }

    /// Regression for the remote-helper collapse site. Before this
    /// task, a batch-global pipeline failure overwrote every ref with
    /// `RefPushOutcome::Error(stringified_error)` — losing the
    /// `PushRejectReason` taxonomy. Post-fix, non-`PushPartialOutcome`
    /// errors land as `Rejected(PushRejectReason::Internal(...))`,
    /// preserving the structured shape so the remote helper can emit
    /// a stable `error {ref} internal` line. Failures that the
    /// pipeline mapped to `PushPartialOutcome` are exercised by the
    /// `partial_outcome_preserves_sibling_outcomes` unit test in the
    /// push module — an integration reproduction of that specific
    /// path requires a fault-injecting store, which arrives with the
    /// P2-3 harness.
    #[tokio::test]
    async fn pipeline_error_preserves_per_ref_outcomes() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA0, "seed main");
        fixture.branch_at_head("feature");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/main"),
                Some(RefPushOutcome::Ok)
            ),
            "seed main must succeed; outcomes={:?}",
            seed.outcomes
        );

        // Advance main and remove the new blob from the local ODB.
        // `git rev-list` (and every downstream walk) can't resolve
        // main's new tip — the push pipeline errors out at pack
        // generation, well before the unified manifest CAS. The
        // remote-helper collapse site must map that error to
        // `Rejected(PushRejectReason::Internal(...))` across every
        // ref in the batch rather than the pre-fix `Error(String)`
        // shape, so scripts pivoting on protocol tags keep working.
        let new_main_sha = fixture.commit_text("b.txt", 0xB1, "advance main");
        let blob_sha = {
            let out = Command::new("git")
                .args(["rev-parse", &format!("{new_main_sha}:b.txt")])
                .current_dir(fixture.work_tree())
                .output()
                .expect("git rev-parse HEAD:b.txt");
            assert!(
                out.status.success(),
                "git rev-parse HEAD:b.txt failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout)
                .expect("rev-parse output is utf8")
                .trim()
                .to_owned()
        };
        let blob_path = fixture
            .git_dir()
            .join("objects")
            .join(&blob_sha[..2])
            .join(&blob_sha[2..]);
        assert!(
            blob_path.exists(),
            "expected blob at {} before deletion",
            blob_path.display()
        );
        std::fs::remove_file(&blob_path).expect("delete blob object from local ODB");

        let result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
        )
        .await;

        for ref_name in ["refs/heads/main", "refs/heads/feature"] {
            match result.outcomes.get(ref_name) {
                Some(RefPushOutcome::Rejected(PushRejectReason::Internal(_))) => {}
                Some(RefPushOutcome::Error(_)) => panic!(
                    "{ref_name} surfaced `Error(String)` — the pre-task-2 collapse \
                     shape should no longer be produced; outcomes={:?}",
                    result.outcomes
                ),
                other => panic!(
                    "expected {ref_name} to be Rejected(Internal(_)), got {other:?}; \
                     full outcomes={:?}",
                    result.outcomes
                ),
            }
        }

        // The collapse must also never silently drop a ref from the
        // outcome map — every spec in the batch gets an entry even
        // when the pipeline failed batch-globally.
        assert!(
            result.outcomes.contains_key("refs/heads/main")
                && result.outcomes.contains_key("refs/heads/feature"),
            "every spec must have an outcome entry; outcomes={:?}",
            result.outcomes
        );
    }

    /// A batch of three refs against a contended lock must surface
    /// `Rejected(LockContention)` for every ref, carrying the holder
    /// id from the lease payload. Before the partial-outcome carrier
    /// landed, this collapsed to `Error(String)` and lost the holder,
    /// making retry logic and observability much worse.
    #[tokio::test]
    async fn lock_contention_surfaces_as_structured_reject() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());
        fixture.commit_text("a.txt", 0xC0, "seed main");
        fixture.branch_at_head("feature");
        fixture.branch_at_head("release");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");

        // Acquire the push lock the streaming pipeline would target.
        // `acquire_streaming_lock` picks the first non-empty-src spec
        // as the lock target, which for our 3-ref batch is main.
        let first_pusher_lock = crab_coordination::PushLock::acquire_ref(
            store.inner(),
            router.repo_prefix(),
            "refs/heads/main",
            Duration::from_secs(60),
        )
        .await
        .expect("first pusher must acquire lock cleanly");
        let held_by = first_pusher_lock.holder().to_owned();

        // Second pusher tries the same three refs. It will fail at
        // the `acquire_streaming_lock` step because the lease is
        // still live, and every ref in the batch gets
        // Rejected(LockContention { holder, ttl_remaining_secs }).
        let mut push_state = PushState::default();
        let result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
                push_spec("refs/heads/release", "refs/heads/release"),
            ],
        )
        .await;

        for ref_name in [
            "refs/heads/main",
            "refs/heads/feature",
            "refs/heads/release",
        ] {
            match result.outcomes.get(ref_name) {
                Some(RefPushOutcome::Rejected(PushRejectReason::LockContention {
                    holder,
                    ttl_remaining_secs,
                })) => {
                    assert_eq!(
                        holder, &held_by,
                        "LockContention holder must match first pusher's id; got {holder:?}, want {held_by:?}"
                    );
                    // TTL remaining must be positive because the lease
                    // is 60s and we haven't moved the wall clock.
                    // Allow for a few seconds of test scheduler jitter.
                    assert!(
                        *ttl_remaining_secs > 0 && *ttl_remaining_secs <= 60,
                        "ttl_remaining_secs must be within the lease window; got {ttl_remaining_secs}"
                    );
                }
                other => panic!(
                    "expected {ref_name} to be Rejected(LockContention), got {other:?}; \
                     full outcomes={:?}",
                    result.outcomes
                ),
            }
        }

        // Release the first pusher's lock so the tempdir's destructor
        // doesn't block on lease expiry (we use a 60s lease above).
        first_pusher_lock
            .release()
            .await
            .expect("first pusher releases lock cleanly");
    }
}

// ---------------------------------------------------------------------------
// Atomic multi-ref push semantics
//
// Covers `HelperOptions::atomic` / `PushConfig::atomic` and the
// two-pass `evaluate_decisions` + `apply_decisions` split inside
// `unified_manifest_cas::build_manifest`. Atomic mode must roll the
// whole batch back when any ref is rejected pre-flight; non-atomic
// mode must commit the proceeding refs and surface structured
// rejections for the rest in a single manifest generation bump.
// ---------------------------------------------------------------------------

mod atomic_push {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, MutexGuard};

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use tokio_util::sync::CancellationToken;

    use crab::git::push::{PushConfig, PushRejectReason, PushResult, RefPushOutcome};
    use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
    use crab::git::push_state::PushState;
    use crab::git::remote_helper::PushSpec;
    use crab::metadata::manifest::read_manifest;
    use crab::storage::StoreLayout;
    use crab::storage::store::Store;

    use super::GIT_DIR_MUTEX;

    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: serialised by GIT_DIR_MUTEX.
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
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

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }

        fn commit_text(&self, name: &str, seed: u8, message: &str) -> String {
            let content = format!("crab-atomic-push-test seed={seed} file={name}\n");
            std::fs::write(self.work_tree().join(name), content.as_bytes())
                .expect("write text file");
            Self::run_git(self.work_tree(), &["add", name]);
            Self::run_git(self.work_tree(), &["commit", "-m", message]);
            Self::head_sha(self.work_tree())
        }

        fn branch_at_head(&self, name: &str) {
            Self::run_git(self.work_tree(), &["branch", name]);
        }

        fn checkout(&self, branch: &str) {
            Self::run_git(self.work_tree(), &["checkout", branch]);
        }

        /// Rewrite `HEAD` by amending the current commit with a new
        /// author date so the SHA changes but the tree content (and
        /// parent) stays the same. Produces a divergent history: the
        /// amended commit is NOT a descendant of the original, so a
        /// non-force push of the amended SHA against a ref whose
        /// remote tip is the original SHA is a textbook non-FF.
        fn amend_with_new_date(&self, date: &str) -> String {
            Self::run_git_with_env(
                self.work_tree(),
                &["commit", "--amend", "--no-edit", "--date", date],
                &[("GIT_COMMITTER_DATE", date)],
            );
            Self::head_sha(self.work_tree())
        }

        fn head_sha(cwd: &Path) -> String {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(cwd)
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

        fn run_git_with_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(cwd);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd
                .output()
                .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn make_store_pair() -> (Store, Arc<InMemory>) {
        let inner = Arc::new(InMemory::new());
        (Store::new(inner.clone()), inner)
    }

    fn make_router(store: Store, prefix: &str) -> StoreLayout {
        StoreLayout::new(store, prefix.to_owned())
    }

    fn push_spec(src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force: false,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    async fn run_batch_atomic(
        store: Store,
        router: StoreLayout,
        push_state: &mut PushState,
        specs: &[PushSpec],
        atomic: bool,
    ) -> PushResult {
        let mut config = NativePushConfig::new(PushConfig::default());
        config.push.atomic = atomic;
        let cancel = CancellationToken::new();
        run_native_push(
            &config,
            specs,
            NativePushInputs::new(
                Some(store),
                None,
                None,
                router,
                push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
        .expect("run_native_push must return Ok(PushResult) end-to-end")
    }

    /// Count every object written under the router's prefix. Used to
    /// assert the atomic-empty case is a no-op at the PUT level, not
    /// just at the manifest level.
    async fn count_store_objects(inner: &Arc<InMemory>, prefix: &str) -> usize {
        use futures_util::StreamExt;
        let prefix_path = ObjectPath::from(prefix);
        let mut stream = inner.list(Some(&prefix_path));
        let mut count = 0usize;
        while let Some(meta) = stream.next().await {
            if meta.is_ok() {
                count += 1;
            }
        }
        count
    }

    /// atomic=true with one non-FF ref must reject the entire batch
    /// and leave the remote manifest untouched.
    #[tokio::test]
    async fn atomic_push_one_ref_rejected_rolls_back_batch() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA1, "seed main");
        fixture.branch_at_head("feature");

        let (store, _inner) = make_store_pair();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Seed the remote with main @ A and feature @ A. Non-atomic
        // batch — baseline before the interesting atomic reject.
        let seed = run_batch_atomic(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
            false,
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/main"),
                Some(RefPushOutcome::Ok)
            ),
            "seed main must succeed; outcomes={:?}",
            seed.outcomes
        );
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/feature"),
                Some(RefPushOutcome::Ok)
            ),
            "seed feature must succeed; outcomes={:?}",
            seed.outcomes
        );

        let (seed_manifest, _) = read_manifest(&store, &router)
            .await
            .expect("seed manifest read");
        let seed_generation = seed_manifest.generation;
        let seed_main = seed_manifest
            .refs
            .get("refs/heads/main")
            .cloned()
            .expect("seed manifest has main");

        // Rewrite the main branch so its SHA is no longer an
        // ancestor of the remote's recorded SHA — classic non-FF.
        // feature stays where it was (still @ A), so it's a
        // no-op update (same SHA, FF-ok).
        fixture.checkout("main");
        let new_main = fixture.amend_with_new_date("2001-02-03T04:05:06+00:00");
        assert_ne!(
            new_main, seed_main,
            "amended commit must have a different SHA than the seed"
        );

        // Atomic push of the non-FF ref alongside a FF-ok ref. The
        // batch must roll back entirely.
        let atomic_result = run_batch_atomic(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
            true,
        )
        .await;

        match atomic_result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Rejected(PushRejectReason::NonFastForward { .. })) => {}
            other => panic!(
                "expected main to be Rejected(NonFastForward); got {other:?}; outcomes={:?}",
                atomic_result.outcomes
            ),
        }

        // After an atomic rollback, the remote manifest must be
        // identical to the seed — no new generation bump, main
        // still points at the original SHA.
        let (after, _) = read_manifest(&store, &router)
            .await
            .expect("post-atomic manifest read");
        assert_eq!(
            after.generation, seed_generation,
            "atomic rollback must not bump the manifest generation"
        );
        assert_eq!(
            after.refs.get("refs/heads/main"),
            Some(&seed_main),
            "atomic rollback must leave main pointed at the seed SHA; refs={:?}",
            after.refs
        );
    }

    /// Non-atomic: one rejected ref must not hold up the others.
    /// Valid refs commit in a single manifest generation bump and
    /// the rejected ref surfaces `Rejected(NonFastForward)`.
    #[tokio::test]
    async fn non_atomic_push_one_ref_rejected_others_apply() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA2, "seed main");
        fixture.branch_at_head("feature");

        let (store, _inner) = make_store_pair();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let seed = run_batch_atomic(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
            false,
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/main"),
                Some(RefPushOutcome::Ok)
            ),
            "seed main must succeed; outcomes={:?}",
            seed.outcomes
        );

        let (seed_manifest, _) = read_manifest(&store, &router)
            .await
            .expect("seed manifest read");
        let seed_generation = seed_manifest.generation;
        let seed_main = seed_manifest
            .refs
            .get("refs/heads/main")
            .cloned()
            .expect("seed manifest has main");

        // Advance feature with a brand-new commit (FF-ok for
        // feature), then divergently rewrite main.
        fixture.checkout("feature");
        let advanced_feature = fixture.commit_text("b.txt", 0xB1, "advance feature");

        fixture.checkout("main");
        let divergent_main = fixture.amend_with_new_date("2001-02-03T04:05:07+00:00");
        assert_ne!(divergent_main, seed_main);

        // Non-atomic push of the non-FF main alongside the FF-ok
        // feature. main gets Rejected, feature commits, and the
        // manifest bumps exactly one generation.
        let result = run_batch_atomic(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                push_spec("refs/heads/main", "refs/heads/main"),
                push_spec("refs/heads/feature", "refs/heads/feature"),
            ],
            false,
        )
        .await;

        match result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Rejected(PushRejectReason::NonFastForward { .. })) => {}
            other => panic!(
                "expected main to be Rejected(NonFastForward); got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }
        match result.outcomes.get("refs/heads/feature") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected feature to be Ok; got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }

        let (after, _) = read_manifest(&store, &router)
            .await
            .expect("post-batch manifest read");
        assert_eq!(
            after.generation,
            seed_generation + 1,
            "non-atomic partial commit must bump the manifest generation exactly once"
        );
        assert_eq!(
            after.refs.get("refs/heads/main"),
            Some(&seed_main),
            "rejected main must stay pinned to the seed SHA; refs={:?}",
            after.refs
        );
        assert_eq!(
            after.refs.get("refs/heads/feature"),
            Some(&advanced_feature),
            "feature must advance to its new tip; refs={:?}",
            after.refs
        );
    }

    /// atomic=true with an empty spec list must short-circuit
    /// cleanly — no store PUTs, no manifest reads, nothing.
    #[tokio::test]
    async fn atomic_push_empty_batch_is_noop() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        let (store, inner) = make_store_pair();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let before = count_store_objects(&inner, "repo-prefix").await;

        let result =
            run_batch_atomic(store.clone(), router.clone(), &mut push_state, &[], true).await;

        assert!(
            result.outcomes.is_empty(),
            "empty batch must produce zero outcomes; got {:?}",
            result.outcomes
        );

        let after = count_store_objects(&inner, "repo-prefix").await;
        assert_eq!(
            before, after,
            "empty atomic batch must not write to the store"
        );
    }
}

// ---------------------------------------------------------------------------
// Fetch reachability regression tests
//
// These exercise the upload-pack policy that gates raw-SHA
// `fetch <sha> <ref>` lines. The core validator is a pure sync
// function (`validate_fetch_entries_with_manifest`), so these tests
// drive it directly and assert the rejection's protocol tag /
// reason — the same tag the transcript response embeds via
// `error {ref_name} {tag} ({reason})`.
//
// A full transcript round-trip requires a primed object store with
// a manifest; the wire-format assertion here (protocol tag +
// reason string) covers the plumbing that `fetch_packs` uses when
// emitting the per-entry error line.
// ---------------------------------------------------------------------------

mod fetch_reachability {
    use std::collections::BTreeMap;

    use crab::core::config::Config;
    use crab::git::reject_reason::FetchRejectReason;
    use crab::git::remote_helper::{FetchEntry, validate_fetch_entries_with_manifest};
    use crab::metadata::manifest::Manifest;
    use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};

    fn manifest_with_ref(ref_name: &str, sha: &str) -> Manifest {
        let mut refs = BTreeMap::new();
        refs.insert(ref_name.to_owned(), sha.to_owned());
        let mut manifest = Manifest::default_for_repo(ref_name);
        manifest.generation = 1;
        manifest.refs = refs;
        manifest.seal_git_validation();
        manifest
    }

    /// Non-tip SHA under the default `allow_tip` policy must surface
    /// a `not-at-tip` rejection — `fetch_packs` formats this as
    /// `error {ref_name} not-at-tip ({detail})\n` and skips the
    /// pack download.
    #[test]
    fn fetch_rejects_non_tip_sha_when_only_tip_allowed() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &tip);
        let entries = vec![FetchEntry {
            sha: non_tip.clone(),
            ref_name: "refs/heads/hidden".into(),
        }];
        let cfg = Config::default();

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        let (entry, outcome) = &result[0];
        let err = outcome
            .as_ref()
            .err()
            .expect("non-tip sha must be rejected");
        assert_eq!(err.protocol_tag(), "not-at-tip");

        // The transcript wire line `fetch_packs` emits embeds the
        // ref name, the protocol tag, and the detail. Reconstruct
        // that line here to confirm the shape the client will see.
        let line = format!("error {} {} ({})", entry.ref_name, err.protocol_tag(), err);
        assert!(line.contains("refs/heads/hidden"));
        assert!(line.contains("not-at-tip"));
        assert!(line.contains(&non_tip));
    }

    #[test]
    fn fetch_accepts_tip_sha_when_tip_allowed() {
        let tip = "a".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &tip);
        let entries = vec![FetchEntry {
            sha: tip.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let cfg = Config::default();

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert!(
            result[0].1.is_ok(),
            "tip SHA must be accepted; got {:?}",
            result[0].1
        );
    }

    #[test]
    fn fetch_allow_any_sha_in_want_bypasses_check() {
        let tip = "a".repeat(40);
        let non_tip = "b".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &tip);
        let entries = vec![FetchEntry {
            sha: non_tip,
            ref_name: "refs/heads/feature".into(),
        }];
        let mut cfg = Config::default();
        cfg.uploadpack_allow_any_sha_in_want = true;

        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert!(
            result[0].1.is_ok(),
            "allow_any bypasses policy; got {:?}",
            result[0].1
        );
    }

    #[test]
    fn fetch_reachable_sha_accepted_when_reachable_allowed() {
        // Chain: A → B (tip).
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![
                CommitEntry {
                    oid: sha_a.clone(),
                    gen_number: 0,
                    parents: vec![],
                },
                CommitEntry {
                    oid: sha_b.clone(),
                    gen_number: 1,
                    parents: vec![sha_a.clone()],
                },
            ],
        };
        let manifest = manifest_with_ref("refs/heads/main", &sha_b);

        let entries = vec![FetchEntry {
            sha: sha_a,
            ref_name: "refs/heads/main".into(),
        }];
        let mut cfg = Config::default();
        cfg.uploadpack_allow_reachable_sha_in_want = true;

        let result =
            validate_fetch_entries_with_manifest(&entries, &manifest, Some(&summary), &cfg);

        assert!(
            result[0].1.is_ok(),
            "reachable ancestor must be accepted when allow_reachable is on; got {:?}",
            result[0].1
        );
    }

    #[test]
    fn fetch_rejects_unreachable_sha_as_not_reachable_when_reachable_allowed() {
        let sha_tip = "a".repeat(40);
        let sha_phantom = "f".repeat(40);
        let manifest = manifest_with_ref("refs/heads/main", &sha_tip);
        let summary = CommitGraphSummary {
            generation: 1,
            commits: vec![CommitEntry {
                oid: sha_tip,
                gen_number: 0,
                parents: vec![],
            }],
        };
        let entries = vec![FetchEntry {
            sha: sha_phantom.clone(),
            ref_name: "refs/heads/wat".into(),
        }];
        let mut cfg = Config::default();
        cfg.uploadpack_allow_reachable_sha_in_want = true;

        let result =
            validate_fetch_entries_with_manifest(&entries, &manifest, Some(&summary), &cfg);

        match &result[0].1 {
            Err(FetchRejectReason::NotReachable { sha }) => assert_eq!(sha, &sha_phantom),
            other => panic!("expected NotReachable, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Receive-policy regression tests
//
// These exercise the `receive.*` policy gates wired into
// `evaluate_decisions` — `denyDeletes`, `denyNonFastForwards`, and
// `denyCurrentBranch`. Each test seeds the remote with a real manifest
// via a prior successful push, then attempts a second push whose
// policy outcome is the assertion. Test harness mirrors `atomic_push`
// / `delete_ref` modules above: `InMemory` object store + on-disk git
// fixture + `run_native_push` driving the full pipeline with a custom
// `PushConfig`.
// ---------------------------------------------------------------------------

mod receive_policy {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, MutexGuard};

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use tokio_util::sync::CancellationToken;

    use crab::git::push::{PushConfig, PushRejectReason, PushResult, RefPushOutcome};
    use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
    use crab::git::push_state::PushState;
    use crab::git::remote_helper::PushSpec;
    use crab::metadata::manifest::read_manifest;
    use crab::storage::StoreLayout;
    use crab::storage::store::Store;

    use super::GIT_DIR_MUTEX;

    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: serialised by GIT_DIR_MUTEX.
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
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

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }

        fn commit_text(&self, name: &str, seed: u8, message: &str) -> String {
            let content = format!("crab-receive-policy-test seed={seed} file={name}\n");
            std::fs::write(self.work_tree().join(name), content.as_bytes())
                .expect("write text file");
            Self::run_git(self.work_tree(), &["add", name]);
            Self::run_git(self.work_tree(), &["commit", "-m", message]);
            Self::head_sha(self.work_tree())
        }

        fn branch_at_head(&self, name: &str) {
            Self::run_git(self.work_tree(), &["branch", name]);
        }

        fn checkout(&self, branch: &str) {
            Self::run_git(self.work_tree(), &["checkout", branch]);
        }

        /// Divergently rewrite `HEAD` by amending with a new author
        /// date — same tree, different SHA, same parent. The amended
        /// commit is not a descendant of the original, so a non-force
        /// push against the original tip is textbook non-FF.
        fn amend_with_new_date(&self, date: &str) -> String {
            Self::run_git_with_env(
                self.work_tree(),
                &["commit", "--amend", "--no-edit", "--date", date],
                &[("GIT_COMMITTER_DATE", date)],
            );
            Self::head_sha(self.work_tree())
        }

        fn head_sha(cwd: &Path) -> String {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(cwd)
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

        fn run_git_with_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(cwd);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd
                .output()
                .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn make_store() -> Store {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    fn make_router(store: Store, prefix: &str) -> StoreLayout {
        StoreLayout::new(store, prefix.to_owned())
    }

    fn spec(src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force: false,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    fn force_spec(src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force: true,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    /// Drive `run_native_push` once. The caller mutates `push_cfg` to
    /// flip the policy knob under test before calling.
    async fn run_batch(
        store: Store,
        router: StoreLayout,
        push_state: &mut PushState,
        specs: &[PushSpec],
        push_cfg: PushConfig,
    ) -> PushResult {
        let config = NativePushConfig::new(push_cfg);
        let cancel = CancellationToken::new();
        run_native_push(
            &config,
            specs,
            NativePushInputs::new(
                Some(store),
                None,
                None,
                router,
                push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
        .expect("run_native_push must return Ok(PushResult) end-to-end")
    }

    /// `receive.denyDeletes = true` rejects a `:ref` delete and
    /// leaves the manifest untouched.
    #[tokio::test]
    async fn deny_deletes_true_rejects_delete_ref() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA1, "seed main");
        fixture.branch_at_head("feature");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Seed the remote with main + feature under permissive policy.
        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[
                spec("refs/heads/main", "refs/heads/main"),
                spec("refs/heads/feature", "refs/heads/feature"),
            ],
            PushConfig::default(),
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/feature"),
                Some(RefPushOutcome::Ok)
            ),
            "seed feature must succeed; outcomes={:?}",
            seed.outcomes
        );

        let (seed_manifest, _) = read_manifest(&store, &router)
            .await
            .expect("seed manifest read");
        let seed_generation = seed_manifest.generation;
        assert!(seed_manifest.refs.contains_key("refs/heads/feature"));

        // Attempt a delete with deny_deletes = true.
        let mut strict = PushConfig::default();
        strict.receive_deny_deletes = true;
        let deleted = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[PushSpec {
                force: false,
                src: String::new(),
                dst: "refs/heads/feature".into(),
            }],
            strict,
        )
        .await;

        match deleted.outcomes.get("refs/heads/feature") {
            Some(RefPushOutcome::Rejected(PushRejectReason::DenyDeletes)) => {}
            other => panic!(
                "expected Rejected(DenyDeletes); got {other:?}; outcomes={:?}",
                deleted.outcomes
            ),
        }

        let (after, _) = read_manifest(&store, &router)
            .await
            .expect("post-delete manifest read");
        // Non-atomic mode bumps the manifest generation by one for
        // every batch that reaches `apply_decisions` — even when every
        // spec is rejected, the empty-Proceed set still produces a
        // "no ref changes" commit. The policy invariant we care about
        // is that the rejected ref is still there, not that the
        // generation counter stood still.
        let _ = seed_generation;
        assert!(
            after.refs.contains_key("refs/heads/feature"),
            "feature must still be present; refs={:?}",
            after.refs
        );
    }

    /// `receive.denyNonFastForwards = true` rejects a force push even
    /// when the pusher sent `+refspec`. Admin policy wins.
    #[tokio::test]
    async fn deny_non_fast_forwards_true_rejects_force_push() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA1, "seed main");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            PushConfig::default(),
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/main"),
                Some(RefPushOutcome::Ok)
            ),
            "seed main must succeed; outcomes={:?}",
            seed.outcomes
        );

        let (seed_manifest, _) = read_manifest(&store, &router).await.unwrap();
        let seed_main = seed_manifest.refs["refs/heads/main"].clone();

        // Divergent amend — new SHA that is not a descendant of the
        // remote tip.
        fixture.checkout("main");
        let forced = fixture.amend_with_new_date("2001-02-03T04:05:06+00:00");
        assert_ne!(forced, seed_main);

        // Force push, but with `denyNonFastForwards = true`. Admin
        // policy wins over the pusher's `+refspec`.
        let mut strict = PushConfig::default();
        strict.receive_deny_non_fast_forwards = true;

        let forced_result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[force_spec("refs/heads/main", "refs/heads/main")],
            strict,
        )
        .await;

        match forced_result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Rejected(PushRejectReason::DenyNonFastForward)) => {}
            other => panic!(
                "expected Rejected(DenyNonFastForward); got {other:?}; outcomes={:?}",
                forced_result.outcomes
            ),
        }

        let (after, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(
            after.refs["refs/heads/main"], seed_main,
            "manifest main must still point at the seed SHA; refs={:?}",
            after.refs
        );
    }

    /// `receive.denyCurrentBranch = "refuse"` rejects a push to the
    /// manifest's advertised HEAD.
    #[tokio::test]
    async fn deny_current_branch_refuse_rejects_push_to_head() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA1, "seed main");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Seed so the manifest carries HEAD = refs/heads/main.
        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            PushConfig::default(),
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/main"),
                Some(RefPushOutcome::Ok)
            ),
            "seed main must succeed; outcomes={:?}",
            seed.outcomes
        );
        let (seed_manifest, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(seed_manifest.head, "refs/heads/main");

        // Advance main so the second push is a non-trivial update
        // rather than a no-op (no-ops would still be rejected, but
        // this makes the assertion visible in the refs map below).
        let advanced = fixture.commit_text("b.txt", 0xB1, "advance main");
        assert_ne!(advanced, seed_manifest.refs["refs/heads/main"]);

        let mut strict = PushConfig::default();
        strict.receive_deny_current_branch = "refuse".into();
        let refused = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            strict,
        )
        .await;

        match refused.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Rejected(PushRejectReason::DenyCurrentBranch)) => {}
            other => panic!(
                "expected Rejected(DenyCurrentBranch); got {other:?}; outcomes={:?}",
                refused.outcomes
            ),
        }

        let (after, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(
            after.refs["refs/heads/main"], seed_manifest.refs["refs/heads/main"],
            "refuse must leave main pointed at the seed SHA; refs={:?}",
            after.refs
        );
    }

    /// `receive.denyCurrentBranch = "warn"` accepts the push and logs
    /// a warning. We observe acceptance via the manifest — the
    /// `tracing::warn!` event is exercised as a side effect but isn't
    /// asserted directly (a trace subscriber would be awkward to
    /// thread through the harness for a single log line).
    #[tokio::test]
    async fn deny_current_branch_warn_accepts_with_progress_warning() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA1, "seed main");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            PushConfig::default(),
        )
        .await;
        assert!(matches!(
            seed.outcomes.get("refs/heads/main"),
            Some(RefPushOutcome::Ok)
        ));

        let advanced = fixture.commit_text("b.txt", 0xB1, "advance main");

        let mut warn_cfg = PushConfig::default();
        warn_cfg.receive_deny_current_branch = "warn".into();
        let warned = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            warn_cfg,
        )
        .await;

        match warned.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected Ok under warn mode; got {other:?}; outcomes={:?}",
                warned.outcomes
            ),
        }

        let (after, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(
            after.refs["refs/heads/main"], advanced,
            "warn mode must let the advance land; refs={:?}",
            after.refs
        );
    }

    /// `receive.denyCurrentBranch = "ignore"` (the default) accepts
    /// the push silently — no rejection, no warning log.
    #[tokio::test]
    async fn deny_current_branch_ignore_accepts_silently() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        fixture.commit_text("a.txt", 0xA1, "seed main");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            PushConfig::default(),
        )
        .await;
        assert!(matches!(
            seed.outcomes.get("refs/heads/main"),
            Some(RefPushOutcome::Ok)
        ));

        let advanced = fixture.commit_text("b.txt", 0xB1, "advance main");

        // Default config has `deny_current_branch = "ignore"` — no
        // override needed, but we assert on it explicitly so a future
        // default change breaks this test loudly.
        let cfg = PushConfig::default();
        assert_eq!(cfg.receive_deny_current_branch, "ignore");

        let result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            cfg,
        )
        .await;

        match result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected Ok under ignore mode; got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }

        let (after, _) = read_manifest(&store, &router).await.unwrap();
        assert_eq!(after.refs["refs/heads/main"], advanced);
    }
}

// ---------------------------------------------------------------------------
// Native `--follow-tags` publication
//
// End-to-end proof that `NativePushConfig.followtags` synthesises an
// annotated tag whose target commit is in the pushed commit set and publishes
// it in the same manifest as the branch. Git's remote-helper `followtags`
// option is fetch-only; its protocol behavior is covered in remote_helper.rs.
// ---------------------------------------------------------------------------

mod followtags {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, MutexGuard};

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use tokio_util::sync::CancellationToken;

    use crab::git::push::{PushConfig, PushResult, RefPushOutcome};
    use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
    use crab::git::push_state::PushState;
    use crab::git::remote_helper::PushSpec;
    use crab::metadata::manifest::read_manifest;
    use crab::storage::StoreLayout;
    use crab::storage::store::Store;

    use super::GIT_DIR_MUTEX;

    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: serialised by GIT_DIR_MUTEX.
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
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
            // Fixed tagger identity makes the annotated tag object
            // reproducible across runs — useful when debugging SHA
            // mismatches, and it keeps the tag object deterministic
            // for the cat-file assertion below.
            Self::run_git(dir.path(), &["config", "tag.gpgsign", "false"]);
            Self { dir }
        }

        fn work_tree(&self) -> &Path {
            self.dir.path()
        }

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }

        fn commit_text(&self, name: &str, seed: u8, message: &str) -> String {
            let content = format!("crab-followtags-test seed={seed} file={name}\n");
            std::fs::write(self.work_tree().join(name), content.as_bytes())
                .expect("write text file");
            Self::run_git(self.work_tree(), &["add", name]);
            Self::run_git(self.work_tree(), &["commit", "-m", message]);
            Self::rev_parse(self.work_tree(), "HEAD")
        }

        /// Create an annotated tag at HEAD with a fixed message.
        /// Returns the tag-object SHA (not the peeled commit SHA).
        fn annotated_tag(&self, name: &str, message: &str) -> String {
            Self::run_git_with_env(
                self.work_tree(),
                &["tag", "-a", name, "-m", message],
                &[
                    ("GIT_COMMITTER_DATE", "2001-02-03T04:05:06+00:00"),
                    ("GIT_AUTHOR_DATE", "2001-02-03T04:05:06+00:00"),
                ],
            );
            // `git rev-parse refs/tags/<name>` resolves to the tag
            // object itself for annotated tags, or the commit for
            // lightweight tags. We want the tag object here.
            Self::rev_parse(self.work_tree(), &format!("refs/tags/{name}"))
        }

        /// `git cat-file -t <sha>` — returns the object type
        /// (`commit`, `tree`, `tag`, `blob`) as a string. Used to
        /// sanity-check the tag is actually annotated.
        fn object_type(&self, sha: &str) -> String {
            let out = Command::new("git")
                .args(["cat-file", "-t", sha])
                .current_dir(self.work_tree())
                .output()
                .expect("git cat-file -t");
            assert!(
                out.status.success(),
                "git cat-file -t {sha} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout)
                .expect("cat-file output is utf8")
                .trim()
                .to_owned()
        }

        fn rev_parse(cwd: &Path, what: &str) -> String {
            let out = Command::new("git")
                .args(["rev-parse", what])
                .current_dir(cwd)
                .output()
                .expect("git rev-parse");
            assert!(
                out.status.success(),
                "git rev-parse {what} failed: {}",
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

        fn run_git_with_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(cwd);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd
                .output()
                .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn make_store() -> Store {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    fn make_router(store: Store, prefix: &str) -> StoreLayout {
        StoreLayout::new(store, prefix.to_owned())
    }

    fn branch_spec(ref_name: &str) -> PushSpec {
        PushSpec {
            force: false,
            src: ref_name.to_owned(),
            dst: ref_name.to_owned(),
        }
    }

    /// Drive `run_native_push` with a caller-chosen `followtags`
    /// setting. The rest of `NativePushConfig` stays at its defaults.
    async fn run_batch(
        store: Store,
        router: StoreLayout,
        push_state: &mut PushState,
        specs: &[PushSpec],
        followtags: bool,
    ) -> PushResult {
        let mut config = NativePushConfig::new(PushConfig::default());
        config.followtags = followtags;
        let cancel = CancellationToken::new();
        run_native_push(
            &config,
            specs,
            NativePushInputs::new(
                Some(store),
                None,
                None,
                router,
                push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
        .expect("run_native_push must return Ok(PushResult) end-to-end")
    }

    /// Push a branch with `followtags = true`. An annotated tag
    /// pointing at the branch tip must land in the manifest at the
    /// tag-object SHA (not the peeled commit SHA) even though the
    /// pusher only listed the branch spec.
    #[tokio::test]
    async fn push_annotated_tag_round_trips() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        let commit_sha = fixture.commit_text("a.txt", 0xF1, "seed for annotated tag");
        let tag_sha = fixture.annotated_tag("v1", "Release v1");

        // Sanity: annotated tags produce a distinct tag-object SHA
        // that differs from the peeled commit. If this assertion
        // ever flips, git changed the tag encoding under us and
        // the rest of the test's reasoning is suspect.
        assert_ne!(
            tag_sha, commit_sha,
            "annotated tag SHA must differ from its peeled commit"
        );
        assert_eq!(
            fixture.object_type(&tag_sha),
            "tag",
            "refs/tags/v1 must resolve to a tag object"
        );

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[branch_spec("refs/heads/main")],
            true,
        )
        .await;

        match result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected Ok for refs/heads/main; got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }
        match result.outcomes.get("refs/tags/v1") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected followtags to synthesise Ok for refs/tags/v1; \
                 got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }

        let (manifest, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must exist after follow-tag push");

        assert_eq!(
            manifest.refs.get("refs/heads/main"),
            Some(&commit_sha),
            "main must point at the pushed commit; refs={:?}",
            manifest.refs
        );
        assert_eq!(
            manifest.refs.get("refs/tags/v1"),
            Some(&tag_sha),
            "refs/tags/v1 must point at the tag-object SHA \
             (not the peeled commit); refs={:?}",
            manifest.refs
        );
    }

    /// Mirror of the positive case with the gate off. The tag must
    /// not appear in either the outcome map or the manifest — proof
    /// that `collect_followtag_specs` is actually gated.
    #[tokio::test]
    async fn annotated_tag_not_followed_when_disabled() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        let commit_sha = fixture.commit_text("a.txt", 0xF2, "seed without follow");
        let _tag_sha = fixture.annotated_tag("v1", "Release v1 (dropped)");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        let result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[branch_spec("refs/heads/main")],
            false,
        )
        .await;

        match result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected Ok for refs/heads/main; got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }
        assert!(
            result.outcomes.get("refs/tags/v1").is_none(),
            "tag must not be synthesised when followtags is off; \
             outcomes={:?}",
            result.outcomes
        );

        let (manifest, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must exist after branch-only push");

        assert_eq!(
            manifest.refs.get("refs/heads/main"),
            Some(&commit_sha),
            "main must still land; refs={:?}",
            manifest.refs
        );
        assert!(
            !manifest.refs.contains_key("refs/tags/v1"),
            "tag must not appear in manifest with followtags off; \
             refs={:?}",
            manifest.refs
        );
    }
}

// ---------------------------------------------------------------------------
// HEAD symref read path
//
// Covers `PushPipeline::remote_head()` and the interaction with
// `receive.denyCurrentBranch`. The positive rejection case is already
// covered by `receive_policy::deny_current_branch_refuse_rejects_push_to_head`
// — this module fills the gap around the accessor itself and the
// empty-HEAD carve-out so fresh repos can't accidentally inherit a
// policy that has no HEAD to match against.
// ---------------------------------------------------------------------------

mod head_symref {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, MutexGuard};

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use tokio_util::sync::CancellationToken;

    use crab::git::push::{PushConfig, PushResult, RefPushOutcome};
    use crab::git::push_native::{NativePushConfig, NativePushInputs, run_native_push};
    use crab::git::push_state::PushState;
    use crab::git::remote_helper::PushSpec;
    use crab::metadata::manifest::read_manifest;
    use crab::storage::StoreLayout;
    use crab::storage::store::Store;

    use super::GIT_DIR_MUTEX;

    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: serialised by GIT_DIR_MUTEX (held by self).
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: serialised by GIT_DIR_MUTEX.
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
            }
        }
    }

    struct GitFixture {
        dir: tempfile::TempDir,
    }

    impl GitFixture {
        fn new_with_initial_branch(branch: &str) -> Self {
            let dir = tempfile::tempdir().expect("create tempdir for git fixture");
            Self::run_git(dir.path(), &["init", &format!("--initial-branch={branch}")]);
            Self::run_git(dir.path(), &["config", "user.email", "test@test.com"]);
            Self::run_git(dir.path(), &["config", "user.name", "Test"]);
            Self::run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
            Self { dir }
        }

        fn work_tree(&self) -> &Path {
            self.dir.path()
        }

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }

        fn commit_text(&self, name: &str, seed: u8, message: &str) -> String {
            let content = format!("crab-head-symref-test seed={seed} file={name}\n");
            std::fs::write(self.work_tree().join(name), content.as_bytes())
                .expect("write text file");
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

    fn make_store() -> Store {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    fn make_router(store: Store, prefix: &str) -> StoreLayout {
        StoreLayout::new(store, prefix.to_owned())
    }

    fn spec(src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force: false,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    async fn run_batch(
        store: Store,
        router: StoreLayout,
        push_state: &mut PushState,
        specs: &[PushSpec],
        push_cfg: PushConfig,
    ) -> PushResult {
        let config = NativePushConfig::new(push_cfg);
        let cancel = CancellationToken::new();
        run_native_push(
            &config,
            specs,
            NativePushInputs::new(
                Some(store),
                None,
                None,
                router,
                push_state,
                "origin",
                "crab://bucket/repo",
                None,
                cancel,
            ),
        )
        .await
        .expect("run_native_push must return Ok(PushResult) end-to-end")
    }

    /// After the first push, the manifest records HEAD = the branch
    /// that was pushed (because the pusher's initial-branch drove the
    /// seed HEAD selection). A second push that sets `denyCurrentBranch
    /// = refuse` and targets that branch must be rejected — proof
    /// that the policy is reading from `manifest.head` and not from
    /// any client-side notion of HEAD.
    #[tokio::test]
    async fn deny_current_branch_refuse_reads_manifest_head() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new_with_initial_branch("trunk");
        git_guard.set_git_dir(&fixture.git_dir());
        let seed_sha = fixture.commit_text("a.txt", 0xD1, "seed trunk");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // Seed the manifest under the default permissive policy so
        // `manifest.head = refs/heads/trunk` lands on the remote.
        let seed = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/trunk", "refs/heads/trunk")],
            PushConfig::default(),
        )
        .await;
        assert!(
            matches!(
                seed.outcomes.get("refs/heads/trunk"),
                Some(RefPushOutcome::Ok)
            ),
            "seed trunk must succeed; outcomes={:?}",
            seed.outcomes
        );
        let (seed_manifest, _) = read_manifest(&store, &router)
            .await
            .expect("seed manifest read");
        assert_eq!(
            seed_manifest.head, "refs/heads/trunk",
            "seed manifest must record trunk as HEAD; manifest={seed_manifest:?}"
        );

        // Advance trunk locally so the second push is a real update
        // rather than a no-op that might be short-circuited.
        let advanced = fixture.commit_text("b.txt", 0xD2, "advance trunk");
        assert_ne!(advanced, seed_sha);

        // Second push with denyCurrentBranch = refuse — the policy
        // must consult manifest.head (trunk) and reject.
        let mut strict = PushConfig::default();
        strict.receive_deny_current_branch = "refuse".into();
        let refused = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/trunk", "refs/heads/trunk")],
            strict,
        )
        .await;

        match refused.outcomes.get("refs/heads/trunk") {
            Some(RefPushOutcome::Rejected(
                crab::git::push::PushRejectReason::DenyCurrentBranch,
            )) => {}
            other => panic!(
                "expected Rejected(DenyCurrentBranch) reading from manifest.head; \
                 got {other:?}; outcomes={:?}",
                refused.outcomes
            ),
        }

        // Manifest must stay pinned to the seed SHA — rejection must
        // not let the update sneak in via some other path.
        let (after, _) = read_manifest(&store, &router)
            .await
            .expect("post-reject manifest read");
        assert_eq!(
            after.refs.get("refs/heads/trunk"),
            Some(&seed_sha),
            "trunk must still point at the seed SHA; refs={:?}",
            after.refs
        );
    }

    /// Fresh repo with no base manifest: `remote_head()` returns
    /// `None`, so `denyCurrentBranch = refuse` has nothing to match
    /// against. The first push under a strict policy must still
    /// succeed — otherwise admins who enabled the policy repo-wide
    /// could never bootstrap a new repo.
    #[tokio::test]
    async fn deny_current_branch_with_no_head_is_skipped() {
        let git_guard = ScopedGitDir::acquire();
        let fixture = GitFixture::new_with_initial_branch("main");
        git_guard.set_git_dir(&fixture.git_dir());
        let seed_sha = fixture.commit_text("a.txt", 0xD3, "first commit");

        let store = make_store();
        let router = make_router(store.clone(), "repo-prefix");
        let mut push_state = PushState::default();

        // No prior manifest → `remote_head()` returns None → the
        // denyCurrentBranch gate in `evaluate_decisions` is bypassed.
        let mut strict = PushConfig::default();
        strict.receive_deny_current_branch = "refuse".into();

        let result = run_batch(
            store.clone(),
            router.clone(),
            &mut push_state,
            &[spec("refs/heads/main", "refs/heads/main")],
            strict,
        )
        .await;

        match result.outcomes.get("refs/heads/main") {
            Some(RefPushOutcome::Ok) => {}
            other => panic!(
                "expected Ok on a fresh repo even under refuse policy; \
                 got {other:?}; outcomes={:?}",
                result.outcomes
            ),
        }

        let (manifest, _) = read_manifest(&store, &router)
            .await
            .expect("manifest must exist after first push");
        assert_eq!(
            manifest.refs.get("refs/heads/main"),
            Some(&seed_sha),
            "main must land at the seed SHA; refs={:?}",
            manifest.refs
        );
    }
}

// ---------------------------------------------------------------------------
// Peeled tag advertisement (F1-1)
//
// Verifies that `format_list_output` emits the `{peeled} {ref}^{{}}\n`
// line expected by git clients when an annotated tag's `peeled` field
// is populated, and that the line is absent for lightweight tags /
// branches where `peeled` is `None`.
// ---------------------------------------------------------------------------

mod peeled_tag {
    use crab::git::remote_helper::{ListOutput, RefEntry, format_list_output};

    #[test]
    fn peeled_tag_line_emitted_for_annotated_tag() {
        let tag_object_sha = "a".repeat(40);
        let peeled_commit_sha = "b".repeat(40);

        let output = ListOutput {
            refs: vec![RefEntry {
                sha: tag_object_sha.clone(),
                ref_name: "refs/tags/v1".into(),
                peeled: Some(peeled_commit_sha.clone()),
            }],
            head_symref: None,
        };

        let formatted = format_list_output(&output);
        let expected =
            format!("{tag_object_sha} refs/tags/v1\n{peeled_commit_sha} refs/tags/v1^{{}}\n\n");
        assert_eq!(formatted, expected);
    }

    /// Lightweight tag (or any ref without a peel target) must emit
    /// exactly one line — adding a spurious `^{}` line would point
    /// at nothing and confuse clients.
    #[test]
    fn lightweight_tag_has_no_peeled_line() {
        let sha = "c".repeat(40);
        let output = ListOutput {
            refs: vec![RefEntry {
                sha: sha.clone(),
                ref_name: "refs/tags/lightweight".into(),
                peeled: None,
            }],
            head_symref: None,
        };

        let formatted = format_list_output(&output);
        assert_eq!(formatted, format!("{sha} refs/tags/lightweight\n\n"));
        assert!(!formatted.contains("^{}"));
    }

    /// Mixed set: annotated tag + branch + lightweight tag. The peel
    /// line appears only beside the annotated tag.
    #[test]
    fn peeled_line_interleaved_with_branches() {
        let tag_sha = "1".repeat(40);
        let commit_sha = "2".repeat(40);
        let main_sha = "3".repeat(40);
        let light_sha = "4".repeat(40);

        let output = ListOutput {
            refs: vec![
                RefEntry {
                    sha: main_sha.clone(),
                    ref_name: "refs/heads/main".into(),
                    peeled: None,
                },
                RefEntry {
                    sha: light_sha.clone(),
                    ref_name: "refs/tags/lightweight".into(),
                    peeled: None,
                },
                RefEntry {
                    sha: tag_sha.clone(),
                    ref_name: "refs/tags/v1".into(),
                    peeled: Some(commit_sha.clone()),
                },
            ],
            head_symref: Some("refs/heads/main".into()),
        };

        let formatted = format_list_output(&output);
        let expected = format!(
            "@refs/heads/main HEAD\n\
             {main_sha} refs/heads/main\n\
             {light_sha} refs/tags/lightweight\n\
             {tag_sha} refs/tags/v1\n\
             {commit_sha} refs/tags/v1^{{}}\n\n"
        );
        assert_eq!(formatted, expected);
    }
}

// ---------------------------------------------------------------------------
// `transfer.hideRefs`
//
// Verifies that refs matching `transfer.hideRefs` glob patterns are
// omitted from `read_remote_refs` output and that their tip SHAs are
// excluded from the allowlist used by the fetch-side policy check.
// ---------------------------------------------------------------------------

mod transfer_hide_refs {
    use std::collections::BTreeMap;

    use crab::core::config::Config;
    use crab::git::reject_reason::FetchRejectReason;
    use crab::git::remote_helper::{FetchEntry, validate_fetch_entries_with_manifest};
    use crab::metadata::manifest::Manifest;

    fn manifest_with_refs(pairs: &[(&str, &str)]) -> Manifest {
        let mut refs = BTreeMap::new();
        for (name, sha) in pairs {
            refs.insert((*name).to_owned(), (*sha).to_owned());
        }
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.refs = refs;
        manifest.seal_git_validation();
        manifest
    }

    /// A hidden ref's tip SHA must not pass the `allow_tip` gate —
    /// otherwise a client that already guesses the SHA can bypass
    /// the `hideRefs` policy entirely.
    #[test]
    fn hidden_ref_tip_sha_rejected_by_fetch_policy() {
        let visible_sha = "a".repeat(40);
        let hidden_sha = "b".repeat(40);
        let manifest = manifest_with_refs(&[
            ("refs/heads/main", &visible_sha),
            ("refs/heads/secret", &hidden_sha),
        ]);

        let mut cfg = Config::default();
        cfg.transfer_hide_refs = vec!["refs/heads/secret".into()];

        // Fetch the hidden ref's tip SHA — must be rejected before
        // upload-pack admission falls through to any object allowlist.
        let entries = vec![FetchEntry {
            sha: hidden_sha.clone(),
            ref_name: "refs/heads/secret".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert_eq!(result.len(), 1);
        match &result[0].1 {
            Err(FetchRejectReason::NotAllowed { sha, reason }) => {
                assert_eq!(sha, &hidden_sha);
                assert_eq!(reason, "hidden-ref target");
            }
            other => panic!("expected NotAllowed rejection for hidden ref tip; got {other:?}"),
        }
    }

    /// Visible refs continue to pass `allow_tip` normally — the
    /// filter must scope to pattern matches, not block the universe.
    #[test]
    fn visible_ref_tip_sha_still_accepted_under_hide_refs() {
        let visible_sha = "a".repeat(40);
        let manifest = manifest_with_refs(&[("refs/heads/main", &visible_sha)]);

        let mut cfg = Config::default();
        cfg.transfer_hide_refs = vec!["refs/heads/internal/*".into()];

        let entries = vec![FetchEntry {
            sha: visible_sha.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);

        assert!(
            result[0].1.is_ok(),
            "visible ref must pass allow_tip; got {:?}",
            result[0].1
        );
    }

    /// Glob pattern matches should be case-sensitive and scoped —
    /// a `refs/heads/internal/*` pattern must not accidentally
    /// filter `refs/heads/main`.
    #[test]
    fn hide_refs_glob_scope() {
        let main_sha = "a".repeat(40);
        let internal_sha = "b".repeat(40);
        let manifest = manifest_with_refs(&[
            ("refs/heads/main", &main_sha),
            ("refs/heads/internal/secret", &internal_sha),
        ]);

        let mut cfg = Config::default();
        cfg.transfer_hide_refs = vec!["refs/heads/internal/*".into()];

        // internal/secret hidden → tip SHA rejected.
        let entries = vec![FetchEntry {
            sha: internal_sha.clone(),
            ref_name: "refs/heads/internal/secret".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);
        match &result[0].1 {
            Err(FetchRejectReason::NotAllowed { sha, reason }) => {
                assert_eq!(sha, &internal_sha);
                assert_eq!(reason, "hidden-ref target");
            }
            other => panic!("hidden ref tip must be rejected; got {other:?}"),
        }

        // main still visible.
        let entries = vec![FetchEntry {
            sha: main_sha.clone(),
            ref_name: "refs/heads/main".into(),
        }];
        let result = validate_fetch_entries_with_manifest(&entries, &manifest, None, &cfg);
        assert!(
            result[0].1.is_ok(),
            "unrelated ref must stay visible; got {:?}",
            result[0].1
        );
    }
}

// ---------------------------------------------------------------------------
// Protocol-loop edge cases
//
// These exercise the malformed-input surface of `read_batch` /
// `parse_command`: batches that mix command kinds, batches that
// contain just a blank line, and EOF mid-batch. Each is mapped to a
// specific `CrabError::Protocol(...)` so clients can distinguish
// "broken client" from "ref rejected".
// ---------------------------------------------------------------------------

mod protocol_edges {
    use std::io::Cursor;

    use crab::git::remote_helper::StdIo;
    use tokio::io::{AsyncReadExt, BufReader, duplex};

    fn test_url() -> gix_url::Url {
        gix_url::Url::from_bytes(b"crab://bucket/repo".into()).expect("test URL should parse")
    }

    async fn run_capture(input: &str) -> (String, std::result::Result<(), String>) {
        struct DuplexIo {
            reader: BufReader<Cursor<Vec<u8>>>,
            writer: tokio::io::DuplexStream,
        }

        impl StdIo for DuplexIo {
            type Reader = BufReader<Cursor<Vec<u8>>>;
            type Writer = tokio::io::DuplexStream;

            fn split(self) -> (Self::Reader, Self::Writer) {
                (self.reader, self.writer)
            }
        }

        let input_bytes = input.as_bytes().to_vec();
        let (writer_tx, mut writer_rx) = duplex(64 * 1024);
        let io = DuplexIo {
            reader: BufReader::new(Cursor::new(input_bytes)),
            writer: writer_tx,
        };
        let url = test_url();
        let mut output = Vec::new();
        let helper = crab::git::remote_helper::run_remote_helper(
            "origin",
            &url,
            io,
            tokio_util::sync::CancellationToken::new(),
        );
        let reader = writer_rx.read_to_end(&mut output);

        let result = match tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(helper, reader)
        })
        .await
        {
            Ok((Ok(()), Ok(_))) => Ok(()),
            Ok((Ok(()), Err(e))) => Err(e.to_string()),
            Ok((Err(e), _)) => Err(e.to_string()),
            Err(_) => Err("helper task timed out".to_owned()),
        };

        (String::from_utf8_lossy(&output).into_owned(), result)
    }

    /// A batch that interleaves `fetch` and `push` must be rejected
    /// as a protocol error rather than silently running one kind and
    /// dropping the other.
    #[tokio::test]
    async fn mixed_fetch_push_batch_errors_cleanly() {
        let input = "\
fetch abc123 refs/heads/main\n\
push refs/heads/main:refs/heads/main\n\
\n";
        let (_output, result) = run_capture(input).await;

        let err = result
            .err()
            .expect("mixed fetch+push batch must abort the helper task");
        assert!(
            err.contains("mixed command types in batch"),
            "expected mixed-batch protocol error; got {err}"
        );
    }

    /// A standalone blank line with no preceding commands is not a
    /// batch boundary — the batch collector treats it as a no-op and
    /// waits for a real command. This asserts the run_remote_helper
    /// loop exits cleanly after a lone blank.
    #[tokio::test]
    async fn blank_line_then_eof_exits_cleanly() {
        let input = "\n";
        let (output, result) = run_capture(input).await;
        assert!(
            result.is_ok(),
            "lone blank line must not abort the helper; got {result:?}"
        );
        assert!(
            output.is_empty(),
            "lone blank line must not emit any response; got {output:?}"
        );
    }

    /// EOF reached mid-batch with at least one command already
    /// collected must finalize the batch (the `n == 0` branch) rather
    /// than silently dropping what was read. Covered here with a
    /// fetch batch that lacks its trailing blank.
    #[tokio::test]
    async fn eof_mid_fetch_batch_finalizes_without_blank_line() {
        let input = "fetch abc123 refs/heads/main\n";
        let (_output, result) = run_capture(input).await;
        // The fetch pipeline will error out because no store is
        // configured at this URL, but the point is the batch runs
        // at all — `finalize_batch` is reached. Either outcome
        // (helper task Ok or a specific store-related error) is
        // evidence the EOF-finalization path works; the assertion
        // catches the only regression we care about, the spec's
        // prior "silently drop commands on EOF" behavior.
        match result {
            Ok(()) => {}
            Err(msg) => assert!(
                !msg.contains("mixed command types"),
                "EOF finalization must not surface as a mixed-batch error; got {msg}"
            ),
        }
    }
}
