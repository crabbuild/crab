use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crab_storage::Store;
use object_store::{ObjectMeta, path::Path};

use super::read_session::VerifiedLocal;

/// Bounded checked-source retention shared by independently drained operations.
///
/// Retention does not prove unread bytes or pin durable garbage collection.
/// Drop this cache after its read sessions close to release retained sources.
#[derive(Clone, Default)]
pub struct LfsReadCache(Arc<Mutex<Proofs>>);

#[derive(Default)]
struct Proofs {
    local: LocalCache,
    remote: VecDeque<RemoteProof>,
    remote_bytes: usize,
}

const MAX_REMOTE_PROOFS: usize = 128;
const MAX_REMOTE_PROOF_BYTES: usize = 8 * 1024 * 1024;

struct RemoteProof {
    transport: (u64, [u8; 32]),
    path: Path,
    oid: [u8; 32],
    size: u64,
    e_tag: Option<String>,
    version: Option<String>,
    bytes: usize,
}

impl LfsReadCache {
    pub(super) fn local(
        &self,
        path: PathBuf,
        oid: [u8; 32],
        size: u64,
    ) -> std::io::Result<Option<LocalEntry>> {
        let mut cache = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("LFS source retention is unavailable"))?;
        Ok(cache.local.claim(LocalKey { path, oid, size }))
    }

    pub(crate) fn matches_remote(
        &self,
        store: &Store,
        path: &Path,
        oid: &[u8; 32],
        meta: &ObjectMeta,
    ) -> std::io::Result<bool> {
        let Some(target) = store.read_prefix_target_identity(path) else {
            return Ok(false);
        };
        let transport = (store.immutable_cache_identity(), *target);
        let mut cache = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("LFS source retention is unavailable"))?;
        let found = cache.remote.iter().position(|proof| {
            proof.transport == transport && proof.path == *path && proof.oid == *oid
        });
        let Some(proof) = found.and_then(|index| cache.remote.remove(index)) else {
            return Ok(false);
        };
        if proof.size != meta.size || proof.e_tag != meta.e_tag || proof.version != meta.version {
            cache.remote_bytes -= proof.bytes;
            return Ok(false);
        }
        cache.remote.push_back(proof);
        Ok(true)
    }

    pub(crate) fn remember_remote(
        &self,
        store: &Store,
        path: &Path,
        oid: &[u8; 32],
        meta: &ObjectMeta,
    ) -> std::io::Result<()> {
        let Some(target) = store.read_prefix_target_identity(path) else {
            return Ok(());
        };
        if !crate::object_store::has_byte_validator(meta) {
            return Ok(());
        }
        let bytes = std::mem::size_of::<RemoteProof>()
            .saturating_add(path.as_ref().len())
            .saturating_add(meta.e_tag.as_ref().map_or(0, String::len))
            .saturating_add(meta.version.as_ref().map_or(0, String::len));
        if bytes > MAX_REMOTE_PROOF_BYTES {
            return Ok(());
        }
        let transport = (store.immutable_cache_identity(), *target);
        let mut cache = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("LFS source retention is unavailable"))?;
        if let Some(index) = cache.remote.iter().position(|proof| {
            proof.transport == transport && proof.path == *path && proof.oid == *oid
        }) && let Some(proof) = cache.remote.remove(index)
        {
            cache.remote_bytes -= proof.bytes;
        }
        while cache.remote.len() >= MAX_REMOTE_PROOFS
            || cache.remote_bytes + bytes > MAX_REMOTE_PROOF_BYTES
        {
            if let Some(proof) = cache.remote.pop_front() {
                cache.remote_bytes -= proof.bytes;
            }
        }
        // Opaque validators are scoped to one bound canonical transport. Scoped
        // routing/staging can reuse tokens from another store, so those reads
        // retain the ordinary fresh-verification path instead of reusing proof.
        cache.remote.push_back(RemoteProof {
            transport,
            path: path.clone(),
            oid: *oid,
            size: meta.size,
            e_tag: meta.e_tag.clone(),
            version: meta.version.clone(),
            bytes,
        });
        cache.remote_bytes += bytes;
        Ok(())
    }
}

