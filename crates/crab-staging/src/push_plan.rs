use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::error::{Result, StagingError};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::FOOTER_SIZE;
use crab_xet::xorb::format::{ChunkPlacement, XorbRef};
use crab_xet::xorb::parser::{xorb_chunks_from_metadata, xorb_metadata_region};

pub use crate::add_push_plan::{
    AddPlanFile, AddPushPlanSummary, ExistingChunkLookup, LocalXorbCandidateLookup,
    prepare_file_push_plans, prepare_file_push_plans_with_progress,
};

pub const FILE_PUSH_PLAN_VERSION: u32 = 1;

const PLAN_DIR: &str = "push-plans";
const PAYLOAD_DIR: &str = "payloads";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePushPlan {
    pub version: u32,
    pub staged_chunk_sequence_verified: bool,
    pub file_hash: String,
    pub file_size: u64,
    pub chunk_count: u64,
    pub chunk_sequence_hash: String,
    #[serde(skip)]
    pub existing: Vec<PlannedExistingChunk>,
    pub prepared_xorbs: Vec<PlannedXorb>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedExistingChunk {
    pub chunk_hash: String,
    pub xorb_hash: String,
    pub chunk_index: u32,
    pub uncompressed_size: u32,
    pub placement_id: String,
    pub origin_proof_id: String,
}

/// Remote chunk placement whose committed origin proof was revalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingChunkCandidate {
    pub xorb_ref: XorbRef,
    pub placement_id: [u8; 32],
    pub origin_proof_id: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedXorb {
    pub hash: String,
    pub payload_hash: String,
    pub bytes: u64,
    /// Whether push should adopt this complete xorb as an upload candidate.
    /// False keeps the xorb solely as local reconstruction authority.
    pub upload: bool,
    pub placements: Vec<PlannedPlacement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedPlacement {
    pub chunk_hash: String,
    pub xorb_hash: String,
    pub chunk_index: u32,
    pub uncompressed_size: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PushPlanStats {
    pub format_version: u32,
    pub verified_prepared_xorbs: bool,
    pub plan_files: u64,
    pub invalid_plan_files: u64,
    pub planned_file_bytes: u64,
    pub planned_chunks: u64,
    pub existing_chunks: u64,
    pub prepared_xorbs: u64,
    pub prepared_chunks: u64,
    pub prepared_bytes: u64,
    pub indexed_prepared_xorbs: u64,
    pub orphaned_indexed_prepared_xorbs: u64,
    pub invalid_indexed_prepared_xorbs: u64,
    pub referenced_prepared_xorb_files: u64,
    pub referenced_prepared_xorb_file_bytes: u64,
    pub missing_prepared_xorb_files: u64,
    pub mismatched_prepared_xorb_files: u64,
    pub stale_prepared_xorb_files: u64,
    pub stale_prepared_xorb_file_bytes: u64,
    pub verified_prepared_xorb_files: u64,
    pub verified_prepared_xorb_file_bytes: u64,
    pub payload_hash_mismatched_prepared_xorb_files: u64,
    pub corrupt_prepared_xorb_files: u64,
    pub metadata_mismatched_prepared_xorb_files: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PushPlanSummaryOptions {
    pub verify_prepared_xorbs: bool,
}

#[derive(Debug)]
pub struct PreparedXorbCandidate {
    pub source: PreparedXorbSource,
    pub xorb_hash: MerkleHash,
    pub planned: PlannedXorb,
    pub placements: Vec<ChunkPlacement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreparedXorbSource {
    Staging,
    LocalCache(PathBuf),
}

impl PreparedXorbCandidate {
    pub fn placement_for(&self, chunk_hash: &MerkleHash) -> Option<&ChunkPlacement> {
        self.placements
            .iter()
            .find(|placement| &placement.chunk_hash == chunk_hash)
    }
}

#[derive(Debug, Default)]
pub struct PreparedXorbCache {
    chunks: HashMap<MerkleHash, Vec<Arc<PreparedXorbCandidate>>>,
    xorbs: HashMap<MerkleHash, Vec<Arc<PreparedXorbCandidate>>>,
}

impl PreparedXorbCache {
    pub(crate) fn is_empty(&self) -> bool {
        self.xorbs.is_empty()
    }

    pub fn candidates_for_chunk(
        &self,
        chunk_hash: &MerkleHash,
    ) -> impl Iterator<Item = Arc<PreparedXorbCandidate>> + '_ {
        self.chunks
            .get(chunk_hash)
            .into_iter()
            .flat_map(|candidates| candidates.iter().cloned())
    }

    pub fn insert_prepared_xorb(&mut self, planned: &PlannedXorb) -> Result<()> {
        self.insert_candidate(PreparedXorbSource::Staging, planned)
    }

    pub fn insert_cached_xorb(
        &mut self,
        source_path: PathBuf,
        planned: &PlannedXorb,
    ) -> Result<()> {
        self.insert_candidate(PreparedXorbSource::LocalCache(source_path), planned)
    }

    fn insert_candidate(
        &mut self,
        source: PreparedXorbSource,
        planned: &PlannedXorb,
    ) -> Result<()> {
        let xorb_hash = planned.hash()?;
        let placements: Vec<ChunkPlacement> = planned
            .placements
            .iter()
            .map(PlannedPlacement::to_placement)
            .collect::<Result<Vec<_>>>()?;
        if placements
            .iter()
            .any(|placement| placement.xorb_hash != xorb_hash)
        {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} contains placement for another xorb",
                xorb_hash.hex()
            )));
        }
        if let Some(existing) = self.xorbs.get(&xorb_hash) {
            let Some(first) = existing.first() else {
                return Err(StagingError::StagingCorrupt(format!(
                    "prepared xorb {} has no cached metadata",
                    xorb_hash.hex()
                )));
            };
            if first.planned.bytes == planned.bytes
                && first.planned.payload_hash == planned.payload_hash
                && placements_match(&first.placements, &placements)
            {
                if existing.iter().any(|candidate| candidate.source == source) {
                    return Ok(());
                }
                let candidate = Arc::new(PreparedXorbCandidate {
                    source,
                    xorb_hash,
                    planned: planned.clone(),
                    placements,
                });
                let mut indexed_chunks = HashSet::new();
                for placement in &candidate.placements {
                    if indexed_chunks.insert(placement.chunk_hash) {
                        self.chunks
                            .entry(placement.chunk_hash)
                            .or_default()
                            .push(Arc::clone(&candidate));
                    }
                }
                let candidates = self.xorbs.get_mut(&xorb_hash).ok_or_else(|| {
                    StagingError::StagingCorrupt(format!(
                        "prepared xorb {} lost cached metadata",
                        xorb_hash.hex()
                    ))
                })?;
                candidates.push(candidate);
                return Ok(());
            }
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} has conflicting cached metadata",
                xorb_hash.hex()
            )));
        }

        let candidate = Arc::new(PreparedXorbCandidate {
            source,
            xorb_hash,
            planned: planned.clone(),
            placements,
        });
        let mut indexed_chunks = HashSet::new();
        for placement in &candidate.placements {
            if indexed_chunks.insert(placement.chunk_hash) {
                self.chunks
                    .entry(placement.chunk_hash)
                    .or_default()
                    .push(Arc::clone(&candidate));
            }
        }
        self.xorbs.insert(xorb_hash, vec![candidate]);
        Ok(())
    }
}

