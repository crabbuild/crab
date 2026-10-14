//! Bounded content ingestion, payload preparation and durable recipe ownership.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::StagingArea;
use crate::index::{ExistingChunkWrite, PreparedChunkClaim};
use crate::push_plan::{ExistingChunkCandidate, ExistingChunkLookup};
use crate::{Result, StagingError as CrabError};
use crab_xet::chunker::GearChunker;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use crab_xet::xorb::format::Chunk;

use crate::stream::VerifiedIndexStat;

mod prepared;
pub(crate) use prepared::StreamStageHooks;
use prepared::{
    PreparedXorbWriteResult, StreamPreparedXorbWriter, cleanup_stream_prepared_xorb_dir,
    persist_stream_prepared_authority,
};
pub use prepared::{
    StreamStagePreparedXorb, StreamStageProgress, StreamStageXorbBuilder,
    cleanup_stream_prepared_xorbs,
};

type PreparedXorbWriteHook = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    Ok(())
}

/// File read buffer size for both streaming passes.
pub const READ_BUF_SIZE: usize = 1024 * 1024;

/// Maximum CDC chunks held before flushing a staging batch.
///
/// Default chunks target 64 KiB and are capped at 128 KiB. The byte cap keeps
/// peak memory near the previous worst case, while the larger chunk cap lets
/// typical data batch closer to 64 MiB before touching the staging index.
pub const STAGE_BATCH_CHUNKS: usize = 1024;
pub const STAGE_BATCH_TARGET_BYTES: u64 = 64 * 1024 * 1024;
const DIRECT_XORB_STAGE_BATCH_TARGET_BYTES: u64 = 64 * 1024 * 1024;

/// Sealed ingestion result with an optional file-adapter stat projection.
#[derive(Debug)]
pub struct StreamStageResult {
    pub batch_id: crate::StagingBatchId,
    pub abs_path: PathBuf,
    pub file_hash: [u8; 32],
    pub size: u64,
    pub chunks: usize,
    pub recipe: crate::recipe::FileRecipe,
    pub prepared_xorbs: Vec<StreamStagePreparedXorb>,
    pub index_stat: Option<VerifiedIndexStat>,
    pub timings: StreamStageTimings,
    pub duration_ms: u64,
}

/// Cumulative worker time spent in the expensive add preparation phases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStageTimings {
    /// Cumulative time spent finding CDC boundaries in worker tasks.
    pub chunking_duration_ms: u64,
    /// Cumulative time spent consulting the bucket-global remote index.
    pub remote_lookup_duration_ms: u64,
    /// Cumulative time spent packing and finalizing compressed xorbs.
    pub compression_duration_ms: u64,
    /// Cumulative time spent materializing and installing prepared payloads.
    pub payload_write_duration_ms: u64,
}

#[derive(Default)]
struct StreamStageTimingAccumulator {
    chunking: Duration,
    remote_lookup: Duration,
    compression: Duration,
    payload_write: Duration,
    hashing: Duration,
    verification: Duration,
    index: Duration,
    authority_wait: Duration,
    reused_prepared_bytes: u64,
    reused_segment_bytes: u64,
    reused_remote_bytes: u64,
}

impl StreamStageTimingAccumulator {
    fn trace(&self, input_bytes: u64, sealed: bool) {
        tracing::debug!(
            input_bytes = input_bytes,
            hashing_ms = duration_ms(self.hashing),
            verification_ms = duration_ms(self.verification),
            chunking_ms = duration_ms(self.chunking),
            authority_index_ms = duration_ms(self.index),
            compression_ms = duration_ms(self.compression),
            payload_write_ms = duration_ms(self.payload_write),
            authority_wait_ms = duration_ms(self.authority_wait),
            input_bytes_reused_prepared = self.reused_prepared_bytes,
            input_bytes_reused_segment = self.reused_segment_bytes,
            input_bytes_reused_remote = self.reused_remote_bytes,
            sealed,
            "ingestion timings"
        );
    }

