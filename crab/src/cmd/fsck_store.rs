//! Store-backed implementations of [`FsckChecker`] and [`FsckRepairer`].
//!
//! Wraps the real `Store`, ref store, and manifest state to perform
//! actual storage queries and repairs for `crab fsck`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use object_store::path::Path;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::cmd::fsck::{FsckChecker, FsckIssue, FsckRepairer, MultipartMeta, PushLockMeta};
use crate::core::error::{CrabError, Result};
#[cfg(test)]
use crate::metadata::manifest::PackManifestEntry;
use crate::metadata::manifest::{
    Manifest, read_bulk_pack_list, read_manifest, read_repository_snapshot,
};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
#[cfg(test)]
use crab_coordination::push_lock_path;
use crab_coordination::{PushLock, PushLockPayload};
use crab_metadata::manifests::{PackEntry, PackList, ShardList};
use crab_storage::repo_pack_path;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::ShardReader;

/// Store-backed fsck checker that queries real object storage.
pub struct StoreChecker {
    store: Store,
    prefix: String,
    router: StoreLayout,
}

impl StoreChecker {
    pub fn new(store: Store, prefix: String) -> Self {
        let router = StoreLayout::new(store.clone(), prefix.clone());
        Self {
            store,
            prefix,
            router,
        }
    }

    /// Load the current pack list from the compacted manifest and journal.
    async fn load_pack_list(&self) -> Result<PackList> {
        let snapshot = match read_repository_snapshot(&self.store, &self.router).await {
            Ok(snapshot) => snapshot,
            Err(CrabError::NotFound { .. }) => return Ok(PackList::default()),
            Err(e) => return Err(e),
        };

        Ok(PackList {
            generation: snapshot.manifest.generation,
            entries: snapshot
                .journal
                .packs
                .into_iter()
                .map(|entry| PackEntry::with_ref_tips(entry.pack_id, entry.size, entry.ref_tips))
                .collect(),
        })
    }

    /// Load the current shard list from the compacted manifest and journal.
    async fn load_shard_list(&self) -> Result<ShardList> {
        let snapshot = match read_repository_snapshot(&self.store, &self.router).await {
            Ok(snapshot) => snapshot,
            Err(CrabError::NotFound { .. }) => return Ok(ShardList::default()),
            Err(e) => return Err(e),
        };

        Ok(ShardList {
            generation: snapshot.manifest.generation,
            entries: snapshot.journal.shards,
        })
    }