fn placements_match(left: &[ChunkPlacement], right: &[ChunkPlacement]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.chunk_hash == right.chunk_hash
                && left.xorb_hash == right.xorb_hash
                && left.chunk_index == right.chunk_index
                && left.uncompressed_size == right.uncompressed_size
        })
}

impl FilePushPlan {
    pub fn new(file_hash: MerkleHash, file_size: u64, chunks: &[(MerkleHash, u64)]) -> Self {
        Self {
            version: FILE_PUSH_PLAN_VERSION,
            staged_chunk_sequence_verified: false,
            file_hash: file_hash.hex(),
            file_size,
            chunk_count: chunks.len() as u64,
            chunk_sequence_hash: blake3::Hash::from(chunk_sequence_hash(chunks))
                .to_hex()
                .to_string(),
            existing: Vec::new(),
            prepared_xorbs: Vec::new(),
        }
    }

    pub fn new_verified_staging(
        file_hash: MerkleHash,
        file_size: u64,
        chunks: &[(MerkleHash, u64)],
    ) -> Self {
        let mut plan = Self::new(file_hash, file_size, chunks);
        plan.staged_chunk_sequence_verified = true;
        plan
    }

    #[must_use]
    pub fn new_verified_recipe(recipe: &crate::recipe::FileRecipe) -> Self {
        Self {
            version: FILE_PUSH_PLAN_VERSION,
            staged_chunk_sequence_verified: true,
            file_hash: recipe.file_hash().hex(),
            file_size: recipe.file_size(),
            chunk_count: recipe.chunk_count(),
            chunk_sequence_hash: blake3::Hash::from(recipe.sequence_hash())
                .to_hex()
                .to_string(),
            existing: Vec::new(),
            prepared_xorbs: Vec::new(),
        }
    }

    pub fn file_hash(&self) -> Result<MerkleHash> {
        parse_hash(&self.file_hash, "file_hash")
    }

    pub fn sequence_hash(&self) -> Result<[u8; 32]> {
        let bytes = blake3::Hash::from_hex(&self.chunk_sequence_hash).map_err(|e| {
            StagingError::StagingCorrupt(format!(
                "invalid push-plan chunk sequence hash {}: {e}",
                self.chunk_sequence_hash
            ))
        })?;
        Ok(*bytes.as_bytes())
    }

    pub fn existing_refs(&self) -> Result<Vec<(MerkleHash, XorbRef)>> {
        self.existing_candidates().map(|candidates| {
            candidates
                .into_iter()
                .map(|(chunk_hash, candidate)| (chunk_hash, candidate.xorb_ref))
                .collect()
        })
    }

    pub fn existing_candidates(&self) -> Result<Vec<(MerkleHash, ExistingChunkCandidate)>> {
        self.existing
            .iter()
            .map(|existing| {
                Ok((
                    parse_hash(&existing.chunk_hash, "existing chunk hash")?,
                    existing.candidate()?,
                ))
            })
            .collect()
    }
}

pub(crate) fn chunk_sequence_hash(chunks: &[(MerkleHash, u64)]) -> [u8; 32] {
    let mut hasher = crate::recipe::new_sequence_hasher();
    for (chunk_hash, size) in chunks {
        crate::recipe::update_sequence_hasher(&mut hasher, *chunk_hash, *size);
    }
    *hasher.finalize().as_bytes()
}

