//! File-backed consolidation of the Git packs selected by a repository manifest.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::coordination::heartbeat::LockHeartbeat;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::push::{
    CommittedManifestAnchor, CommittedPackIndex, publish_committed_pack_locators,
};
use crate::metadata::manifest::{
    BulkData, Manifest, PackManifestEntry, compact_pack_index, read_bulk_pack_list, read_manifest,
    upload_segmented_bulk, write_manifest_cas,
};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_coordination::PushLock;
use crab_git::pack_locator::{PackLocationIter, write_pack_reverse_index};
use crab_storage::{repo_pack_index_path, repo_pack_path, repo_pack_reverse_index_path};
use crab_xet::hash::MerkleHash;

const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

/// Configuration for the repack operation.
#[derive(Debug, Clone)]
pub struct RepackConfig {
    /// Push lock TTL during repack.
    pub lock_ttl: Duration,
    /// Whether this is a dry-run (report stats without modifying remote).
    pub dry_run: bool,
    /// Maximum number of concurrent pack/index downloads.
    pub download_concurrency: usize,
}

impl Default for RepackConfig {
    fn default() -> Self {
        Self {
            lock_ttl: Duration::from_mins(5),
            dry_run: false,
            download_concurrency: 8,
        }
    }
}

/// Outcome of a repack operation.
#[derive(Debug, Clone)]
pub struct RepackOutcome {
    /// Number of packs before repack.
    pub packs_before: usize,
    /// Number of packs after repack.
    pub packs_after: usize,
    /// Total bytes across all packs before repack.
    pub bytes_before: u64,
    /// Total bytes across all packs after repack.
    pub bytes_after: u64,
    /// Wall-clock time for the operation.
    pub elapsed: Duration,
}

impl RepackOutcome {
    /// Convert to the structured output summary payload.
    pub fn to_summary(&self) -> RepackSummary {
        RepackSummary {
            packs_before: self.packs_before as u64,
            packs_after: self.packs_after as u64,
            bytes_before: self.bytes_before,
            bytes_after: self.bytes_after,
            elapsed_ms: self.elapsed.as_millis() as u64,
        }
    }
}

/// Terminal result payload for structured output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RepackSummary {
    /// Number of packs before repack.
    pub packs_before: u64,
    /// Number of packs after repack.
    pub packs_after: u64,
    /// Total bytes across all packs before repack.
    pub bytes_before: u64,
    /// Total bytes across all packs after repack.
    pub bytes_after: u64,
    /// Wall-clock duration in milliseconds.
    pub elapsed_ms: u64,
}

struct GeneratedPack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reverse_index_path: PathBuf,
    pack_id: String,
    pack_hash: [u8; 32],
    pack_size: u64,
    index_hash: [u8; 32],
    index_size: u64,
    reverse_index_hash: [u8; 32],
    reverse_index_size: u64,
    object_count: u64,
    git_sha1: String,
}

