use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crab_cell_runtime::{
    RecoveryArtifact, RecoveryArtifactKey, RecoveryArtifactStore, ReplicaLimits,
};

const MAX_ARTIFACTS: usize = 256;

/// One server-owned, digest-addressed recovery bundle cache.
///
/// Entries are never trusted on startup. They are admitted only after the
/// immutable object and manifest have been published, and every hit reparses
/// and rehashes the bundle before returning it.
pub(crate) struct RecoveryArtifactRegistry {
    root: PathBuf,
    limits: ReplicaLimits,
    budget: crab_cell_runtime::DiskBudget,
    admission: Mutex<()>,
    entries: Mutex<BTreeMap<RecoveryArtifactKey, Arc<ArtifactEntry>>>,
    clock: AtomicU64,
}

struct ArtifactEntry {
    path: PathBuf,
    size: u64,
    _reservation: crab_cell_runtime::DiskReservation,
    last_used: AtomicU64,
}

impl crab_ltx::bundle::BundleLease for ArtifactEntry {}

impl Drop for ArtifactEntry {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl RecoveryArtifactRegistry {
    pub(crate) fn new(
        root: PathBuf,
        limits: ReplicaLimits,
        budget: crab_cell_runtime::DiskBudget,
    ) -> crate::Result<Self> {
        if !root.is_absolute() || budget.capacity() == 0 {
            return Err(crate::Error::Config(
                "recovery artifact registry requires an absolute path and disk budget",
            ));
        }
        fs::create_dir_all(&root)?;
        reclaim_stale_files(&root)?;
        sync_directory(&root)?;
        Ok(Self {
            root,
            limits,
            budget,
            admission: Mutex::new(()),
            entries: Mutex::new(BTreeMap::new()),
            clock: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn next_access(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed).saturating_add(1)
    }

    fn evict_one(entries: &mut BTreeMap<RecoveryArtifactKey, Arc<ArtifactEntry>>) -> bool {
        let Some(key) = entries
            .iter()
            .filter(|(_, entry)| Arc::strong_count(entry) == 1)
            .min_by_key(|(_, entry)| entry.last_used.load(Ordering::Relaxed))
            .map(|(key, _)| *key)
        else {
            return false;
        };
        entries.remove(&key).is_some()
    }

    fn reserve(&self, size: u64) -> crab_cell_runtime::Result<crab_cell_runtime::DiskReservation> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| crab_cell_runtime::Error::Node("recovery artifact lock poisoned"))?;
        while entries.len() >= MAX_ARTIFACTS && Self::evict_one(&mut entries) {}
        drop(entries);
        loop {
            match self.budget.try_reserve(size) {
                Ok(reservation) => return Ok(reservation),
                Err(_) => {
                    let mut entries = self.entries.lock().map_err(|_| {
                        crab_cell_runtime::Error::Node("recovery artifact lock poisoned")
                    })?;
                    if !Self::evict_one(&mut entries) {
                        return Err(crab_cell_runtime::Error::Capacity(
                            "recovery artifact cache",
                        ));
                    }
                }
            }
        }
    }

    fn invalidate(&self, key: &RecoveryArtifactKey, entry: &Arc<ArtifactEntry>) {
        if let Ok(mut entries) = self.entries.lock()
            && entries
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(key);
        }
    }

    fn destination(&self, key: &RecoveryArtifactKey) -> PathBuf {
        self.root
            .join(format!("{}.bundle", encode_hex(&key.cache_digest())))
    }
}

