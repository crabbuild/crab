//! xet-core `Client` Adapter over Crab cache, storage, metadata, and Xet xorbs.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use crab_cache::CacheKey;
use crab_cache_store::CachingStore;
use crab_metadata::file_index_lookup::{FileIndexLookupSession, SharedFileIndexLookup};
use crab_xet::hash::MerkleHash;
use crab_xet::shard::{MDBFileInfo, ShardReader};
use crab_xet::shard_parse::MAX_SHARD_SIZE_BYTES;
use crab_xet::xorb::format::SerializedXorbObject;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use xet_client::cas_client::ShardUploadProgressCallback;
use xet_client::cas_client::adaptive_concurrency::{
    AdaptiveConcurrencyController, ConnectionPermit,
};
use xet_client::cas_client::progress_tracked_streams::ProgressCallback;
use xet_client::cas_client::{Client, URLProvider};
use xet_client::cas_types::{
    BatchQueryReconstructionResponse, ChunkRange, FileChunkHashesResponse, FileRange,
    HexMerkleHash, HttpRange, QueryReconstructionResponseV2, XorbMultiRangeFetch,
    XorbRangeDescriptor, XorbReconstructionFetchInfo, XorbReconstructionTerm,
};
use xet_client::error::{ClientError, Result as ClientResult};

use crate::{ReadError, Result};

mod term_cache;

type StoreLayout = crab_storage::StoreLayout<crab_storage::Store>;

type SharedShardHints = Arc<RwLock<HashMap<MerkleHash, MerkleHash>>>;

/// Invocation-local observations emitted by the canonical read adapter.
pub trait ReadMetrics: Send + Sync {
    fn shard_hint_hit(&self);
    fn shard_hint_miss(&self);
}

#[async_trait::async_trait]
pub trait XorbAvailability: Send + Sync {
    async fn ensure_available(&self, path: &object_store::path::Path) -> Result<()>;
}

const XORB_URL_PREFIX: &str = "crab-xorb://";

/// Read-only Adapter implementing xet-core's `Client` trait.
pub struct StoreClient {
    store: CachingStore,
    router: StoreLayout,
    concurrency: Arc<AdaptiveConcurrencyController>,
    file_index_lookup: Option<SharedFileIndexLookup>,
    shard_hints: SharedShardHints,
    metrics: Option<Arc<dyn ReadMetrics>>,
    availability: Option<Arc<dyn XorbAvailability>>,
    failures: Option<Arc<crate::error::OperationFailures>>,
    cancellation: CancellationToken,
    chunk_cache: Option<term_cache::TermCache>,
    selective_xorb_reads: bool,
}

impl StoreClient {
    #[must_use]
    pub fn new(
        store: CachingStore,
        router: StoreLayout,
        concurrency: Arc<AdaptiveConcurrencyController>,
    ) -> Self {
        Self {
            store,
            router,
            concurrency,
            file_index_lookup: None,
            shard_hints: Arc::new(RwLock::new(HashMap::new())),
            metrics: None,
            availability: None,
            failures: None,
            cancellation: CancellationToken::new(),
            chunk_cache: None,
            selective_xorb_reads: false,
        }
    }

    #[must_use]
    pub fn with_file_index_lookup(mut self, lookup: SharedFileIndexLookup) -> Self {
        self.file_index_lookup = Some(lookup);
        self
    }

    #[must_use]
    pub fn with_metrics<M>(mut self, metrics: Arc<M>) -> Self
    where
        M: ReadMetrics + 'static,
    {
        self.metrics = Some(metrics);
        self
    }

