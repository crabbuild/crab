use super::*;

struct TestRoot {
    _temp: tempfile::TempDir,
    path: PathBuf,
}

impl TestRoot {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cache");
        std::fs::create_dir(&path).unwrap();
        Self { _temp: temp, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn assert_busy<T>(result: Result<T>) {
    assert!(matches!(result, Err(CacheError::Io(error)) if error.kind() == ErrorKind::WouldBlock));
}

#[test]
fn directory_creation_and_refresh_share_one_exclusive_owner() {
    let dir = TestRoot::new();
    let path = dir.path().join("cache.git");
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&path, &cancel).unwrap();
    std::fs::create_dir(owner.path()).unwrap();
    assert_busy(CacheUseGuard::acquire(&path, &cancel));
    drop(owner);
    assert_eq!(
        CacheUseGuard::acquire(&path, &cancel).unwrap().path(),
        path.canonicalize().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn owner_release_is_not_delayed_by_an_inherited_descriptor() {
    let dir = TestRoot::new();
    let path = dir.path().join("cache.git");
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&path, &cancel).unwrap();
    // dup and fork share the same flock open-file description. Retain a
    // duplicate to model a concurrent child before its close-on-exec runs.
    let inherited = owner._lock.0.try_clone().unwrap();
    drop(owner);
    CacheUseGuard::acquire(&path, &cancel)
        .expect("an unrelated inherited descriptor must not extend cache ownership");
    drop(inherited);
}

#[cfg(unix)]
#[test]
fn cleaner_release_is_not_delayed_by_an_inherited_descriptor() {
    let dir = TestRoot::new();
    let cancel = CancellationToken::new();
    let cleaner = CacheCleanGuard::acquire(dir.path(), &cancel).unwrap();
    let inherited = cleaner._lock.0.try_clone().unwrap();
    drop(cleaner);
    CacheUseGuard::acquire(&dir.path().join("cache.git"), &cancel)
        .expect("an unrelated inherited descriptor must not extend cleanup admission");
    drop(inherited);
}

#[test]
fn cleanup_refuses_active_owner_before_directory_creation() {
    let dir = TestRoot::new();
    let path = dir.path().join("nested/cache.git");
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&path, &cancel).unwrap();
    assert_busy(CacheCleanGuard::acquire(dir.path(), &cancel));
    std::fs::create_dir(owner.path()).unwrap();
    assert_busy(CacheCleanGuard::acquire(owner.path(), &cancel));
    drop(owner);
    assert!(CacheCleanGuard::acquire(dir.path(), &cancel).is_ok());
}

#[test]
fn cleanup_blocks_new_owners_after_its_initial_scan() {
    let dir = TestRoot::new();
    let cancel = CancellationToken::new();
    let _clean = CacheCleanGuard::acquire(dir.path(), &cancel).unwrap();
    assert_busy(CacheUseGuard::acquire(
        &dir.path().join("new/cache.git"),
        &cancel,
    ));
}

#[test]
fn overlapping_cleaners_reject_in_both_start_orders() {
    let dir = TestRoot::new();
    let child = dir.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let cancel = CancellationToken::new();
    let parent_owner = CacheCleanGuard::acquire(dir.path(), &cancel).unwrap();
    assert_busy(CacheCleanGuard::acquire(&child, &cancel));
    drop(parent_owner);
    let _child_owner = CacheCleanGuard::acquire(&child, &cancel).unwrap();
    assert_busy(CacheCleanGuard::acquire(dir.path(), &cancel));
}

#[test]
fn cleaner_and_new_user_never_both_gain_admission() {
    let dir = TestRoot::new();
    let path = dir.path().join("mirror.git");
    let cancel = CancellationToken::new();
    for _ in 0..32 {
        let barrier = std::sync::Barrier::new(2);
        let (user, cleaner) = std::thread::scope(|scope| {
            let user = scope.spawn(|| {
                barrier.wait();
                CacheUseGuard::acquire(&path, &cancel)
            });
            let cleaner = scope.spawn(|| {
                barrier.wait();
                CacheCleanGuard::acquire(dir.path(), &cancel)
            });
            (user.join().unwrap(), cleaner.join().unwrap())
        });
        assert!(!(user.is_ok() && cleaner.is_ok()));
        if user.is_err() {
            assert_busy(user);
        }
        if cleaner.is_err() {
            assert_busy(cleaner);
        }
    }
}

#[test]
fn parent_owner_and_nested_cleanup_reject_in_both_start_orders() {
    let dir = TestRoot::new();
    let child = dir.path().join("objects");
    std::fs::create_dir(&child).unwrap();
    let cancel = CancellationToken::new();
    let parent_owner = CacheUseGuard::acquire(dir.path(), &cancel).unwrap();
    assert_busy(CacheCleanGuard::acquire(&child, &cancel));
    assert_busy(CacheUseGuard::acquire(&child, &cancel));
    drop(parent_owner);
    let child_cleaner = CacheCleanGuard::acquire(&child, &cancel).unwrap();
    assert_busy(CacheUseGuard::acquire(dir.path(), &cancel));
    drop(child_cleaner);
    let _child_owner = CacheUseGuard::acquire(&child, &cancel).unwrap();
    assert_busy(CacheUseGuard::acquire(dir.path(), &cancel));
}

#[test]
fn clean_preserves_open_lock_inodes_and_reports_only_removed_data() {
    let dir = TestRoot::new();
    let path = dir.path().join("nested/cache.git");
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&path, &cancel).unwrap();
    std::fs::create_dir(owner.path()).unwrap();
    std::fs::write(owner.path().join("payload"), b"data").unwrap();
    let opened_before_cleanup =
        open_lock(&lock_path(owner.path(), USE_LOCK_SUFFIX).unwrap(), false).unwrap();
    drop(owner);
    let clean = CacheCleanGuard::acquire(dir.path(), &cancel).unwrap();
    assert_eq!(
        clean.clean(&cancel).unwrap(),
        CacheCleanStats {
            files_removed: 1,
            bytes_reclaimed: 4
        }
    );
    drop(clean);
    assert!(!path.exists());
    assert!(FileExt::try_lock_exclusive(&opened_before_cleanup).unwrap());
    assert_busy(CacheUseGuard::acquire(&path, &cancel));
    drop(opened_before_cleanup);
    assert!(CacheUseGuard::acquire(&path, &cancel).is_ok());
}