    fn finish(self) -> StreamStageTimings {
        StreamStageTimings {
            chunking_duration_ms: duration_ms(self.chunking),
            remote_lookup_duration_ms: duration_ms(self.remote_lookup),
            compression_duration_ms: duration_ms(self.compression),
            payload_write_duration_ms: duration_ms(self.payload_write),
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

struct PreparedRecipeCursor {
    recipe: crate::recipe::FileRecipe,
    page: Option<crate::recipe::RecipePage>,
    page_index: usize,
    next_occurrence: u64,
    next_offset: u64,
}

impl PreparedRecipeCursor {
    fn new(recipe: crate::recipe::FileRecipe) -> Self {
        Self {
            recipe,
            page: None,
            page_index: 0,
            next_occurrence: 0,
            next_offset: 0,
        }
    }

    fn next(&mut self, staging: &StagingArea) -> Result<Option<crate::recipe::RecipeChunk>> {
        if self.next_occurrence == self.recipe.chunk_count() {
            return Ok(None);
        }
        if self.next_occurrence > self.recipe.chunk_count() {
            return Err(CrabError::StagingCorrupt(
                "prepared recipe cursor exceeded its chunk count".to_owned(),
            ));
        }

        if self
            .page
            .as_ref()
            .is_none_or(|page| self.page_index == page.chunks.len())
        {
            let page = staging.recipe_page(&self.recipe, self.next_occurrence)?;
            if page.start_occurrence != self.next_occurrence
                || page.start_offset != self.next_offset
                || page.chunks.is_empty()
                || page.next_occurrence() > self.recipe.chunk_count()
            {
                return Err(CrabError::StagingCorrupt(format!(
                    "prepared recipe page at occurrence {} is non-contiguous",
                    self.next_occurrence
                )));
            }
            self.page = Some(page);
            self.page_index = 0;
        }

        let chunk = self
            .page
            .as_ref()
            .and_then(|page| page.chunks.get(self.page_index))
            .copied()
            .ok_or_else(|| {
                CrabError::StagingCorrupt(
                    "prepared recipe page ended before its declared chunks".to_owned(),
                )
            })?;
        if chunk.offset != self.next_offset || chunk.len == 0 {
            return Err(CrabError::StagingCorrupt(format!(
                "prepared recipe chunk {} has an invalid byte range",
                self.next_occurrence
            )));
        }
        self.page_index += 1;
        self.next_occurrence = self.next_occurrence.checked_add(1).ok_or_else(|| {
            CrabError::StagingCorrupt("prepared recipe occurrence overflow".to_owned())
        })?;
        self.next_offset = self.next_offset.checked_add(chunk.len).ok_or_else(|| {
            CrabError::StagingCorrupt("prepared recipe byte offset overflow".to_owned())
        })?;
        Ok(Some(chunk))
    }
}

enum PreparedVerification {
    Matching,
    Exact,
    Changed {
        verified_occurrences: u64,
        bytes: Bytes,
    },
}

struct PreparedRecipeVerifier {
    cursor: PreparedRecipeCursor,
    expected: Option<crate::recipe::RecipeChunk>,
    pending: BytesMut,
    verified_occurrences: u64,
}

impl PreparedRecipeVerifier {
    fn new(recipe: crate::recipe::FileRecipe) -> Self {
        Self {
            cursor: PreparedRecipeCursor::new(recipe),
            expected: None,
            pending: BytesMut::new(),
            verified_occurrences: 0,
        }
    }

    fn feed(&mut self, mut input: &[u8], staging: &StagingArea) -> Result<PreparedVerification> {
        while !input.is_empty() {
            if self.expected.is_none() {
                self.expected = self.cursor.next(staging)?;
            }
            let Some(expected) = self.expected else {
                return Ok(self.changed(input));
            };
            let expected_len = usize::try_from(expected.len).map_err(|_| {
                CrabError::StagingCorrupt(
                    "prepared recipe chunk length cannot be represented".to_owned(),
                )
            })?;
            let needed = expected_len
                .checked_sub(self.pending.len())
                .ok_or_else(|| {
                    CrabError::StagingCorrupt(
                        "prepared verification buffered beyond its chunk boundary".to_owned(),
                    )
                })?;
            let take = needed.min(input.len());
            self.pending.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.pending.len() != expected_len {
                return Ok(PreparedVerification::Matching);
            }
            if crab_xet::hash::compute_data_hash(&self.pending) != expected.chunk_hash {
                return Ok(self.changed(input));
            }
            self.pending.clear();
            self.expected = None;
            self.verified_occurrences =
                self.verified_occurrences.checked_add(1).ok_or_else(|| {
                    CrabError::StagingCorrupt(
                        "prepared verification occurrence overflow".to_owned(),
                    )
                })?;
        }
        Ok(PreparedVerification::Matching)
    }

    fn finish(&mut self, staging: &StagingArea) -> Result<PreparedVerification> {
        if !self.pending.is_empty() {
            return Ok(self.changed(&[]));
        }
        if self.expected.is_none() {
            self.expected = self.cursor.next(staging)?;
        }
        if self.expected.is_some() {
            return Ok(self.changed(&[]));
        }
        Ok(PreparedVerification::Exact)
    }

    fn changed(&mut self, remaining: &[u8]) -> PreparedVerification {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(remaining);
        PreparedVerification::Changed {
            verified_occurrences: self.verified_occurrences,
            bytes: bytes.freeze(),
        }
    }
}

/// One bounded content ingestion operation shared by file and protocol adapters.
///
/// Call `finish_input` at the content terminator, validate the source, then seal.
/// Abort explicitly on input errors; unfinished operations are recovered on reopen.
pub struct IngestSession<'a> {
    staging: &'a StagingArea,
    path: PathBuf,
    batch_id: crate::StagingBatchId,
    provisional: MerkleHash,
    owned_preparation: Option<crate::AddPreparationId>,
    preparation_id: crate::AddPreparationId,
    progress: StreamStageProgress,
    cancel: CancellationToken,
    hooks: StreamStageHooks,
    chunker: Option<GearChunker>,
    prepared: Option<PreparedRecipeVerifier>,
    candidate_hash: Option<MerkleHash>,
    hasher: blake3::Hasher,
    batch: Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: u64,
    recipe_recorder: crate::recipe::RecipeRecorder,
    builder_permit: Option<OwnedSemaphorePermit>,
    xorb_builder: Option<XorbBuilder>,
    xorb_writer: Option<StreamPreparedXorbWriter>,
    chunk_index_offset: u64,
    recipe_byte_offset: u64,
    total_chunks: usize,
    total: u64,
    timings: StreamStageTimingAccumulator,
    scratch: FlushBatchScratch,
}

/// Content-complete operation awaiting caller validation and durable recipe sealing.
pub struct CompletedInput<'a> {
    session: IngestSession<'a>,
    recipe: crate::recipe::FileRecipe,
    prepared_xorbs: Vec<StreamStagePreparedXorb>,
}

impl<'a> IngestSession<'a> {
    /// Begin a file in the supplied preparation, or own a single-file preparation.
    pub async fn begin(
        staging: &'a StagingArea,
        path: &Path,
        progress: StreamStageProgress,
        cancel: &CancellationToken,
        prepared: Option<crate::recipe::FileRecipe>,
    ) -> Result<Self> {
        Self::begin_with_hooks(
            staging,
            path,
            progress,
            cancel,
            prepared,
            StreamStageHooks::default(),
        )
        .await
    }