/// Consolidate all packs selected by one pinned manifest generation.
pub async fn run_repack(
    store: &Store,
    prefix: &str,
    config: &RepackConfig,
    cancel: &CancellationToken,
) -> Result<RepackOutcome> {
    let start = Instant::now();
    check_cancelled(cancel)?;
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    let lock = PushLock::acquire_internal(
        store.inner(),
        router.repo_prefix(),
        crab_coordination::REPACK_RESOURCE,
        config.lock_ttl,
    )
    .await?;
    let operation_cancel = cancel.child_token();
    let heartbeat = LockHeartbeat::spawn(
        store.clone(),
        lock.path().to_owned(),
        lock.holder().to_owned(),
        lock.ttl(),
        lock.ttl() / 3,
        operation_cancel.clone(),
    );
    let result = run_repack_locked(store, &router, config, &operation_cancel, start).await;
    heartbeat.stop().await;
    let release_result = lock.release().await.map_err(CrabError::from);
    match (result, release_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn run_repack_locked(
    store: &Store,
    router: &StoreLayout,
    config: &RepackConfig,
    cancel: &CancellationToken,
    start: Instant,
) -> Result<RepackOutcome> {
    let (manifest, manifest_etag) = read_manifest(store, router).await?;
    let packs = read_bulk_pack_list(store, router, &manifest.pack_index_hash).await?;
    let packs_before = packs.len();
    let bytes_before = packs.iter().map(|pack| pack.size).sum();
    if packs_before <= 1 {
        return Ok(outcome(
            packs_before,
            packs_before,
            bytes_before,
            bytes_before,
            start,
        ));
    }
    if config.dry_run {
        return Ok(outcome(packs_before, 1, bytes_before, bytes_before, start));
    }
    let visibility = read_current_visibility(store, router, &manifest).await?;

    let temp = tempfile::tempdir()?;
    let git_dir = temp.path().join("source.git");
    initialize_bare_repository(&git_dir)?;
    download_source_packs(
        store,
        router,
        &packs,
        &git_dir.join("objects/pack"),
        config.download_concurrency,
        cancel,
    )
    .await?;
    check_cancelled(cancel)?;

    let refs = manifest.refs.values().cloned().collect::<BTreeSet<_>>();
    let output_dir = temp.path().join("output");
    std::fs::create_dir_all(&output_dir)?;
    let git_dir_for_pack = git_dir.clone();
    let refs_for_pack = refs.clone();
    let output_for_pack = output_dir.clone();
    let generated = tokio::task::spawn_blocking(move || {
        build_and_validate_pack(&git_dir_for_pack, &output_for_pack, &refs_for_pack)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("repack worker join failed: {error}")))??;
    check_cancelled(cancel)?;

    upload_generated_pack(store, router, &generated, cancel).await?;
    let new_generation = manifest.generation.checked_add(1).ok_or_else(|| {
        CrabError::Internal("manifest generation overflow during repack".to_owned())
    })?;
    let replacement = PackManifestEntry {
        pack_id: generated.pack_id.clone(),
        size: generated.pack_size,
        content_hash: generated.pack_id.clone(),
        ref_tips: refs.into_iter().collect(),
        object_count: generated.object_count,
    };
    let (pack_index_hash, _index, pack_write) =
        compact_pack_index(new_generation, std::slice::from_ref(&replacement))?;
    upload_segmented_bulk(
        store,
        router,
        &BulkData {
            shard_index: crab_metadata::segmented::SegmentWrite::default(),
            pack_index: pack_write,
        },
    )
    .await?;
    check_cancelled(cancel)?;

    let committed = repack_manifest(manifest, new_generation, pack_index_hash);
    write_manifest_cas(store, router, &committed, &manifest_etag).await?;
    if let Some(visibility) = visibility {
        let visibility = rebind_visibility(visibility, &committed);
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        if let Err(error) = crab_metadata::git_visibility::upload_if_absent(
            store.as_storage(),
            &storage_router,
            &visibility,
        )
        .await
        {
            warn!(
                error = %error,
                generation = committed.generation,
                "repack committed; Git visibility proof requires repair"
            );
        }
    } else {
        debug!(
            generation = committed.generation,
            "repack preserved repository without a Git visibility proof"
        );
    }
    let anchor = CommittedManifestAnchor {
        generation: committed.generation,
        shard_index_hash: manifest_hash_or_default(&committed.shard_index_hash)?,
        pack_index_hash: manifest_hash_or_default(&committed.pack_index_hash)?,
    };
    if let Err(error) = publish_committed_pack_locators(
        store,
        router,
        &[CommittedPackIndex {
            pack: &replacement,
            idx_path: &generated.index_path,
            rev_path: &generated.reverse_index_path,
            git_sha1: &generated.git_sha1,
        }],
        anchor,
        config.lock_ttl,
        cancel,
    )
    .await
    {
        warn!(error = %error, "repack committed; locator publication requires repair");
    }

    info!(
        generation = committed.generation,
        pack_id = %replacement.pack_id,
        packs_before,
        "repack manifest committed"
    );
    Ok(outcome(
        packs_before,
        1,
        bytes_before,
        generated.pack_size,
        start,
    ))
}

fn outcome(
    packs_before: usize,
    packs_after: usize,
    bytes_before: u64,
    bytes_after: u64,
    start: Instant,
) -> RepackOutcome {
    RepackOutcome {
        packs_before,
        packs_after,
        bytes_before,
        bytes_after,
        elapsed: start.elapsed(),
    }
}

fn repack_manifest(mut manifest: Manifest, generation: u64, pack_index_hash: String) -> Manifest {
    manifest.generation = generation;
    manifest.created_at = now_iso8601();
    manifest.pusher = None;
    manifest.session_id = format!("repack-{generation}");
    manifest.pack_index_hash = pack_index_hash;
    // `run` validates the replacement pack against the complete temporary ODB
    // before this helper commits its single-pack inventory.
    manifest.seal_git_validation();
    manifest
}

async fn read_current_visibility(
    store: &Store,
    router: &StoreLayout,
    manifest: &Manifest,
) -> Result<Option<crab_metadata::git_visibility::GitVisibilityIndex>> {
    if manifest.refs.is_empty() || manifest.pack_index_hash.is_empty() {
        return Ok(None);
    }
    let storage_router =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    match crab_metadata::git_visibility::read_with_format(
        store.as_storage(),
        &storage_router,
        manifest.generation,
        &manifest.pack_index_hash,
        &manifest.git_validation_digest,
    )
    .await
    {
        Ok(read) => {
            if read.index.matches_manifest(manifest) {
                if read.format == crab_metadata::git_visibility::GitVisibilityFormat::V1 {
                    crab_metadata::git_visibility::upload_if_absent(
                        store.as_storage(),
                        &storage_router,
                        &read.index,
                    )
                    .await?;
                }
                Ok(Some(read.index))
            } else {
                Err(CrabError::CorruptObject {
                    path: storage_router
                        .git_visibility_path(&manifest.git_validation_digest)
                        .as_ref()
                        .to_owned(),
                    reason: "Git visibility proof does not match manifest refs".to_owned(),
                })
            }
        }
        Err(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn rebind_visibility(
    mut visibility: crab_metadata::git_visibility::GitVisibilityIndex,
    manifest: &Manifest,
) -> crab_metadata::git_visibility::GitVisibilityIndex {
    visibility.generation = manifest.generation;
    visibility
        .pack_index_hash
        .clone_from(&manifest.pack_index_hash);
    visibility
        .git_validation_digest
        .clone_from(&manifest.git_validation_digest);
    visibility
}

fn manifest_hash_or_default(value: &str) -> Result<MerkleHash> {
    if value.is_empty() {
        return Ok(MerkleHash::default());
    }
    MerkleHash::from_hex(value)
        .map_err(|error| CrabError::Internal(format!("invalid manifest content hash: {error}")))
}

async fn download_source_packs(
    store: &Store,
    router: &StoreLayout,
    packs: &[PackManifestEntry],
    pack_dir: &Path,
    concurrency: usize,
    cancel: &CancellationToken,
) -> Result<()> {
    let results = futures_util::stream::iter(packs.iter().cloned().map(|pack| {
        let store = store.clone();
        let pack_path = repo_pack_path(router.repo_prefix(), &pack.pack_id);
        let index_path = repo_pack_index_path(router.repo_prefix(), &pack.pack_id);
        let local_pack = pack_dir.join(format!("pack-{}.pack", pack.pack_id));
        let local_index = pack_dir.join(format!("pack-{}.idx", pack.pack_id));
        let local_reverse_index = pack_dir.join(format!("pack-{}.rev", pack.pack_id));
        async move {
            let (pack_size, _) = tokio::try_join!(
                store.download_to_path(&pack_path, &local_pack),
                store.download_to_path(&index_path, &local_index)
            )?;
            if pack_size != pack.size {
                return Err(CrabError::CorruptObject {
                    path: pack_path.as_ref().to_owned(),
                    reason: format!("pack has {pack_size} bytes, manifest records {}", pack.size),
                });
            }
            let expected_id = pack.pack_id.clone();
            tokio::task::spawn_blocking(move || {
                let (hash, _) = hash_file(&local_pack)?;
                if blake3::Hash::from_bytes(hash).to_hex().as_str() != expected_id {
                    return Err(CrabError::CorruptObject {
                        path: local_pack.display().to_string(),
                        reason: "pack body hash does not match manifest pack id".to_owned(),
                    });
                }
                write_pack_reverse_index(&local_index, &local_reverse_index)
                    .map_err(crab_git::pack::PackError::from)?;
                let locations =
                    PackLocationIter::open(&local_index, &local_reverse_index, pack_size)
                        .map_err(crab_git::pack::PackError::from)?;
                if locations.object_count() != pack.object_count {
                    return Err(CrabError::CorruptObject {
                        path: local_index.display().to_string(),
                        reason: format!(
                            "index has {} objects, manifest records {}",
                            locations.object_count(),
                            pack.object_count
                        ),
                    });
                }
                run_git(
                    Command::new("git")
                        .arg("verify-pack")
                        .arg("-v")
                        .arg(&local_index)
                        .stdout(Stdio::null()),
                    "verify source pack",
                )
            })
            .await
            .map_err(|error| {
                CrabError::Internal(format!("pack verification join failed: {error}"))
            })?
        }
    }))
    .buffer_unordered(concurrency.max(1))
    .collect::<Vec<_>>()
    .await;
    check_cancelled(cancel)?;
    for result in results {
        result?;
    }
    Ok(())
}

fn initialize_bare_repository(git_dir: &Path) -> Result<()> {
    run_git(
        Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(git_dir),
        "initialize temporary bare repository",
    )
}

fn build_and_validate_pack(
    source_git_dir: &Path,
    output_dir: &Path,
    refs: &BTreeSet<String>,
) -> Result<GeneratedPack> {
    if refs.is_empty() {
        return Err(CrabError::Internal(
            "cannot repack a manifest with no refs".to_owned(),
        ));
    }
    let pack_path = output_dir.join("replacement.pack");
    let index_path = output_dir.join("replacement.idx");
    let reverse_index_path = output_dir.join("replacement.rev");
    let stdout = File::create(&pack_path)?;
    let mut child = Command::new("git")
        .arg(format!("--git-dir={}", source_git_dir.display()))
        .arg("pack-objects")
        .arg("--stdout")
        .arg("--revs")
        .arg("--delta-base-offset")
        .arg("--window-memory=256m")
        .arg("--depth=64")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout))
        .spawn()?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            CrabError::Internal("git pack-objects did not expose stdin".to_owned())
        })?;
        for oid in refs {
            writeln!(stdin, "{oid}")?;
        }
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(CrabError::Internal(format!(
            "git pack-objects failed with {status}"
        )));
    }
    run_git(
        Command::new("git")
            .arg("index-pack")
            .arg("-o")
            .arg(&index_path)
            .arg(&pack_path)
            .stdout(Stdio::null()),
        "index replacement pack",
    )?;
    write_pack_reverse_index(&index_path, &reverse_index_path)
        .map_err(crab_git::pack::PackError::from)?;
    if !reverse_index_path.is_file() {
        return Err(CrabError::Internal(
            "reverse-index generation did not create the canonical reverse index".to_owned(),
        ));
    }
    let pack_size = std::fs::metadata(&pack_path)?.len();
    let locations = PackLocationIter::open(&index_path, &reverse_index_path, pack_size)
        .map_err(crab_git::pack::PackError::from)?;
    let object_count = locations.object_count();
    let git_sha1 = locations.pack_checksum().to_string();
    run_git(
        Command::new("git")
            .arg("verify-pack")
            .arg("-v")
            .arg(&index_path)
            .stdout(Stdio::null()),
        "verify replacement pack",
    )?;
    validate_replacement_repository(
        output_dir,
        refs,
        &pack_path,
        &index_path,
        &reverse_index_path,
    )?;

    let (pack_hash, hashed_pack_size) = hash_file(&pack_path)?;
    if hashed_pack_size != pack_size {
        return Err(CrabError::Internal(
            "replacement pack changed during validation".to_owned(),
        ));
    }
    let pack_id = blake3::Hash::from_bytes(pack_hash).to_hex().to_string();
    let (index_hash, index_size) = hash_file(&index_path)?;
    let (reverse_index_hash, reverse_index_size) = hash_file(&reverse_index_path)?;
    Ok(GeneratedPack {
        pack_path,
        index_path,
        reverse_index_path,
        pack_id,
        pack_hash,
        pack_size,
        index_hash,
        index_size,
        reverse_index_hash,
        reverse_index_size,
        object_count,
        git_sha1,
    })
}

