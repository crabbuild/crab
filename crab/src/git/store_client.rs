//! Adapter that implements xet-core's `Client` trait over crab's
//! object-store-backed `Store` and `StoreLayout`.
//!
//! This is the integration seam between crab's storage layer and
//! xet-core's `FileReconstructor`. The reconstructor drives the read
//! path by calling:
//!
//! - [`Client::get_file_reconstruction_info`] / [`Client::get_reconstruction`] to
//!   resolve a file hash to reconstruction terms (file-index → shard → [`MDBFileInfo`]).
//! - [`Client::get_file_term_data`] to fetch and decompress a xorb range.
//! - [`Client::acquire_download_permit`] to throttle concurrent downloads.
//!
//! Upload paths are not implemented — the push pipeline goes through
//! the existing crab code rather than this adapter.
//!
//! # URL encoding
//!
//! xet-core's response type threads download URLs through [`URLProvider`];
//! we encode xorb references as opaque `crab-xorb://{hash_hex}` strings.
//! `get_file_term_data` parses this back to a `MerkleHash`, fetches the
//! xorb via the object store, and decompresses the requested chunk
//! ranges using the crab [`XorbParser`]. The URL format is private to
//! this adapter; xet-core never inspects it.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
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

use crate::cache::CacheKey;
use crate::core::error::CrabError;
use crate::core::metrics::Metrics;
use crab_cache_store::CachingStore;
use crab_metadata::file_index_lookup::{FileIndexLookupSession, SharedFileIndexLookup};
use crab_xet::shard::ShardReader;
type StoreLayout = crab_storage::StoreLayout<crab_storage::Store>;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::MDBFileInfo;
use crab_xet::xorb::format::SerializedXorbObject;

pub(crate) type SharedShardHints = Arc<RwLock<HashMap<MerkleHash, MerkleHash>>>;

/// URL scheme prefix used to encode xorb references in reconstruction
/// responses. Opaque to xet-core; parsed back by [`StoreClient::get_file_term_data`].
///
/// Full format: `crab-xorb://{hex}?chunks={s1}-{e1},{s2}-{e2},...`
/// Chunk ranges are half-open `[start, end)` intervals, matching
/// xet-core's [`ChunkRange`]. Multi-range terms concatenate ranges
/// comma-separated.
const XORB_URL_PREFIX: &str = "crab-xorb://";

/// Implements xet-core's [`Client`] trait over a crab [`CachingStore`] +
/// [`StoreLayout`] pair.
///
/// Cheap to clone-via-[`Arc`]: the store is already arc-backed internally
/// and the layout is a thin wrapper over a small set of strings.
pub struct StoreClient {
    store: CachingStore,
    router: StoreLayout,
    concurrency: Arc<AdaptiveConcurrencyController>,
    file_index_lookup: Option<SharedFileIndexLookup>,
    shard_hints: SharedShardHints,
    metrics: Option<Arc<Metrics>>,
}