impl PlannedExistingChunk {
    pub fn from_candidate(chunk_hash: MerkleHash, candidate: ExistingChunkCandidate) -> Self {
        Self {
            chunk_hash: chunk_hash.hex(),
            xorb_hash: candidate.xorb_ref.xorb_hash.hex(),
            chunk_index: candidate.xorb_ref.chunk_index,
            uncompressed_size: candidate.xorb_ref.uncompressed_size,
            placement_id: blake3::Hash::from(candidate.placement_id)
                .to_hex()
                .to_string(),
            origin_proof_id: blake3::Hash::from(candidate.origin_proof_id)
                .to_hex()
                .to_string(),
        }
    }

    pub fn candidate(&self) -> Result<ExistingChunkCandidate> {
        Ok(ExistingChunkCandidate {
            xorb_ref: XorbRef {
                xorb_hash: parse_hash(&self.xorb_hash, "existing xorb hash")?,
                chunk_index: self.chunk_index,
                uncompressed_size: self.uncompressed_size,
            },
            placement_id: parse_hash_bytes(&self.placement_id, "existing placement id")?,
            origin_proof_id: parse_hash_bytes(&self.origin_proof_id, "existing origin proof id")?,
        })
    }
}

impl PlannedXorb {
    pub fn hash(&self) -> Result<MerkleHash> {
        parse_hash(&self.hash, "prepared xorb hash")
    }

    pub(crate) fn payload_hash_bytes(&self) -> Result<[u8; 32]> {
        parse_hash_bytes(&self.payload_hash, "prepared xorb payload hash")
    }
}

impl PlannedPlacement {
    pub fn from_placement(placement: &ChunkPlacement) -> Self {
        Self {
            chunk_hash: placement.chunk_hash.hex(),
            xorb_hash: placement.xorb_hash.hex(),
            chunk_index: placement.chunk_index,
            uncompressed_size: placement.uncompressed_size,
        }
    }

    pub fn to_placement(&self) -> Result<ChunkPlacement> {
        Ok(ChunkPlacement {
            chunk_hash: parse_hash(&self.chunk_hash, "planned placement chunk hash")?,
            xorb_hash: parse_hash(&self.xorb_hash, "planned placement xorb hash")?,
            chunk_index: self.chunk_index,
            uncompressed_size: self.uncompressed_size,
        })
    }
}

pub fn prepared_xorb_path(root: &Path, xorb_hash: &MerkleHash) -> PathBuf {
    let hex = xorb_hash.hex();
    root.join(PLAN_DIR)
        .join(PAYLOAD_DIR)
        .join(&hex[..2])
        .join(format!("{hex}.xorb"))
}

pub(crate) fn prepared_xorb_file_matches_cached_plan(
    path: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() != planned.bytes {
        return false;
    }
    parse_prepared_xorb_metadata(path, file_hash, xorb_hash, planned).is_ok()
}

pub async fn materialize_prepared_xorb(
    root: &Path,
    candidate: &PreparedXorbCandidate,
) -> Result<bool> {
    let source = match &candidate.source {
        PreparedXorbSource::Staging => prepared_xorb_path(root, &candidate.xorb_hash),
        PreparedXorbSource::LocalCache(path) => path.clone(),
    };
    materialize_prepared_xorb_file(root, &source, &candidate.planned).await
}

async fn materialize_prepared_xorb_file(
    root: &Path,
    source: &Path,
    planned: &PlannedXorb,
) -> Result<bool> {
    let xorb_hash = planned.hash()?;
    let target = prepared_xorb_path(root, &xorb_hash);
    if source == target {
        return prepared_xorb_file_matches_plan(source, &xorb_hash, &xorb_hash, planned).await;
    }

    if !prepared_xorb_file_matches_plan(source, &xorb_hash, &xorb_hash, planned).await? {
        return Ok(false);
    }

    if prepared_xorb_file_matches_plan(&target, &xorb_hash, &xorb_hash, planned).await? {
        return Ok(true);
    }
    fail_if_existing_prepared_xorb_is_corrupt(&target, &xorb_hash).await?;

    let parent = target
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    match tokio::fs::hard_link(source, &target).await {
        Ok(()) => {
            sync_parent_directory(&target).await?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            prepared_xorb_file_matches_plan(&target, &xorb_hash, &xorb_hash, planned).await
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::CrossesDevices
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            match copy_prepared_xorb(source, &target, &xorb_hash, planned).await {
                Ok(copied) => Ok(copied),
                Err(StagingError::Io(copy_err))
                    if matches!(
                        copy_err.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::Unsupported
                            | std::io::ErrorKind::CrossesDevices
                    ) =>
                {
                    Ok(false)
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e.into()),
    }
}

async fn copy_prepared_xorb(
    source: &Path,
    target: &Path,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<bool> {
    let parent = target
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = unique_prepared_xorb_temp_path(target, "copy");
    let mut tmp_guard = TempFileGuard::new(tmp.clone());

    async {
        tokio::fs::copy(source, &tmp).await?;
        if !prepared_xorb_file_matches_plan(&tmp, xorb_hash, xorb_hash, planned).await? {
            return Ok(false);
        }
        let payload_hash = planned.payload_hash_bytes()?;
        install_prepared_xorb_temp(&tmp, target, xorb_hash, &payload_hash, planned.bytes).await?;
        tmp_guard.disarm();
        Ok(true)
    }
    .await
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn prepared_xorb_file_matches_plan(
    path: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<bool> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !metadata.is_file() || metadata.len() != planned.bytes {
        return Ok(false);
    }

    match parse_prepared_xorb_metadata_async(path, file_hash, xorb_hash, planned).await {
        Ok(()) => Ok(true),
        Err(StagingError::Io(e)) => Err(e.into()),
        Err(_) => Ok(false),
    }
}

pub fn validate_prepared_xorb_metadata(
    len: usize,
    footer: &[u8],
    metadata: &[u8],
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<Vec<ChunkPlacement>> {
    let (chunks, parsed_hash) = xorb_chunks_from_metadata(len, footer, metadata)?;
    if parsed_hash != *xorb_hash {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} for file {} parses as {}",
            xorb_hash.hex(),
            file_hash.hex(),
            parsed_hash.hex()
        )));
    }

    let placements: Vec<ChunkPlacement> = planned
        .placements
        .iter()
        .map(PlannedPlacement::to_placement)
        .collect::<Result<Vec<_>>>()?;
    if chunks.len() != placements.len() {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} has {} chunks, plan has {} placements",
            xorb_hash.hex(),
            chunks.len(),
            placements.len()
        )));
    }
    for placement in &placements {
        if placement.xorb_hash != *xorb_hash {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} plan contains placement for {}",
                xorb_hash.hex(),
                placement.xorb_hash.hex()
            )));
        }
        let meta = chunks
            .get(usize::try_from(placement.chunk_index).map_err(|_| {
                StagingError::Internal(
                    "prepared xorb placement index does not fit usize".to_owned(),
                )
            })?)
            .ok_or_else(|| {
                StagingError::StagingCorrupt(format!(
                    "prepared xorb {} plan references missing placement {}",
                    xorb_hash.hex(),
                    placement.chunk_index
                ))
            })?;
        if meta.hash != placement.chunk_hash || meta.uncompressed_len != placement.uncompressed_size
        {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} placement {} does not match parsed chunk metadata",
                xorb_hash.hex(),
                placement.chunk_index
            )));
        }
    }

    Ok(placements)
}