pub(super) const LOCAL_BLOCK_BYTES: usize = 1024 * 1024;
const MAX_LOCAL_PROOF_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const MAX_LOCAL_FILES: usize = 128;

type LocalEntry = Arc<Mutex<Option<VerifiedLocal>>>;

#[derive(PartialEq, Eq)]
struct LocalKey {
    path: PathBuf,
    oid: [u8; 32],
    size: u64,
}

impl LocalKey {
    fn proof_bytes(&self) -> u64 {
        self.size.div_ceil(LOCAL_BLOCK_BYTES as u64) * 32
    }
}

#[derive(Default)]
struct LocalCache {
    entries: VecDeque<(LocalKey, LocalEntry)>,
    proof_bytes: u64,
}

impl LocalCache {
    fn claim(&mut self, key: LocalKey) -> Option<LocalEntry> {
        if let Some(entry) = self
            .entries
            .iter()
            .position(|(found, _)| *found == key)
            .and_then(|index| self.entries.remove(index))
        {
            let claim = Arc::clone(&entry.1);
            self.entries.push_back(entry);
            return Some(claim);
        }
        let required = key.proof_bytes();
        if required > MAX_LOCAL_PROOF_BYTES {
            return None;
        }
        while self.entries.len() >= MAX_LOCAL_FILES
            || self.proof_bytes + required > MAX_LOCAL_PROOF_BYTES
        {
            // A worker or waiter still owns this exact incarnation. Evicting
            // its entry would admit a second verifier for the same source.
            let index = self
                .entries
                .iter()
                .position(|(_, entry)| Arc::strong_count(entry) == 1)?;
            if let Some((retired, _)) = self.entries.remove(index) {
                self.proof_bytes -= retired.proof_bytes();
            }
        }
        let entry = Arc::new(Mutex::new(None));
        self.proof_bytes += required;
        self.entries.push_back((key, Arc::clone(&entry)));
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_proof_retention_bounds_record_count_and_validator_bytes() {
        use object_store::{ObjectStoreExt, memory::InMemory};
        let raw = Arc::new(InMemory::new());
        let base = Path::from("base");
        raw.put(&base, bytes::Bytes::from_static(b"proof fixture").into())
            .await
            .unwrap();
        let mut meta = raw.head(&base).await.unwrap();
        let store = Store::new(raw).with_target_identity([1; 32]);
        let cache = LfsReadCache::default();
        let oid = [2; 32];
        for index in 0..=MAX_REMOTE_PROOFS {
            let path = Path::from(index.to_string());
            cache.remember_remote(&store, &path, &oid, &meta).unwrap();
        }
        assert!(
            !cache
                .matches_remote(&store, &Path::from("0"), &oid, &meta)
                .unwrap()
        );
        let latest = Path::from(MAX_REMOTE_PROOFS.to_string());
        assert!(cache.matches_remote(&store, &latest, &oid, &meta).unwrap());
        meta.e_tag = Some("x".repeat(MAX_REMOTE_PROOF_BYTES));
        let oversized = Path::from("oversized");
        cache
            .remember_remote(&store, &oversized, &oid, &meta)
            .unwrap();
        assert!(
            !cache
                .matches_remote(&store, &oversized, &oid, &meta)
                .unwrap()
        );
    }

    #[test]
    fn proof_budget_cannot_evict_live_source_claims() {
        let mut cache = LocalCache::default();
        let full_size = MAX_LOCAL_PROOF_BYTES / 32 * LOCAL_BLOCK_BYTES as u64;
        let key = |name: &str| LocalKey {
            oid: [1; 32],
            path: name.into(),
            size: full_size,
        };
        let first = cache.claim(key("first")).unwrap();
        let same = cache.claim(key("first")).unwrap();
        assert!(
            Arc::ptr_eq(&first, &same)
                && cache.claim(key("second")).is_none()
                && cache.proof_bytes == MAX_LOCAL_PROOF_BYTES
        );
        drop(first);
        drop(same);
        assert!(
            cache.claim(key("second")).is_some()
                && cache.entries.len() == 1
                && cache.proof_bytes == MAX_LOCAL_PROOF_BYTES
        );
    }
}