    #[must_use]
    pub fn with_dyn_metrics(mut self, metrics: Arc<dyn ReadMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    #[must_use]
    pub fn with_availability(mut self, availability: Arc<dyn XorbAvailability>) -> Self {
        self.availability = Some(availability);
        self
    }

    #[must_use]
    pub fn with_shard_hint(self, file_hash: MerkleHash, shard_hash: MerkleHash) -> Self {
        self.insert_shard_hint(file_hash, shard_hash);
        self
    }

    pub(crate) fn with_operation(
        mut self,
        failures: Arc<crate::error::OperationFailures>,
        cancellation: CancellationToken,
    ) -> Self {
        self.failures = Some(failures);
        self.cancellation = cancellation;
        self
    }

    pub(crate) fn with_chunk_cache(
        mut self,
        cache: Arc<dyn xet_client::chunk_cache::ChunkCache>,
        prefix: String,
    ) -> Self {
        self.chunk_cache = Some(term_cache::TermCache::new(cache, prefix));
        self
    }

    pub(crate) fn with_selective_xorb_reads(mut self) -> Self {
        // Whole-file hydration avoids storing a second complete xorb beside its
        // output. Range hydration opts in so cold reads fetch only needed payload.
        self.selective_xorb_reads = true;
        self
    }

    fn record_error(&self, error: ClientError) -> ClientError {
        match &self.failures {
            Some(failures) => failures.read_error(error),
            None => error,
        }
    }

    fn map_read_error(&self, error: ReadError) -> ClientError {
        self.record_error(ClientError::internal(error))
    }

    fn insert_shard_hint(&self, file_hash: MerkleHash, shard_hash: MerkleHash) {
        let mut hints = match self.shard_hints.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        hints.insert(file_hash, shard_hash);
    }

    fn shard_hint(&self, file_hash: &MerkleHash) -> Option<MerkleHash> {
        let hints = match self.shard_hints.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        hints.get(file_hash).copied()
    }

    async fn load_shard_for_file(
        &self,
        file_hash: &MerkleHash,
    ) -> ClientResult<Option<(ShardReader, MerkleHash)>> {
        if let Some(hint_hash) = self.shard_hint(file_hash) {
            match self.load_shard(&hint_hash).await {
                Ok(shard) if shard.has_file(file_hash) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %hint_hash.hex(),
                        "read store_client: shard-hint hit"
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.shard_hint_hit();
                    }
                    return Ok(Some((shard, hint_hash)));
                }
                Ok(_) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %hint_hash.hex(),
                        "read store_client: shard-hint stale, falling back to file-index"
                    );
                }
                // Admission failure is terminal; trying the index would hide
                // the rejected work behind an unrelated missing-index error.
                Err(e) if crab_storage::read_rejection(&e).is_some() => {
                    return Err(self.map_read_error(e));
                }
                Err(e) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %hint_hash.hex(),
                        error = %e,
                        "read store_client: shard-hint fetch failed, falling back to file-index"
                    );
                }
            }
            if let Some(metrics) = &self.metrics {
                metrics.shard_hint_miss();
            }
        }

        let shard_hash = match self.resolve_file_index(file_hash).await {
            Ok(h) => h,
            Err(ReadError::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(self.map_read_error(e)),
        };

        if !self.store.has_cache_service()
            && !self
                .store
                .local_cache()
                .contains(&CacheKey::Shard(shard_hash))
                .await
        {
            let shard_path = self.router.shard_path(&shard_hash);
            match crab_metadata::bloom_prefilter::check_shard_file_bloom(
                self.store.origin(),
                &shard_path,
                file_hash,
            )
            .await
            {
                Ok(crab_metadata::bloom_prefilter::BloomCheck::DefinitelyAbsent) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %shard_hash.hex(),
                        "bloom pre-filter: file absent from shard, skipping download"
                    );
                    return Ok(None);
                }
                Ok(_) => {}
                Err(e) if crab_storage::read_rejection(&e).is_some() => {
                    return Err(self.map_read_error(e.into()));
                }
                Err(e) => {
                    debug!(
                        shard_hash = %shard_hash.hex(),
                        error = %e,
                        "bloom pre-filter failed, falling back to full shard download"
                    );
                }
            }
        }

        match self.load_shard(&shard_hash).await {
            Ok(shard) => Ok(Some((shard, shard_hash))),
            Err(ReadError::NotFound { .. }) => Ok(None),
            Err(e) => Err(self.map_read_error(e)),
        }
    }

    async fn resolve_file_index(&self, file_hash: &MerkleHash) -> Result<MerkleHash> {
        let hit = match &self.file_index_lookup {
            Some(lookup) => lookup.lookup(file_hash).await?,
            None => {
                let storage = self.store.cache_aware_storage();
                let session =
                    FileIndexLookupSession::open_for_storage(&storage, self.router.repo_prefix())
                        .await?;
                let result = session.lookup(file_hash).await;
                if let Err(close_error) = session.close().await {
                    warn!(error = %close_error, "read store_client: file-index lookup close failed");
                }
                result?
            }
        };

        match hit {
            Some(shard_hash) => Ok(shard_hash),
            None => Err(ReadError::NotFound {
                path: format!("file_index:{}", file_hash.hex()),
            }),
        }
    }

    async fn resolve_file_indexes_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> Result<Vec<Option<MerkleHash>>> {
        if let Some(lookup) = &self.file_index_lookup {
            return lookup.lookup_batch(file_hashes).await.map_err(Into::into);
        }

        let storage = self.store.cache_aware_storage();
        let session =
            FileIndexLookupSession::open_for_storage(&storage, self.router.repo_prefix()).await?;
        let result = session.lookup_batch(file_hashes).await;
        if let Err(close_err) = session.close().await {
            warn!(
                err = %close_err,
                "read store_client: file-index lookup session close failed after batch read"
            );
        }
        result.map_err(Into::into)
    }

    async fn load_shard(&self, shard_hash: &MerkleHash) -> Result<ShardReader> {
        let path = self.router.shard_path(shard_hash);
        debug!(shard_hash = %shard_hash.hex(), "read store_client: downloading shard");
        let (data, _) = self
            .store
            .get_with_etag_bounded(&path, MAX_SHARD_SIZE_BYTES as u64)
            .await?;

        Ok(ShardReader::from_bytes(data, *shard_hash))
    }
}