impl StoreClient {
    /// Construct a new adapter.
    ///
    /// `concurrency` is the shared download-permit source. Hydrate,
    /// prefetch, and filter-process smudge all bind the same controller
    /// so the total in-flight download count is globally bounded.
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
        }
    }

    /// Reuse a caller-owned file-index reader across this adapter's
    /// reconstruction requests.
    #[must_use]
    pub fn with_file_index_lookup(mut self, lookup: SharedFileIndexLookup) -> Self {
        self.file_index_lookup = Some(lookup);
        self
    }

    /// Attach shared perf counters for shard-hint hit/miss accounting.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Use pointer-carried shard hints before opening the file-index DB.
    ///
    /// Hints are advisory: a missing, stale, or unreadable hinted shard falls
    /// through to the normal file-index lookup.
    #[must_use]
    pub fn with_shard_hint(self, file_hash: MerkleHash, shard_hash: MerkleHash) -> Self {
        self.insert_shard_hint(file_hash, shard_hash);
        self
    }

    /// Return the shared shard-hint map used by this adapter.
    ///
    /// The delayed-smudge prefetch queue inserts per-pointer hints here
    /// before spawning reconstruction tasks, so the shared `StoreClient`
    /// can use the same fast path as inline hydrate.
    #[must_use]
    pub(crate) fn shared_shard_hints(&self) -> SharedShardHints {
        Arc::clone(&self.shard_hints)
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

    /// Resolve a file hash to its owning shard reader via the file-index
    /// indirection. Used by both [`Client::get_file_reconstruction_info`]
    /// and [`Client::get_reconstruction`] to produce reconstruction terms.
    ///
    /// Returns `Ok(None)` when either the file-index entry or the shard
    /// it points at is missing — the canonical "not found" shape for
    /// xet-core's Client trait. Everything else surfaces as a
    /// [`ClientError`].
    ///
    /// Applies the bloom pre-filter when the shard is not yet in the
    /// local cache: a small Range-GET on the shard's v2 bloom trailer
    /// can prove the file is absent before we pull the whole shard body.
    /// A definitive-absent result is surfaced as `Ok(None)` so the
    /// caller reports "file not found" without a costly full download.
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
                        "store_client: shard-hint hit"
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.inc_shard_hint_hits();
                    }
                    return Ok(Some((shard, hint_hash)));
                }
                Ok(_) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %hint_hash.hex(),
                        "store_client: shard-hint stale, falling back to file-index"
                    );
                }
                Err(e) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %hint_hash.hex(),
                        error = %e,
                        "store_client: shard-hint fetch failed, falling back to file-index"
                    );
                }
            }
            if let Some(metrics) = &self.metrics {
                metrics.inc_shard_hint_misses();
            }
        }

        let shard_hash = match self.resolve_file_index(file_hash).await {
            Ok(h) => h,
            Err(CrabError::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(map_crab_error(e)),
        };

        // Bloom pre-filter: skip the full shard download when the bloom
        // proves the file is absent. Only meaningful for uncached shards
        // — a local cache hit is already faster than any Range-GET.
        // With a cache service configured, keep immutable shard reads behind
        // that boundary; bloom probes against origin would bypass the service.
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
                    // The file-index said this shard owns `file_hash`,
                    // but the bloom proves otherwise. Either the index
                    // is stale or the shard has been rewritten. Either
                    // way, skip the full-body fetch and report the file
                    // as missing.
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %shard_hash.hex(),
                        "bloom pre-filter: file absent from shard, skipping download"
                    );
                    return Ok(None);
                }
                Ok(_) => {
                    // PossiblyPresent or NoBloom — fall through to a
                    // full download.
                }
                Err(e) => {
                    // Bloom check failures are advisory; log and fall
                    // back to the full download.
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
            Err(CrabError::NotFound { .. }) => Ok(None),
            Err(e) => Err(map_crab_error(e)),
        }
    }

    /// Fetch the shard hash for a file via the per-repo `file_index_db`.
    ///
    /// Uses the caller-provided shared lookup session when present;
    /// otherwise falls back to the one-shot compatibility helper.
    ///
    /// A miss (no file-index entry for `file_hash`) maps to
    /// [`CrabError::NotFound`] so `load_shard_for_file` can surface
    /// the canonical "file not found" shape `Ok(None)` to xet-core.
    async fn resolve_file_index(
        &self,
        file_hash: &MerkleHash,
    ) -> crate::core::error::Result<MerkleHash> {
        let hit = match &self.file_index_lookup {
            Some(lookup) => lookup.lookup(file_hash).await?,
            None => {
                let session = FileIndexLookupSession::open_for_storage(
                    self.store.origin(),
                    self.router.repo_prefix(),
                )
                .await?;
                let result = session.lookup(file_hash).await;
                if let Err(close_error) = session.close().await {
                    warn!(error = %close_error, "store_client: file-index lookup close failed");
                }
                result?
            }
        };

        match hit {
            Some(shard_hash) => Ok(shard_hash),
            None => Err(CrabError::NotFound {
                path: format!("file_index:{}", file_hash.hex()),
            }),
        }
    }

    /// Fetch shard hashes for a batch of files through one read-only
    /// file-index session. Results are aligned with `file_hashes`.
    async fn resolve_file_indexes_batch(
        &self,
        file_hashes: &[MerkleHash],
    ) -> crate::core::error::Result<Vec<Option<MerkleHash>>> {
        if let Some(lookup) = &self.file_index_lookup {
            return lookup
                .lookup_batch(file_hashes)
                .await
                .map_err(CrabError::from);
        }

        let session = FileIndexLookupSession::open_for_storage(
            self.store.origin(),
            self.router.repo_prefix(),
        )
        .await?;

        let result = session.lookup_batch(file_hashes).await;
        if let Err(close_err) = session.close().await {
            warn!(
                err = %close_err,
                "store_client: file-index lookup session close failed after batch read"
            );
        }
        result.map_err(CrabError::from)
    }

    /// Fetch a shard via the `LocalCache`, falling back to the object
    /// store on miss. Hash verification happens inside `LocalCache`.
    async fn load_shard(&self, shard_hash: &MerkleHash) -> crate::core::error::Result<ShardReader> {
        let key = CacheKey::Shard(*shard_hash);
        let origin = self.store.origin().clone();
        let path = self.router.shard_path(shard_hash);
        let hash = *shard_hash;

        let data = self
            .store
            .local_cache()
            .get_or_fetch_with(&key, || {
                let origin = origin;
                let path = path;
                async move {
                    debug!(shard_hash = %hash.hex(), "store_client: downloading shard");
                    let (data, _) = origin.get_with_etag(&path).await?;
                    Ok::<_, CrabError>(data)
                }
            })
            .await?;

        Ok(ShardReader::from_bytes(data, *shard_hash))
    }
}