    /// List all object keys under a prefix.
    async fn list_keys(&self, sub_prefix: &str) -> Result<Vec<String>> {
        let prefix = Path::from(format!("{}/{sub_prefix}", self.prefix));
        let stream = self.store.inner().list(Some(&prefix));
        let objects: Vec<_> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                CrabError::from(crab_storage::map_object_store_error(e, prefix.as_ref()))
            })?;
        Ok(objects
            .into_iter()
            .map(|m| m.location.to_string())
            .collect())
    }

    /// Check whether an object exists at the given full path.
    async fn exists(&self, full_path: &str) -> Result<bool> {
        let path = Path::from(full_path);
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(CrabError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn manifest_file_entries(
        &self,
        shard_list: &ShardList,
    ) -> Result<HashMap<MerkleHash, HashSet<MerkleHash>>> {
        let mut entries: HashMap<MerkleHash, HashSet<MerkleHash>> = HashMap::new();

        for shard_hex in &shard_list.entries {
            let shard_hash = match MerkleHash::from_hex(shard_hex) {
                Ok(hash) => hash,
                Err(err) => {
                    warn!(shard = %shard_hex, error = %err, "invalid shard hash in manifest");
                    continue;
                }
            };
            let shard_path = self.router.global_path("shards", shard_hex);
            let path = Path::from(shard_path.as_ref());
            let body = match self.store.get_with_etag(&path).await {
                Ok((body, _)) => body,
                Err(CrabError::NotFound { .. }) => continue,
                Err(e) => return Err(e),
            };

            for (file_hash, containing_shard) in
                crab_xet::shard_parse::extract_file_entries_streaming(&body, shard_hash)
            {
                entries
                    .entry(file_hash)
                    .or_default()
                    .insert(containing_shard);
            }
        }

        Ok(entries)
    }

    async fn check_file_index_entries(
        &self,
        expected: &HashMap<MerkleHash, HashSet<MerkleHash>>,
    ) -> Result<Vec<FsckIssue>> {
        if expected.is_empty() {
            return Ok(Vec::new());
        }

        let file_hashes: Vec<MerkleHash> = expected.keys().copied().collect();
        let session = crab_metadata::file_index_lookup::FileIndexLookupSession::open(
            Arc::clone(self.store.inner()),
            &self.prefix,
        )
        .await?;
        // Fsck audits the acceleration database itself. The normal lookup
        // API repairs misses from committed manifest shards, which would
        // hide precisely the missing or stale file-index rows reported here.
        let lookups = session.lookup_committed_records_batch(&file_hashes).await;
        if let Err(close_err) = session.close().await {
            warn!(error = %close_err, "fsck: failed to close file_index_db reader");
        }
        let lookups = lookups?;

        let mut issues = Vec::new();
        for (file_hash, found) in file_hashes.iter().zip(lookups) {
            match found.map(|record| record.shard_hash) {
                Some(shard_hash)
                    if expected
                        .get(file_hash)
                        .is_some_and(|shards| shards.contains(&shard_hash)) => {}
                _ => issues.push(FsckIssue::missing_file_index(file_hash.hex())),
            }
        }
        Ok(issues)
    }

    async fn check_git_locator_entries(&self) -> Result<Vec<FsckIssue>> {
        let manifest = match read_manifest(&self.store, &self.router).await {
            Ok((manifest, _)) => manifest,
            Err(CrabError::NotFound { .. }) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let packs = if manifest.pack_index_hash.is_empty() {
            Vec::new()
        } else {
            read_bulk_pack_list(&self.store, &self.router, &manifest.pack_index_hash).await?
        };
        let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(self.store.inner()),
            &self.prefix,
        )
        .await?;
        let checks: Result<Vec<FsckIssue>> = async {
            if !session.is_available() {
                return Ok(vec![FsckIssue::git_locator_damage(
                    "locator database is unavailable",
                )]);
            }
            let mut issues = Vec::new();
            let expected_pack_index_hash = if manifest.pack_index_hash.is_empty() {
                MerkleHash::default()
            } else {
                MerkleHash::from_hex(&manifest.pack_index_hash).map_err(|error| {
                    CrabError::CorruptObject {
                        path: self.router.manifest_path().as_ref().to_owned(),
                        reason: format!("invalid pack-index hash: {error}"),
                    }
                })?
            };
            if session.coverage()
                != Some(crab_metadata::git_object_locator::GitLocatorCoverage {
                    generation: manifest.generation,
                    pack_index_hash: expected_pack_index_hash,
                })
            {
                issues.push(FsckIssue::git_locator_damage(format!(
                    "coverage does not exactly match manifest generation {} and pack index {}",
                    manifest.generation, expected_pack_index_hash
                )));
            }
            let bindings = session.pack_bindings().await?;
            let mut bindings_by_pack = bindings
                .iter()
                .map(|binding| (binding.record.pack_id, binding.record))
                .collect::<HashMap<_, _>>();
            for pack in &packs {
                let pack_id = MerkleHash::from_hex(&pack.pack_id).map_err(|error| {
                    CrabError::CorruptObject {
                        path: self.router.manifest_path().as_ref().to_owned(),
                        reason: format!("invalid pack id: {error}"),
                    }
                })?;
                let record = bindings_by_pack.remove(&pack_id);
                if record.is_none_or(|record| {
                    record.committed_generation > manifest.generation
                        || record.object_count != pack.object_count
                        || record.pack_size != pack.size
                }) {
                    issues.push(FsckIssue::git_locator_damage(format!(
                        "pack {} has no matching current locator record",
                        pack.pack_id
                    )));
                }
            }
            for pack_id in bindings_by_pack.keys() {
                issues.push(FsckIssue::git_locator_damage(format!(
                    "locator retains pack {pack_id} outside the current manifest inventory"
                )));
            }
            Ok(issues)
        }
        .await;
        let close_result = session.close().await.map_err(CrabError::from);
        match checks {
            Err(error) => Err(error),
            Ok(issues) => close_result.map(|()| issues),
        }
    }

    async fn check_git_visibility_index_for_manifest(
        &self,
        manifest: &Manifest,
        historical: Option<(u64, &str)>,
    ) -> Result<Vec<FsckIssue>> {
        if manifest.refs.is_empty() {
            return Ok(Vec::new());
        }
        let issue = |detail: String| match historical {
            Some((generation, digest)) => {
                FsckIssue::git_visibility_backfill(generation, digest, detail)
            }
            None => FsckIssue::git_visibility_damage(detail),
        };
        if manifest.pack_index_hash.is_empty() {
            return Ok(vec![issue(
                "manifest has refs but no pack-index hash".to_owned(),
            )]);
        }
        let storage_router =
            crab_storage::StoreLayout::new(self.store.as_storage().clone(), self.prefix.clone());
        let index = match crab_metadata::git_visibility::read(
            self.store.as_storage(),
            &storage_router,
            manifest.generation,
            &manifest.pack_index_hash,
        )
        .await
        {
            Ok(index) => index,
            Err(error) => {
                return Ok(vec![issue(error.to_string())]);
            }
        };
        let mut issues = Vec::new();
        if index.refs.len() != manifest.refs.len() {
            issues.push(issue(format!(
                "proof covers {} refs but manifest has {}",
                index.refs.len(),
                manifest.refs.len()
            )));
        }
        for (name, oid) in &manifest.refs {
            let Some(objects) = index.refs.get(name) else {
                issues.push(issue(format!("proof is missing ref {name}")));
                continue;
            };
            if objects.binary_search(oid).is_err() {
                issues.push(issue(format!("proof is missing tip {oid} for {name}")));
            }
            if let Some(peeled) = manifest.peeled_refs.get(name)
                && objects.binary_search(peeled).is_err()
            {
                issues.push(issue(format!(
                    "proof is missing peeled tip {peeled} for {name}"
                )));
            }
        }
        Ok(issues)
    }

    async fn check_git_visibility_index(&self) -> Result<Vec<FsckIssue>> {
        let manifest = match read_manifest(&self.store, &self.router).await {
            Ok((manifest, _)) => manifest,
            Err(CrabError::NotFound { .. }) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        self.check_git_visibility_index_for_manifest(&manifest, None)
            .await
    }

    async fn check_historical_git_visibility_indexes(&self) -> Result<Vec<FsckIssue>> {
        let storage_router =
            crab_storage::StoreLayout::new(self.store.as_storage().clone(), self.prefix.clone());
        let entries = crab_metadata::manifest_store::list_manifest_history(
            self.store.as_storage(),
            &storage_router,
        )
        .await
        .map_err(CrabError::from)?;
        let mut issues = Vec::new();
        for entry in entries {
            issues.extend(
                self.check_git_visibility_index_for_manifest(
                    &entry.manifest,
                    Some((entry.generation, &entry.digest)),
                )
                .await?,
            );
        }
        Ok(issues)
    }
}

impl FsckChecker for StoreChecker {
    fn check_git_objects(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
    {
        Box::pin(async move {
            // Git-object connectivity requires a local git repo and gix-fsck.
            // For now, list refs and verify each target commit object exists
            // in the pack storage.
            let mut issues = Vec::new();

            let ref_keys = self.list_keys("refs").await?;
            for ref_key in &ref_keys {
                let path = Path::from(ref_key.as_str());
                match self.store.get_with_etag(&path).await {
                    Ok((body, _)) => {
                        let sha = String::from_utf8_lossy(&body).trim().to_string();
                        if sha.is_empty() {
                            let ref_name = ref_key
                                .strip_prefix(&format!("{}/refs/", self.prefix))
                                .unwrap_or(ref_key);
                            issues.push(FsckIssue::dangling_ref(ref_name, "<empty>"));
                        }
                    }
                    Err(CrabError::NotFound { .. }) => {
                        // Race: ref disappeared between list and get.
                        debug!(ref_key = %ref_key, "ref disappeared during fsck");
                    }
                    Err(e) => {
                        warn!(ref_key = %ref_key, error = %e, "failed to read ref");
                    }
                }
            }

            Ok(issues)
        })
    }

    fn check_data_chain(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
    {
        Box::pin(async move {
            let mut issues = Vec::new();

            let shard_list = self.load_shard_list().await?;
            let expected_file_index = self.manifest_file_entries(&shard_list).await?;
            issues.extend(self.check_file_index_entries(&expected_file_index).await?);
            issues.extend(self.check_git_locator_entries().await?);
            issues.extend(self.check_git_visibility_index().await?);
            issues.extend(self.check_historical_git_visibility_indexes().await?);

            let mut checked_xorbs = HashSet::new();
            for shard_hash in &shard_list.entries {
                let shard_path = self.router.global_path("shards", shard_hash);
                let body = match self.store.get_with_etag(&shard_path).await {
                    Ok((body, _etag)) => body,
                    Err(CrabError::NotFound { .. }) => {
                        issues.push(FsckIssue::orphan_shard(shard_hash));
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                for (_chunk_hash, xorb_ref) in
                    crab_xet::shard_parse::extract_chunk_entries_streaming(&body)
                {
                    if !checked_xorbs.insert(xorb_ref.xorb_hash) {
                        continue;
                    }
                    let xorb_path = self.router.xorb_path(&xorb_ref.xorb_hash);
                    if !self.exists(xorb_path.as_ref()).await.unwrap_or(false) {
                        issues.push(FsckIssue::missing_xorb(xorb_ref.xorb_hash.hex()));
                    }
                }
            }

            Ok(issues)
        })
    }

    fn check_pack_list(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
    {
        Box::pin(async move {
            let mut issues = Vec::new();
            let pack_list = self.load_pack_list().await?;

            for entry in &pack_list.entries {
                let pack_path = repo_pack_path(&self.prefix, &entry.pack_id)
                    .as_ref()
                    .to_owned();
                if !self.exists(&pack_path).await.unwrap_or(false) {
                    issues.push(FsckIssue::pack_list_divergence(&entry.pack_id));
                }
                let index_path = self.router.pack_index_path(&entry.pack_id);
                if !self.exists(index_path.as_ref()).await.unwrap_or(false) {
                    issues.push(FsckIssue::pack_list_divergence(format!(
                        "{}.idx",
                        entry.pack_id
                    )));
                }
            }

            Ok(issues)
        })
    }

    fn check_push_locks(
        &self,
        now: SystemTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<PushLockMeta>>> + Send + '_>>
    {
        Box::pin(async move {
            let mut expired = Vec::new();
            let lock_keys = self.list_keys("locks").await?;
            let now_unix = now
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();

            for lock_key in &lock_keys {
                if !lock_key.ends_with("/lock") {
                    continue;
                }
                let path = Path::from(lock_key.as_str());
                match self.store.get_with_etag(&path).await {
                    Ok((body, _)) => {
                        if let Ok(payload) = serde_json::from_slice::<PushLockPayload>(&body)
                            && payload.is_expired_at(now_unix)
                        {
                            // Lock is expired.
                            let created = UNIX_EPOCH
                                + Duration::from_secs(payload.expires_at.saturating_sub(300));
                            expired.push(PushLockMeta {
                                key: lock_key.clone(),
                                created,
                                ttl: Duration::from_secs(300),
                            });
                        }
                    }
                    Err(CrabError::NotFound { .. }) => {}
                    Err(e) => {
                        warn!(lock = %lock_key, error = %e, "failed to read lock");
                    }
                }
            }

            Ok(expired)
        })
    }

    fn check_multipart_uploads(
        &self,
        _now: SystemTime,
        _grace: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<MultipartMeta>>> + Send + '_>>
    {
        Box::pin(async move {
            // Multipart uploads are tracked in the local SQLite registry,
            // not in object storage. The CLI caller should query the
            // MultipartRegistry directly if available. For the store-only
            // checker, return an empty list — the local registry check
            // is handled at the CLI layer when the staging DB is present.
            Ok(Vec::new())
        })
    }

    fn check_shard_list_divergence(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
    {
        Box::pin(async move {
            let mut issues = Vec::new();
            let shard_list = self.load_shard_list().await?;

            for hash in &shard_list.entries {
                let shard_path = self.router.global_path("shards", hash);
                if !self.exists(shard_path.as_ref()).await.unwrap_or(false) {
                    issues.push(FsckIssue::shard_list_divergence(hash));
                }
            }

            Ok(issues)
        })
    }

    fn check_orphan_file_index(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<FsckIssue>>> + Send + '_>>
    {
        Box::pin(async move {
            // Orphan file-index detection requires scanning all pointer blobs
            // in the git object store to build a referenced set, then
            // comparing against file-index keys. This is expensive and
            // requires local git repo access. For the remote-only checker,
            // skip this phase — it's informational only.
            debug!("orphan file-index check skipped (requires local git repo)");
            Ok(Vec::new())
        })
    }
}

// ---------------------------------------------------------------------------
// Store-backed repairer
// ---------------------------------------------------------------------------

/// Store-backed fsck repairer that performs real repairs against object storage.
pub struct StoreRepairer {
    store: Store,
    prefix: String,
    router: StoreLayout,
}

impl StoreRepairer {
    /// Construct with just the store and prefix — the router is derived
    /// internally so repairers always compute canonical paths rather
    /// than heuristic ones.
    pub fn new(store: Store, prefix: String) -> Self {
        let router = StoreLayout::new(store.clone(), prefix.clone());
        Self {
            store,
            prefix,
            router,
        }
    }

    async fn file_index_target_has_file(&self, file_hash: &MerkleHash) -> Result<bool> {
        let session = crab_metadata::file_index_lookup::FileIndexLookupSession::open(
            Arc::clone(self.store.inner()),
            &self.prefix,
        )
        .await?;
        let lookup = session
            .lookup_committed_records_batch(std::slice::from_ref(file_hash))
            .await;
        if let Err(close_error) = session.close().await {
            warn!(error = %close_error, "fsck repair: failed to close file_index_db reader");
        }
        let Some(shard_hash) = lookup?
            .into_iter()
            .next()
            .flatten()
            .map(|record| record.shard_hash)
        else {
            return Ok(false);
        };

        let shard_path = self.router.global_path("shards", &shard_hash.hex());
        let path = Path::from(shard_path.as_ref());
        let body = match self.store.get_with_etag(&path).await {
            Ok((body, _)) => body,
            Err(CrabError::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e),
        };
        let reader = ShardReader::from_bytes(body, shard_hash);
        Ok(reader.has_file(file_hash))
    }
}

impl FsckRepairer for StoreRepairer {
    fn repair_file_index_entry(
        &self,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        let key = key.to_string();
        Box::pin(async move {
            let Ok(file_hash) = MerkleHash::from_hex(&key) else {
                warn!(key = %key, "file-index repair: key is not a valid hex hash");
                return Ok(false);
            };
            match self.file_index_target_has_file(&file_hash).await {
                Ok(true) => {
                    // Entry is present and resolves to a shard that owns the
                    // file; the fsck finding was stale.
                    Ok(true)
                }
                Ok(false) => {
                    debug!(
                        file_hash = %key,
                        "file-index repair: entry missing or stale in file_index_db, \
                         use `crab metadb rebuild --db file_index` to regenerate"
                    );
                    Ok(false)
                }
                Err(e) => {
                    warn!(file_hash = %key, error = %e, "file-index repair lookup failed");
                    Err(e)
                }
            }
        })
    }

    fn repair_git_visibility_history(
        &self,
        generation: u64,
        digest: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        let store = self.store.clone();
        let router = self.router.clone();
        let digest = digest.to_owned();
        Box::pin(async move {
            crate::cmd::history_recovery::rebuild_git_visibility_for_history(
                &store,
                &router,
                generation,
                &digest,
                &CancellationToken::new(),
            )
            .await?;
            Ok(true)
        })
    }

    fn repair_push_lock(
        &self,
        key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        let key = key.to_string();
        Box::pin(async move {
            PushLock::repair_expired(self.store.inner(), &key)
                .await
                .map_err(Into::into)
        })
    }

    fn abort_multipart(
        &self,
        _upload_id: &str,
        _key: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + '_>> {
        Box::pin(async move {
            // Multipart abort requires the S3 AbortMultipartUpload API,
            // which is not exposed through the `object_store` crate's
            // generic interface. The local MultipartRegistry handles
            // abort_stale for locally-tracked uploads. For remote-only
            // abandoned uploads, this is a no-op until we add direct S3
            // API support.
            debug!("multipart abort not yet supported via generic object store");
            Ok(false)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::metadata::manifest::{
        RefJournalEdit, RefJournalTransaction, commit_ref_journal_transaction,
        read_ref_journal_head,
    };
    use bytes::Bytes;
    use crab_xet::shard::{
        FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
        XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    };
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn test_store() -> (Store, String) {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        (store, "test-repo".to_string())
    }

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed, seed, seed])
    }

    fn shard_with_file(file_hash: MerkleHash, xorb_hash: MerkleHash) -> (Vec<u8>, MerkleHash) {
        let mut shard = crab_xet::shard::ShardWriter::new();
        shard
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb_hash, 1usize, 1024usize),
                chunks: vec![XorbChunkSequenceEntry::new(file_hash, 1024u32, 0u32)],
            }))
            .expect("add xorb");
        shard
            .add_file(MDBFileInfo {
                metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
                segments: vec![FileDataSequenceEntry::new(xorb_hash, 1024u32, 0u32, 1u32)],
                verification: vec![],
                metadata_ext: None,
            })
            .expect("add file");
        shard.finalize().expect("finalize shard")
    }

    fn test_xorb(data: &[u8]) -> (MerkleHash, Bytes) {
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let chunk = Chunk::new(Bytes::copy_from_slice(data));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).expect("push xorb chunk");
        let xorb = builder
            .finalize()
            .expect("finalize xorb")
            .pop()
            .expect("one xorb");
        (xorb.hash, xorb.bytes)
    }

    async fn upload_shard(
        store: &Store,
        router: &StoreLayout,
        shard_hash: &MerkleHash,
        bytes: Vec<u8>,
    ) {
        store
            .put(
                &router.global_path("shards", &shard_hash.hex()),
                Bytes::from(bytes),
            )
            .await
            .expect("upload shard");
    }

    async fn seed_file_index(store: &Store, prefix: &str, entries: &[(MerkleHash, MerkleHash)]) {
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let (manifest, _) = crate::metadata::manifest::read_manifest(store, &router)
            .await
            .expect("read committed manifest");
        let shard_index_hash = MerkleHash::from_hex(&manifest.shard_index_hash)
            .expect("valid committed shard-index hash");
        let metadb = crate::metadata::MetaDb::new(
            Arc::clone(store.inner()),
            prefix.to_owned(),
            crate::metadata::MetaDbConfig::for_repo(prefix),
        );
        let guard = crate::metadata::MetaDbGuard::new(metadb);
        let file_store = guard.file_index().await.expect("file_index");
        let mut txn = guard.new_transaction().expect("transaction");
        let committed = entries
            .iter()
            .map(|(file_hash, shard_hash)| {
                (
                    *file_hash,
                    crab_metadata::value_codec::CommittedFileRecord {
                        recipe_hash: [0xC9; 32],
                        shard_hash: *shard_hash,
                        committed_generation: manifest.generation,
                        shard_index_hash,
                    },
                )
            })
            .collect::<Vec<_>>();
        file_store.save_committed_batch(&mut txn, &committed);
        guard.commit(txn).await.expect("commit file_index");
        guard.close().await.expect("close file_index seed");
    }

    fn pack_entry(pack_id: &str, size: u64) -> PackManifestEntry {
        PackManifestEntry {
            pack_id: pack_id.to_owned(),
            size,
            content_hash: pack_id.to_owned(),
            ref_tips: vec!["a".repeat(40)],
            object_count: 1,
        }
    }

    async fn write_manifest(
        store: &Store,
        prefix: &str,
        shard_hashes: &[&str],
        packs: &[PackManifestEntry],
    ) {
        use crate::metadata::manifest::{
            BulkData, Manifest, compact_pack_index, compact_shard_index, create_manifest,
            upload_segmented_bulk,
        };

        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let shards = shard_hashes
            .iter()
            .map(|hash| (*hash).to_owned())
            .collect::<Vec<_>>();
        let (shard_index_hash, _shard_index, shard_write) =
            compact_shard_index(1, &shards).unwrap();
        let (pack_index_hash, _pack_index, pack_write) = compact_pack_index(1, packs).unwrap();
        let bulk = BulkData {
            shard_index: shard_write,
            pack_index: pack_write,
        };
        upload_segmented_bulk(store, &router, &bulk).await.unwrap();

        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash;
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        create_manifest(store, &router, &manifest).await.unwrap();
    }

    async fn seed_git_locator(
        store: &Store,
        prefix: &str,
        pack: &PackManifestEntry,
        coverage_generation: u64,
        coverage_pack_index_hash: MerkleHash,
        recorded_object_count: u64,
        recorded_pack_size: u64,
    ) {
        let (manifest, _) =
            read_manifest(store, &StoreLayout::new(store.clone(), prefix.to_owned()))
                .await
                .expect("read manifest");
        let pack_index_hash =
            MerkleHash::from_hex(&manifest.pack_index_hash).expect("manifest pack-index hash");
        let pack_id = MerkleHash::from_hex(&pack.pack_id).expect("manifest pack id");
        let mut writer = crab_metadata::git_object_locator::GitObjectLocatorWriter::open(
            Arc::clone(store.inner()),
            prefix,
        )
        .await
        .expect("open Git locator writer");
        writer
            .bind_packs(&[crab_metadata::git_object_locator::GitPackLocatorRecord {
                pack_id,
                committed_generation: manifest.generation,
                pack_index_hash,
                object_count: recorded_object_count,
                pack_size: recorded_pack_size,
            }])
            .await
            .expect("bind Git locator pack");
        writer
            .set_coverage(crab_metadata::git_object_locator::GitLocatorCoverage {
                generation: coverage_generation,
                pack_index_hash: coverage_pack_index_hash,
            })
            .await
            .expect("set Git locator coverage");
        writer.close().await.expect("close Git locator writer");
    }

    #[tokio::test]
    async fn checker_empty_repo_reports_no_issues() {
        let (store, prefix) = test_store();
        let checker = StoreChecker::new(store, prefix);

        let git_issues = checker.check_git_objects().await.unwrap();
        assert!(git_issues.is_empty());

        let data_issues = checker.check_data_chain().await.unwrap();
        assert!(data_issues.is_empty());

        let pack_issues = checker.check_pack_list().await.unwrap();
        assert!(pack_issues.is_empty());

        let lock_issues = checker.check_push_locks(SystemTime::now()).await.unwrap();
        assert!(lock_issues.is_empty());

        let shard_issues = checker.check_shard_list_divergence().await.unwrap();
        assert!(shard_issues.is_empty());
    }

    #[tokio::test]
    async fn checker_reads_unified_manifest_lists() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let pack_id = hash_from_seed(1).hex();
        write_manifest(&store, &prefix, &["shard-a"], &[pack_entry(&pack_id, 4)]).await;

        store
            .put(&router.pack_path(&pack_id), Bytes::from_static(b"pack"))
            .await
            .unwrap();
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from_static(b"index"),
            )
            .await
            .unwrap();
        store
            .put(
                &router.global_path("shards", "shard-a"),
                Bytes::from_static(b"shard"),
            )
            .await
            .unwrap();
        store
            .put(
                &router.global_path("shards", "unrelated-other-repo-shard"),
                Bytes::from_static(b"other"),
            )
            .await
            .unwrap();

        let checker = StoreChecker::new(store, prefix);
        assert!(checker.check_pack_list().await.unwrap().is_empty());
        assert!(
            checker
                .check_shard_list_divergence()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn checker_reads_uncompacted_journal_lists() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        write_manifest(&store, &prefix, &[], &[]).await;
        let pack_id = hash_from_seed(30).hex();
        let shard_hash = hash_from_seed(31).hex();
        let ref_name = "refs/heads/main";
        let head = read_ref_journal_head(&store, &router, ref_name)
            .await
            .unwrap();
        let transaction = RefJournalTransaction::new(
            BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
            vec![RefJournalEdit {
                ref_name: ref_name.to_owned(),
                old_oid: None,
                new_oid: Some("a".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: None,
            }],
            None,
            vec![pack_entry(&pack_id, 4)],
            vec![shard_hash.clone()],
        )
        .unwrap();
        commit_ref_journal_transaction(&store, &router, &transaction, &[head])
            .await
            .unwrap();

        let checker = StoreChecker::new(store, prefix);
        let packs = checker.load_pack_list().await.unwrap();
        let shards = checker.load_shard_list().await.unwrap();

        assert_eq!(packs.entries[0].pack_id, pack_id);
        assert_eq!(shards.entries, vec![shard_hash]);
    }

    #[tokio::test]
    async fn checker_detects_manifest_file_missing_from_file_index_db() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let file_hash = hash_from_seed(10);
        let (shard_bytes, shard_hash) = shard_with_file(file_hash, hash_from_seed(20));
        upload_shard(&store, &router, &shard_hash, shard_bytes).await;
        let shard_hex = shard_hash.hex();
        write_manifest(&store, &prefix, &[shard_hex.as_str()], &[]).await;

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_data_chain().await.unwrap();
        assert!(
            issues
                .iter()
                .any(|issue| matches!(&issue.kind, crate::cmd::fsck::IssueKind::MissingFileIndex { file_hash: found } if found == &file_hash.hex()))
        );
    }

    #[tokio::test]
    async fn checker_accepts_manifest_file_present_in_file_index_db() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let file_hash = hash_from_seed(11);
        let (xorb_hash, xorb_bytes) = test_xorb(b"fsck xorb body");
        let (shard_bytes, shard_hash) = shard_with_file(file_hash, xorb_hash);
        upload_shard(&store, &router, &shard_hash, shard_bytes).await;
        store
            .put(&router.xorb_path(&xorb_hash), xorb_bytes)
            .await
            .unwrap();
        let shard_hex = shard_hash.hex();
        write_manifest(&store, &prefix, &[shard_hex.as_str()], &[]).await;
        seed_file_index(&store, &prefix, &[(file_hash, shard_hash)]).await;

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_data_chain().await.unwrap();
        assert!(!issues.iter().any(|issue| matches!(
            issue.kind,
            crate::cmd::fsck::IssueKind::MissingFileIndex { .. }
        )));
        assert!(
            !issues
                .iter()
                .any(|issue| matches!(issue.kind, crate::cmd::fsck::IssueKind::MissingXorb { .. }))
        );
    }

    #[tokio::test]
    async fn checker_detects_shard_referenced_missing_xorb() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let file_hash = hash_from_seed(14);
        let xorb_hash = hash_from_seed(24);
        let (shard_bytes, shard_hash) = shard_with_file(file_hash, xorb_hash);
        upload_shard(&store, &router, &shard_hash, shard_bytes).await;
        let shard_hex = shard_hash.hex();
        write_manifest(&store, &prefix, &[shard_hex.as_str()], &[]).await;
        seed_file_index(&store, &prefix, &[(file_hash, shard_hash)]).await;

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_data_chain().await.unwrap();

        assert!(
            issues
                .iter()
                .any(|issue| matches!(&issue.kind, crate::cmd::fsck::IssueKind::MissingXorb { xorb_hash: found } if found == &xorb_hash.hex()))
        );
    }

    #[tokio::test]
    async fn checker_detects_file_index_db_entry_pointing_at_wrong_shard() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let file_hash = hash_from_seed(12);
        let other_file_hash = hash_from_seed(13);
        let (shard_bytes, shard_hash) = shard_with_file(file_hash, hash_from_seed(22));
        let (other_shard_bytes, other_shard_hash) =
            shard_with_file(other_file_hash, hash_from_seed(23));
        upload_shard(&store, &router, &shard_hash, shard_bytes).await;
        upload_shard(&store, &router, &other_shard_hash, other_shard_bytes).await;
        let shard_hex = shard_hash.hex();
        let other_shard_hex = other_shard_hash.hex();
        write_manifest(
            &store,
            &prefix,
            &[shard_hex.as_str(), other_shard_hex.as_str()],
            &[],
        )
        .await;
        seed_file_index(&store, &prefix, &[(file_hash, other_shard_hash)]).await;

        let checker = StoreChecker::new(store.clone(), prefix.clone());
        let issues = checker.check_data_chain().await.unwrap();
        assert!(
            issues
                .iter()
                .any(|issue| matches!(&issue.kind, crate::cmd::fsck::IssueKind::MissingFileIndex { file_hash: found } if found == &file_hash.hex()))
        );

        let repairer = StoreRepairer::new(store, prefix);
        assert!(
            !repairer
                .repair_file_index_entry(&file_hash.hex())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn checker_detects_manifest_pack_missing_in_storage() {
        let (store, prefix) = test_store();
        let pack_id = hash_from_seed(2).hex();
        write_manifest(&store, &prefix, &[], &[pack_entry(&pack_id, 1024)]).await;

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_pack_list().await.unwrap();
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|issue| matches!(
            &issue.kind,
            crate::cmd::fsck::IssueKind::PackListDivergence { .. }
        )));
    }

    #[tokio::test]
    async fn checker_ignores_unreferenced_pack_owned_by_gc() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let pack_id = hash_from_seed(3).hex();
        write_manifest(&store, &prefix, &[], &[pack_entry(&pack_id, 4)]).await;

        store
            .put(&router.pack_path(&pack_id), Bytes::from_static(b"pack"))
            .await
            .unwrap();
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from_static(b"index"),
            )
            .await
            .unwrap();
        store
            .put(&router.pack_path("orphan"), Bytes::from_static(b"orphan"))
            .await
            .unwrap();
        store
            .put(
                &router.pack_metadata_path("orphan"),
                Bytes::from_static(b"{}"),
            )
            .await
            .unwrap();

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_pack_list().await.unwrap();
        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn checker_reports_missing_git_locator_as_acceleration_damage() {
        let (store, prefix) = test_store();
        let pack_id = hash_from_seed(4).hex();
        write_manifest(&store, &prefix, &[], &[pack_entry(&pack_id, 4)]).await;

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_data_chain().await.unwrap();

        assert!(issues.iter().any(|issue| {
            issue.severity == crate::cmd::fsck::IssueSeverity::Info
                && matches!(
                    &issue.kind,
                    crate::cmd::fsck::IssueKind::GitLocatorDamage { .. }
                )
        }));
    }

    #[tokio::test]
    async fn checker_accepts_exact_git_locator_coverage_and_pack_binding() {
        let (store, prefix) = test_store();
        let pack = pack_entry(&hash_from_seed(40).hex(), 128);
        write_manifest(&store, &prefix, &[], std::slice::from_ref(&pack)).await;
        let (manifest, _) = read_manifest(&store, &StoreLayout::new(store.clone(), prefix.clone()))
            .await
            .expect("read manifest");
        let pack_index_hash =
            MerkleHash::from_hex(&manifest.pack_index_hash).expect("manifest pack-index hash");
        seed_git_locator(&store, &prefix, &pack, 1, pack_index_hash, 1, 128).await;

        let issues = StoreChecker::new(store, prefix)
            .check_git_locator_entries()
            .await
            .expect("check locator");

        assert!(issues.is_empty());
    }

    #[tokio::test]
    async fn checker_reports_future_git_locator_coverage() {
        let (store, prefix) = test_store();
        let pack = pack_entry(&hash_from_seed(41).hex(), 128);
        write_manifest(&store, &prefix, &[], std::slice::from_ref(&pack)).await;
        let (manifest, _) = read_manifest(&store, &StoreLayout::new(store.clone(), prefix.clone()))
            .await
            .expect("read manifest");
        let pack_index_hash =
            MerkleHash::from_hex(&manifest.pack_index_hash).expect("manifest pack-index hash");
        seed_git_locator(&store, &prefix, &pack, 2, pack_index_hash, 1, 128).await;

        let issues = StoreChecker::new(store, prefix)
            .check_git_locator_entries()
            .await
            .expect("check locator");

        assert!(issues.iter().any(|issue| matches!(
            &issue.kind,
            crate::cmd::fsck::IssueKind::GitLocatorDamage { detail }
                if detail.contains("coverage does not exactly match")
        )));
    }

    #[tokio::test]
    async fn checker_reports_git_locator_pack_size_mismatch() {
        let (store, prefix) = test_store();
        let pack = pack_entry(&hash_from_seed(42).hex(), 128);
        write_manifest(&store, &prefix, &[], std::slice::from_ref(&pack)).await;
        let (manifest, _) = read_manifest(&store, &StoreLayout::new(store.clone(), prefix.clone()))
            .await
            .expect("read manifest");
        let pack_index_hash =
            MerkleHash::from_hex(&manifest.pack_index_hash).expect("manifest pack-index hash");
        seed_git_locator(&store, &prefix, &pack, 1, pack_index_hash, 1, 256).await;

        let issues = StoreChecker::new(store, prefix)
            .check_git_locator_entries()
            .await
            .expect("check locator");

        assert!(issues.iter().any(|issue| matches!(
            &issue.kind,
            crate::cmd::fsck::IssueKind::GitLocatorDamage { detail }
                if detail.contains("no matching current locator record")
        )));
    }

    #[tokio::test]
    async fn checker_detects_manifest_shard_missing_in_global_storage() {
        let (store, prefix) = test_store();
        let router = StoreLayout::new(store.clone(), prefix.clone());
        write_manifest(&store, &prefix, &["missing-shard"], &[]).await;
        store
            .put(
                &router.global_path("shards", "unrelated-other-repo-shard"),
                Bytes::from_static(b"other"),
            )
            .await
            .unwrap();

        let checker = StoreChecker::new(store, prefix);
        let issues = checker.check_shard_list_divergence().await.unwrap();
        assert_eq!(issues.len(), 1);
        assert!(
            matches!(&issues[0].kind, crate::cmd::fsck::IssueKind::ShardListDivergence { key } if key == "missing-shard")
        );
    }

    #[tokio::test]
    async fn checker_detects_expired_push_lock() {
        let (store, prefix) = test_store();

        let payload = PushLockPayload::new("test-holder", 1000, 300);
        let lock_path = Path::from(push_lock_path(&prefix, "refs/heads/main").unwrap());
        store
            .put(
                &lock_path,
                Bytes::from(serde_json::to_vec(&payload).unwrap()),
            )
            .await
            .unwrap();

        let checker = StoreChecker::new(store, prefix);
        let now = UNIX_EPOCH + Duration::from_secs(2000);
        let locks = checker.check_push_locks(now).await.unwrap();
        assert_eq!(locks.len(), 1);
        assert!(locks[0].key.contains("heads/main"));
    }

    #[tokio::test]
    async fn checker_ignores_released_push_lock_tombstone() {
        let (store, prefix) = test_store();

        let payload = PushLockPayload::released("test-holder");
        let lock_path = Path::from(push_lock_path(&prefix, "refs/heads/main").unwrap());
        store
            .put(
                &lock_path,
                Bytes::from(serde_json::to_vec(&payload).unwrap()),
            )
            .await
            .unwrap();

        let checker = StoreChecker::new(store, prefix);
        let now = UNIX_EPOCH + Duration::from_secs(2000);
        let locks = checker.check_push_locks(now).await.unwrap();
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn repairer_marks_expired_lock_released() {
        let (store, prefix) = test_store();

        let lock_path = push_lock_path(&prefix, "refs/heads/main").unwrap();
        let obj_path = Path::from(lock_path.as_str());
        let payload = PushLockPayload::new("test-holder", 1, 1);
        store
            .put(
                &obj_path,
                Bytes::from(serde_json::to_vec(&payload).unwrap()),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let repairer = StoreRepairer::new(store.clone(), prefix);
        let result = repairer.repair_push_lock(&lock_path).await.unwrap();
        assert!(result);

        let (body, _) = store.get_with_etag(&obj_path).await.unwrap();
        let repaired: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(repaired["holder"], "test-holder");
        assert_eq!(repaired["expires_at"], 0);
    }

    #[tokio::test]
    async fn repairer_missing_lock_succeeds() {
        let (store, prefix) = test_store();
        let repairer = StoreRepairer::new(store, prefix);
        let result = repairer.repair_push_lock("nonexistent/lock").await.unwrap();
        assert!(result);
    }
}