fn xorb_url(hash: &MerkleHash, chunks: &[ChunkRange]) -> String {
    let mut url = format!("{XORB_URL_PREFIX}{}", hash.hex());
    if !chunks.is_empty() {
        url.push_str("?chunks=");
        for (i, range) in chunks.iter().enumerate() {
            if i > 0 {
                url.push(',');
            }
            url.push_str(&range.to_string());
        }
    }
    url
}

fn parse_xorb_url(url: &str) -> ClientResult<(MerkleHash, Vec<ChunkRange>)> {
    let body = url
        .strip_prefix(XORB_URL_PREFIX)
        .ok_or_else(|| ClientError::Other(format!("unrecognized xorb url: {url}")))?;

    let (hex, chunks_part) = match body.split_once("?chunks=") {
        Some((hex, chunks)) => (hex, Some(chunks)),
        None => (body, None),
    };

    let hash = MerkleHash::from_hex(hex)
        .map_err(|e| ClientError::Other(format!("invalid xorb url hash: {e}")))?;

    let ranges = match chunks_part {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .filter(|seg| !seg.is_empty())
            .map(|seg| {
                seg.parse::<ChunkRange>()
                    .map_err(|e| ClientError::Other(format!("invalid chunk range {seg:?}: {e}")))
            })
            .collect::<ClientResult<Vec<_>>>()?,
    };

    Ok((hash, ranges))
}

fn build_response_v2(file_info: &MDBFileInfo) -> QueryReconstructionResponseV2 {
    let mut terms = Vec::with_capacity(file_info.segments.len());
    let mut xorbs: HashMap<HexMerkleHash, Vec<XorbMultiRangeFetch>> = HashMap::new();

    for seg in &file_info.segments {
        let chunks = ChunkRange::new(seg.chunk_index_start, seg.chunk_index_end);
        terms.push(XorbReconstructionTerm {
            hash: HexMerkleHash::from(seg.xorb_hash),
            unpacked_length: seg.unpacked_segment_bytes,
            range: chunks,
        });

        let fetch = XorbMultiRangeFetch {
            url: xorb_url(&seg.xorb_hash, std::slice::from_ref(&chunks)),
            ranges: vec![XorbRangeDescriptor {
                chunks,
                bytes: HttpRange::new(0, u64::from(seg.unpacked_segment_bytes)),
            }],
        };
        xorbs
            .entry(HexMerkleHash::from(seg.xorb_hash))
            .or_default()
            .push(fetch);
    }

    QueryReconstructionResponseV2 {
        offset_into_first_range: 0,
        terms,
        xorbs,
    }
}