/// Returns an empty push-plan inventory with the current format metadata.
pub fn empty_push_plan_stats(options: PushPlanSummaryOptions) -> PushPlanStats {
    PushPlanStats {
        format_version: FILE_PUSH_PLAN_VERSION,
        verified_prepared_xorbs: options.verify_prepared_xorbs,
        ..PushPlanStats::default()
    }
}

pub(crate) fn new_push_plan_stats(options: PushPlanSummaryOptions) -> PushPlanStats {
    empty_push_plan_stats(options)
}

pub async fn write_prepared_xorb(root: &Path, xorb_hash: &MerkleHash, bytes: Bytes) -> Result<u64> {
    let path = prepared_xorb_path(root, xorb_hash);
    let parent = path
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    validate_prepared_xorb_bytes_identity(&bytes, xorb_hash)?;
    let payload_hash = *blake3::hash(&bytes).as_bytes();
    let byte_count = bytes.len() as u64;
    if prepared_xorb_file_matches_identity(&path, xorb_hash, &payload_hash, byte_count).await? {
        return Ok(byte_count);
    }
    fail_if_existing_prepared_xorb_is_corrupt(&path, xorb_hash).await?;

    let tmp = unique_prepared_xorb_temp_path(&path, "write");
    let mut tmp_guard = TempFileGuard::new(tmp.clone());
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    drop(file);
    install_prepared_xorb_temp(&tmp, &path, xorb_hash, &payload_hash, byte_count).await?;
    tmp_guard.disarm();
    Ok(byte_count)
}

pub async fn move_prepared_xorb(
    root: &Path,
    xorb_hash: &MerkleHash,
    source_path: &Path,
) -> Result<u64> {
    let path = prepared_xorb_path(root, xorb_hash);
    let parent = path
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = unique_prepared_xorb_temp_path(&path, "move");
    let mut tmp_guard = TempFileGuard::new(tmp.clone());
    tokio::fs::rename(source_path, &tmp).await?;
    let bytes = tokio::fs::metadata(&tmp).await?.len();
    let payload_hash = hash_prepared_xorb_file_async(&tmp).await?;
    validate_prepared_xorb_file_identity(&tmp, xorb_hash, bytes).await?;
    if prepared_xorb_file_matches_identity(&path, xorb_hash, &payload_hash, bytes).await? {
        tokio::fs::remove_file(&tmp).await?;
        tmp_guard.disarm();
        return Ok(bytes);
    }
    fail_if_existing_prepared_xorb_is_corrupt(&path, xorb_hash).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    install_prepared_xorb_temp(&tmp, &path, xorb_hash, &payload_hash, bytes).await?;
    tmp_guard.disarm();
    Ok(bytes)
}

fn unique_prepared_xorb_temp_path(target: &Path, operation: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    target.with_extension(format!(
        "xorb.{operation}-{}-{nonce}-{sequence}.tmp",
        std::process::id()
    ))
}

