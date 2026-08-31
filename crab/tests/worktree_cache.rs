use std::path::Path;
use std::process::{Command, Output};

use crab::cache::{HydratedPointerCache, hydrated_pointer};
use crab_types::pointer::Pointer;

fn run_git<I, S>(cwd: &Path, args: I) -> Option<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
}

fn fixture() -> Option<tempfile::TempDir> {
    let tmp = tempfile::tempdir().ok()?;
    let repo = tmp.path().join("repo");
    if !Command::new("git")
        .args(["init", "-q", repo.to_str()?])
        .status()
        .ok()?
        .success()
    {
        return None;
    }
    run_git(&repo, ["config", "user.email", "worktree@crab.dev"])?;
    run_git(&repo, ["config", "user.name", "crab-worktree"])?;
    std::fs::write(repo.join("model.bin"), b"main model\n").ok()?;
    run_git(&repo, ["add", "model.bin"])?;
    let commit = run_git(&repo, ["commit", "-qm", "main"])?;
    if !commit.status.success() {
        return None;
    }

    let linked = tmp.path().join("linked");
    let add = run_git(
        &repo,
        [
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str()?,
            "HEAD",
        ],
    )?;
    if !add.status.success() {
        return None;
    }
    Some(tmp)
}

fn entry(path: &Path) -> (crab::cache::HydratedEntry, Vec<u8>) {
    let content = std::fs::read(path).expect("read hydrated content");
    let pointer = Pointer {
        file_hash: *blake3::hash(&content).as_bytes(),
        size: content.len() as u64,
        shard_hint: None,
    }
    .serialize();
    let entry = hydrated_pointer::entry_for_path(path, &pointer).expect("cacheable entry");
    (entry, pointer)
}

#[test]
fn hydrated_cache_path_is_worktree_scoped_for_main_and_linked_worktrees() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();

    let main_cache = hydrated_pointer::cache_path_for_worktree_root(&repo).expect("main cache");
    let linked_cache =
        hydrated_pointer::cache_path_for_worktree_root(&linked).expect("linked cache");

    assert_eq!(
        main_cache,
        repo.join(".crab")
            .join("worktrees")
            .join("main")
            .join("hydrated-pointers-v1.sqlite")
    );
    assert_eq!(
        linked_cache,
        repo.join(".crab")
            .join("worktrees")
            .join("linked")
            .join("hydrated-pointers-v1.sqlite")
    );
    assert_ne!(main_cache, linked_cache);
}

#[test]
#[cfg(unix)]
fn same_relative_path_uses_independent_hydrated_cache_per_worktree() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let linked = tmp.path().join("linked").canonicalize().unwrap();

    let checkout = run_git(&linked, ["checkout", "-q", "-b", "feature"]).expect("checkout");
    if !checkout.status.success() {
        eprintln!("SKIP: failed to create linked branch");
        return;
    }
    std::fs::write(linked.join("model.bin"), b"linked model\n").expect("write linked model");
    run_git(&linked, ["add", "model.bin"]).expect("git add");
    let commit = run_git(&linked, ["commit", "-qm", "linked"]).expect("commit");
    if !commit.status.success() {
        eprintln!("SKIP: linked commit failed");
        return;
    }

    let main_cache = hydrated_pointer::cache_path_for_worktree_root(&repo).expect("main cache");
    let linked_cache =
        hydrated_pointer::cache_path_for_worktree_root(&linked).expect("linked cache");

    let (main_entry, main_pointer) = entry(&repo.join("model.bin"));
    let (linked_entry, linked_pointer) = entry(&linked.join("model.bin"));
    HydratedPointerCache::update_on_disk(&main_cache, [("model.bin".to_owned(), main_entry)])
        .expect("write main cache");
    HydratedPointerCache::update_on_disk(&linked_cache, [("model.bin".to_owned(), linked_entry)])
        .expect("write linked cache");

    let main = HydratedPointerCache::load_sync(&main_cache);
    let linked = HydratedPointerCache::load_sync(&linked_cache);

    assert_eq!(
        main.get("model.bin")
            .and_then(hydrated_pointer::decode_pointer),
        Some(main_pointer)
    );
    assert_eq!(
        linked
            .get("model.bin")
            .and_then(hydrated_pointer::decode_pointer),
        Some(linked_pointer)
    );
}

#[test]
#[cfg(unix)]
fn legacy_shared_hydrated_cache_is_ignored_and_not_rewritten() {
    let Some(tmp) = fixture() else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    let repo = tmp.path().join("repo").canonicalize().unwrap();
    let legacy = repo.join(".crab").join("hydrated-pointers.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).expect("create legacy parent");
    std::fs::write(&legacy, b"legacy sentinel").expect("write legacy cache");

    let canonical = hydrated_pointer::cache_path_for_worktree_root(&repo).expect("main cache");
    assert_ne!(legacy, canonical);

    let (entry, pointer) = entry(&repo.join("model.bin"));
    HydratedPointerCache::update_on_disk(&canonical, [("model.bin".to_owned(), entry)])
        .expect("write canonical cache");

    assert_eq!(
        std::fs::read(&legacy).expect("read legacy cache"),
        b"legacy sentinel"
    );
    assert_eq!(
        HydratedPointerCache::load_sync(&canonical)
            .get("model.bin")
            .and_then(hydrated_pointer::decode_pointer),
        Some(pointer)
    );
}