fn build_range_response_v2(
    file_info: &MDBFileInfo,
    shard: &ShardReader,
    requested: FileRange,
) -> ClientResult<Option<QueryReconstructionResponseV2>> {
    if requested.end < requested.start {
        return Err(ClientError::InvalidRange);
    }
    let file_size = file_info
        .segments
        .iter()
        .try_fold(0_u64, |total, segment| {
            total.checked_add(u64::from(segment.unpacked_segment_bytes))
        })
        .ok_or_else(|| ClientError::Other("reconstruction file size overflow".to_owned()))?;
    if requested.start >= file_size {
        return Ok(None);
    }
    let requested = FileRange::new(requested.start, requested.end.min(file_size));
    let mut terms = Vec::new();
    let mut xorbs: HashMap<HexMerkleHash, Vec<XorbMultiRangeFetch>> = HashMap::new();
    let mut segment_start = 0_u64;
    let mut offset_into_first_range = None;

    for segment in &file_info.segments {
        let segment_end = segment_start
            .checked_add(u64::from(segment.unpacked_segment_bytes))
            .ok_or_else(|| ClientError::Other("reconstruction segment overflow".to_owned()))?;
        if segment_end <= requested.start {
            segment_start = segment_end;
            continue;
        }
        if segment_start >= requested.end {
            break;
        }

        let xorb = shard
            .get_xorb_info(&segment.xorb_hash)
            .map_err(ClientError::internal)?
            .ok_or_else(|| {
                ClientError::Other(format!(
                    "reconstruction shard lacks xorb metadata for {}",
                    segment.xorb_hash.hex()
                ))
            })?;
        let chunk_start = usize::try_from(segment.chunk_index_start)
            .map_err(|_| ClientError::Other("xorb chunk index overflow".to_owned()))?;
        let chunk_end = usize::try_from(segment.chunk_index_end)
            .map_err(|_| ClientError::Other("xorb chunk index overflow".to_owned()))?;
        let chunks = xorb.chunks.get(chunk_start..chunk_end).ok_or_else(|| {
            ClientError::Other(format!(
                "reconstruction chunk range {}..{} exceeds xorb {} metadata",
                segment.chunk_index_start,
                segment.chunk_index_end,
                segment.xorb_hash.hex()
            ))
        })?;
        let declared_size = chunks.iter().try_fold(0_u64, |total, chunk| {
            total.checked_add(u64::from(chunk.unpacked_segment_bytes))
        });
        if declared_size != Some(u64::from(segment.unpacked_segment_bytes)) {
            return Err(ClientError::Other(format!(
                "reconstruction segment size does not match xorb {} chunk metadata",
                segment.xorb_hash.hex()
            )));
        }

        let mut selected_start = 0_usize;
        let mut selected_chunk_file_start = segment_start;
        while selected_start < chunks.len() {
            let next = selected_chunk_file_start
                .checked_add(u64::from(chunks[selected_start].unpacked_segment_bytes))
                .ok_or_else(|| ClientError::Other("reconstruction chunk overflow".to_owned()))?;
            if next > requested.start {
                break;
            }
            selected_chunk_file_start = next;
            selected_start += 1;
        }

        let mut selected_end = selected_start;
        let mut selected_size = 0_u64;
        let mut selected_chunk_file_end = selected_chunk_file_start;
        while selected_end < chunks.len() && selected_chunk_file_end < requested.end {
            let size = u64::from(chunks[selected_end].unpacked_segment_bytes);
            selected_size = selected_size
                .checked_add(size)
                .ok_or_else(|| ClientError::Other("reconstruction chunk overflow".to_owned()))?;
            selected_chunk_file_end = selected_chunk_file_end
                .checked_add(size)
                .ok_or_else(|| ClientError::Other("reconstruction chunk overflow".to_owned()))?;
            selected_end += 1;
        }
        if selected_start == selected_end {
            segment_start = segment_end;
            continue;
        }

        if offset_into_first_range.is_none() {
            offset_into_first_range = Some(requested.start - selected_chunk_file_start);
        }
        let selected_start = segment
            .chunk_index_start
            .checked_add(
                u32::try_from(selected_start).map_err(|_| {
                    ClientError::Other("selected xorb chunk index overflow".to_owned())
                })?,
            )
            .ok_or_else(|| ClientError::Other("selected xorb chunk index overflow".to_owned()))?;
        let selected_end = segment
            .chunk_index_start
            .checked_add(
                u32::try_from(selected_end).map_err(|_| {
                    ClientError::Other("selected xorb chunk index overflow".to_owned())
                })?,
            )
            .ok_or_else(|| ClientError::Other("selected xorb chunk index overflow".to_owned()))?;
        let chunks = ChunkRange::new(selected_start, selected_end);
        let unpacked_length = u32::try_from(selected_size)
            .map_err(|_| ClientError::Other("selected xorb range exceeds u32".to_owned()))?;
        terms.push(XorbReconstructionTerm {
            hash: HexMerkleHash::from(segment.xorb_hash),
            unpacked_length,
            range: chunks,
        });
        xorbs
            .entry(HexMerkleHash::from(segment.xorb_hash))
            .or_default()
            .push(XorbMultiRangeFetch {
                url: xorb_url(&segment.xorb_hash, std::slice::from_ref(&chunks)),
                ranges: vec![XorbRangeDescriptor {
                    chunks,
                    // This adapter encodes chunk addressing in its private URL;
                    // Xet still requires a well-formed inclusive HTTP range.
                    bytes: HttpRange::new(0, selected_size - 1),
                }],
            });
        segment_start = segment_end;
    }

    Ok(Some(QueryReconstructionResponseV2 {
        offset_into_first_range: offset_into_first_range.unwrap_or(0),
        terms,
        xorbs,
    }))
}

