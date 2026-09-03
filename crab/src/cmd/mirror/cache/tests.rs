use std::fs::File;

use fs4::fs_std::FileExt;

use super::super::tests::{base_args, test_options};
use super::super::{ProcessCommand, ProcessOutput, SystemCommandRunner};
use super::*;
use crate::core::output::OutputMode;
use crab_cache::lifecycle::CacheCleanGuard;

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Mirror test",
            "-c",
            "user.email=mirror@example.invalid",
        ])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn refs(path: &Path) -> String {
    git(path, &["for-each-ref", "--format=%(refname) %(objectname)"])
}

fn source(path: &Path) {
    std::fs::create_dir(path).unwrap();
    git(path, &["init", "-b", "main"]);
    std::fs::write(path.join("payload"), b"mirror cached bytes\n").unwrap();
    git(path, &["add", "payload"]);
    git(path, &["commit", "-m", "initial"]);
    git(path, &["branch", "branch/other"]);
    git(path, &["tag", "-a", "version-one", "-m", "annotated"]);
    git(path, &["update-ref", "refs/notes/example", "HEAD"]);
}

#[test]
fn rebuild_preserves_nested_lock_identity_and_matches_native_mirror_refs() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    source(&source_path);
    let cache_path = temp.path().join("cache.git");
    let nested = cache_path.join("objects/nested");
    let cancel = CancellationToken::new();
    let child = CacheUseGuard::acquire(&nested, &cancel).unwrap();
    let marker = nested.with_file_name("nested.crab-cache-use.lock");
    let opened_before_cleanup = File::open(&marker).unwrap();
    drop(child);
    let mut args = base_args(cache_path.clone());
    args.source = source_path.to_string_lossy().into_owned();

    for _ in 0..2 {
        CacheCleanGuard::acquire(&cache_path, &cancel)
            .unwrap()
            .clean(&cancel)
            .unwrap();
        let owner = CacheUseGuard::acquire(&cache_path, &cancel).unwrap();
        assert!(
            prepare_cache(
                &args,
                &owner,
                &cancel,
                &test_options(),
                &mut SystemCommandRunner::default()
            )
            .unwrap()
        );
        assert_eq!(refs(owner.path()), refs(&source_path));
        assert_eq!(
            git(owner.path(), &["symbolic-ref", "HEAD"]),
            "refs/heads/main"
        );
        git(owner.path(), &["fsck", "--strict", "--full"]);
        drop(owner);
        assert!(FileExt::try_lock_exclusive(&opened_before_cleanup).unwrap());
        assert!(matches!(CacheUseGuard::acquire(&nested, &cancel),
            Err(crab_cache::CacheError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock));
        FileExt::unlock(&opened_before_cleanup).unwrap();
    }
}

#[test]
fn detached_head_is_fetched_without_publishing_an_extra_ref() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    source(&source_path);
    git(&source_path, &["checkout", "--detach"]);
    git(
        &source_path,
        &["commit", "--allow-empty", "-m", "detached-only"],
    );
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = source_path.to_string_lossy().into_owned();
    prepare_cache(
        &args,
        &owner,
        &cancel,
        &test_options(),
        &mut SystemCommandRunner::default(),
    )
    .unwrap();
    assert_eq!(
        git(owner.path(), &["rev-parse", "HEAD"]),
        git(&source_path, &["rev-parse", "HEAD"])
    );
    assert_eq!(refs(owner.path()), refs(&source_path));
    git(owner.path(), &["fsck", "--strict", "--full"]);
}

#[test]
fn canonical_local_source_uses_a_file_transport_without_losing_path_identity() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source # % name");
    source(&source_path);
    let source_path = source_path.canonicalize().unwrap();
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache # %.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = source_path.to_str().unwrap().to_owned();
    prepare_cache(
        &args,
        &owner,
        &cancel,
        &test_options(),
        &mut SystemCommandRunner::default(),
    )
    .unwrap();
    let configured = git(owner.path(), &["config", "--get", "remote.origin.url"]);
    assert_eq!(
        configured,
        url::Url::from_file_path(&source_path).unwrap().as_str()
    );
    assert_eq!(refs(owner.path()), refs(&source_path));
    assert_eq!(
        super::super::hook::mirror_hook_status(Path::new(&args.source)).state,
        super::super::types::MirrorHookState::Missing
    );
    git(owner.path(), &["fsck", "--strict", "--full"]);
}

#[test]
fn unrelated_nonempty_directory_is_not_initialized_or_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("cache.git");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("keep"), b"unrelated").unwrap();
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&path, &cancel).unwrap();
    assert!(
        prepare_cache(
            &base_args(path.clone()),
            &owner,
            &cancel,
            &test_options(),
            &mut SystemCommandRunner::default()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"unrelated");
    assert!(!path.join("HEAD").exists());
}

struct InterruptedConfig;