    pub(crate) async fn begin_with_hooks(
        staging: &'a StagingArea,
        path: &Path,
        mut progress: StreamStageProgress,
        cancel: &CancellationToken,
        prepared: Option<crate::recipe::FileRecipe>,
        hooks: StreamStageHooks,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let preparation = progress
            .xorb_builder
            .as_ref()
            .and_then(StreamStageXorbBuilder::preparation_id);
        let (batch_id, owned_preparation) = staging.begin_ingest_batch(preparation)?;
        if let Some(id) = &owned_preparation
            && let Some(factory) = progress.xorb_builder.as_mut()
        {
            *factory = factory.clone().bind_preparation(id.clone());
        }
        let preparation_id = progress
            .xorb_builder
            .as_ref()
            .and_then(StreamStageXorbBuilder::preparation_id)
            .or(owned_preparation.as_ref())
            .ok_or_else(|| CrabError::Internal("missing ingestion preparation".to_owned()))?
            .clone();
        let provisional = provisional_file_hash(path, &batch_id);
        let mut session = Self {
            staging,
            path: path.to_path_buf(),
            batch_id,
            provisional,
            owned_preparation,
            preparation_id,
            progress,
            cancel: cancel.clone(),
            hooks,
            chunker: Some(GearChunker::new()),
            prepared: None,
            candidate_hash: None,
            hasher: blake3::Hasher::new(),
            batch: Vec::with_capacity(STAGE_BATCH_CHUNKS),
            batch_payload_bytes: 0,
            recipe_recorder: crate::recipe::RecipeRecorder::new(
                crate::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            ),
            builder_permit: None,
            xorb_builder: None,
            xorb_writer: None,
            chunk_index_offset: 0,
            recipe_byte_offset: 0,
            total_chunks: 0,
            total: 0,
            timings: StreamStageTimingAccumulator::default(),
            scratch: FlushBatchScratch::default(),
        };
        let setup = (|| -> Result<()> {
            staging.pre_register_file_with_path(&provisional, 0, &path.to_string_lossy())?;
            if let Some(recipe) = prepared {
                // Pin pages and payloads before either adapter consumes input;
                // the previous owner may retire while verification is running.
                staging.retain_verified_recipe(&session.batch_id, path, &recipe)?;
                session.candidate_hash = Some(recipe.file_hash());
                session.prepared = Some(PreparedRecipeVerifier::new(recipe));
            }
            Ok(())
        })();
        if let Err(error) = setup {
            session.abort().await?;
            return Err(error);
        }
        Ok(session)
    }

    /// Consume bytes without treating input-block boundaries as chunk boundaries.
    pub async fn feed(&mut self, bytes: Bytes) -> Result<()> {
        check_cancelled(&self.cancel)?;
        let hashing_start = Instant::now();
        self.hasher.update(&bytes);
        self.timings.hashing = self.timings.hashing.saturating_add(hashing_start.elapsed());
        self.total = self
            .total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| CrabError::StagingCorrupt("input size overflow".to_owned()))?;
        if let Some(counter) = &self.progress.bytes_done {
            counter.fetch_add(bytes.len() as u64, Relaxed);
        }
        if let Some(counter) = &self.progress.chunk_bytes_done {
            counter.fetch_add(bytes.len() as u64, Relaxed);
        }
        if let Some(verifier) = self.prepared.as_mut() {
            let verification_start = Instant::now();
            let verification = verifier.feed(&bytes, self.staging)?;
            self.timings.verification = self
                .timings
                .verification
                .saturating_add(verification_start.elapsed());
            match verification {
                PreparedVerification::Matching => return Ok(()),
                PreparedVerification::Changed {
                    verified_occurrences,
                    bytes,
                } => {
                    return self.replay_changed(verified_occurrences, bytes).await;
                }
                PreparedVerification::Exact => {
                    return Err(CrabError::Internal(
                        "verification ended before EOF".to_owned(),
                    ));
                }
            }
        }
        self.feed_chunks(bytes).await
    }