#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
impl Client for StoreClient {
    async fn get_file_reconstruction_info(
        &self,
        file_hash: &MerkleHash,
    ) -> ClientResult<Option<(MDBFileInfo, Option<MerkleHash>)>> {
        let Some((shard, shard_hash)) = self.load_shard_for_file(file_hash).await? else {
            return Ok(None);
        };

        let Some(info) = shard
            .get_file_info(file_hash)
            .map_err(|e| self.map_read_error(e.into()))?
        else {
            warn!(
                file_hash = %file_hash.hex(),
                shard_hash = %shard_hash.hex(),
                "file-index points at shard that does not contain the file"
            );
            return Ok(None);
        };
        Ok(Some((info, Some(shard_hash))))
    }

    async fn get_reconstruction(
        &self,
        file_id: &MerkleHash,
        bytes_range: Option<FileRange>,
    ) -> ClientResult<Option<QueryReconstructionResponseV2>> {
        let Some((shard, shard_hash)) = self.load_shard_for_file(file_id).await? else {
            return Err(self.map_read_error(ReadError::NotFound {
                path: format!("file_index:{}", file_id.hex()),
            }));
        };

        let Some(file_info) = shard
            .get_file_info(file_id)
            .map_err(|e| self.map_read_error(e.into()))?
        else {
            return Err(self.map_read_error(ReadError::CorruptObject {
                path: self.router.shard_path(&shard_hash).to_string(),
                reason: format!("shard does not contain indexed file {}", file_id.hex()),
            }));
        };

        match bytes_range {
            Some(range) => build_range_response_v2(&file_info, &shard, range),
            None => Ok(Some(build_response_v2(&file_info))),
        }
    }

    async fn batch_get_reconstruction(
        &self,
        file_ids: &[MerkleHash],
    ) -> ClientResult<BatchQueryReconstructionResponse> {
        let mut files = HashMap::new();
        let mut fetch_info: HashMap<HexMerkleHash, Vec<XorbReconstructionFetchInfo>> =
            HashMap::new();
        if file_ids.is_empty() {
            return Ok(BatchQueryReconstructionResponse { files, fetch_info });
        }

        let shard_hashes = self
            .resolve_file_indexes_batch(file_ids)
            .await
            .map_err(|error| self.map_read_error(error))?;
        let mut files_by_shard: HashMap<MerkleHash, Vec<MerkleHash>> = HashMap::new();
        for (file_id, shard_hash) in file_ids.iter().zip(shard_hashes) {
            if let Some(shard_hash) = shard_hash {
                files_by_shard.entry(shard_hash).or_default().push(*file_id);
            }
        }

        for (shard_hash, shard_file_ids) in files_by_shard {
            let shard = match self.load_shard(&shard_hash).await {
                Ok(shard) => shard,
                Err(ReadError::NotFound { .. }) => continue,
                Err(e) => return Err(self.map_read_error(e)),
            };

            for file_id in shard_file_ids {
                let Some(file_info) = shard
                    .get_file_info(&file_id)
                    .map_err(|e| self.map_read_error(e.into()))?
                else {
                    warn!(
                        file_hash = %file_id.hex(),
                        shard_hash = %shard_hash.hex(),
                        "file-index points at shard that does not contain the file"
                    );
                    continue;
                };
                let response = build_response_v2(&file_info);
                files.insert(HexMerkleHash::from(file_id), response.terms);
                for (xorb_hash, fetches) in response.xorbs {
                    let entries = fetch_info.entry(xorb_hash).or_default();
                    for fetch in fetches {
                        for range in fetch.ranges {
                            entries.push(XorbReconstructionFetchInfo {
                                range: range.chunks,
                                url: fetch.url.clone(),
                                url_range: range.bytes,
                            });
                        }
                    }
                }
            }
        }

        Ok(BatchQueryReconstructionResponse { files, fetch_info })
    }