#[test]
fn preview_is_nonmutating_and_ignores_coordination_markers() {
    let dir = TestRoot::new();
    let cancel = CancellationToken::new();
    assert_eq!(
        cleanup_preview(dir.path(), &cancel).unwrap(),
        CacheCleanStats::default()
    );
    let owner = CacheUseGuard::acquire(&dir.path().join("mirror.git"), &cancel).unwrap();
    std::fs::write(dir.path().join("payload"), b"hello").unwrap();
    assert_eq!(
        cleanup_preview(dir.path(), &cancel).unwrap(),
        CacheCleanStats {
            files_removed: 1,
            bytes_reclaimed: 5
        }
    );
    assert!(!lock_path(dir.path(), CLEAN_LOCK_SUFFIX).unwrap().exists());
    assert!(dir.path().join("payload").exists());
    drop(owner);
}

#[test]
fn cancellation_releases_admission_and_preserves_unremoved_data() {
    let dir = TestRoot::new();
    std::fs::write(dir.path().join("payload"), b"data").unwrap();
    let cancel = CancellationToken::new();
    let clean = CacheCleanGuard::acquire(dir.path(), &cancel).unwrap();
    cancel.cancel();
    assert!(matches!(clean.clean(&cancel), Err(CacheError::Cancelled)));
    assert!(matches!(
        cleanup_preview(dir.path(), &cancel),
        Err(CacheError::Cancelled)
    ));
    drop(clean);
    assert!(dir.path().join("payload").exists());
    assert!(CacheCleanGuard::acquire(dir.path(), &CancellationToken::new()).is_ok());
}

#[cfg(unix)]
#[test]
fn physical_ownership_survives_alias_removal_and_cleanup_never_follows_links() {
    let dir = TestRoot::new();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("cache.git");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("keep"), b"data").unwrap();
    let alias = dir.path().join("alias.git");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    let cancel = CancellationToken::new();
    let owner = CacheUseGuard::acquire(&alias, &cancel).unwrap();
    assert_busy(CacheUseGuard::acquire(&path, &cancel));
    CacheCleanGuard::acquire(dir.path(), &cancel)
        .unwrap()
        .clean(&cancel)
        .unwrap();
    assert!(!alias.exists());
    assert!(owner.path().join("keep").exists());
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn payload_cleanup_uses_the_same_directory_admission() {
    let dir = TestRoot::new();
    let cache = crate::LocalCache::new(dir.path().join("private"));
    let key = crate::CacheKey::Chunk(crab_xet::hash::compute_data_hash(b"data"));
    cache.put(&key, b"data").await.unwrap();
    let owner = CacheUseGuard::acquire(&cache.root().join("mirror.git"), &CancellationToken::new())
        .unwrap();
    let sentinel = cache.root().join("keep");
    std::fs::write(&sentinel, b"user data").unwrap();
    assert_busy(crate::clean_cache(cache.root(), false, &CancellationToken::new()).await);
    assert!(cache.contains(&key).await);
    drop(owner);
    crate::clean_cache(cache.root(), false, &CancellationToken::new())
        .await
        .unwrap();
    assert!(!cache.contains(&key).await);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"user data");
}
