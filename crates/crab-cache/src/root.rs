//! Cache-root resolution shared by Crab clients.

use std::path::PathBuf;

#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
use crate::Result;

/// Return the default Crab cache root directory.
///
/// Respects `CRAB_CACHE_DIR` when set, otherwise falls back to
/// `~/.cache/crab`. Every client that touches the local cache should call this
/// instead of hard-coding the path.
#[must_use]
pub fn default_cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("CRAB_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".cache/crab"),
        |h| PathBuf::from(h).join(".cache/crab"),
    )
}

/// Create or validate a private, non-symlinked cache directory.
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub fn ensure_private_cache_directory(path: &std::path::Path) -> Result<()> {
    crate::private_fs::ensure_directory(path)
}

/// Return whether an existing root is safe to consume as private cache state.
#[must_use]
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub fn private_cache_directory_is_safe(path: &std::path::Path) -> bool {
    crate::private_fs::directory_is_safe(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    #[cfg(all(unix, any(feature = "local-cache", feature = "xet-chunk-cache")))]
    use crate::CacheError;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        cache_dir: Option<String>,
    }

    impl EnvGuard {
        fn unset_cache_dir() -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let cache_dir = std::env::var("CRAB_CACHE_DIR").ok();
            // SAFETY: tests that mutate CRAB_CACHE_DIR hold ENV_LOCK, so no
            // other test in this module observes a partially restored env.
            unsafe { std::env::remove_var("CRAB_CACHE_DIR") };
            Self {
                _lock: lock,
                cache_dir,
            }
        }

        fn set_cache_dir(path: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let cache_dir = std::env::var("CRAB_CACHE_DIR").ok();
            // SAFETY: tests that mutate CRAB_CACHE_DIR hold ENV_LOCK, so no
            // other test in this module observes a partially restored env.
            unsafe { std::env::set_var("CRAB_CACHE_DIR", path) };
            Self {
                _lock: lock,
                cache_dir,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.cache_dir {
                Some(value) => {
                    // SAFETY: ENV_LOCK is held for the lifetime of this guard.
                    unsafe { std::env::set_var("CRAB_CACHE_DIR", value) };
                }
                None => {
                    // SAFETY: ENV_LOCK is held for the lifetime of this guard.
                    unsafe { std::env::remove_var("CRAB_CACHE_DIR") };
                }
            }
        }
    }

    #[test]
    fn default_cache_root_honors_env_override() {
        let _guard = EnvGuard::set_cache_dir("/tmp/crab-cache-root-test");

        assert_eq!(
            default_cache_root(),
            PathBuf::from("/tmp/crab-cache-root-test")
        );
    }

    #[test]
    fn default_cache_root_uses_home_fallback() {
        let _guard = EnvGuard::unset_cache_dir();
        let home = std::env::var_os("HOME");

        let expected = home.map_or_else(
            || PathBuf::from(".cache/crab"),
            |h| PathBuf::from(h).join(".cache/crab"),
        );
        assert_eq!(default_cache_root(), expected);
    }

    #[cfg(all(unix, any(feature = "local-cache", feature = "xet-chunk-cache")))]
    #[test]
    fn creates_private_directory_and_rejects_unsafe_mode() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache").join("nested");

        ensure_private_cache_directory(&root).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&root).unwrap().mode() & 0o777,
            0o700
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            ensure_private_cache_directory(&root),
            Err(CacheError::UnsafeRoot { .. })
        ));
    }

    #[cfg(all(unix, any(feature = "local-cache", feature = "xet-chunk-cache")))]
    #[test]
    fn rejects_symlinked_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("cache");
        symlink(&real, &link).unwrap();

        assert!(matches!(
            ensure_private_cache_directory(&link),
            Err(CacheError::UnsafeRoot { .. })
        ));
    }
}