    async fn replay_changed(&mut self, occurrences: u64, remaining: Bytes) -> Result<()> {
        let verifier = self
            .prepared
            .take()
            .ok_or_else(|| CrabError::Internal("missing prepared verifier".to_owned()))?;
        let mut cursor = PreparedRecipeCursor::new(verifier.cursor.recipe);
        // xet-data 1.6 resets Gear state at every CDC cut. Retain verified
        // prefix terms directly; replay only the last verified chunk because
        // the original EOF tail may not be a cut when incoming content extends it.
        let retained = occurrences.saturating_sub(1);
        while self.chunk_index_offset < retained {
            check_cancelled(&self.cancel)?;
            self.scratch.terms.clear();
            for _ in 0..(retained - self.chunk_index_offset).min(STAGE_BATCH_CHUNKS as u64) {
                let term = cursor.next(self.staging)?.ok_or_else(|| {
                    CrabError::StagingCorrupt("verified prefix exceeded recipe".to_owned())
                })?;
                self.recipe_recorder.record(term.chunk_hash, term.len)?;
                self.scratch.terms.push((term.chunk_hash, term.len));
            }
            let index_start = Instant::now();
            let authority = self.staging.record_and_claim_chunks(
                &self.preparation_id,
                &self.batch_id,
                self.chunk_index_offset,
                self.recipe_byte_offset,
                &self.scratch.terms,
                &[],
                false,
            )?;
            self.timings.index = self.timings.index.saturating_add(index_start.elapsed());
            for (claim, (_, bytes)) in authority.iter().zip(self.scratch.terms.iter()) {
                match claim {
                    PreparedChunkClaim::Prepared(_) => self.timings.reused_prepared_bytes += bytes,
                    PreparedChunkClaim::Segment => self.timings.reused_segment_bytes += bytes,
                    _ => {
                        return Err(CrabError::StagingCorrupt(
                            "verified prefix lost local authority".to_owned(),
                        ));
                    }
                }
            }
            self.chunk_index_offset += self.scratch.terms.len() as u64;
            self.recipe_byte_offset = cursor.next_offset;
            self.total_chunks += self.scratch.terms.len();
        }
        if occurrences > 0 {
            let term = cursor.next(self.staging)?.ok_or_else(|| {
                CrabError::StagingCorrupt("verified replay exceeded recipe".to_owned())
            })?;
            let payload = self
                .staging
                .get_chunk(&term.chunk_hash)
                .await?
                .ok_or_else(|| {
                    CrabError::StagingCorrupt("verified replay payload disappeared".to_owned())
                })?;
            if payload.len() as u64 != term.len
                || crab_xet::hash::compute_data_hash(&payload) != term.chunk_hash
            {
                return Err(CrabError::StagingCorrupt(
                    "verified replay payload identity changed".to_owned(),
                ));
            }
            // Incoming bytes already updated the whole-file hash exactly once.
            self.feed_chunks(payload).await?;
        }
        self.feed_chunks(remaining).await
    }

    async fn start_builder(&mut self) -> Result<()> {
        if self.xorb_writer.is_none()
            && let Some(factory) = &self.progress.xorb_builder
        {
            let (permit, builder) = factory.build(&self.cancel).await?;
            self.builder_permit = Some(permit);
            self.xorb_builder = Some(builder);
            self.xorb_writer = Some(StreamPreparedXorbWriter::spawn(
                self.staging.root().to_path_buf(),
                self.provisional,
                factory,
                self.hooks.clone(),
            ));
        }
        Ok(())
    }

    async fn feed_chunks(&mut self, bytes: Bytes) -> Result<()> {
        let start = Instant::now();
        let emitted = self
            .chunker
            .as_mut()
            .ok_or_else(|| CrabError::Internal("input already finished".to_owned()))?
            .feed_bytes(&bytes);
        self.timings.chunking = self.timings.chunking.saturating_add(start.elapsed());
        append_emitted_chunks(
            emitted,
            &mut self.batch,
            &mut self.batch_payload_bytes,
            &mut self.recipe_recorder,
            self.progress.chunks_done.as_ref(),
            &mut self.total_chunks,
        )?;
        if batch_needs_flush(
            &self.batch,
            self.batch_payload_bytes,
            STAGE_BATCH_TARGET_BYTES,
        ) {
            self.start_builder().await?;
            self.flush(false).await?;
        }
        Ok(())
    }

    async fn flush(&mut self, final_batch: bool) -> Result<()> {
        let mut writer = self.xorb_writer.as_mut();
        if final_batch {
            flush_batch(
                &self.provisional,
                self.staging,
                &self.batch_id,
                &mut self.batch,
                &mut self.batch_payload_bytes,
                &mut self.chunk_index_offset,
                &mut self.recipe_byte_offset,
                &mut self.xorb_builder,
                &mut writer,
                self.progress.existing_lookup.as_deref(),
                &self.preparation_id,
                &mut self.scratch,
                &mut self.timings,
                &self.cancel,
                &self.hooks,
            )
            .await
        } else {
            flush_full_batches(
                &self.provisional,
                self.staging,
                &self.batch_id,
                &mut self.batch,
                &mut self.batch_payload_bytes,
                &mut self.chunk_index_offset,
                &mut self.recipe_byte_offset,
                &mut self.xorb_builder,
                &mut writer,
                self.progress.existing_lookup.as_deref(),
                &self.preparation_id,
                &mut self.scratch,
                &mut self.timings,
                &self.cancel,
                &self.hooks,
            )
            .await
        }
    }

