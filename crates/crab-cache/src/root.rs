//! Cache-root resolution shared by Crab clients.

use std::path::PathBuf;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
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
}