    async fn acquire_download_permit(&self) -> ClientResult<ConnectionPermit> {
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(self.map_read_error(ReadError::Cancelled)),
            result = self.concurrency.acquire_connection_permit() => {
                result.map_err(|error| self.record_error(error))
            }
        }
    }

    async fn get_file_term_data(
        &self,
        url_info: Box<dyn URLProvider>,
        _download_permit: ConnectionPermit,
        _progress_callback: Option<ProgressCallback>,
        _uncompressed_size_if_known: Option<usize>,
    ) -> ClientResult<(Bytes, Vec<u32>)> {
        // Xet's final writer join waits for its source futures. Cancel the
        // read at this adapter boundary so a pending source can release that
        // join instead of retaining the destination after caller cancellation.
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(self.map_read_error(ReadError::Cancelled)),
            result = async {
        let (url, _http_ranges) = url_info
            .retrieve_url()
            .await
            .map_err(|error| self.record_error(error))?;
        let (xorb_hash, chunk_ranges) =
            parse_xorb_url(&url).map_err(|error| self.record_error(error))?;

        if let Some(cache) = &self.chunk_cache
            && let Some(cached) = cache.read(xorb_hash, &chunk_ranges).await {
            return Ok(cached);
        }
        let xorb_path = self.router.xorb_path(&xorb_hash);
        if let Some(availability) = &self.availability {
            availability
                .ensure_available(&xorb_path)
                .await
                .map_err(|error| self.map_read_error(error))?;
        }
        let ranges = chunk_ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>();
        let decoded = if self.selective_xorb_reads {
            self.store
                .get_xorb_chunks(&xorb_path, &xorb_hash, &ranges)
                .await
        } else {
            self.store
                .get_xorb_chunks_without_install(&xorb_path, &xorb_hash, &ranges)
                .await
        }
            .map_err(ReadError::from)
            .map_err(|error| self.map_read_error(error))?;
        // Keep cache publication within the download and decoded-buffer admission.
        // Returning first would let a detached put retain bytes after permit release.
        if let Some(cache) = &self.chunk_cache {
            cache.write(xorb_hash, &chunk_ranges, &decoded.0, &decoded.1).await;
        }
        Ok(decoded)
            } => result,
        }
    }

    async fn query_for_global_dedup_shard(
        &self,
        _prefix: &str,
        _chunk_hash: &MerkleHash,
    ) -> ClientResult<Option<Bytes>> {
        Ok(None)
    }

    async fn acquire_upload_permit(&self) -> ClientResult<ConnectionPermit> {
        Err(ClientError::Other(
            "StoreClient is read-only; uploads go through the crab push pipeline".into(),
        ))
    }

    async fn upload_shard(
        &self,
        _shard_data: Bytes,
        _upload_permit: ConnectionPermit,
        _progress_callback: Option<ShardUploadProgressCallback>,
    ) -> ClientResult<()> {
        Err(ClientError::Other(
            "StoreClient is read-only; shard uploads go through the crab push pipeline".into(),
        ))
    }

    async fn get_file_chunk_hashes(
        &self,
        _file_id: &MerkleHash,
        _dirty_ranges: Vec<FileRange>,
    ) -> ClientResult<FileChunkHashesResponse> {
        Err(ClientError::Other(
            "StoreClient is read-only; file chunk hash queries go through the crab push pipeline"
                .into(),
        ))
    }

    async fn upload_xorb(
        &self,
        _prefix: &str,
        _serialized_xorb_object: SerializedXorbObject,
        _progress_callback: Option<ProgressCallback>,
        _upload_permit: ConnectionPermit,
    ) -> ClientResult<u64> {
        Err(ClientError::Other(
            "StoreClient is read-only; xorb uploads go through the crab push pipeline".into(),
        ))
    }
}

#[cfg(test)]
mod tests;