fn validate_replacement_repository(
    output_dir: &Path,
    refs: &BTreeSet<String>,
    pack_path: &Path,
    index_path: &Path,
    reverse_index_path: &Path,
) -> Result<()> {
    let validation = output_dir.join("validation.git");
    initialize_bare_repository(&validation)?;
    let pack_dir = validation.join("objects/pack");
    std::fs::copy(pack_path, pack_dir.join("pack-replacement.pack"))?;
    std::fs::copy(index_path, pack_dir.join("pack-replacement.idx"))?;
    std::fs::copy(reverse_index_path, pack_dir.join("pack-replacement.rev"))?;
    for (index, oid) in refs.iter().enumerate() {
        let validation_ref = format!("refs/heads/crab-repack-{index}");
        run_git(
            Command::new("git")
                .arg(format!("--git-dir={}", validation.display()))
                .arg("update-ref")
                .arg(&validation_ref)
                .arg(oid),
            "pin replacement validation ref",
        )?;
        if index == 0 {
            run_git(
                Command::new("git")
                    .arg(format!("--git-dir={}", validation.display()))
                    .arg("symbolic-ref")
                    .arg("HEAD")
                    .arg(&validation_ref),
                "pin replacement validation HEAD",
            )?;
        }
    }
    run_git(
        Command::new("git")
            .arg(format!("--git-dir={}", validation.display()))
            .arg("fsck")
            .arg("--strict")
            .arg("--full")
            .arg("--no-reflogs"),
        "validate replacement repository",
    )
}