    /// Finalize content and workers without publishing a recipe or pointer.
    pub async fn finish_input(mut self) -> Result<CompletedInput<'a>> {
        let exact = if let Some(verifier) = self.prepared.as_mut() {
            match verifier.finish(self.staging) {
                Ok(PreparedVerification::Exact) => {
                    let recipe = verifier.cursor.recipe.clone();
                    if recipe.file_size() != self.total
                        || <[u8; 32]>::from(recipe.file_hash())
                            != *self.hasher.finalize().as_bytes()
                    {
                        self.abort().await?;
                        return Err(CrabError::StagingCorrupt(
                            "prepared input identity disagrees with recipe".to_owned(),
                        ));
                    }
                    Some(recipe)
                }
                Ok(PreparedVerification::Changed {
                    verified_occurrences,
                    bytes,
                }) => {
                    if let Err(error) = self.replay_changed(verified_occurrences, bytes).await {
                        self.abort().await?;
                        return Err(error);
                    }
                    None
                }
                Ok(PreparedVerification::Matching) => {
                    self.abort().await?;
                    return Err(CrabError::StagingCorrupt(
                        "prepared verification incomplete at EOF".to_owned(),
                    ));
                }
                Err(error) => {
                    self.abort().await?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let finish = if exact.is_some() {
            Ok(())
        } else {
            self.finish_chunks().await
        };
        if let Err(error) = finish {
            self.abort().await?;
            return Err(error);
        }
        let writes = match self.xorb_writer.take() {
            Some(writer) => writer.finish().await,
            None => Ok(PreparedXorbWriteResult {
                xorbs: Vec::new(),
                duration: Duration::ZERO,
            }),
        };
        let writes = match writes {
            Ok(writes) => writes,
            Err(error) => {
                self.abort().await?;
                return Err(error);
            }
        };
        self.builder_permit.take();
        self.timings.payload_write = self.timings.payload_write.saturating_add(writes.duration);
        let recorder = std::mem::replace(
            &mut self.recipe_recorder,
            crate::recipe::RecipeRecorder::new(crate::recipe::ChunkingPolicyId::XetGearV1_64KiB),
        );
        let recipe = match exact.map(Ok).unwrap_or_else(|| {
            recorder.seal(
                MerkleHash::from(*self.hasher.finalize().as_bytes()),
                self.total,
            )
        }) {
            Ok(recipe) => recipe,
            Err(error) => {
                self.abort().await?;
                return Err(error);
            }
        };
        self.total_chunks = usize::try_from(recipe.chunk_count())
            .map_err(|_| CrabError::StagingCorrupt("recipe count exceeds usize".to_owned()))?;
        Ok(CompletedInput {
            session: self,
            recipe,
            prepared_xorbs: writes.xorbs,
        })
    }

    async fn finish_chunks(&mut self) -> Result<()> {
        check_cancelled(&self.cancel)?;
        let start = Instant::now();
        let chunker = self
            .chunker
            .take()
            .ok_or_else(|| CrabError::Internal("input already finished".to_owned()))?;
        if let Some(last) = chunker.finalize() {
            append_emitted_chunks(
                vec![last],
                &mut self.batch,
                &mut self.batch_payload_bytes,
                &mut self.recipe_recorder,
                self.progress.chunks_done.as_ref(),
                &mut self.total_chunks,
            )?;
        }
        self.timings.chunking = self.timings.chunking.saturating_add(start.elapsed());
        if !self.batch.is_empty() {
            self.start_builder().await?;
        }
        self.flush(true).await?;
        if let Some(builder) = self.xorb_builder.take() {
            let writer = self
                .xorb_writer
                .as_mut()
                .ok_or_else(|| CrabError::Internal("missing xorb writer".to_owned()))?;
            let mut permit = if builder.staged_count() > 0 {
                Some(writer.factory.acquire_materialization(&self.cancel).await?)
            } else {
                None
            };
            let start = Instant::now();
            let finalized = tokio::task::spawn_blocking(move || builder.finalize())
                .await
                .map_err(|error| CrabError::Io(std::io::Error::other(error)))??;
            self.timings.compression = self.timings.compression.saturating_add(start.elapsed());
            if finalized.len() > 1 {
                return Err(CrabError::Internal(
                    "undrained xorb rollover at finalization".to_owned(),
                ));
            }
            for result in finalized {
                let permit = permit
                    .take()
                    .ok_or_else(|| CrabError::Internal("unreserved final xorb".to_owned()))?;
                writer.submit_reserved(result, permit).await?;
            }
        }
        Ok(())
    }

    /// Abort unpublished ingestion, retaining payloads owned by other operations.
    pub async fn abort(mut self) -> Result<()> {
        if let Some(factory) = &self.progress.xorb_builder {
            factory.fail_preparation();
        }
        self.xorb_builder.take();
        if let Some(writer) = self.xorb_writer.take() {
            // Join before removing temporary files: a detached writer can recreate
            // them after cleanup and retain admission needed by sibling workers.
            if let Err(error) = writer.finish().await {
                warn!(%error, "aborted ingestion writer failed");
            }
        }
        if let Some(id) = &self.owned_preparation {
            self.staging.abort_add_preparation(id)?;
        }
        self.staging.rollback_batch(&self.batch_id)?;
        self.staging.retire_file(&self.provisional)?;
        self.staging.unregister_file(&self.provisional)?;
        cleanup_stream_prepared_xorb_dir(self.staging.root(), &self.provisional, &self.path);
        Ok(())
    }
}

impl CompletedInput<'_> {
    /// Verified content identity available before source validation.
    #[must_use]
    pub fn file_hash(&self) -> [u8; 32] {
        *self.session.hasher.finalize().as_bytes()
    }

    /// Number of input bytes consumed.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.session.total
    }

    /// Discard complete content when validation or an authoritative fast path wins.
    pub async fn abort(self) -> Result<()> {
        self.session.timings.trace(self.session.total, false);
        self.session.abort().await
    }