async fn install_prepared_xorb_temp(
    temp: &Path,
    target: &Path,
    xorb_hash: &MerkleHash,
    payload_hash: &[u8; 32],
    bytes: u64,
) -> Result<()> {
    validate_prepared_xorb_file_identity(temp, xorb_hash, bytes).await?;
    let actual_payload_hash = hash_prepared_xorb_file_async(temp).await?;
    if &actual_payload_hash != payload_hash {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} temporary payload digest changed before sealing",
            xorb_hash.hex()
        )));
    }

    match tokio::fs::hard_link(temp, target).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !prepared_xorb_file_matches_identity(target, xorb_hash, payload_hash, bytes).await? {
                return Err(StagingError::StagingCorrupt(format!(
                    "content-addressed prepared xorb {} already exists with different bytes",
                    xorb_hash.hex()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    tokio::fs::remove_file(temp).await?;
    sync_parent_directory(target).await
}

async fn fail_if_existing_prepared_xorb_is_corrupt(
    path: &Path,
    xorb_hash: &MerkleHash,
) -> Result<()> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Err(StagingError::StagingCorrupt(format!(
            "content-addressed prepared xorb {} exists but fails identity validation",
            xorb_hash.hex()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn prepared_xorb_file_matches_identity(
    path: &Path,
    xorb_hash: &MerkleHash,
    payload_hash: &[u8; 32],
    bytes: u64,
) -> Result<bool> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() != bytes {
        return Ok(false);
    }
    if hash_prepared_xorb_file_async(path).await? != *payload_hash {
        return Ok(false);
    }
    Ok(validate_prepared_xorb_file_identity(path, xorb_hash, bytes)
        .await
        .is_ok())
}

async fn hash_prepared_xorb_file_async(path: &Path) -> Result<[u8; 32]> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_prepared_xorb_file(&path))
        .await
        .map_err(|error| StagingError::Internal(format!("prepared xorb hash join: {error}")))?
}

async fn validate_prepared_xorb_file_identity(
    path: &Path,
    xorb_hash: &MerkleHash,
    bytes: u64,
) -> Result<()> {
    let path = path.to_path_buf();
    let xorb_hash = *xorb_hash;
    tokio::task::spawn_blocking(move || {
        let len = usize::try_from(bytes).map_err(|_| {
            StagingError::StagingCorrupt(format!(
                "prepared xorb {} is too large to validate on this platform",
                xorb_hash.hex()
            ))
        })?;
        if len < FOOTER_SIZE {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} is too small for a footer",
                xorb_hash.hex()
            )));
        }
        let mut file = std::fs::File::open(path)?;
        if file.metadata()?.len() != bytes {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} changed size during validation",
                xorb_hash.hex()
            )));
        }
        file.seek(SeekFrom::Start((len - FOOTER_SIZE) as u64))?;
        let mut footer = vec![0; FOOTER_SIZE];
        file.read_exact(&mut footer)?;
        let region = xorb_metadata_region(len, &footer)?;
        file.seek(SeekFrom::Start(region.offset as u64))?;
        let mut metadata = vec![0; region.len];
        file.read_exact(&mut metadata)?;
        let (_, parsed_hash) = xorb_chunks_from_metadata(len, &footer, &metadata)?;
        if parsed_hash != xorb_hash {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} body identifies as {}",
                xorb_hash.hex(),
                parsed_hash.hex()
            )));
        }
        Ok(())
    })
    .await
    .map_err(|error| StagingError::Internal(format!("prepared xorb validation join: {error}")))?
}

fn validate_prepared_xorb_bytes_identity(bytes: &[u8], xorb_hash: &MerkleHash) -> Result<()> {
    if bytes.len() < FOOTER_SIZE {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} is too small for a footer",
            xorb_hash.hex()
        )));
    }
    let footer = &bytes[bytes.len() - FOOTER_SIZE..];
    let region = xorb_metadata_region(bytes.len(), footer)?;
    let metadata = bytes
        .get(region.offset..region.offset + region.len)
        .ok_or_else(|| {
            StagingError::StagingCorrupt(format!(
                "prepared xorb {} metadata range is outside the payload",
                xorb_hash.hex()
            ))
        })?;
    let (_, parsed_hash) = xorb_chunks_from_metadata(bytes.len(), footer, metadata)?;
    if parsed_hash != *xorb_hash {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} body identifies as {}",
            xorb_hash.hex(),
            parsed_hash.hex()
        )));
    }
    Ok(())
}

#[cfg(unix)]
async fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?
        .to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
        .await
        .map_err(|error| StagingError::Internal(format!("prepared xorb fsync join: {error}")))??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn parse_hash(value: &str, label: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value)
        .map_err(|e| StagingError::Internal(format!("invalid {label} in add-time push plan: {e}")))
}

fn parse_hash_bytes(value: &str, label: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|error| {
            StagingError::Internal(format!("invalid {label} in add-time push plan: {error}"))
        })
}