/// Encode a xorb reference as an opaque URL string.
///
/// The full format is `crab-xorb://{hex}?chunks={s}-{e},{s}-{e},...`.
/// `chunks` is a comma-separated list of half-open `[start, end)`
/// intervals; [`parse_xorb_url`] reconstructs them on the other side.
fn xorb_url(hash: &MerkleHash, chunks: &[ChunkRange]) -> String {
    let mut url = format!("{XORB_URL_PREFIX}{}", hash.hex());
    if !chunks.is_empty() {
        url.push_str("?chunks=");
        for (i, range) in chunks.iter().enumerate() {
            if i > 0 {
                url.push(',');
            }
            // ChunkRange's Display writes "{start}-{end}".
            url.push_str(&range.to_string());
        }
    }
    url
}

/// Parse a URL produced by [`xorb_url`] back into its xorb hash and
/// chunk-range list.
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

/// Map a crab error into xet-core's `ClientError`. `NotFound` is
/// surfaced as `FileNotFound` / `XORBNotFound` when we can tell which
/// kind of object was missing; everything else funnels into
/// `ClientError::Other` with the stable `CRAB-E####` code preserved
/// in the message for downstream debugging.
fn map_crab_error(e: CrabError) -> ClientError {
    match e {
        CrabError::Cancelled => ClientError::Other("cancelled".to_string()),
        other => ClientError::Other(other.to_string()),
    }
}

fn map_xet_error(e: crab_xet::error::XetError) -> ClientError {
    map_crab_error(CrabError::from(e))
}

