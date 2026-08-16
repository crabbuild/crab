use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::{Result, StagingError};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::FOOTER_SIZE;
use crab_xet::xorb::format::{ChunkPlacement, XorbRef};
use crab_xet::xorb::parser::{xorb_chunks_from_metadata, xorb_metadata_region};

pub use crate::add_push_plan::{
    AddPlanFile, AddPushPlanSummary, ExistingChunkLookup, LocalXorbCandidateLookup,
    prepare_file_push_plans, prepare_file_push_plans_with_progress,
};

pub const FILE_PUSH_PLAN_VERSION: u32 = 4;

const PLAN_DIR: &str = "push-plans";
const FILE_DIR: &str = "files";
const XORB_DIR: &str = "xorbs";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePushPlan {
    pub version: u32,
    pub staged_chunk_sequence_verified: bool,
    pub file_hash: String,
    pub file_size: u64,
    pub chunks: Vec<PlannedChunk>,
    #[serde(default)]
    pub existing: Vec<PlannedExistingChunk>,
    #[serde(default)]
    pub prepared_xorbs: Vec<PlannedXorb>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedChunk {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedExistingChunk {
    pub chunk_hash: String,
    pub xorb_hash: String,
    pub chunk_index: u32,
    pub uncompressed_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    StagingFile(MerkleHash),
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

    pub fn insert_prepared_xorb(
        &mut self,
        source_file_hash: MerkleHash,
        planned: &PlannedXorb,
    ) -> Result<()> {
        self.insert_candidate(PreparedXorbSource::StagingFile(source_file_hash), planned)
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
            chunks: chunks
                .iter()
                .map(|(hash, size)| PlannedChunk {
                    hash: hash.hex(),
                    size: *size,
                })
                .collect(),
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

    pub fn file_hash(&self) -> Result<MerkleHash> {
        parse_hash(&self.file_hash, "file_hash")
    }

    pub fn chunk_pairs(&self) -> Result<Vec<(MerkleHash, u64)>> {
        self.chunks
            .iter()
            .map(|chunk| Ok((parse_hash(&chunk.hash, "chunk hash")?, chunk.size)))
            .collect()
    }

    pub fn existing_refs(&self) -> Result<Vec<(MerkleHash, XorbRef)>> {
        self.existing
            .iter()
            .map(|existing| {
                Ok((
                    parse_hash(&existing.chunk_hash, "existing chunk hash")?,
                    XorbRef {
                        xorb_hash: parse_hash(&existing.xorb_hash, "existing xorb hash")?,
                        chunk_index: existing.chunk_index,
                        uncompressed_size: existing.uncompressed_size,
                    },
                ))
            })
            .collect()
    }
}

pub(crate) fn serialize_file_push_plan(plan: &FilePushPlan) -> Result<Vec<u8>> {
    serde_json::to_vec(plan)
        .map_err(|e| StagingError::Internal(format!("failed to serialize add-time push plan: {e}")))
}

pub(crate) fn deserialize_file_push_plan(bytes: &[u8]) -> Result<FilePushPlan> {
    serde_json::from_slice(bytes)
        .map_err(|e| StagingError::Internal(format!("failed to parse add-time push plan: {e}")))
}

pub(crate) fn serialize_planned_xorb(plan: &PlannedXorb) -> Result<Vec<u8>> {
    serde_json::to_vec(plan)
        .map_err(|e| StagingError::Internal(format!("failed to serialize prepared xorb plan: {e}")))
}

pub(crate) fn deserialize_planned_xorb(bytes: &[u8]) -> Result<PlannedXorb> {
    serde_json::from_slice(bytes)
        .map_err(|e| StagingError::Internal(format!("failed to parse prepared xorb plan: {e}")))
}

pub(crate) fn chunk_sequence_hash(chunks: &[(MerkleHash, u64)]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(chunks.len() as u64).to_le_bytes());
    for (chunk_hash, size) in chunks {
        let hash_bytes: [u8; 32] = (*chunk_hash).into();
        hasher.update(&hash_bytes);
        hasher.update(&size.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

impl PlannedExistingChunk {
    pub fn from_ref(chunk_hash: MerkleHash, xorb_ref: XorbRef) -> Self {
        Self {
            chunk_hash: chunk_hash.hex(),
            xorb_hash: xorb_ref.xorb_hash.hex(),
            chunk_index: xorb_ref.chunk_index,
            uncompressed_size: xorb_ref.uncompressed_size,
        }
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

pub(crate) fn file_plan_path(root: &Path, file_hash: &MerkleHash) -> PathBuf {
    root.join(PLAN_DIR)
        .join(FILE_DIR)
        .join(format!("{}.json", file_hash.hex()))
}

pub fn prepared_xorb_path(root: &Path, file_hash: &MerkleHash, xorb_hash: &MerkleHash) -> PathBuf {
    root.join(PLAN_DIR)
        .join(XORB_DIR)
        .join(file_hash.hex())
        .join(format!("{}.xorb", xorb_hash.hex()))
}

#[cfg(test)]
fn summarize_push_plans(root: &Path) -> Result<PushPlanStats> {
    summarize_push_plans_with_options(root, PushPlanSummaryOptions::default())
}

#[cfg(test)]
fn load_prepared_xorb_cache(root: &Path) -> Result<PreparedXorbCache> {
    load_prepared_xorb_cache_filtered(root, None)
}

#[cfg(test)]
fn load_prepared_xorb_cache_for_chunks(
    root: &Path,
    wanted_chunks: &HashSet<MerkleHash>,
) -> Result<PreparedXorbCache> {
    load_prepared_xorb_cache_filtered(root, Some(wanted_chunks))
}

#[cfg(test)]
fn load_prepared_xorb_cache_filtered(
    root: &Path,
    wanted_chunks: Option<&HashSet<MerkleHash>>,
) -> Result<PreparedXorbCache> {
    let mut cache = PreparedXorbCache::default();
    let Some(entries) = read_dir_if_exists(&root.join(PLAN_DIR).join(FILE_DIR))? else {
        return Ok(cache);
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(plan) = serde_json::from_slice::<FilePushPlan>(&bytes) else {
            continue;
        };
        if plan.version != FILE_PUSH_PLAN_VERSION {
            continue;
        }
        let Ok(file_hash) = plan.file_hash() else {
            continue;
        };
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(file_hash.hex().as_str()) {
            continue;
        }
        if let Some(wanted_chunks) = wanted_chunks
            && !file_plan_intersects_chunks(&plan, wanted_chunks)
        {
            continue;
        }

        for planned_xorb in &plan.prepared_xorbs {
            if let Some(wanted_chunks) = wanted_chunks
                && !planned_xorb_intersects_chunks(planned_xorb, wanted_chunks)
            {
                continue;
            }
            let Ok(xorb_hash) = planned_xorb.hash() else {
                continue;
            };
            let path = prepared_xorb_path(root, &file_hash, &xorb_hash);
            if !prepared_xorb_file_matches_cached_plan(&path, &file_hash, &xorb_hash, planned_xorb)
            {
                continue;
            }
            if cache.insert_prepared_xorb(file_hash, planned_xorb).is_err() {
                continue;
            }
        }
    }

    Ok(cache)
}

#[cfg(test)]
fn planned_xorb_intersects_chunks(
    planned_xorb: &PlannedXorb,
    wanted_chunks: &HashSet<MerkleHash>,
) -> bool {
    planned_xorb.placements.iter().any(|placement| {
        parse_hash(&placement.chunk_hash, "planned placement chunk hash")
            .is_ok_and(|chunk_hash| wanted_chunks.contains(&chunk_hash))
    })
}

#[cfg(test)]
fn file_plan_intersects_chunks(plan: &FilePushPlan, wanted_chunks: &HashSet<MerkleHash>) -> bool {
    plan.chunks.iter().any(|chunk| {
        parse_hash(&chunk.hash, "planned chunk hash")
            .is_ok_and(|chunk_hash| wanted_chunks.contains(&chunk_hash))
    })
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

pub async fn link_prepared_xorb(
    root: &Path,
    source_file_hash: &MerkleHash,
    target_file_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<bool> {
    let xorb_hash = planned.hash()?;
    let source = prepared_xorb_path(root, source_file_hash, &xorb_hash);
    materialize_prepared_xorb_file(root, &source, target_file_hash, planned).await
}

pub async fn materialize_prepared_xorb(
    root: &Path,
    candidate: &PreparedXorbCandidate,
    target_file_hash: &MerkleHash,
) -> Result<bool> {
    let source = match &candidate.source {
        PreparedXorbSource::StagingFile(source_file_hash) => {
            prepared_xorb_path(root, source_file_hash, &candidate.xorb_hash)
        }
        PreparedXorbSource::LocalCache(path) => path.clone(),
    };
    materialize_prepared_xorb_file(root, &source, target_file_hash, &candidate.planned).await
}

async fn materialize_prepared_xorb_file(
    root: &Path,
    source: &Path,
    target_file_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<bool> {
    let xorb_hash = planned.hash()?;
    let target = prepared_xorb_path(root, target_file_hash, &xorb_hash);
    if source == target {
        return prepared_xorb_file_matches_plan(source, target_file_hash, &xorb_hash, planned)
            .await;
    }

    if !prepared_xorb_file_matches_plan(source, target_file_hash, &xorb_hash, planned).await? {
        return Ok(false);
    }

    if prepared_xorb_file_matches_plan(&target, target_file_hash, &xorb_hash, planned).await? {
        return Ok(true);
    }
    match tokio::fs::metadata(&target).await {
        Ok(meta) if meta.is_file() => tokio::fs::remove_file(&target).await?,
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

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
            prepared_xorb_file_matches_plan(&target, target_file_hash, &xorb_hash, planned).await
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::CrossesDevices
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            match copy_prepared_xorb(source, &target, target_file_hash, &xorb_hash, planned).await {
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
    target_file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    planned: &PlannedXorb,
) -> Result<bool> {
    let parent = target
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let tmp = target.with_extension(format!("xorb.copy-{}-{nonce}.tmp", std::process::id()));
    let mut tmp_guard = TempFileGuard::new(tmp.clone());

    async {
        tokio::fs::copy(source, &tmp).await?;
        if !prepared_xorb_file_matches_plan(&tmp, target_file_hash, xorb_hash, planned).await? {
            return Ok(false);
        }
        tokio::fs::File::open(&tmp).await?.sync_all().await?;
        tokio::fs::rename(&tmp, target).await?;
        sync_parent_directory(target).await?;
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

#[cfg(test)]
fn summarize_push_plans_with_options(
    root: &Path,
    options: PushPlanSummaryOptions,
) -> Result<PushPlanStats> {
    let mut stats = new_push_plan_stats(options);
    let mut referenced_xorbs = HashSet::new();

    scan_file_plans(root, options, &mut stats, &mut referenced_xorbs)?;
    scan_stale_prepared_xorbs(root, &referenced_xorbs, &mut stats)?;

    Ok(stats)
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

pub async fn write_prepared_xorb(
    root: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    bytes: Bytes,
) -> Result<u64> {
    let path = prepared_xorb_path(root, file_hash, xorb_hash);
    let parent = path
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = path.with_extension("xorb.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, &path).await?;
    sync_parent_directory(&path).await?;
    Ok(bytes.len() as u64)
}

pub async fn move_prepared_xorb(
    root: &Path,
    file_hash: &MerkleHash,
    xorb_hash: &MerkleHash,
    source_path: &Path,
) -> Result<u64> {
    let path = prepared_xorb_path(root, file_hash, xorb_hash);
    let parent = path
        .parent()
        .ok_or_else(|| StagingError::Internal("prepared xorb path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = path.with_extension("xorb.tmp");
    match tokio::fs::remove_file(&tmp).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    tokio::fs::rename(source_path, &tmp).await?;
    let bytes = tokio::fs::metadata(&tmp).await?.len();
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, &path).await?;
    sync_parent_directory(&path).await?;
    Ok(bytes)
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

#[cfg(test)]
pub(crate) async fn write_file_push_plan(root: &Path, plan: &FilePushPlan) -> Result<()> {
    let file_hash = plan.file_hash()?;
    let path = file_plan_path(root, &file_hash);
    let parent = path
        .parent()
        .ok_or_else(|| StagingError::Internal("push plan path has no parent".to_owned()))?;
    tokio::fs::create_dir_all(parent).await?;
    let bytes = serialize_file_push_plan(plan)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

pub(crate) fn remove_file_push_plan(root: &Path, file_hash: &MerkleHash) -> Result<()> {
    let plan_path = file_plan_path(root, file_hash);
    match std::fs::remove_file(&plan_path) {
        Ok(()) => {}
        Err(e) if is_missing_sidecar_path(&e) => {}
        Err(e) => return Err(e.into()),
    }

    let xorb_dir = root.join(PLAN_DIR).join(XORB_DIR).join(file_hash.hex());
    match std::fs::remove_dir_all(&xorb_dir) {
        Ok(()) => {}
        Err(e) if is_missing_sidecar_path(&e) => {}
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

pub(crate) fn retain_file_prepared_xorbs(
    root: &Path,
    file_hash: &MerkleHash,
    keep_hashes: &HashSet<MerkleHash>,
) -> Result<()> {
    let xorb_dir = root.join(PLAN_DIR).join(XORB_DIR).join(file_hash.hex());
    let Some(entries) = read_dir_if_exists(&xorb_dir)? else {
        return Ok(());
    };
    let keep_files = keep_hashes
        .iter()
        .map(|hash| format!("{}.xorb", hash.hex()))
        .collect::<HashSet<_>>();

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.ends_with(".xorb") || keep_files.contains(file_name) {
            continue;
        }
        std::fs::remove_file(entry.path())?;
    }

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

#[cfg(test)]
fn scan_file_plans(
    root: &Path,
    options: PushPlanSummaryOptions,
    stats: &mut PushPlanStats,
    referenced_xorbs: &mut HashSet<PathBuf>,
) -> Result<()> {
    let Some(entries) = read_dir_if_exists(&root.join(PLAN_DIR).join(FILE_DIR))? else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let bytes = fs::read(&path)?;
        let Ok(plan) = serde_json::from_slice::<FilePushPlan>(&bytes) else {
            stats.invalid_plan_files += 1;
            continue;
        };

        if !accumulate_file_plan(root, &path, &plan, options, stats, referenced_xorbs)? {
            stats.invalid_plan_files += 1;
        }
    }

    Ok(())
}

#[cfg(test)]
fn accumulate_file_plan(
    root: &Path,
    path: &Path,
    plan: &FilePushPlan,
    options: PushPlanSummaryOptions,
    stats: &mut PushPlanStats,
    referenced_xorbs: &mut HashSet<PathBuf>,
) -> Result<bool> {
    if plan.version != FILE_PUSH_PLAN_VERSION {
        return Ok(false);
    }
    let Ok(file_hash) = plan.file_hash() else {
        return Ok(false);
    };
    if path.file_stem().and_then(|stem| stem.to_str()) != Some(file_hash.hex().as_str()) {
        return Ok(false);
    }
    accumulate_file_plan_for_hash(root, &file_hash, plan, options, stats, referenced_xorbs)
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
    let Ok(chunks) = plan.chunk_pairs() else {
        return Ok(false);
    };
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
        .checked_add(chunks.len() as u64)
        .ok_or_else(|| StagingError::Internal("push-plan chunk count overflowed".to_owned()))?;
    stats.existing_chunks = stats
        .existing_chunks
        .checked_add(plan.existing.len() as u64)
        .ok_or_else(|| {
            StagingError::Internal("push-plan existing chunk count overflowed".to_owned())
        })?;

    for planned_xorb in &plan.prepared_xorbs {
        let xorb_hash = planned_xorb.hash()?;
        let path = prepared_xorb_path(root, file_hash, &xorb_hash);
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
    let Some(file_dirs) = read_dir_if_exists(&root.join(PLAN_DIR).join(XORB_DIR))? else {
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

    fn file_hash_from_chunks(chunks: &[(MerkleHash, Vec<u8>)]) -> MerkleHash {
        let mut hasher = blake3::Hasher::new();
        for (_, data) in chunks {
            hasher.update(data);
        }
        MerkleHash::from(*hasher.finalize().as_bytes())
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

    fn write_prepared_plan(
        root: &Path,
        file_hash: MerkleHash,
        chunks: &[(MerkleHash, Vec<u8>)],
        placements: Vec<ChunkPlacement>,
        xorb_bytes: Bytes,
        xorb_hash: MerkleHash,
    ) -> FilePushPlan {
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<_> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let mut plan = FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
        plan.prepared_xorbs.push(PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(&xorb_bytes).to_hex().to_string(),
            bytes: xorb_bytes.len() as u64,
            upload: true,
            placements: placements
                .iter()
                .map(PlannedPlacement::from_placement)
                .collect(),
        });

        let plan_path = file_plan_path(root, &file_hash);
        std::fs::create_dir_all(plan_path.parent().expect("plan parent")).expect("mkdir plan");
        std::fs::write(
            &plan_path,
            serde_json::to_vec(&plan).expect("serialize push plan"),
        )
        .expect("write plan");

        let xorb_path = prepared_xorb_path(root, &file_hash, &xorb_hash);
        std::fs::create_dir_all(xorb_path.parent().expect("xorb parent")).expect("mkdir xorb");
        std::fs::write(&xorb_path, xorb_bytes).expect("write xorb");
        plan
    }

    #[test]
    fn summarize_push_plans_reports_valid_invalid_and_stale_inventory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_hash = hash(1);
        let xorb_hash = hash(2);
        let mismatched_xorb_hash = hash(3);
        let missing_xorb_hash = hash(4);
        let stale_xorb_hash = hash(5);
        let chunk_a = hash(11);
        let chunk_b = hash(12);
        let chunk_c = hash(13);
        let chunk_d = hash(14);

        let mut plan = FilePushPlan::new_verified_staging(
            file_hash,
            900,
            &[
                (chunk_a, 100),
                (chunk_b, 200),
                (chunk_c, 300),
                (chunk_d, 300),
            ],
        );
        plan.existing.push(PlannedExistingChunk::from_ref(
            chunk_a,
            XorbRef {
                xorb_hash: hash(9),
                chunk_index: 0,
                uncompressed_size: 100,
            },
        ));
        for (xorb_hash, chunk_hash, chunk_index, bytes) in [
            (xorb_hash, chunk_b, 0, 4),
            (mismatched_xorb_hash, chunk_c, 1, 5),
            (missing_xorb_hash, chunk_d, 2, 7),
        ] {
            let placement = ChunkPlacement {
                chunk_hash,
                xorb_hash,
                chunk_index,
                uncompressed_size: bytes as u32,
            };
            plan.prepared_xorbs.push(PlannedXorb {
                hash: xorb_hash.hex(),
                payload_hash: hash(30 + chunk_index as u8).hex(),
                bytes,
                upload: true,
                placements: vec![PlannedPlacement::from_placement(&placement)],
            });
        }

        let plan_path = file_plan_path(tmp.path(), &file_hash);
        std::fs::create_dir_all(plan_path.parent().expect("plan parent")).expect("mkdir");
        std::fs::write(
            &plan_path,
            serde_json::to_vec(&plan).expect("serialize push plan"),
        )
        .expect("write plan");
        std::fs::write(plan_path.with_file_name("not-json.json"), b"nope").expect("write invalid");

        let xorb_path = prepared_xorb_path(tmp.path(), &file_hash, &xorb_hash);
        std::fs::create_dir_all(xorb_path.parent().expect("xorb parent")).expect("mkdir");
        std::fs::write(&xorb_path, [0u8; 4]).expect("write xorb");
        std::fs::write(
            prepared_xorb_path(tmp.path(), &file_hash, &mismatched_xorb_hash),
            [0u8; 6],
        )
        .expect("write mismatched xorb");
        std::fs::write(
            prepared_xorb_path(tmp.path(), &file_hash, &stale_xorb_hash),
            [0u8; 8],
        )
        .expect("write stale xorb");

        let stats = summarize_push_plans(tmp.path()).expect("summarize");
        assert_eq!(stats.format_version, FILE_PUSH_PLAN_VERSION);
        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.invalid_plan_files, 1);
        assert_eq!(stats.planned_file_bytes, 900);
        assert_eq!(stats.planned_chunks, 4);
        assert_eq!(stats.existing_chunks, 1);
        assert_eq!(stats.prepared_xorbs, 3);
        assert_eq!(stats.prepared_chunks, 3);
        assert_eq!(stats.prepared_bytes, 16);
        assert_eq!(stats.referenced_prepared_xorb_files, 2);
        assert_eq!(stats.referenced_prepared_xorb_file_bytes, 10);
        assert_eq!(stats.missing_prepared_xorb_files, 1);
        assert_eq!(stats.mismatched_prepared_xorb_files, 1);
        assert_eq!(stats.stale_prepared_xorb_files, 1);
        assert_eq!(stats.stale_prepared_xorb_file_bytes, 8);
    }

    #[test]
    fn summarize_push_plans_handles_missing_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stats = summarize_push_plans(tmp.path()).expect("summarize");
        assert_eq!(stats.format_version, FILE_PUSH_PLAN_VERSION);
        assert_eq!(stats.plan_files, 0);
        assert_eq!(stats.invalid_plan_files, 0);
        assert_eq!(stats.prepared_xorbs, 0);
        assert_eq!(stats.stale_prepared_xorb_files, 0);
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
            .insert_prepared_xorb(hash(1), &planned)
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
            .insert_prepared_xorb(hash(1), &planned)
            .expect("insert first source");
        cache
            .insert_prepared_xorb(hash(1), &planned)
            .expect("same source should be ignored");
        assert_eq!(cache.candidates_for_chunk(&hash(10)).count(), 1);

        cache
            .insert_prepared_xorb(hash(2), &planned)
            .expect("alternate source should be retained");

        let candidates: Vec<_> = cache.candidates_for_chunk(&hash(10)).collect();
        assert_eq!(candidates.len(), 2);
        assert!(matches!(
            candidates[0].source,
            PreparedXorbSource::StagingFile(source) if source == hash(1)
        ));
        assert!(matches!(
            candidates[1].source,
            PreparedXorbSource::StagingFile(source) if source == hash(2)
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
            .insert_prepared_xorb(hash(1), &planned)
            .expect("insert first source");
        let err = cache
            .insert_prepared_xorb(hash(2), &conflicting)
            .expect_err("same xorb hash with conflicting metadata must be rejected");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert_eq!(cache.candidates_for_chunk(&hash(10)).count(), 1);
    }

    #[test]
    fn load_prepared_xorb_cache_skips_metadata_mismatched_xorb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("load-cache-mismatch-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let file_hash = file_hash_from_chunks(&chunks);
        let (xorb_bytes, xorb_hash, mut placements) = xorb_with_chunks(&chunks);
        placements[0].uncompressed_size += 1;
        let planned_chunk = placements[0].chunk_hash;
        write_prepared_plan(
            tmp.path(),
            file_hash,
            &chunks,
            placements,
            xorb_bytes,
            xorb_hash,
        );

        let cache = load_prepared_xorb_cache(tmp.path()).expect("load cache");

        assert_eq!(cache.candidates_for_chunk(&planned_chunk).count(), 0);
    }

    #[test]
    fn load_prepared_xorb_cache_for_chunks_skips_unwanted_xorbs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wanted_chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("wanted-prepared-cache-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let unwanted_chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("unwanted-prepared-cache-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let wanted_file_hash = file_hash_from_chunks(&wanted_chunks);
        let unwanted_file_hash = file_hash_from_chunks(&unwanted_chunks);
        let (wanted_xorb_bytes, wanted_xorb_hash, wanted_placements) =
            xorb_with_chunks(&wanted_chunks);
        let (unwanted_xorb_bytes, unwanted_xorb_hash, unwanted_placements) =
            xorb_with_chunks(&unwanted_chunks);
        write_prepared_plan(
            tmp.path(),
            wanted_file_hash,
            &wanted_chunks,
            wanted_placements,
            wanted_xorb_bytes,
            wanted_xorb_hash,
        );
        write_prepared_plan(
            tmp.path(),
            unwanted_file_hash,
            &unwanted_chunks,
            unwanted_placements,
            unwanted_xorb_bytes,
            unwanted_xorb_hash,
        );

        let wanted_chunk = wanted_chunks[0].0;
        let unwanted_chunk = unwanted_chunks[0].0;
        let cache = load_prepared_xorb_cache_for_chunks(tmp.path(), &HashSet::from([wanted_chunk]))
            .expect("load filtered cache");

        assert_eq!(cache.candidates_for_chunk(&wanted_chunk).count(), 1);
        assert_eq!(cache.candidates_for_chunk(&unwanted_chunk).count(), 0);
    }

    #[test]
    fn load_prepared_xorb_cache_for_chunks_skips_non_overlapping_plan_before_xorbs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let unwanted_chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("non-overlap-plan-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let file_hash = file_hash_from_chunks(&unwanted_chunks);
        let (xorb_bytes, xorb_hash, mut placements) = xorb_with_chunks(&unwanted_chunks);
        placements[0].uncompressed_size += 1;
        let planned_chunk = placements[0].chunk_hash;
        write_prepared_plan(
            tmp.path(),
            file_hash,
            &unwanted_chunks,
            placements,
            xorb_bytes,
            xorb_hash,
        );

        let cache = load_prepared_xorb_cache_for_chunks(tmp.path(), &HashSet::from([hash(200)]))
            .expect("load filtered cache");

        assert_eq!(cache.candidates_for_chunk(&planned_chunk).count(), 0);
    }

    #[tokio::test]
    async fn link_prepared_xorb_replaces_stale_target_with_valid_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("repair-target-prepared-xorb-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let source_file_hash = file_hash_from_chunks(&chunks);
        let target_file_hash = hash(99);
        let (xorb_bytes, xorb_hash, placements) = xorb_with_chunks(&chunks);
        let source_plan = write_prepared_plan(
            tmp.path(),
            source_file_hash,
            &chunks,
            placements,
            xorb_bytes.clone(),
            xorb_hash,
        );
        let planned = source_plan
            .prepared_xorbs
            .first()
            .expect("prepared xorb")
            .clone();
        let target_path = prepared_xorb_path(tmp.path(), &target_file_hash, &xorb_hash);
        std::fs::create_dir_all(target_path.parent().expect("target parent"))
            .expect("mkdir target");
        std::fs::write(&target_path, vec![0xA5; xorb_bytes.len()]).expect("write stale target");

        let linked = link_prepared_xorb(tmp.path(), &source_file_hash, &target_file_hash, &planned)
            .await
            .expect("link prepared xorb");

        assert!(linked);
        assert_eq!(
            std::fs::read(&target_path).expect("read repaired target"),
            xorb_bytes.to_vec()
        );
        parse_prepared_xorb_metadata(&target_path, &target_file_hash, &xorb_hash, &planned)
            .expect("target metadata validates");
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
        let source_file_hash = file_hash_from_chunks(&chunks);
        let target_file_hash = hash(88);
        let (xorb_bytes, xorb_hash, placements) = xorb_with_chunks(&chunks);
        let source_plan = write_prepared_plan(
            tmp.path(),
            source_file_hash,
            &chunks,
            placements,
            xorb_bytes.clone(),
            xorb_hash,
        );
        let planned = source_plan
            .prepared_xorbs
            .first()
            .expect("prepared xorb")
            .clone();
        let source_path = prepared_xorb_path(tmp.path(), &source_file_hash, &xorb_hash);
        let target_path = prepared_xorb_path(tmp.path(), &target_file_hash, &xorb_hash);

        let copied = copy_prepared_xorb(
            &source_path,
            &target_path,
            &target_file_hash,
            &xorb_hash,
            &planned,
        )
        .await
        .expect("copy prepared xorb");

        assert!(copied);
        assert_eq!(
            std::fs::read(&target_path).expect("read copied target"),
            xorb_bytes.to_vec()
        );
        parse_prepared_xorb_metadata(&target_path, &target_file_hash, &xorb_hash, &planned)
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
        let source_file_hash = file_hash_from_chunks(&chunks);
        let target_file_hash = hash(87);
        let (xorb_bytes, xorb_hash, placements) = xorb_with_chunks(&chunks);
        let source_plan = write_prepared_plan(
            tmp.path(),
            source_file_hash,
            &chunks,
            placements,
            xorb_bytes,
            xorb_hash,
        );
        let mut planned = source_plan
            .prepared_xorbs
            .first()
            .expect("prepared xorb")
            .clone();
        planned
            .placements
            .first_mut()
            .expect("planned placement")
            .uncompressed_size += 1;
        let source_path = prepared_xorb_path(tmp.path(), &source_file_hash, &xorb_hash);
        let target_path = prepared_xorb_path(tmp.path(), &target_file_hash, &xorb_hash);

        let copied = copy_prepared_xorb(
            &source_path,
            &target_path,
            &target_file_hash,
            &xorb_hash,
            &planned,
        )
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

    #[test]
    fn summarize_push_plans_verifies_prepared_xorb_payload_and_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("verify-prepared-xorb-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let file_hash = file_hash_from_chunks(&chunks);
        let (xorb_bytes, xorb_hash, placements) = xorb_with_chunks(&chunks);
        let xorb_len = xorb_bytes.len() as u64;
        write_prepared_plan(
            tmp.path(),
            file_hash,
            &chunks,
            placements,
            xorb_bytes,
            xorb_hash,
        );

        let stats = summarize_push_plans_with_options(
            tmp.path(),
            PushPlanSummaryOptions {
                verify_prepared_xorbs: true,
            },
        )
        .expect("summarize");

        assert!(stats.verified_prepared_xorbs);
        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.prepared_xorbs, 1);
        assert_eq!(stats.verified_prepared_xorb_files, 1);
        assert_eq!(stats.verified_prepared_xorb_file_bytes, xorb_len);
        assert_eq!(stats.payload_hash_mismatched_prepared_xorb_files, 0);
        assert_eq!(stats.corrupt_prepared_xorb_files, 0);
        assert_eq!(stats.metadata_mismatched_prepared_xorb_files, 0);
    }

    #[test]
    fn summarize_push_plans_reports_prepared_xorb_metadata_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..2u8)
            .map(|idx| {
                let data = format!("mismatch-prepared-xorb-chunk-{idx}").into_bytes();
                (compute_data_hash(&data), data)
            })
            .collect();
        let file_hash = file_hash_from_chunks(&chunks);
        let (xorb_bytes, xorb_hash, mut placements) = xorb_with_chunks(&chunks);
        placements[0].uncompressed_size += 1;
        write_prepared_plan(
            tmp.path(),
            file_hash,
            &chunks,
            placements,
            xorb_bytes,
            xorb_hash,
        );

        let stats = summarize_push_plans_with_options(
            tmp.path(),
            PushPlanSummaryOptions {
                verify_prepared_xorbs: true,
            },
        )
        .expect("summarize");

        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.prepared_xorbs, 1);
        assert_eq!(stats.verified_prepared_xorb_files, 0);
        assert_eq!(stats.metadata_mismatched_prepared_xorb_files, 1);
        assert_eq!(stats.payload_hash_mismatched_prepared_xorb_files, 0);
    }
}
