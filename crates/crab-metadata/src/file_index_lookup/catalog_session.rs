//! Bounded verified file bindings for one captured catalog lookup session.
use super::{MetadataError, Result};
use crate::capsule_protocol::PointerCatalogLookup;
use crab_storage::Store;
use crab_xet::hash::MerkleHash;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_BINDINGS: usize = 4096;

pub(super) struct CatalogSession {
    lookup: Arc<PointerCatalogLookup>,
    bindings: Mutex<Bindings>,
}

#[derive(Default)]
struct Bindings {
    entries: HashMap<MerkleHash, Option<MerkleHash>>,
    order: VecDeque<MerkleHash>,
}

impl Bindings {
    fn insert(&mut self, file: MerkleHash, shard: Option<MerkleHash>) {
        if self.entries.contains_key(&file) {
            return;
        }
        if self.entries.len() == MAX_BINDINGS
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(file, shard);
        self.order.push_back(file);
    }
}

impl CatalogSession {
    pub(super) fn new(lookup: Arc<PointerCatalogLookup>) -> Self {
        Self {
            lookup,
            bindings: Mutex::new(Bindings::default()),
        }
    }

    fn bindings(&self) -> Result<MutexGuard<'_, Bindings>> {
        self.bindings
            .lock()
            .map_err(|_| MetadataError::Internal("catalog binding cache is poisoned".to_owned()))
    }

    pub(super) async fn lookup_batch(
        &self,
        store: &Store,
        files: &[MerkleHash],
    ) -> Result<Vec<Option<MerkleHash>>> {
        self.lookup.admit_file_queries(files.len())?;
        let cached = {
            let bindings = self.bindings()?;
            files
                .iter()
                .map(|file| bindings.entries.get(file).copied())
                .collect::<Option<Vec<_>>>()
        };
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let hashes = files.iter().map(MerkleHash::hex).collect::<Vec<_>>();
        // Reuse the complete original batch proof when any answer is uncached.
        // This preserves its aggregate key/page budget, including duplicate keys.
        let selected = self.lookup.files(store, &hashes).await?;
        let result = hashes
            .iter()
            .map(|hash| {
                selected
                    .files()
                    .get(hash)
                    .map(|entry| {
                        MerkleHash::from_hex(entry.shard_hash()).map_err(|source| {
                            MetadataError::CorruptObject {
                                path: "capsule-protocol pointer catalog".to_owned(),
                                reason: format!(
                                    "invalid shard hash {}: {source}",
                                    entry.shard_hash()
                                ),
                            }
                        })
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        // Build every answer before bounded insertion: a batch larger than the
        // cache still returns all results, even when its own entries are evicted.
        let mut bindings = self.bindings()?;
        for (file, shard) in files.iter().copied().zip(result.iter().copied()) {
            bindings.insert(file, shard);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_binding_memory_stays_bounded_across_hits_and_misses() {
        let mut bindings = Bindings::default();
        for index in 0..MAX_BINDINGS * 3 {
            let mut bytes = [0; 32];
            bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
            let file = MerkleHash::from(bytes);
            let shard = (index % 2 == 0).then_some(file);
            bindings.insert(file, shard);
            bindings.insert(file, shard);
            assert_eq!(bindings.entries.len(), (index + 1).min(MAX_BINDINGS));
        }
    }
}