/// Build a [`QueryReconstructionResponseV2`] from a shard's `MDBFileInfo`,
/// optionally trimmed to a requested byte range.
///
/// Each `FileDataSequenceEntry` becomes one [`XorbReconstructionTerm`] and
/// one entry in the `xorbs` map. The URL in each entry is the opaque
/// `crab-xorb://` form; the xet-core reconstructor will hand it back
/// to us via [`URLProvider::retrieve_url`] and we'll parse it in
/// [`Client::get_file_term_data`].
///
/// When `byte_range` is `Some`, segments entirely before the range are
/// dropped and `offset_into_first_range` is set so the first returned
/// segment aligns with the requested start. Returns `None` when the
/// range starts at or past EOF — this matches xet-core's contract
/// (`FileReconstructor` uses `None` as the end-of-file signal that
/// stops prefetching).
///
/// The returned terms preserve full-segment granularity because we
/// don't know the byte size of individual chunks without parsing the
/// xorb. The manager layer trims the last segment's byte_range at the
/// upper bound, so returning whole segments is safe.
fn build_response_v2(
    file_info: &MDBFileInfo,
    byte_range: Option<FileRange>,
) -> Option<QueryReconstructionResponseV2> {
    // Build a (start_offset, end_offset) for each segment in the file.
    let mut cumulative: u64 = 0;
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(file_info.segments.len());
    for seg in &file_info.segments {
        let size = u64::from(seg.unpacked_segment_bytes);
        spans.push((cumulative, cumulative + size));
        cumulative += size;
    }
    let file_size = cumulative;

    // Compute which segments overlap the requested range, and by how much
    // the first one is offset past its natural start.
    let (first_idx, last_idx, offset_into_first_range) = match byte_range {
        Some(range) => {
            // Range entirely past EOF → xet-core expects Ok(None) so the
            // prefetch manager can terminate cleanly.
            if range.start >= file_size {
                return None;
            }

            let first = spans.iter().position(|(_, end)| *end > range.start)?;
            let last = spans
                .iter()
                .rposition(|(start, _)| *start < range.end.min(file_size))
                .unwrap_or(first);

            let offset = range.start.saturating_sub(spans[first].0);
            (first, last, offset)
        }
        None => {
            if file_info.segments.is_empty() {
                (0, 0, 0)
            } else {
                (0, file_info.segments.len() - 1, 0)
            }
        }
    };

    if file_info.segments.is_empty() {
        return Some(QueryReconstructionResponseV2 {
            offset_into_first_range: 0,
            terms: Vec::new(),
            xorbs: HashMap::new(),
        });
    }

    let selected = &file_info.segments[first_idx..=last_idx];
    let mut terms = Vec::with_capacity(selected.len());
    let mut xorbs: HashMap<HexMerkleHash, Vec<XorbMultiRangeFetch>> = HashMap::new();

    for seg in selected {
        let chunks = ChunkRange::new(seg.chunk_index_start, seg.chunk_index_end);
        terms.push(XorbReconstructionTerm {
            hash: HexMerkleHash::from(seg.xorb_hash),
            unpacked_length: seg.unpacked_segment_bytes,
            range: chunks,
        });

        // Byte range is unknown without parsing the xorb; advertise a
        // conservative upper bound of `unpacked_segment_bytes` so the
        // transfer-progress accumulator doesn't wrap. Using `u64::MAX`
        // here used to propagate through `.length()` sums and blew up
        // downstream arithmetic.
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

    Some(QueryReconstructionResponseV2 {
        offset_into_first_range,
        terms,
        xorbs,
    })
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

        let Some(info) = shard.get_file_info(file_hash).map_err(map_xet_error)? else {
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
        // xet-core's prefetch manager treats `Ok(None)` here as "the
        // requested byte range is past EOF" — it stops the prefetch
        // loop silently and the reconstruction completes with 0 bytes.
        // If we returned `Ok(None)` for a genuinely missing shard or
        // file-index entry, the user would see a successful exit code
        // with an empty file on disk. Surface those conditions as
        // errors instead so the CLI reports a clear failure.
        let Some((shard, shard_hash)) = self.load_shard_for_file(file_id).await? else {
            return Err(ClientError::Other(format!(
                "cannot reconstruct file {}: shard not found (file-index \
                 entry missing or shard body unreachable)",
                file_id.hex(),
            )));
        };

        let Some(file_info) = shard.get_file_info(file_id).map_err(map_xet_error)? else {
            return Err(ClientError::Other(format!(
                "cannot reconstruct file {}: shard {} does not contain an \
                 entry for the requested file (stale file-index or shard \
                 rewrite)",
                file_id.hex(),
                shard_hash.hex(),
            )));
        };

        // Trim to the requested byte range. When the range starts past
        // EOF, `build_response_v2` returns `None`, which xet-core's
        // prefetch manager treats as the "no more file" signal. This
        // is the only valid `Ok(None)` path for this method.
        Ok(build_response_v2(&file_info, bytes_range))
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
            .map_err(map_crab_error)?;
        let mut files_by_shard: HashMap<MerkleHash, Vec<MerkleHash>> = HashMap::new();
        for (file_id, shard_hash) in file_ids.iter().zip(shard_hashes.into_iter()) {
            if let Some(shard_hash) = shard_hash {
                files_by_shard.entry(shard_hash).or_default().push(*file_id);
            }
        }

        for (shard_hash, shard_file_ids) in files_by_shard {
            let shard = match self.load_shard(&shard_hash).await {
                Ok(shard) => shard,
                Err(CrabError::NotFound { .. }) => continue,
                Err(e) => return Err(map_crab_error(e)),
            };

            for file_id in shard_file_ids {
                let Some(file_info) = shard.get_file_info(&file_id).map_err(map_xet_error)? else {
                    warn!(
                        file_hash = %file_id.hex(),
                        shard_hash = %shard_hash.hex(),
                        "file-index points at shard that does not contain the file"
                    );
                    continue;
                };
                let Some(response) = build_response_v2(&file_info, None) else {
                    continue;
                };
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
        self.concurrency.acquire_connection_permit().await
    }

    async fn get_file_term_data(
        &self,
        url_info: Box<dyn URLProvider>,
        _download_permit: ConnectionPermit,
        _progress_callback: Option<ProgressCallback>,
        _uncompressed_size_if_known: Option<usize>,
    ) -> ClientResult<(Bytes, Vec<u32>)> {
        let (url, _http_ranges) = url_info.retrieve_url().await?;
        let (xorb_hash, chunk_ranges) = parse_xorb_url(&url)?;
        let xorb_path = self.router.xorb_path(&xorb_hash);
        let ranges = chunk_ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>();
        self.store
            .get_xorb_chunks_without_install(&xorb_path, &xorb_hash, &ranges)
            .await
            .map_err(CrabError::from)
            .map_err(map_crab_error)
    }

    async fn query_for_global_dedup_shard(
        &self,
        _prefix: &str,
        _chunk_hash: &MerkleHash,
    ) -> ClientResult<Option<Bytes>> {
        // Global dedup query is a push-path feature; the read-path
        // adapter never needs it.
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::sync::Arc;

    use crab_xet::hash::MerkleHash;
    use crab_xet::shard::{
        FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
        XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    };
    use object_store::memory::InMemory;

    use super::*;
    use crate::core::config::CacheConfig;
    use crate::metadata::metadb::{MetaDb, MetaDbConfig, MetaDbGuard};
    use crate::storage::store::Store;
    use crab_cache_store::CachingStore;
    use crab_xet::shard::ShardWriter;

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([
            seed,
            seed.wrapping_mul(31),
            seed.wrapping_mul(97),
            seed.wrapping_mul(127),
        ])
    }

    fn test_client() -> (StoreClient, tempfile::TempDir) {
        let inner = Arc::new(InMemory::new());
        let origin = Store::new(inner);
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let local_cache = Arc::new(crate::cache::LocalCache::new(
            cache_dir.path().to_path_buf(),
        ));
        let caching =
            CachingStore::new_with_local_cache(origin, &CacheConfig::default(), local_cache)
                .expect("CachingStore builds with default config");
        let router = StoreLayout::new(caching.origin().clone(), "org/test".to_string());

        let concurrency = AdaptiveConcurrencyController::new_download(
            xet_runtime::core::XetContext::default().expect("xet context"),
            "crab-hydrate-test",
        );
        let client = StoreClient::new(caching, router, concurrency);
        (client, cache_dir)
    }

    #[test]
    fn store_client_uses_caching_store_local_cache() {
        let origin = Store::new(Arc::new(InMemory::new()));
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let local_cache = Arc::new(crate::cache::LocalCache::new(
            cache_dir.path().to_path_buf(),
        ));
        let caching =
            CachingStore::new_with_local_cache(origin, &CacheConfig::default(), local_cache)
                .expect("caching store");
        let expected = Arc::clone(caching.local_cache());
        let router = StoreLayout::new(caching.origin().clone(), "org/cache-owner".to_owned());
        let concurrency = AdaptiveConcurrencyController::new_download(
            xet_runtime::core::XetContext::default().expect("xet context"),
            "crab-cache-owner-test",
        );

        let client = StoreClient::new(caching, router, concurrency);

        assert!(Arc::ptr_eq(&expected, client.store.local_cache()));
    }

    async fn seed_file_index(client: &StoreClient, entries: &[(MerkleHash, MerkleHash)]) {
        let shard_hashes = entries
            .iter()
            .map(|(_, shard_hash)| shard_hash.hex())
            .collect::<Vec<_>>();
        let (shard_index_hash, _, shard_write) = crab_metadata::manifests::append_shard_index(
            crab_metadata::segmented::SegmentIndex::default(),
            1,
            &shard_hashes,
        )
        .expect("build shard index");
        crab_metadata::manifest_store::upload_segmented_bulk(
            client.store.origin(),
            &client.router,
            &crab_metadata::manifests::BulkData {
                shard_index: shard_write,
                pack_index: crab_metadata::segmented::SegmentWrite::default(),
            },
        )
        .await
        .expect("upload shard index");
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.shard_index_hash = shard_index_hash.clone();
        manifest.seal_git_validation();
        crab_metadata::manifest_store::create_manifest(
            client.store.origin(),
            &client.router,
            &manifest,
        )
        .await
        .expect("create manifest");

        let metadb = MetaDb::new(
            Arc::clone(client.store.origin().inner()),
            client.router.repo_prefix().to_owned(),
            MetaDbConfig::for_repo(client.router.repo_prefix()),
        );
        let guard = MetaDbGuard::new(metadb);
        let file_store = guard.file_index().await.expect("file_index");
        let mut txn = guard.new_transaction().expect("transaction");
        let shard_index_hash =
            MerkleHash::from_hex(&shard_index_hash).expect("valid shard-index hash");
        let committed = entries
            .iter()
            .map(|(file_hash, shard_hash)| {
                (
                    *file_hash,
                    crab_metadata::value_codec::CommittedFileRecord {
                        recipe_hash: [0xC8; 32],
                        shard_hash: *shard_hash,
                        committed_generation: 1,
                        shard_index_hash,
                    },
                )
            })
            .collect::<Vec<_>>();
        file_store.save_committed_batch(&mut txn, &committed);
        guard.commit(txn).await.expect("commit file_index");
        guard.close().await.expect("close file_index seed");
    }

    #[test]
    fn xorb_url_round_trip_single_range() {
        let hash = hash_from_seed(42);
        let range = ChunkRange::new(3, 10);
        let url = xorb_url(&hash, &[range]);
        let (parsed_hash, parsed_ranges) = parse_xorb_url(&url).expect("parse");
        assert_eq!(parsed_hash, hash);
        assert_eq!(parsed_ranges, vec![range]);
    }

    #[test]
    fn xorb_url_round_trip_multi_range() {
        let hash = hash_from_seed(7);
        let ranges = vec![
            ChunkRange::new(0, 5),
            ChunkRange::new(8, 12),
            ChunkRange::new(20, 21),
        ];
        let url = xorb_url(&hash, &ranges);
        let (parsed_hash, parsed_ranges) = parse_xorb_url(&url).expect("parse");
        assert_eq!(parsed_hash, hash);
        assert_eq!(parsed_ranges, ranges);
    }

    #[test]
    fn xorb_url_rejects_foreign_prefix() {
        let err =
            parse_xorb_url("https://example.com/xorb").expect_err("should reject non-crab url");
        match err {
            ClientError::Other(msg) => assert!(msg.contains("unrecognized")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn xorb_url_rejects_bad_hash() {
        let err = parse_xorb_url("crab-xorb://not-hex?chunks=0-1")
            .expect_err("should reject non-hex hash");
        match err {
            ClientError::Other(msg) => assert!(msg.contains("invalid xorb url hash")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn build_response_v2_covers_every_segment() {
        let file_hash = hash_from_seed(1);
        let xorb_a = hash_from_seed(2);
        let xorb_b = hash_from_seed(3);
        let info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 2u32, false, false),
            segments: vec![
                FileDataSequenceEntry::new(xorb_a, 1024u32, 0u32, 4u32),
                FileDataSequenceEntry::new(xorb_b, 2048u32, 5u32, 13u32),
            ],
            verification: vec![],
            metadata_ext: None,
        };

        let response =
            build_response_v2(&info, None).expect("full-file reconstruction should be Some");
        assert_eq!(response.terms.len(), 2);
        assert_eq!(response.terms[0].range, ChunkRange::new(0, 4));
        assert_eq!(response.terms[0].unpacked_length, 1024);
        assert_eq!(response.terms[1].range, ChunkRange::new(5, 13));
        assert_eq!(response.xorbs.len(), 2);

        let fetches_a = &response.xorbs[&HexMerkleHash::from(xorb_a)];
        assert_eq!(fetches_a.len(), 1);
        let (parsed_hash, parsed_ranges) =
            parse_xorb_url(&fetches_a[0].url).expect("url encodes xorb ref");
        assert_eq!(parsed_hash, xorb_a);
        assert_eq!(parsed_ranges, vec![ChunkRange::new(0, 4)]);
    }

    #[test]
    fn build_response_v2_represents_zero_byte_file() {
        let file_hash = hash_from_seed(4);
        let info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 0u32, false, false),
            segments: vec![],
            verification: vec![],
            metadata_ext: None,
        };

        let response = build_response_v2(&info, None).expect("zero-byte file should reconstruct");
        assert!(response.terms.is_empty());
        assert!(response.xorbs.is_empty());
        assert_eq!(response.offset_into_first_range, 0);
    }

    #[tokio::test]
    async fn get_file_reconstruction_info_returns_none_for_unknown_file() {
        let (client, _tmp) = test_client();
        let unknown = hash_from_seed(999);
        let result = client
            .get_file_reconstruction_info(&unknown)
            .await
            .expect("not-found path returns Ok(None)");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_reconstruction_errors_for_unknown_file() {
        let (client, _tmp) = test_client();
        let unknown = hash_from_seed(1234);
        let err = client.get_reconstruction(&unknown, None).await.expect_err(
            "shard-missing must error out — returning Ok(None) would \
                 cause a silent 0-byte reconstruction",
        );
        match err {
            ClientError::Other(msg) => {
                assert!(
                    msg.contains("shard not found"),
                    "expected shard error, got {msg}"
                );
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shard_hint_resolves_without_file_index_db() {
        let (client, _tmp) = test_client();
        let file_hash = hash_from_seed(52);
        let xorb_hash = hash_from_seed(53);
        let file_info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
            segments: vec![FileDataSequenceEntry::new(xorb_hash, 4096u32, 0u32, 4u32)],
            verification: vec![],
            metadata_ext: None,
        };

        let mut shard = ShardWriter::new();
        shard.add_file(file_info).expect("add file");
        let (shard_bytes, shard_hash) = shard.finalize().expect("finalize shard");
        client
            .store
            .origin()
            .put(
                &client.router.shard_path(&shard_hash),
                Bytes::from(shard_bytes),
            )
            .await
            .expect("upload shard");

        let hinted = client.with_shard_hint(file_hash, shard_hash);
        let (info, resolved_shard) = hinted
            .get_file_reconstruction_info(&file_hash)
            .await
            .expect("hint lookup succeeds")
            .expect("hinted shard contains file");

        assert_eq!(resolved_shard, Some(shard_hash));
        assert_eq!(info.segments.len(), 1);
        assert_eq!(info.segments[0].xorb_hash, xorb_hash);
    }

    #[tokio::test]
    async fn shard_hint_hit_updates_metrics() {
        let (client, _tmp) = test_client();
        let metrics = Arc::new(Metrics::default());
        let file_hash = hash_from_seed(62);
        let xorb_hash = hash_from_seed(63);
        let file_info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
            segments: vec![FileDataSequenceEntry::new(xorb_hash, 4096u32, 0u32, 4u32)],
            verification: vec![],
            metadata_ext: None,
        };

        let mut shard = ShardWriter::new();
        shard.add_file(file_info).expect("add file");
        let (shard_bytes, shard_hash) = shard.finalize().expect("finalize shard");
        client
            .store
            .origin()
            .put(
                &client.router.shard_path(&shard_hash),
                Bytes::from(shard_bytes),
            )
            .await
            .expect("upload shard");

        let hinted = client
            .with_metrics(Arc::clone(&metrics))
            .with_shard_hint(file_hash, shard_hash);
        let _ = hinted
            .get_file_reconstruction_info(&file_hash)
            .await
            .expect("hint lookup succeeds")
            .expect("hinted shard contains file");

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.shard_hint_hits, 1);
        assert_eq!(snapshot.shard_hint_misses, 0);
    }

    #[tokio::test]
    async fn stale_shard_hint_updates_miss_metrics() {
        let (client, _tmp) = test_client();
        let metrics = Arc::new(Metrics::default());
        let file_hash = hash_from_seed(72);
        let missing_shard_hash = hash_from_seed(73);

        let hinted = client
            .with_metrics(Arc::clone(&metrics))
            .with_shard_hint(file_hash, missing_shard_hash);
        let result = hinted
            .get_file_reconstruction_info(&file_hash)
            .await
            .expect("stale hint falls through to canonical not found");
        assert!(result.is_none());

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.shard_hint_hits, 0);
        assert_eq!(snapshot.shard_hint_misses, 1);
    }

    #[tokio::test]
    async fn acquire_download_permit_succeeds() {
        let (client, _tmp) = test_client();
        let _permit = client
            .acquire_download_permit()
            .await
            .expect("download permit available");
    }

    #[tokio::test]
    async fn upload_paths_are_rejected() {
        let (client, _tmp) = test_client();

        let err = client
            .acquire_upload_permit()
            .await
            .err()
            .expect("acquire_upload_permit must report unsupported");
        assert!(matches!(err, ClientError::Other(_)));

        let permit = client.acquire_download_permit().await.unwrap();
        let err = client
            .upload_shard(Bytes::new(), permit, None)
            .await
            .expect_err("upload_shard must report unsupported");
        assert!(matches!(err, ClientError::Other(_)));
    }

    #[tokio::test]
    async fn batch_get_reconstruction_empty_is_empty() {
        let (client, _tmp) = test_client();
        let response = client
            .batch_get_reconstruction(&[])
            .await
            .expect("empty batch succeeds");
        assert!(response.files.is_empty());
        assert!(response.fetch_info.is_empty());
    }

    #[tokio::test]
    async fn batch_get_reconstruction_returns_hits_and_omits_misses() {
        let (client, _tmp) = test_client();
        let file_hash = hash_from_seed(42);
        let missing_hash = hash_from_seed(43);
        let xorb_hash = hash_from_seed(44);
        let file_info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
            segments: vec![FileDataSequenceEntry::new(xorb_hash, 2048u32, 2u32, 6u32)],
            verification: vec![],
            metadata_ext: None,
        };

        let mut shard = ShardWriter::new();
        let xorb_chunks = (0..6u64)
            .map(|index| {
                XorbChunkSequenceEntry::new(
                    hash_from_seed(100 + index),
                    512,
                    u32::try_from(index * 512).expect("test offset fits u32"),
                )
            })
            .collect::<Vec<_>>();
        shard
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb_hash, 6, 3072),
                chunks: xorb_chunks,
            }))
            .expect("add xorb metadata");
        shard.add_file(file_info).expect("add file");
        let (shard_bytes, shard_hash) = shard.finalize().expect("finalize shard");
        client
            .store
            .origin()
            .put(
                &client.router.shard_path(&shard_hash),
                Bytes::from(shard_bytes),
            )
            .await
            .expect("upload shard");
        seed_file_index(&client, &[(file_hash, shard_hash)]).await;

        let response = client
            .batch_get_reconstruction(&[file_hash, missing_hash])
            .await
            .expect("batch reconstruction");
        let file_key = HexMerkleHash::from(file_hash);
        let missing_key = HexMerkleHash::from(missing_hash);

        assert!(!response.files.contains_key(&missing_key));
        let terms = response.files.get(&file_key).expect("hit terms");
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].hash, HexMerkleHash::from(xorb_hash));
        assert_eq!(terms[0].unpacked_length, 2048);
        assert_eq!(terms[0].range, ChunkRange::new(2, 6));

        let fetches = response
            .fetch_info
            .get(&HexMerkleHash::from(xorb_hash))
            .expect("xorb fetch info");
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0].range, ChunkRange::new(2, 6));
        let (parsed_hash, parsed_ranges) = parse_xorb_url(&fetches[0].url).expect("fetch url");
        assert_eq!(parsed_hash, xorb_hash);
        assert_eq!(parsed_ranges, vec![ChunkRange::new(2, 6)]);
    }

    /// `get_file_term_data` must return `offsets.len() == num_chunks + 1`
    /// with `offsets[0] == 0` and `offsets[last] == data.len()`. The
    /// xet-core `DiskCache::put` validator rejects anything else as
    /// `InvalidArguments`, and every chunk-cache put during hydration
    /// then fails silently with a warning — exactly the bug this test
    /// guards against.
    #[tokio::test]
    async fn get_file_term_data_offsets_match_xet_core_contract() {
        use xet_client::cas_types::HttpRange;

        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        // Build a real xorb with 5 chunks so the offset Vec is non-trivial.
        let mut builder = XorbBuilder::new();
        for i in 0u32..5 {
            let size = 1024 + (i as usize) * 128;
            let data: Vec<u8> = (0..size as u32)
                .map(|j| (j.wrapping_mul(i.wrapping_mul(2654435761))) as u8)
                .collect();
            let chunk = Chunk::new(Bytes::from(data));
            builder.push(&chunk, RunId(0)).unwrap();
        }
        let xorbs = builder.finalize().unwrap();
        let xorb = xorbs.into_iter().next().expect("one xorb");

        // Stand up a fresh client and upload the xorb to the expected path.
        let inner = Arc::new(InMemory::new());
        let origin = Store::new(inner);
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let local_cache = Arc::new(crate::cache::LocalCache::new(
            cache_dir.path().to_path_buf(),
        ));
        let caching =
            CachingStore::new_with_local_cache(origin, &CacheConfig::default(), local_cache)
                .expect("CachingStore builds with default config");
        let router = StoreLayout::new(caching.origin().clone(), "org/test".to_string());
        let concurrency = AdaptiveConcurrencyController::new_fixed(
            xet_runtime::core::XetContext::default().expect("xet context"),
            "crab-term-test",
            1,
        );

        let xorb_path = router.xorb_path(&xorb.hash);
        caching
            .origin()
            .put(&xorb_path, xorb.bytes.clone())
            .await
            .expect("upload xorb");

        let client = StoreClient::new(caching, router, concurrency);

        // Single-range URL spanning every chunk in the xorb.
        let range = ChunkRange::new(0, 5);
        let url = xorb_url(&xorb.hash, std::slice::from_ref(&range));

        struct FixedURL {
            url: String,
        }

        #[async_trait::async_trait]
        impl URLProvider for FixedURL {
            async fn retrieve_url(&self) -> ClientResult<(String, Vec<HttpRange>)> {
                Ok((self.url.clone(), vec![]))
            }
            async fn refresh_url(&self) -> ClientResult<()> {
                Ok(())
            }
        }

        let permit = client
            .acquire_download_permit()
            .await
            .expect("download permit");
        let (data, offsets) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_file_term_data(Box::new(FixedURL { url }), permit, None, None),
        )
        .await
        .expect("cache-owned term fetch must not deadlock")
        .expect("fetch term data");

        // xet-core contract: one offset per chunk plus a trailing length.
        assert_eq!(offsets.len(), 6, "5 chunks + 1 trailing offset");
        assert_eq!(offsets[0], 0, "first offset must be 0");
        assert_eq!(
            offsets[5] as usize,
            data.len(),
            "last offset must equal data length",
        );

        // Offsets must be strictly increasing (DiskCache::put validator).
        for pair in offsets.windows(2) {
            assert!(pair[0] < pair[1], "offsets must be strictly increasing");
        }
    }

    #[tokio::test]
    async fn get_file_term_data_repairs_corrupt_local_xorb_cache_from_origin() {
        use xet_client::cas_types::HttpRange;

        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let chunks: Vec<Bytes> = vec![
            Bytes::from_static(b"first chunk payload"),
            Bytes::from_static(b"second chunk payload"),
        ];
        let mut builder = XorbBuilder::new();
        for chunk in &chunks {
            builder.push(&Chunk::new(chunk.clone()), RunId(0)).unwrap();
        }
        let xorb = builder.finalize().unwrap().pop().expect("one xorb");

        let inner = Arc::new(InMemory::new());
        let origin = Store::new(inner);
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let local_cache = Arc::new(crate::cache::LocalCache::new(
            cache_dir.path().to_path_buf(),
        ));
        let caching =
            CachingStore::new_with_local_cache(origin, &CacheConfig::default(), local_cache)
                .expect("CachingStore builds with default config");
        let store_cache = Arc::clone(caching.local_cache());
        let router = StoreLayout::new(caching.origin().clone(), "org/test".to_string());
        let concurrency = AdaptiveConcurrencyController::new_fixed(
            xet_runtime::core::XetContext::default().expect("xet context"),
            "crab-term-cache-repair-test",
            1,
        );

        let xorb_path = router.xorb_path(&xorb.hash);
        caching
            .origin()
            .put(&xorb_path, xorb.bytes.clone())
            .await
            .expect("upload xorb");
        store_cache
            .put_unchecked_for_test(&CacheKey::Xorb(xorb.hash), b"not a valid xorb")
            .await
            .expect("seed corrupt store xorb");

        let client = StoreClient::new(caching, router, concurrency);
        let range = ChunkRange::new(0, chunks.len() as u32);
        let url = xorb_url(&xorb.hash, std::slice::from_ref(&range));

        struct FixedURL {
            url: String,
        }

        #[async_trait::async_trait]
        impl URLProvider for FixedURL {
            async fn retrieve_url(&self) -> ClientResult<(String, Vec<HttpRange>)> {
                Ok((self.url.clone(), vec![]))
            }
            async fn refresh_url(&self) -> ClientResult<()> {
                Ok(())
            }
        }

        let permit = client
            .acquire_download_permit()
            .await
            .expect("download permit");
        let (data, offsets) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.get_file_term_data(Box::new(FixedURL { url }), permit, None, None),
        )
        .await
        .expect("cache repair must not deadlock")
        .expect("corrupt local xorb should be repaired from origin");

        let expected = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(data.as_ref(), expected.as_slice());
        assert_eq!(
            offsets,
            vec![0, chunks[0].len() as u32, expected.len() as u32]
        );

        let repaired = store_cache
            .get_or_fetch(&CacheKey::Xorb(xorb.hash), || async {
                panic!("repaired cache should be present")
            })
            .await
            .expect("read repaired xorb");
        assert_eq!(repaired, xorb.bytes);
    }
}