fn run_git(command: &mut Command, operation: &str) -> Result<()> {
    debug!(operation, command = ?command, "running git repack subprocess");
    let status = command.status()?;
    if status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "{operation} failed with {status}"
    )))
}

fn hash_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok((*hasher.finalize().as_bytes(), size))
}

async fn upload_generated_pack(
    store: &Store,
    router: &StoreLayout,
    generated: &GeneratedPack,
    cancel: &CancellationToken,
) -> Result<()> {
    store
        .put_multipart_file_retry(
            &repo_pack_path(router.repo_prefix(), &generated.pack_id),
            &generated.pack_path,
            generated.pack_size,
            generated.pack_hash,
            MULTIPART_PART_SIZE,
            cancel,
            None,
        )
        .await?;
    store
        .put_multipart_file_retry(
            &repo_pack_index_path(router.repo_prefix(), &generated.pack_id),
            &generated.index_path,
            generated.index_size,
            generated.index_hash,
            MULTIPART_PART_SIZE,
            cancel,
            None,
        )
        .await?;
    store
        .put_multipart_file_retry(
            &repo_pack_reverse_index_path(router.repo_prefix(), &generated.pack_id),
            &generated.reverse_index_path,
            generated.reverse_index_size,
            generated.reverse_index_hash,
            MULTIPART_PART_SIZE,
            cancel,
            None,
        )
        .await
}