impl RecoveryArtifactStore for RecoveryArtifactRegistry {
    fn retain(
        &self,
        key: RecoveryArtifactKey,
        bundle: crab_ltx::bundle::Bundle,
    ) -> crab_cell_runtime::Result<()> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| crab_cell_runtime::Error::Node("recovery artifact lock poisoned"))?;
        if bundle.is_empty() || bundle.len() > self.limits.max_plan_bytes {
            return Err(crab_cell_runtime::Error::Capacity("recovery artifact size"));
        }
        let digest = key.bundle_digest();
        if bundle.digest() != digest {
            return Err(crab_cell_runtime::Error::Ltx(
                crab_ltx::CrabError::ChecksumMismatch,
            ));
        }
        let destination = self.destination(&key);
        let size = bundle.len();
        if let Some(existing) = self
            .entries
            .lock()
            .map_err(|_| crab_cell_runtime::Error::Node("recovery artifact lock poisoned"))?
            .get(&key)
            .cloned()
        {
            let valid = existing.size == size
                && crab_ltx::bundle::Bundle::decode_file(&existing.path, self.limits)
                    .is_ok_and(|candidate| candidate.digest() == digest);
            if valid {
                existing
                    .last_used
                    .store(self.next_access(), Ordering::Relaxed);
                return Ok(());
            }
            self.invalidate(&key, &existing);
        }

        let reservation = self.reserve(size)?;
        let source = match bundle.detach_file() {
            Ok(path) => path,
            Err(error) => return Err(crab_cell_runtime::Error::Ltx(error)),
        };
        if destination.exists() {
            let _ = fs::remove_file(&source);
            return Err(crab_cell_runtime::Error::Node(
                "recovery artifact destination already exists",
            ));
        }
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            encode_hex(&digest),
            self.next_access()
        ));
        if let Err(error) = relocate_file(&source, &temporary) {
            let _ = fs::remove_file(&source);
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        if let Err(error) = sync_file(&temporary).and_then(|()| {
            fs::rename(&temporary, &destination)?;
            sync_directory(&self.root)
        }) {
            let _ = fs::remove_file(&temporary);
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }

        let entry = Arc::new(ArtifactEntry {
            path: destination,
            size,
            _reservation: reservation,
            last_used: AtomicU64::new(self.next_access()),
        });
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| crab_cell_runtime::Error::Node("recovery artifact lock poisoned"))?;
        if let Some(previous) = entries.insert(key, Arc::clone(&entry)) {
            drop(previous);
        }
        Ok(())
    }

    fn load(
        &self,
        key: &RecoveryArtifactKey,
    ) -> crab_cell_runtime::Result<Option<RecoveryArtifact>> {
        let entry = {
            let entries = self
                .entries
                .lock()
                .map_err(|_| crab_cell_runtime::Error::Node("recovery artifact lock poisoned"))?;
            entries.get(key).cloned()
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        entry.last_used.store(self.next_access(), Ordering::Relaxed);
        let bundle = match crab_ltx::bundle::Bundle::decode_file(&entry.path, self.limits) {
            Ok(bundle) if bundle.digest() == key.bundle_digest() => bundle,
            Ok(_) | Err(_) => {
                self.invalidate(key, &entry);
                return Ok(None);
            }
        };
        Ok(Some(RecoveryArtifact::new(bundle, entry)))
    }
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn sync_file(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn relocate_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            fs::copy(source, destination)?;
            sync_file(destination)?;
            fs::remove_file(source)
        }
        Err(error) => Err(error),
    }
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    OpenOptions::new().read(true).open(path)?.sync_all()
}

fn reclaim_stale_files(root: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".bundle") || name.ends_with(".tmp") {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_cell_runtime::{
        ApplicationId, CellId, Digest, IncarnationId, RecoveryArtifactKey, RootRef, SessionId,
    };
    use crab_ltx::{Limits, Position, bundle::BundleBuilder, bundle::BundleEntry};
    use tempfile::TempDir;

    fn bundle_fixture(directory: &Path, marker: u8) -> crab_ltx::bundle::Bundle {
        let database_path = directory.join(format!("database-{marker}"));
        let mut database = crab_ltx::Db::open(&database_path, Limits::default()).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        database.capture().unwrap();
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO values_ VALUES (1)", [])?;
                Ok(())
            })
            .unwrap();
        let capture = database.capture().unwrap();
        let segment = capture.segments.first().unwrap();
        let mut builder = BundleBuilder::new_temp(directory, Limits::default()).unwrap();
        builder
            .push(BundleEntry::for_cell(
                [marker; 32],
                [marker; 16],
                segment.info().clone(),
                std::fs::read(segment.path()).unwrap(),
            ))
            .unwrap();
        let bundle = builder.finish().unwrap();
        database.close().unwrap();
        bundle
    }

    fn key(bundle: &crab_ltx::bundle::Bundle, marker: u8) -> RecoveryArtifactKey {
        RecoveryArtifactKey::new(
            SessionId::from_bytes([1; 16]),
            2,
            ApplicationId::from_bytes([2; 16]),
            CellId::from_bytes([marker; 32]),
            IncarnationId::from_bytes([marker; 16]),
            3,
            4,
            4,
            RootRef {
                digest: Digest::from_bytes([5; 32]),
                txid: 1,
                checksum: (1_u64 << 63) | 1,
                commit_sequence: 1,
            },
            Position {
                txid: 2,
                checksum: (1_u64 << 63) | 2,
            },
            2,
            Digest::from_bytes(bundle.digest()),
        )
    }

    #[test]
    fn retains_verified_bundle_and_drops_corrupt_cache_entry() {
        let temporary = TempDir::new().unwrap();
        let registry = RecoveryArtifactRegistry::new(
            temporary.path().join("registry"),
            Limits::default(),
            crab_cell_runtime::DiskBudget::new(1 << 20),
        )
        .unwrap();
        let bundle = bundle_fixture(temporary.path(), 7);
        let key = key(&bundle, 7);
        registry.retain(key, bundle).unwrap();
        assert_eq!(registry.entry_count(), 1);
        assert!(registry.load(&key).unwrap().is_some());

        let path = registry.destination(&key);
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(registry.load(&key).unwrap().is_none());
        assert_eq!(registry.entry_count(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn reclaims_only_registry_artifacts_from_previous_process() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("registry");
        std::fs::create_dir_all(&root).unwrap();
        let stale = root.join("stale.bundle");
        let temporary_file = root.join("stale.tmp");
        let unrelated = root.join("operator-note");
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&temporary_file, b"stale").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        let _registry = RecoveryArtifactRegistry::new(
            root,
            Limits::default(),
            crab_cell_runtime::DiskBudget::new(1 << 20),
        )
        .unwrap();
        assert!(!stale.exists());
        assert!(!temporary_file.exists());
        assert!(unrelated.exists());
    }
}