    /// Install durable payload authority and a complete unpublished recipe lease.
    pub async fn seal(mut self) -> Result<StreamStageResult> {
        let seal = self.seal_inner().await;
        match seal {
            Ok(()) => {
                let session = self.session;
                session.timings.trace(session.total, true);
                Ok(StreamStageResult {
                    batch_id: session.batch_id,
                    abs_path: session.path,
                    file_hash: *session.hasher.finalize().as_bytes(),
                    size: session.total,
                    chunks: session.total_chunks,
                    recipe: self.recipe,
                    prepared_xorbs: self.prepared_xorbs,
                    index_stat: None,
                    timings: session.timings.finish(),
                    duration_ms: 0,
                })
            }
            Err(error) => {
                let staging = self.session.staging;
                let hash = self.recipe.file_hash();
                self.session.abort().await?;
                staging.retire_file_if_unleased(&hash)?;
                Err(error)
            }
        }
    }

    async fn seal_inner(&mut self) -> Result<()> {
        let s = &mut self.session;
        check_cancelled(&s.cancel)?;
        let hash = self.recipe.file_hash();
        if s.staging.has_verified_local_recipe(&self.recipe)? {
            s.staging.retire_file(&s.provisional)?;
            s.staging.unregister_file(&s.provisional)?;
        } else {
            s.staging
                .adopt_staged_file(&s.provisional, &hash, s.total)?;
        }
        if s.progress.xorb_builder.is_some() {
            let start = Instant::now();
            persist_stream_prepared_authority(
                s.staging,
                &s.batch_id,
                &mut self.prepared_xorbs,
                s.progress.xorb_builder.as_ref(),
                &s.hooks,
            )
            .await?;
            s.timings.payload_write = s.timings.payload_write.saturating_add(start.elapsed());
            if let Some(factory) = &s.progress.xorb_builder {
                factory.authority_changed();
                let wait_start = Instant::now();
                factory
                    .wait_for_recording_authority(s.staging, &s.batch_id, &s.cancel)
                    .await?;
                s.timings.authority_wait = s
                    .timings
                    .authority_wait
                    .saturating_add(wait_start.elapsed());
            }
        }
        // A complete recipe may also reuse pending segment authorities. Persist
        // their bytes before the lease can authorize any pointer or upload.
        s.staging.flush_pending().await?;
        // Exact verification already owns this verified recipe through begin's
        // retained lease. Rebuilding its occurrences adds I/O without new proof.
        if s.prepared.is_none() {
            s.staging
                .record_verified_recipe_lease(&s.batch_id, &s.path, &self.recipe)?;
        }
        s.staging
            .record_file_path(&hash, &s.path.to_string_lossy())?;
        if let Some(candidate) = s.candidate_hash
            && candidate != hash
        {
            s.staging.retire_file_if_unleased(&candidate)?;
        }
        if let Some(id) = &s.owned_preparation {
            s.staging.finalize_add_preparation(id)?;
        }
        s.staging.flush_index().await?;
        cleanup_stream_prepared_xorb_dir(s.staging.root(), &s.provisional, &s.path);
        Ok(())
    }
}

fn provisional_file_hash(path: &Path, batch_id: &crate::StagingBatchId) -> MerkleHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab provisional add staging v1\0");
    hasher.update(&super::staging_path_bytes(path));
    hasher.update(batch_id.as_str().as_bytes());
    MerkleHash::from(*hasher.finalize().as_bytes())
}

#[derive(Default)]
struct FlushBatchScratch {
    terms: Vec<(MerkleHash, u64)>,
    remote_authority: Vec<ExistingChunkWrite>,
    existing: Vec<Option<ExistingChunkCandidate>>,
    to_pack: Vec<(Chunk, RunId)>,
}

fn append_emitted_chunks(
    chunks: Vec<crab_xet::chunker::Chunk>,
    batch: &mut Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: &mut u64,
    recipe_recorder: &mut crate::recipe::RecipeRecorder,
    chunks_done: Option<&Arc<AtomicU64>>,
    total_chunks: &mut usize,
) -> Result<()> {
    let count = chunks.len();
    for chunk in chunks {
        let hash = MerkleHash::from(<[u8; 32]>::from(chunk.hash));
        let size = chunk.data.len() as u64;
        recipe_recorder.record(hash, size)?;
        *batch_payload_bytes = batch_payload_bytes.checked_add(size).ok_or_else(|| {
            CrabError::StagingCorrupt("stream staging batch byte count overflow".to_owned())
        })?;
        batch.push((hash, chunk.data));
    }
    *total_chunks += count;
    if count > 0
        && let Some(counter) = chunks_done
    {
        counter.fetch_add(count as u64, Relaxed);
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "batch flush owns several mutable staging buffers without packing them into a transient struct"
)]
async fn flush_full_batches(
    file_hash: &MerkleHash,
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    batch: &mut Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: &mut u64,
    chunk_index_offset: &mut u64,
    recipe_byte_offset: &mut u64,
    xorb_builder: &mut Option<XorbBuilder>,
    xorb_writer: &mut Option<&mut StreamPreparedXorbWriter>,
    existing_lookup: Option<&dyn ExistingChunkLookup>,
    preparation_id: &crate::AddPreparationId,
    scratch: &mut FlushBatchScratch,
    timings: &mut StreamStageTimingAccumulator,
    cancel: &CancellationToken,
    hooks: &StreamStageHooks,
) -> Result<()> {
    let target_bytes = stage_batch_target_bytes(xorb_builder.is_some());
    while batch_needs_flush(batch, *batch_payload_bytes, target_bytes) {
        let (flush_len, flush_bytes) = batch_flush_prefix(batch, target_bytes);
        let tail = batch.split_off(flush_len);
        let mut full = std::mem::replace(batch, tail);
        let mut full_payload_bytes = flush_bytes;
        *batch_payload_bytes = batch_payload_bytes.saturating_sub(flush_bytes);
        flush_batch(
            file_hash,
            staging,
            batch_id,
            &mut full,
            &mut full_payload_bytes,
            chunk_index_offset,
            recipe_byte_offset,
            xorb_builder,
            xorb_writer,
            existing_lookup,
            preparation_id,
            scratch,
            timings,
            cancel,
            hooks,
        )
        .await?;
        check_cancelled(cancel)?;
    }
    Ok(())
}