pub(crate) fn accumulate_file_plan_for_hash(
    root: &Path,
    file_hash: &MerkleHash,
    plan: &FilePushPlan,
    options: PushPlanSummaryOptions,
    stats: &mut PushPlanStats,
    referenced_xorbs: &mut HashSet<PathBuf>,
) -> Result<bool> {
    if plan.version != FILE_PUSH_PLAN_VERSION {
        return Ok(false);
    }
    let Ok(plan_file_hash) = plan.file_hash() else {
        return Ok(false);
    };
    if &plan_file_hash != file_hash {
        return Ok(false);
    }
    if plan.sequence_hash().is_err() {
        return Ok(false);
    }
    if plan.existing_refs().is_err() {
        return Ok(false);
    }
    for planned_xorb in &plan.prepared_xorbs {
        if planned_xorb.hash().is_err()
            || planned_xorb
                .placements
                .iter()
                .any(|placement| placement.to_placement().is_err())
        {
            return Ok(false);
        }
    }

    stats.plan_files += 1;
    stats.planned_file_bytes = stats
        .planned_file_bytes
        .checked_add(plan.file_size)
        .ok_or_else(|| StagingError::Internal("push-plan file byte count overflowed".to_owned()))?;
    stats.planned_chunks = stats
        .planned_chunks
        .checked_add(plan.chunk_count)
        .ok_or_else(|| StagingError::Internal("push-plan chunk count overflowed".to_owned()))?;
    for planned_xorb in &plan.prepared_xorbs {
        let xorb_hash = planned_xorb.hash()?;
        let path = prepared_xorb_path(root, &xorb_hash);
        referenced_xorbs.insert(path.clone());

        stats.prepared_xorbs = stats
            .prepared_xorbs
            .checked_add(1)
            .ok_or_else(|| StagingError::Internal("push-plan xorb count overflowed".to_owned()))?;
        stats.prepared_chunks = stats
            .prepared_chunks
            .checked_add(planned_xorb.placements.len() as u64)
            .ok_or_else(|| {
                StagingError::Internal("push-plan prepared chunk count overflowed".to_owned())
            })?;
        stats.prepared_bytes = stats
            .prepared_bytes
            .checked_add(planned_xorb.bytes)
            .ok_or_else(|| {
                StagingError::Internal("push-plan prepared byte count overflowed".to_owned())
            })?;

        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {
                stats.referenced_prepared_xorb_files += 1;
                stats.referenced_prepared_xorb_file_bytes = stats
                    .referenced_prepared_xorb_file_bytes
                    .checked_add(meta.len())
                    .ok_or_else(|| {
                        StagingError::Internal(
                            "push-plan referenced xorb byte count overflowed".to_owned(),
                        )
                    })?;
                if meta.len() != planned_xorb.bytes {
                    stats.mismatched_prepared_xorb_files += 1;
                } else if options.verify_prepared_xorbs {
                    verify_referenced_prepared_xorb(
                        &path,
                        file_hash,
                        &xorb_hash,
                        planned_xorb,
                        stats,
                    )?;
                }
            }
            Ok(_) => stats.missing_prepared_xorb_files += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                stats.missing_prepared_xorb_files += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(true)
}

fn verify_referenced_prepared_xorb(
    path: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
    stats: &mut PushPlanStats,
) -> Result<()> {
    let Ok(payload_hash) = hash_prepared_xorb_file(path) else {
        stats.corrupt_prepared_xorb_files += 1;
        return Ok(());
    };
    let payload_hash_hex = blake3::Hash::from(payload_hash).to_hex().to_string();
    if payload_hash_hex != planned.payload_hash {
        stats.payload_hash_mismatched_prepared_xorb_files += 1;
        return Ok(());
    }

    let parsed = parse_prepared_xorb_metadata(path, file_hash, xorb_hash, planned);
    match parsed {
        Ok(()) => {
            stats.verified_prepared_xorb_files += 1;
            stats.verified_prepared_xorb_file_bytes = stats
                .verified_prepared_xorb_file_bytes
                .checked_add(planned.bytes)
                .ok_or_else(|| {
                    StagingError::Internal(
                        "push-plan verified xorb byte count overflowed".to_owned(),
                    )
                })?;
        }
        Err(_) => stats.metadata_mismatched_prepared_xorb_files += 1,
    }
    Ok(())
}

fn hash_prepared_xorb_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn parse_prepared_xorb_metadata(
    path: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<()> {
    let len = usize::try_from(planned.bytes).map_err(|_| {
        StagingError::Internal(format!(
            "prepared xorb {} for file {} is too large to address on this platform",
            xorb_hash.hex(),
            file_hash.hex()
        ))
    })?;
    if len < FOOTER_SIZE {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} for file {} is too small for a footer",
            xorb_hash.hex(),
            file_hash.hex()
        )));
    }

    let mut file = fs::File::open(path)?;
    let footer_offset = u64::try_from(len - FOOTER_SIZE).map_err(|_| {
        StagingError::Internal("prepared xorb footer offset does not fit u64".to_owned())
    })?;
    file.seek(SeekFrom::Start(footer_offset))?;
    let mut footer = vec![0u8; FOOTER_SIZE];
    file.read_exact(&mut footer)?;

    let region = xorb_metadata_region(len, &footer)?;
    let metadata_offset = u64::try_from(region.offset).map_err(|_| {
        StagingError::Internal("prepared xorb metadata offset does not fit u64".to_owned())
    })?;
    file.seek(SeekFrom::Start(metadata_offset))?;
    let mut metadata = vec![0u8; region.len];
    file.read_exact(&mut metadata)?;

    validate_prepared_xorb_metadata(len, &footer, &metadata, file_hash, xorb_hash, planned)
        .map(|_| ())
}