impl CommandRunner for InterruptedConfig {
    fn run(&mut self, command: &ProcessCommand, mode: OutputMode) -> Result<ProcessOutput> {
        if command.args.iter().any(|arg| arg == "remote.origin.fetch") {
            return Err(CrabError::Cancelled);
        }
        SystemCommandRunner::default().run(command, mode)
    }
}

#[test]
fn interrupted_initialization_resumes_through_the_same_refresh_path() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    source(&source_path);
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = source_path.to_string_lossy().into_owned();
    assert!(matches!(
        prepare_cache(
            &args,
            &owner,
            &cancel,
            &test_options(),
            &mut InterruptedConfig
        ),
        Err(CrabError::Cancelled)
    ));
    assert!(
        !prepare_cache(
            &args,
            &owner,
            &cancel,
            &test_options(),
            &mut SystemCommandRunner::default()
        )
        .unwrap()
    );
    assert_eq!(refs(owner.path()), refs(&source_path));
}

#[test]
fn empty_source_initializes_then_refreshes_when_its_first_ref_appears() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    std::fs::create_dir(&source_path).unwrap();
    git(&source_path, &["init", "-b", "new-main"]);
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = source_path.to_string_lossy().into_owned();
    assert!(
        prepare_cache(
            &args,
            &owner,
            &cancel,
            &test_options(),
            &mut SystemCommandRunner::default()
        )
        .unwrap()
    );
    assert!(refs(owner.path()).is_empty());
    git(&source_path, &["commit", "--allow-empty", "-m", "first"]);
    assert!(
        !prepare_cache(
            &args,
            &owner,
            &cancel,
            &test_options(),
            &mut SystemCommandRunner::default()
        )
        .unwrap()
    );
    assert_eq!(refs(owner.path()), refs(&source_path));
    assert_eq!(
        git(owner.path(), &["symbolic-ref", "HEAD"]),
        "refs/heads/new-main"
    );
}

#[test]
fn refresh_prunes_source_deletions_and_fetches_forced_tag_updates() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    source(&source_path);
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = source_path.to_string_lossy().into_owned();
    prepare_cache(
        &args,
        &owner,
        &cancel,
        &test_options(),
        &mut SystemCommandRunner::default(),
    )
    .unwrap();
    git(&source_path, &["branch", "-D", "branch/other"]);
    git(&source_path, &["commit", "--allow-empty", "-m", "next"]);
    git(&source_path, &["tag", "-f", "version-one"]);
    git(
        owner.path(),
        &[
            "config",
            "--add",
            "remote.origin.url",
            "https://invalid.example/unused",
        ],
    );
    git(
        owner.path(),
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/extra/*",
        ],
    );
    prepare_cache(
        &args,
        &owner,
        &cancel,
        &test_options(),
        &mut SystemCommandRunner::default(),
    )
    .unwrap();
    assert_eq!(refs(owner.path()), refs(&source_path));
    assert_eq!(
        git(
            owner.path(),
            &["config", "--get-all", "remote.origin.fetch"]
        ),
        "+refs/*:refs/*"
    );
    git(owner.path(), &["fsck", "--strict", "--full"]);
}

#[test]
fn fetch_success_cannot_hide_rejected_shallow_source_refs() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    source(&source_path);
    git(&source_path, &["commit", "--allow-empty", "-m", "next"]);
    let shallow = temp.path().join("shallow");
    git(
        temp.path(),
        &[
            "clone",
            "--depth=1",
            "--no-local",
            source_path.to_str().unwrap(),
            shallow.to_str().unwrap(),
        ],
    );
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = shallow.to_string_lossy().into_owned();
    let result = prepare_cache(
        &args,
        &owner,
        &cancel,
        &test_options(),
        &mut SystemCommandRunner::default(),
    );
    assert!(
        result.is_err(),
        "a partial source must not be treated as an empty, fully mirrored source"
    );
}

struct MovingSource<'a>(&'a Path);

impl CommandRunner for MovingSource<'_> {
    fn run(&mut self, command: &ProcessCommand, mode: OutputMode) -> Result<ProcessOutput> {
        if command.args.first().is_some_and(|arg| arg == "fetch") {
            git(
                self.0,
                &["commit", "--allow-empty", "-m", "concurrent source update"],
            );
        }
        SystemCommandRunner::default().run(command, mode)
    }
}

#[test]
fn changed_source_advertisement_requires_a_new_inspection() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    source(&source_path);
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&temp.path().join("cache.git"), &cancel).unwrap();
    let mut args = base_args(owner.path().to_owned());
    args.source = source_path.to_string_lossy().into_owned();
    assert!(
        prepare_cache(
            &args,
            &owner,
            &cancel,
            &test_options(),
            &mut MovingSource(&source_path)
        )
        .is_err()
    );
    prepare_cache(
        &args,
        &owner,
        &cancel,
        &test_options(),
        &mut SystemCommandRunner::default(),
    )
    .unwrap();
    assert_eq!(refs(owner.path()), refs(&source_path));
}
