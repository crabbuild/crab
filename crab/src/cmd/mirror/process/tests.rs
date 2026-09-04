use super::*;
use crate::core::error::CrabError;
use crate::git::process::CANCELLATION_GRACE;
use crab_cache::lifecycle::{CacheCleanGuard, CacheUseGuard};
use std::io::{self, Read as _};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE: &str = "cmd::mirror::process::tests::child_fixture";
const MODE: &str = "CRAB_MIRROR_PROCESS_TEST";
const ROOT: &str = "CRAB_MIRROR_PROCESS_TEST_ROOT";

#[cfg(unix)]
static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn mark_terminated(_: libc::c_int) {
    TERMINATED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn command(mode: &str, root: &Path) -> ProcessCommand {
    ProcessCommand::new(
        std::env::current_exe().unwrap().to_string_lossy(),
        vec![
            "--exact".to_owned(),
            FIXTURE.to_owned(),
            "--nocapture".to_owned(),
        ],
    )
    .env(MODE, mode.into())
    .env(ROOT, root.as_os_str().to_owned())
}

fn wait_for(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "missing {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn child_fixture() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
    match mode.as_str() {
        "duplex" => {
            io::stdout().write_all(&vec![b'o'; 1024 * 1024]).unwrap();
            io::stderr().write_all(&vec![b'e'; 1024 * 1024]).unwrap();
            let mut input = String::new();
            io::stdin().read_to_string(&mut input).unwrap();
            println!("INPUT_BYTES={}", input.len());
        }
        "flood-stdout" | "flood-stderr" => {
            let mut output: Box<dyn io::Write> = if mode == "flood-stdout" {
                Box::new(io::stdout())
            } else {
                Box::new(io::stderr())
            };
            loop {
                output.write_all(&[b'x'; 8192]).unwrap();
            }
        }
        "writer" => {
            std::fs::write(root.join("writer.pid"), std::process::id().to_string()).unwrap();
            loop {
                std::fs::write(root.join("heartbeat"), format!("{:?}", Instant::now())).unwrap();
                thread::sleep(Duration::from_millis(10));
            }
        }
        "tree" | "orphan" => {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", FIXTURE, "--nocapture"])
                .env(MODE, "writer")
                .stdin(Stdio::null())
                .stdout(if mode == "tree" {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .stderr(if mode == "tree" {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .spawn()
                .unwrap();
            wait_for(&root.join("heartbeat"));
            std::fs::write(root.join("ready"), b"ready").unwrap();
            if mode == "tree" {
                child.wait().unwrap();
            }
        }
        "sleep" => {
            std::fs::write(root.join("ready"), b"ready").unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        #[cfg(unix)]
        "grace" | "ignore-term" => {
            let handler = if mode == "grace" {
                mark_terminated as *const () as libc::sighandler_t
            } else {
                libc::SIG_IGN
            };
            // SAFETY: only this isolated fixture's signal disposition changes;
            // the handler uses a lock-free atomic, not allocation or I/O.
            unsafe {
                libc::signal(libc::SIGTERM, handler);
            }
            std::fs::write(root.join("ready"), b"ready").unwrap();
            while !TERMINATED.load(std::sync::atomic::Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }
            thread::sleep(Duration::from_millis(150));
            std::fs::write(root.join("closed"), b"durable shutdown").unwrap();
        }
        "exit-code" => std::process::exit(23),
        "reject-input" => {
            // Close the read end while staying alive. A failed stdin writer
            // must trigger cleanup instead of waiting on this child's sleep.
            #[cfg(unix)]
            {
                // SAFETY: this isolated fixture deliberately closes only its
                // own stdin descriptor; no other thread uses that descriptor.
                unsafe {
                    libc::close(libc::STDIN_FILENO);
                }
            }
            #[cfg(windows)]
            {
                std::process::exit(7);
            }
            thread::sleep(Duration::from_secs(30));
        }
        other => panic!("unknown fixture {other}"),
    }
}

#[test]
fn drains_both_output_pipes_while_feeding_large_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = command("duplex", dir.path()).stdin("i".repeat(2 * 1024 * 1024));
    let cancel = CancellationToken::new();
    let deadline = cancel.clone();
    let (done, receiver) = mpsc::channel::<()>();
    let timeout = thread::spawn(move || {
        if receiver.recv_timeout(Duration::from_secs(15)).is_err() {
            deadline.cancel();
        }
    });
    let result = SystemCommandRunner::new(cancel).run(&cmd, OutputMode::Json);
    let _ = done.send(());
    timeout.join().unwrap();
    let output = result.unwrap();
    assert!(output.status.success);
    assert!(output.stdout.contains("INPUT_BYTES=2097152"));
    assert!(output.stdout.contains(&"o".repeat(1024 * 1024)));
    assert_eq!(output.stderr, "e".repeat(1024 * 1024));
}

#[test]
fn output_capture_accepts_the_boundary_and_rejects_overflow() {
    assert_eq!(capture_output(&b"abcd"[..], 4).unwrap(), b"abcd");
    assert_eq!(
        capture_output(&b"abcde"[..], 4).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn output_overflow_stops_child_before_cache_ownership_returns() {
    for mode in ["flood-stdout", "flood-stderr"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.git");
        let cancel = CancellationToken::new();
        let owner = CacheUseGuard::acquire(&path, &cancel).unwrap();
        let result = SystemCommandRunner::new(cancel.clone())
            .run(&command(mode, dir.path()), OutputMode::Json);
        assert!(
            matches!(result, Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
        drop(owner);
        CacheUseGuard::acquire(&path, &cancel)
            .expect("output overflow must not leak cache ownership");
    }
}

#[test]
fn cancellation_interrupts_a_stalled_blob_stream_before_releasing_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.git");
    let cancel = CancellationToken::new();
    thread::scope(|scope| {
        let child_cancel = cancel.clone();
        let path = &path;
        let root = dir.path();
        let worker = scope.spawn(move || {
            let _owner = CacheUseGuard::acquire(path, &child_cancel).unwrap();
            // Git's shell alias emits only protocol bytes; Rust's test harness
            // prefixes stdout with status text and would fail before stalling.
            let command = ProcessCommand::new("git", vec![
                "-c".into(),
                "alias.stalled-blob=!read oid; printf '%s blob 1048576\\n' \"$oid\"; touch ready; sleep 30".into(),
                "stalled-blob".into(),
            ]).current_dir(Some(root)).verify_blobs(vec![crab_git::batch::BlobHeader {
                    oid: [3; 20],
                    size: 1024 * 1024,
                }]);
            SystemCommandRunner::new(child_cancel).run(&command, OutputMode::Json)
        });
        wait_for(&root.join("ready"));
        assert!(CacheUseGuard::acquire(path, &CancellationToken::new()).is_err());
        cancel.cancel();
        assert!(matches!(worker.join().unwrap(), Err(CrabError::Cancelled)));
    });
    CacheUseGuard::acquire(&path, &CancellationToken::new()).unwrap();
}

#[test]
fn native_blob_larger_than_capture_limit_is_streamed_and_hashed() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--bare", "--quiet"]);
    let size = MAX_CAPTURE_BYTES + 64 * 1024;
    let mut file = std::fs::File::create(dir.path().join("fixture.bin")).unwrap();
    for _ in 0..size / 65536 {
        file.write_all(&[0x80; 65536]).unwrap();
    }
    drop(file);
    let oid = git(&["hash-object", "-w", "fixture.bin"]);
    let blob = crab_git::batch::BlobHeader {
        oid: gix_hash::ObjectId::from_hex(oid.as_bytes())
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap(),
        size,
    };
    let command = ProcessCommand::new(
        "git",
        vec![
            "--no-replace-objects".into(),
            "--git-dir=.".into(),
            "cat-file".into(),
            "--batch".into(),
        ],
    )
    .current_dir(Some(dir.path()))
    .env_remove(super::super::GIT_ENV_REMOVALS)
    .verify_blobs(vec![blob]);
    let output = SystemCommandRunner::default()
        .run(&command, OutputMode::Json)
        .unwrap();
    assert!(output.status.success);
    assert!(output.stdout.is_empty());
    git(&["update-ref", "refs/tags/large-blob", &oid]);
    git(&["repack", "-adq"]);
    let packed = SystemCommandRunner::default()
        .run(&command, OutputMode::Json)
        .unwrap();
    assert!(packed.status.success);
    assert!(packed.stdout.is_empty());
}

#[test]
fn pre_cancelled_command_never_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        SystemCommandRunner::new(cancel).run(&command("sleep", dir.path()), OutputMode::Json),
        Err(CrabError::Cancelled)
    ));
    assert!(!dir.path().join("ready").exists());
}

fn assert_writer_stopped(root: &Path) {
    let before = std::fs::read(root.join("heartbeat")).unwrap();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(std::fs::read(root.join("heartbeat")).unwrap(), before);
}

#[test]
fn cancellation_stops_descendant_writer_before_cache_owner_returns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.git");
    std::fs::create_dir(&path).unwrap();
    let cancel = CancellationToken::new();
    thread::scope(|scope| {
        let worker_cancel = cancel.clone();
        let path = &path;
        let worker = scope.spawn(move || {
            let _owner = CacheUseGuard::acquire(path, &worker_cancel).unwrap();
            SystemCommandRunner::new(worker_cancel).run(&command("tree", path), OutputMode::Json)
        });
        wait_for(&path.join("ready"));
        assert!(CacheCleanGuard::acquire(path, &CancellationToken::new()).is_err());
        cancel.cancel();
        assert!(matches!(worker.join().unwrap(), Err(CrabError::Cancelled)));
    });
    let _clean = CacheCleanGuard::acquire(&path, &CancellationToken::new()).unwrap();
    assert_writer_stopped(&path);
}

#[test]
fn successful_leader_cannot_leave_a_detached_pipe_writer() {
    let dir = tempfile::tempdir().unwrap();
    let output = SystemCommandRunner::default()
        .run(&command("orphan", dir.path()), OutputMode::Json)
        .unwrap();
    assert!(output.status.success);
    assert_writer_stopped(dir.path());
}

#[test]
fn stdin_failure_stops_child_instead_of_waiting_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let result = SystemCommandRunner::default().run(
        &command("reject-input", dir.path()).stdin("i".repeat(2 * 1024 * 1024)),
        OutputMode::Json,
    );
    assert!(matches!(result, Err(CrabError::Io(_))));
    assert!(start.elapsed() < Duration::from_secs(15));
}

#[cfg(unix)]
#[test]
fn cancellation_allows_graceful_shutdown_then_escalates_an_unresponsive_child() {
    for mode in ["grace", "ignore-term"] {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        thread::scope(|scope| {
            let worker_cancel = cancel.clone();
            let root = dir.path();
            let worker = scope.spawn(move || {
                SystemCommandRunner::new(worker_cancel).run(&command(mode, root), OutputMode::Json)
            });
            wait_for(&root.join("ready"));
            let start = Instant::now();
            cancel.cancel();
            assert!(matches!(worker.join().unwrap(), Err(CrabError::Cancelled)));
            if mode == "grace" {
                assert!(root.join("closed").exists());
                assert!(start.elapsed() < CANCELLATION_GRACE);
            } else {
                assert!(start.elapsed() >= CANCELLATION_GRACE);
                assert!(start.elapsed() < CANCELLATION_GRACE + Duration::from_secs(5));
            }
        });
    }
}

#[test]
fn cleanup_preserves_a_completed_childs_failure_status() {
    let dir = tempfile::tempdir().unwrap();
    let output = SystemCommandRunner::default()
        .run(&command("exit-code", dir.path()), OutputMode::Json)
        .unwrap();
    assert_eq!(
        output.status,
        ProcessStatus {
            success: false,
            code: Some(23)
        }
    );
}