fn stage_batch_target_bytes(building_xorbs: bool) -> u64 {
    if building_xorbs {
        DIRECT_XORB_STAGE_BATCH_TARGET_BYTES
    } else {
        STAGE_BATCH_TARGET_BYTES
    }
}

fn batch_needs_flush(
    batch: &[(MerkleHash, Bytes)],
    batch_payload_bytes: u64,
    target_bytes: u64,
) -> bool {
    batch.len() >= STAGE_BATCH_CHUNKS || batch_payload_bytes >= target_bytes
}

fn batch_flush_prefix(batch: &[(MerkleHash, Bytes)], target_bytes: u64) -> (usize, u64) {
    batch_flush_prefix_from_lengths(batch.iter().map(|(_, data)| data.len()), target_bytes)
}

fn batch_flush_prefix_from_lengths(
    lengths: impl IntoIterator<Item = usize>,
    target_bytes: u64,
) -> (usize, u64) {
    let mut bytes = 0u64;
    let mut count = 0usize;
    for (idx, len) in lengths.into_iter().enumerate() {
        count = idx + 1;
        bytes = bytes.saturating_add(len as u64);
        if idx + 1 >= STAGE_BATCH_CHUNKS || bytes >= target_bytes {
            return (idx + 1, bytes);
        }
    }
    (count, bytes)
}