fn now_iso8601() -> String {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let hours = time_of_day / 3_600;
    let minutes = (time_of_day % 3_600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    use super::*;

    #[test]
    fn repack_manifest_changes_only_generation_owned_fields() {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        manifest.shard_index_hash = "b".repeat(64);
        manifest.commit_graph_hash = Some("c".repeat(64));
        manifest.ref_registry_hash = Some("d".repeat(64));

        let updated = repack_manifest(manifest.clone(), 9, "e".repeat(64));

        assert_eq!(updated.generation, 9);
        assert_eq!(updated.pack_index_hash, "e".repeat(64));
        assert_eq!(updated.refs, manifest.refs);
        assert_eq!(updated.shard_index_hash, manifest.shard_index_hash);
        assert_eq!(updated.commit_graph_hash, manifest.commit_graph_hash);
        assert_eq!(updated.ref_registry_hash, manifest.ref_registry_hash);
    }

    #[test]
    fn rebind_visibility_changes_only_generation_anchor() {
        let mut refs = std::collections::BTreeMap::new();
        refs.insert("refs/heads/main".to_owned(), vec!["a".repeat(40)]);
        let visibility = crab_metadata::git_visibility::GitVisibilityIndex::new(
            3,
            "b".repeat(64),
            "d".repeat(64),
            refs.clone(),
        );
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 4;
        manifest.pack_index_hash = "c".repeat(64);
        manifest.seal_git_validation();

        let rebound = rebind_visibility(visibility, &manifest);

        assert_eq!(rebound.generation, 4);
        assert_eq!(rebound.pack_index_hash, "c".repeat(64));
        assert_eq!(
            rebound.git_validation_digest,
            manifest.git_validation_digest
        );
        assert_eq!(rebound.refs, refs);
    }

    #[tokio::test]
    async fn missing_visibility_remains_optional() -> Result<()> {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(backend);
        let router = StoreLayout::new(store.clone(), "org/repack-test".to_owned());
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.pack_index_hash = "a".repeat(64);
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "b".repeat(40));

        assert!(
            read_current_visibility(&store, &router, &manifest)
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repack_commits_one_verified_pack_and_locator_generation() -> Result<()> {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(Arc::clone(&backend));
        let prefix = "org/repack-test";
        let router = StoreLayout::new(store.clone(), prefix.to_owned());
        let source = tempfile::tempdir()?;
        let repository = source.path().join("repository");
        initialize_work_repository(&repository)?;

        std::fs::write(repository.join("first.txt"), b"first\n")?;
        commit_all(&repository, "first")?;
        let first = snapshot_repository_pack(&repository, source.path(), "first")?;
        std::fs::write(repository.join("second.txt"), b"second\n")?;
        commit_all(&repository, "second")?;
        let second = snapshot_repository_pack(&repository, source.path(), "second")?;
        let tip = git_output(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .arg("rev-parse")
                .arg("HEAD"),
            "resolve test tip",
        )?;

        let entries = vec![
            upload_test_pack(&store, &router, &first, &tip).await?,
            upload_test_pack(&store, &router, &second, &tip).await?,
        ];
        let (shard_index_hash, _shard_index, shard_write) =
            crate::metadata::manifest::compact_shard_index(1, &[])?;
        let (pack_index_hash, _pack_index, pack_write) = compact_pack_index(1, &entries)?;
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: shard_write,
                pack_index: pack_write,
            },
        )
        .await?;
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.created_at = now_iso8601();
        manifest.session_id = "fixture".to_owned();
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), tip.clone());
        manifest.shard_index_hash = shard_index_hash;
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        crate::metadata::manifest::create_manifest(&store, &router, &manifest).await?;
        crate::git::push::publish_git_visibility_index_from_git_dir(
            &repository.join(".git"),
            &manifest,
            &store,
            &router,
        )
        .await?;

        let outcome = run_repack(
            &store,
            prefix,
            &RepackConfig {
                lock_ttl: Duration::from_secs(60),
                dry_run: false,
                download_concurrency: 2,
            },
            &CancellationToken::new(),
        )
        .await?;

        assert_eq!(outcome.packs_before, 2);
        assert_eq!(outcome.packs_after, 1);
        let (committed, _) = read_manifest(&store, &router).await?;
        assert_eq!(committed.generation, 2);
        assert_eq!(committed.refs.get("refs/heads/main"), Some(&tip));
        let replacement = read_bulk_pack_list(&store, &router, &committed.pack_index_hash).await?;
        assert_eq!(replacement.len(), 1);
        store
            .head(&router.pack_path(&replacement[0].pack_id))
            .await?;
        store
            .head(&router.pack_index_path(&replacement[0].pack_id))
            .await?;
        store
            .head(&router.pack_reverse_index_path(&replacement[0].pack_id))
            .await?;
        let session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            prefix,
        )
        .await?;
        assert_eq!(
            session.coverage(),
            Some(crab_metadata::git_object_locator::GitLocatorCoverage {
                generation: 2,
                pack_index_hash: manifest_hash_or_default(&committed.pack_index_hash)?,
            })
        );
        session.close().await?;
        let storage_router = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let visibility = crab_metadata::git_visibility::read(
            store.as_storage(),
            &storage_router,
            committed.generation,
            &committed.pack_index_hash,
            &committed.git_validation_digest,
        )
        .await?;
        assert_eq!(visibility.refs.len(), 1);
        assert!(
            visibility.refs["refs/heads/main"]
                .binary_search(&tip)
                .is_ok()
        );
        Ok(())
    }

    fn initialize_work_repository(repository: &Path) -> Result<()> {
        run_git(
            Command::new("git")
                .arg("init")
                .arg("--quiet")
                .arg(repository),
            "initialize test repository",
        )?;
        run_git(
            Command::new("git").arg("-C").arg(repository).args([
                "config",
                "user.name",
                "Crab Test",
            ]),
            "configure test user name",
        )?;
        run_git(
            Command::new("git").arg("-C").arg(repository).args([
                "config",
                "user.email",
                "crab@example.invalid",
            ]),
            "configure test user email",
        )
    }

    fn commit_all(repository: &Path, message: &str) -> Result<()> {
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["add", "."]),
            "stage test commit",
        )?;
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["commit", "--quiet", "-m", message]),
            "create test commit",
        )
    }

    struct TestPack {
        pack_path: PathBuf,
        index_path: PathBuf,
    }

    fn snapshot_repository_pack(
        repository: &Path,
        destination: &Path,
        name: &str,
    ) -> Result<TestPack> {
        run_git(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["gc", "--quiet"]),
            "pack test repository",
        )?;
        let pack_dir = repository.join(".git/objects/pack");
        let source_pack = std::fs::read_dir(&pack_dir)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .ok_or_else(|| CrabError::Internal("test repository has no pack".to_owned()))?;
        let source_index = source_pack.with_extension("idx");
        let pack_path = destination.join(format!("{name}.pack"));
        let index_path = destination.join(format!("{name}.idx"));
        std::fs::copy(source_pack, &pack_path)?;
        std::fs::copy(source_index, &index_path)?;
        Ok(TestPack {
            pack_path,
            index_path,
        })
    }

    async fn upload_test_pack(
        store: &Store,
        router: &StoreLayout,
        pack: &TestPack,
        tip: &str,
    ) -> Result<PackManifestEntry> {
        let (hash, size) = hash_file(&pack.pack_path)?;
        let pack_id = blake3::Hash::from_bytes(hash).to_hex().to_string();
        let reverse_index_path = pack.index_path.with_extension("rev");
        write_pack_reverse_index(&pack.index_path, &reverse_index_path)
            .map_err(crab_git::pack::PackError::from)?;
        let locations = PackLocationIter::open(&pack.index_path, &reverse_index_path, size)
            .map_err(crab_git::pack::PackError::from)?;
        store
            .put(
                &router.pack_path(&pack_id),
                Bytes::from(std::fs::read(&pack.pack_path)?),
            )
            .await?;
        store
            .put(
                &router.pack_index_path(&pack_id),
                Bytes::from(std::fs::read(&pack.index_path)?),
            )
            .await?;
        Ok(PackManifestEntry {
            pack_id: pack_id.clone(),
            size,
            content_hash: pack_id,
            ref_tips: vec![tip.to_owned()],
            object_count: locations.object_count(),
        })
    }

    fn git_output(command: &mut Command, operation: &str) -> Result<String> {
        let output = command.output()?;
        if !output.status.success() {
            return Err(CrabError::Internal(format!(
                "{operation} failed with {}",
                output.status
            )));
        }
        String::from_utf8(output.stdout)
            .map(|value| value.trim().to_owned())
            .map_err(|error| CrabError::Internal(format!("{operation} returned non-UTF8: {error}")))
    }
}