async fn parse_prepared_xorb_metadata_async(
    path: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<()> {
    let len = usize::try_from(planned.bytes).map_err(|_| {
        StagingError::Internal(format!(
            "prepared xorb {} for file {} is too large to address on this platform",
            xorb_hash.hex(),
            file_hash.hex()
        ))
    })?;
    if len < FOOTER_SIZE {
        return Err(StagingError::StagingCorrupt(format!(
            "prepared xorb {} for file {} is too small for a footer",
            xorb_hash.hex(),
            file_hash.hex()
        )));
    }

    let mut file = tokio::fs::File::open(path).await?;
    let footer_offset = u64::try_from(len - FOOTER_SIZE).map_err(|_| {
        StagingError::Internal("prepared xorb footer offset does not fit u64".to_owned())
    })?;
    file.seek(SeekFrom::Start(footer_offset)).await?;
    let mut footer = vec![0u8; FOOTER_SIZE];
    file.read_exact(&mut footer).await?;

    let region = xorb_metadata_region(len, &footer)?;
    let metadata_offset = u64::try_from(region.offset).map_err(|_| {
        StagingError::Internal("prepared xorb metadata offset does not fit u64".to_owned())
    })?;
    file.seek(SeekFrom::Start(metadata_offset)).await?;
    let mut metadata = vec![0u8; region.len];
    file.read_exact(&mut metadata).await?;

    validate_prepared_xorb_metadata(len, &footer, &metadata, file_hash, xorb_hash, planned)
        .map(|_| ())
}

pub(crate) fn scan_stale_prepared_xorbs(
    root: &Path,
    referenced_xorbs: &HashSet<PathBuf>,
    stats: &mut PushPlanStats,
) -> Result<()> {
    let Some(file_dirs) = read_dir_if_exists(&root.join(PLAN_DIR).join(PAYLOAD_DIR))? else {
        return Ok(());
    };

    for file_dir in file_dirs {
        let file_dir = file_dir?;
        if !file_dir.file_type()?.is_dir() {
            continue;
        }
        let Some(xorbs) = read_dir_if_exists(&file_dir.path())? else {
            continue;
        };
        for xorb in xorbs {
            let xorb = xorb?;
            let path = xorb.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("xorb")
                || !xorb.file_type()?.is_file()
                || referenced_xorbs.contains(&path)
            {
                continue;
            }
            let len = xorb.metadata()?.len();
            stats.stale_prepared_xorb_files += 1;
            stats.stale_prepared_xorb_file_bytes = stats
                .stale_prepared_xorb_file_bytes
                .checked_add(len)
                .ok_or_else(|| {
                    StagingError::Internal("push-plan stale xorb byte count overflowed".to_owned())
                })?;
        }
    }

    Ok(())
}

fn read_dir_if_exists(path: &Path) -> Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if is_missing_sidecar_path(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn is_missing_sidecar_path(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crab_xet::hash::compute_data_hash;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    fn hash(byte: u8) -> MerkleHash {
        MerkleHash::from([byte; 32])
    }

    fn xorb_with_chunks(
        chunks: &[(MerkleHash, Vec<u8>)],
    ) -> (Bytes, MerkleHash, Vec<ChunkPlacement>) {
        let mut builder = XorbBuilder::new();
        for (chunk_hash, data) in chunks {
            builder
                .push(
                    &Chunk {
                        hash: *chunk_hash,
                        data: Bytes::from(data.clone()),
                    },
                    RunId(0),
                )
                .expect("push chunk");
        }
        let mut results = builder.finalize().expect("finalize xorb");
        assert_eq!(results.len(), 1);
        let result = results.pop().expect("one xorb");
        (result.bytes, result.hash, result.placements)
    }

    #[test]
    fn proof_identifiers_round_trip_as_raw_bytes() {
        let placement_id = std::array::from_fn(|index| index as u8);
        let origin_proof_id = std::array::from_fn(|index| (31 - index) as u8);
        let candidate = ExistingChunkCandidate {
            xorb_ref: XorbRef {
                xorb_hash: hash(9),
                chunk_index: 7,
                uncompressed_size: 4096,
            },
            placement_id,
            origin_proof_id,
        };
        let planned = PlannedExistingChunk::from_candidate(hash(8), candidate);

        assert_eq!(planned.candidate().expect("decode candidate"), candidate);
    }

    #[test]
    fn prepared_xorb_cache_rejects_placements_for_another_xorb() {
        let xorb_hash = hash(2);
        let wrong_xorb_hash = hash(3);
        let placement = ChunkPlacement {
            chunk_hash: hash(10),
            xorb_hash: wrong_xorb_hash,
            chunk_index: 0,
            uncompressed_size: 42,
        };
        let planned = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: hash(30).hex(),
            bytes: 128,
            upload: true,
            placements: vec![PlannedPlacement::from_placement(&placement)],
        };
        let mut cache = PreparedXorbCache::default();

        let err = cache
            .insert_prepared_xorb(&planned)
            .expect_err("mismatched placement xorb must not be indexed");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert_eq!(cache.candidates_for_chunk(&hash(10)).count(), 0);
    }

    #[test]
    fn prepared_xorb_cache_preserves_alternate_sources_for_same_xorb() {
        let xorb_hash = hash(2);
        let placement = ChunkPlacement {
            chunk_hash: hash(10),
            xorb_hash,
            chunk_index: 0,
            uncompressed_size: 42,
        };
        let planned = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: hash(30).hex(),
            bytes: 128,
            upload: true,
            placements: vec![PlannedPlacement::from_placement(&placement)],
        };
        let mut cache = PreparedXorbCache::default();

        cache
            .insert_prepared_xorb(&planned)
            .expect("insert staging source");
        cache
            .insert_prepared_xorb(&planned)
            .expect("same source should be ignored");
        assert_eq!(cache.candidates_for_chunk(&hash(10)).count(), 1);

        cache
            .insert_cached_xorb("cached.xorb".into(), &planned)
            .expect("local cache source should be retained");

        let candidates: Vec<_> = cache.candidates_for_chunk(&hash(10)).collect();
        assert_eq!(candidates.len(), 2);
        assert!(matches!(candidates[0].source, PreparedXorbSource::Staging));
        assert!(matches!(
            candidates[1].source,
            PreparedXorbSource::LocalCache(ref path) if path == Path::new("cached.xorb")
        ));
    }

    #[test]
    fn prepared_xorb_cache_rejects_conflicting_duplicate_xorb_metadata() {
        let xorb_hash = hash(2);
        let placement = ChunkPlacement {
            chunk_hash: hash(10),
            xorb_hash,
            chunk_index: 0,
            uncompressed_size: 42,
        };
        let mut conflicting_placement = placement.clone();
        conflicting_placement.uncompressed_size = 43;
        let planned = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: hash(30).hex(),
            bytes: 128,
            upload: true,
            placements: vec![PlannedPlacement::from_placement(&placement)],
        };
        let conflicting = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: hash(30).hex(),
            bytes: 128,
            upload: true,
            placements: vec![PlannedPlacement::from_placement(&conflicting_placement)],
        };
        let mut cache = PreparedXorbCache::default();

        cache
            .insert_prepared_xorb(&planned)
            .expect("insert first source");
        let err = cache
            .insert_prepared_xorb(&conflicting)
            .expect_err("same xorb hash with conflicting metadata must be rejected");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert_eq!(cache.candidates_for_chunk(&hash(10)).count(), 1);
    }

    #[tokio::test]
    async fn copy_prepared_xorb_writes_valid_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("copy-prepared-xorb-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let (xorb_bytes, xorb_hash, placements) = xorb_with_chunks(&chunks);
        let planned = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(&xorb_bytes).to_hex().to_string(),
            bytes: xorb_bytes.len() as u64,
            upload: true,
            placements: placements
                .iter()
                .map(PlannedPlacement::from_placement)
                .collect(),
        };
        let source_path = tmp.path().join("cached-source.xorb");
        std::fs::write(&source_path, &xorb_bytes).expect("write cached source");
        let target_path = prepared_xorb_path(tmp.path(), &xorb_hash);

        let copied = copy_prepared_xorb(&source_path, &target_path, &xorb_hash, &planned)
            .await
            .expect("copy prepared xorb");

        assert!(copied);
        assert_eq!(
            std::fs::read(&target_path).expect("read copied target"),
            xorb_bytes.to_vec()
        );
        parse_prepared_xorb_metadata(&target_path, &xorb_hash, &xorb_hash, &planned)
            .expect("copied target validates");
    }

    #[tokio::test]
    async fn copy_prepared_xorb_removes_temp_on_validation_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("copy-prepared-xorb-invalid-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let (xorb_bytes, xorb_hash, placements) = xorb_with_chunks(&chunks);
        let mut planned = PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(&xorb_bytes).to_hex().to_string(),
            bytes: xorb_bytes.len() as u64,
            upload: true,
            placements: placements
                .iter()
                .map(PlannedPlacement::from_placement)
                .collect(),
        };
        planned
            .placements
            .first_mut()
            .expect("planned placement")
            .uncompressed_size += 1;
        let source_path = tmp.path().join("invalid-cached-source.xorb");
        std::fs::write(&source_path, &xorb_bytes).expect("write cached source");
        let target_path = prepared_xorb_path(tmp.path(), &xorb_hash);

        let copied = copy_prepared_xorb(&source_path, &target_path, &xorb_hash, &planned)
            .await
            .expect("copy should report validation miss");

        assert!(!copied);
        assert!(!target_path.exists());
        let target_parent = target_path.parent().expect("target parent");
        assert_eq!(
            std::fs::read_dir(target_parent)
                .expect("read target parent")
                .count(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_identical_prepared_writes_create_one_immutable_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [(
            compute_data_hash(b"convergent payload"),
            b"convergent payload".to_vec(),
        )];
        let (bytes, xorb_hash, _) = xorb_with_chunks(&chunks);
        let root = tmp.path().to_path_buf();
        let first = tokio::spawn({
            let root = root.clone();
            let bytes = bytes.clone();
            async move { write_prepared_xorb(&root, &xorb_hash, bytes).await }
        });
        let second = tokio::spawn({
            let root = root.clone();
            let bytes = bytes.clone();
            async move { write_prepared_xorb(&root, &xorb_hash, bytes).await }
        });

        assert_eq!(
            first.await.expect("first task").expect("first write"),
            bytes.len() as u64
        );
        assert_eq!(
            second.await.expect("second task").expect("second write"),
            bytes.len() as u64
        );
        assert_eq!(
            std::fs::read(prepared_xorb_path(&root, &xorb_hash)).expect("read sealed body"),
            bytes
        );
    }

    #[tokio::test]
    async fn prepared_write_rejects_corrupt_existing_content_addressed_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [(
            compute_data_hash(b"immutable payload"),
            b"immutable payload".to_vec(),
        )];
        let (bytes, xorb_hash, _) = xorb_with_chunks(&chunks);
        let path = prepared_xorb_path(tmp.path(), &xorb_hash);
        std::fs::create_dir_all(path.parent().expect("payload parent")).expect("create parent");
        std::fs::write(&path, b"corrupt").expect("write corrupt collision");

        let error = write_prepared_xorb(tmp.path(), &xorb_hash, bytes)
            .await
            .expect_err("corrupt content-addressed body must fail closed");

        assert!(matches!(error, StagingError::StagingCorrupt(_)));
        assert_eq!(std::fs::read(path).expect("read collision"), b"corrupt");
    }

    #[test]
    fn temp_file_guard_removes_armed_temp_file_on_drop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("prepared-copy.tmp");
        std::fs::write(&path, b"temporary prepared xorb").expect("write temp");

        drop(TempFileGuard::new(path.clone()));

        assert!(!path.exists());
    }

    #[test]
    fn temp_file_guard_leaves_disarmed_temp_file_on_drop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("prepared-copy.tmp");
        std::fs::write(&path, b"persisted prepared xorb").expect("write temp");
        let mut guard = TempFileGuard::new(path.clone());
        guard.disarm();

        drop(guard);

        assert!(path.exists());
    }
}