#[expect(
    clippy::too_many_arguments,
    reason = "batch state and writer lifecycle stay explicit across the blocking pack boundary"
)]
async fn flush_batch(
    file_hash: &MerkleHash,
    staging: &StagingArea,
    batch_id: &crate::StagingBatchId,
    batch: &mut Vec<(MerkleHash, Bytes)>,
    batch_payload_bytes: &mut u64,
    chunk_index_offset: &mut u64,
    recipe_byte_offset: &mut u64,
    xorb_builder: &mut Option<XorbBuilder>,
    xorb_writer: &mut Option<&mut StreamPreparedXorbWriter>,
    existing_lookup: Option<&dyn ExistingChunkLookup>,
    preparation_id: &crate::AddPreparationId,
    scratch: &mut FlushBatchScratch,
    timings: &mut StreamStageTimingAccumulator,
    cancel: &CancellationToken,
    hooks: &StreamStageHooks,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let batch_start = *chunk_index_offset;
    let lookup_start = Instant::now();
    classify_existing_batch(
        batch,
        existing_lookup,
        cancel,
        &mut scratch.terms,
        &mut scratch.existing,
    )
    .await?;
    let existing = &mut scratch.existing;
    timings.remote_lookup = timings.remote_lookup.saturating_add(lookup_start.elapsed());
    for (candidate, (_, data)) in existing.iter_mut().zip(batch.iter()) {
        if candidate.as_ref().is_some_and(|candidate| {
            u64::from(candidate.xorb_ref.uncompressed_size) != data.len() as u64
                || candidate.placement_id == [0; 32]
                || candidate.origin_proof_id == [0; 32]
        }) {
            *candidate = None;
        }
    }
    scratch.remote_authority.clear();
    scratch
        .remote_authority
        .extend(
            batch
                .iter()
                .zip(existing.iter())
                .filter_map(|((chunk_hash, _), candidate)| {
                    candidate.map(|candidate| ExistingChunkWrite {
                        chunk_hash: (*chunk_hash).into(),
                        xorb_hash: candidate.xorb_ref.xorb_hash.into(),
                        chunk_index: candidate.xorb_ref.chunk_index,
                        uncompressed_size: candidate.xorb_ref.uncompressed_size,
                        placement_id: candidate.placement_id,
                        origin_proof_id: candidate.origin_proof_id,
                    })
                }),
        );
    scratch.terms.clear();
    scratch
        .terms
        .extend(batch.iter().map(|(hash, data)| (*hash, data.len() as u64)));
    // Retain existing authority before any payload work. Segment consumers must
    // reuse prepared bodies too; otherwise switching policy copies the file.
    let index_start = Instant::now();
    let claims = staging.record_and_claim_chunks(
        preparation_id,
        batch_id,
        *chunk_index_offset,
        *recipe_byte_offset,
        &scratch.terms,
        &scratch.remote_authority,
        xorb_builder.is_some(),
    )?;
    timings.index = timings.index.saturating_add(index_start.elapsed());
    let mut claims = claims.into_iter();
    let mut missing = Vec::with_capacity(batch.len());
    for (candidate, (_, data)) in existing.iter().zip(batch.iter()) {
        let bytes = data.len() as u64;
        let unknown = if candidate.is_some() {
            timings.reused_remote_bytes += bytes;
            false
        } else {
            match claims.next().ok_or_else(|| {
                CrabError::Internal("authority claim cardinality was truncated".to_owned())
            })? {
                PreparedChunkClaim::Prepared(_) => {
                    timings.reused_prepared_bytes += bytes;
                    false
                }
                PreparedChunkClaim::Segment => {
                    timings.reused_segment_bytes += bytes;
                    false
                }
                PreparedChunkClaim::Pending => false,
                PreparedChunkClaim::Claimed | PreparedChunkClaim::Missing => true,
            }
        };
        missing.push(unknown);
    }
    if claims.next().is_some() {
        return Err(CrabError::Internal(
            "authority claim cardinality exceeded input".to_owned(),
        ));
    }
    if xorb_builder.is_none() {
        let mut start = 0usize;
        while start < batch.len() {
            while start < batch.len() && !missing[start] {
                start += 1;
            }
            if start == batch.len() {
                break;
            }
            let mut end = start + 1;
            while end < batch.len() && missing[end] {
                end += 1;
            }
            staging
                .stage_owned_chunks_batch_for_retired_file(
                    batch[start..end].to_vec(),
                    file_hash,
                    batch_start.checked_add(start as u64).ok_or_else(|| {
                        CrabError::StagingCorrupt(
                            "stream staging miss-run offset overflow".to_owned(),
                        )
                    })?,
                )
                .await?;
            start = end;
        }
    }
    *recipe_byte_offset =
        scratch
            .terms
            .iter()
            .try_fold(*recipe_byte_offset, |offset, (_, size)| {
                offset.checked_add(*size).ok_or_else(|| {
                    CrabError::StagingCorrupt("stream recipe byte offset overflow".to_owned())
                })
            })?;
    let batch_len = u64::try_from(batch.len()).map_err(|_| {
        CrabError::StagingCorrupt(format!(
            "stream staging batch length {} cannot be represented",
            batch.len()
        ))
    })?;
    *chunk_index_offset = chunk_index_offset.checked_add(batch_len).ok_or_else(|| {
        CrabError::StagingCorrupt(format!(
            "stream staging chunk index overflow at offset {}",
            *chunk_index_offset
        ))
    })?;
    if xorb_builder.is_some() {
        let mut to_pack = std::mem::take(&mut scratch.to_pack);
        to_pack.clear();
        to_pack.extend(
            batch
                .iter()
                .zip(missing.iter())
                .filter(|(_, missing)| **missing)
                .map(|((hash, data), _)| {
                    (
                        Chunk {
                            hash: *hash,
                            data: data.clone(),
                        },
                        RunId(0),
                    )
                }),
        );
        let mut builder = xorb_builder.take().ok_or_else(|| {
            CrabError::Internal("direct xorb builder disappeared before pack".to_owned())
        })?;
        let writer = xorb_writer.take().ok_or_else(|| {
            CrabError::Internal("direct xorb writer disappeared before pack".to_owned())
        })?;
        let writer_factory = writer.factory.clone();
        let sender = writer.sender.as_ref().cloned().ok_or_else(|| {
            CrabError::Internal("direct xorb writer already finished before pack".to_owned())
        })?;
        let mut next_sequence = writer.next_sequence;
        let runtime = tokio::runtime::Handle::current();
        let pack_cancel = cancel.clone();
        let compression_start = Instant::now();
        let (returned_builder, returned_sequence, returned_to_pack) =
            tokio::task::spawn_blocking(move || {
                builder.push_batch_with_rollover_admission(
                    &to_pack,
                    || runtime.block_on(writer_factory.acquire_materialization(&pack_cancel)),
                    |result, permit| {
                        StreamPreparedXorbWriter::submit_reserved_blocking(
                            &sender,
                            &mut next_sequence,
                            result,
                            permit,
                        )
                    },
                )?;
                Ok::<_, CrabError>((builder, next_sequence, to_pack))
            })
            .await
            .map_err(|error| CrabError::Io(std::io::Error::other(error)))??;
        timings.compression = timings
            .compression
            .saturating_add(compression_start.elapsed());
        *xorb_builder = Some(returned_builder);
        writer.next_sequence = returned_sequence;
        *xorb_writer = Some(writer);
        scratch.to_pack = returned_to_pack;
        scratch.to_pack.clear();
    }
    batch.clear();
    *batch_payload_bytes = 0;
    if let Some(hook) = &hooks.after_batch_flush {
        hook();
    }
    Ok(())
}

async fn classify_existing_batch(
    batch: &[(MerkleHash, Bytes)],
    lookup: Option<&dyn ExistingChunkLookup>,
    cancel: &CancellationToken,
    terms: &mut Vec<(MerkleHash, u64)>,
    existing: &mut Vec<Option<ExistingChunkCandidate>>,
) -> Result<()> {
    existing.clear();
    let Some(lookup) = lookup else {
        existing.resize(batch.len(), None);
        return Ok(());
    };
    terms.clear();
    terms.extend(batch.iter().map(|(hash, data)| (*hash, data.len() as u64)));
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        result = lookup.lookup_existing_candidates(terms) => result,
    };
    match result {
        Ok(candidates) if candidates.len() == batch.len() => {
            existing.extend(candidates);
        }
        Ok(candidates) => {
            warn!(
                returned = candidates.len(),
                requested = batch.len(),
                "remote chunk classifier returned malformed cardinality; packing batch locally"
            );
            existing.resize(batch.len(), None);
        }
        Err(CrabError::Cancelled) => return Err(CrabError::Cancelled),
        Err(error) => {
            warn!(
                error = %error,
                chunks = batch.len(),
                "remote chunk classifier unavailable; packing batch locally"
            );
            existing.resize(batch.len(), None);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
#[path = "ingest/tests.rs"]
mod tests;
