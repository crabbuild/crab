//! Exact live publication paths shared by writers and Git status snapshots.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tempfile::NamedTempFile;

#[derive(Default)]
struct Publications {
    active: HashSet<PathBuf>,
    snapshots: Vec<Weak<Mutex<HashSet<PathBuf>>>>,
}

pub(crate) struct RegistryState {
    pub(crate) creating: tokio::sync::Mutex<()>,
    state: Mutex<Publications>,
}

pub(crate) type Registry = Arc<RegistryState>;

static REGISTRIES: LazyLock<tokio::sync::Mutex<HashMap<PathBuf, Weak<RegistryState>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

pub(crate) async fn registry(root: &Path) -> Registry {
    let mut roots = REGISTRIES.lock().await;
    roots.retain(|_, registry| registry.strong_count() > 0);
    if let Some(registry) = roots.get(root).and_then(Weak::upgrade) {
        return registry;
    }
    let registry = Arc::new(RegistryState {
        creating: tokio::sync::Mutex::new(()),
        state: Mutex::new(Publications::default()),
    });
    roots.insert(root.to_owned(), Arc::downgrade(&registry));
    registry
}

pub(crate) struct StatusSnapshot {
    registry: Registry,
    paths: Arc<Mutex<HashSet<PathBuf>>>,
}

pub(crate) async fn status_snapshot(root: &Path) -> StatusSnapshot {
    let registry = registry(root).await;
    let paths = {
        let mut publications = registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let paths = Arc::new(Mutex::new(publications.active.clone()));
        publications
            .snapshots
            .retain(|snapshot| snapshot.strong_count() > 0);
        publications.snapshots.push(Arc::downgrade(&paths));
        paths
    };
    StatusSnapshot { registry, paths }
}

impl StatusSnapshot {
    pub(crate) async fn finish(self) -> HashSet<PathBuf> {
        // Creation and registration share this lock. A status scan cannot
        // finish in the gap after exclusive creation but before ownership.
        let _creation = self.registry.creating.lock().await;
        self.paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(crate) struct Publication {
    registry: Registry,
    path: PathBuf,
}

impl Drop for Publication {
    fn drop(&mut self) {
        // Release does no filesystem I/O and can run on an async caller's error
        // path. Creation has a separate async gate, so a blocked disk cannot
        // make guard destruction block on its own runtime.
        self.registry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .remove(&self.path);
    }
}

pub(crate) fn owned_temporary(
    registry: &Registry,
    builder: &tempfile::Builder<'_, '_>,
    parent: &Path,
) -> io::Result<(NamedTempFile, Publication)> {
    // Only this physical worktree is held during acquisition. A blocked
    // filesystem must not stall status reads for an unrelated repository.
    let _creation = registry.creating.blocking_lock();
    let temporary = builder.tempfile_in(parent)?;
    let mut publications = registry
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temporary.path().to_owned();
    publications.active.insert(path.clone());
    publications.snapshots.retain(|snapshot| {
        if let Some(paths) = snapshot.upgrade() {
            paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.clone());
            true
        } else {
            false
        }
    });
    Ok((
        temporary,
        Publication {
            registry: registry.clone(),
            path,
        },
    ))
}
