use std::sync::{Mutex, MutexGuard};

#[cfg(any(feature = "fuse", feature = "nfs"))]
mod read;
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub(crate) use read::StoredPointer;

pub static GIT_DIR_MUTEX: Mutex<()> = Mutex::new(());

pub struct CleanGitEnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous_git_dir: Option<String>,
    previous_work_tree: Option<String>,
    previous_common_dir: Option<String>,
}

impl CleanGitEnvGuard {
    pub fn new() -> Self {
        let lock = GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous_git_dir = std::env::var("GIT_DIR").ok();
        let previous_work_tree = std::env::var("GIT_WORK_TREE").ok();
        let previous_common_dir = std::env::var("GIT_COMMON_DIR").ok();
        // SAFETY: access is serialized by GIT_DIR_MUTEX.
        unsafe {
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
            std::env::remove_var("GIT_COMMON_DIR");
        }
        Self {
            _lock: lock,
            previous_git_dir,
            previous_work_tree,
            previous_common_dir,
        }
    }
}

impl Drop for CleanGitEnvGuard {
    fn drop(&mut self) {
        // SAFETY: access is serialized by GIT_DIR_MUTEX.
        unsafe {
            restore_env("GIT_DIR", self.previous_git_dir.as_deref());
            restore_env("GIT_WORK_TREE", self.previous_work_tree.as_deref());
            restore_env("GIT_COMMON_DIR", self.previous_common_dir.as_deref());
        }
    }
}

unsafe fn restore_env(name: &str, value: Option<&str>) {
    match value {
        Some(value) => unsafe { std::env::set_var(name, value) },
        None => unsafe { std::env::remove_var(name) },
    }
}
