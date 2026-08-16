//! Term resolution: file_hash → Vec<FileDataSequenceEntry>.
//!
//! Resolves file hashes to their reconstruction terms by following the
//! resolution chain: shard_hint → file-index GET → shard download →
//! `ShardReader::get_file_info()`. Shard downloads are deduplicated
//! within a batch and cached on disk via the unified `LocalCache`.

use std::collections::HashMap;
use std::sync::Arc;

use object_store::path::Path as ObjectPath;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, warn};

use crab_cache::{CacheKey, LocalCache};
use crab_cache_store::CachingStore;
use crab_diff::chunk_sequence::{ChunkOrigin, ChunkSequence, ChunkSpan};
use crab_diff::types::ChunkSequenceSourceKind;
use crab_metadata::file_index_lookup::FileIndexLookupSession;
use crab_storage::Store;
use crab_xet::hash::{MerkleHash, xorb_hash};
use crab_xet::shard::ShardReader;
use crab_xet::shard::{FileDataSequenceEntry, XorbChunkSequenceEntry};
use crab_xet::xorb::builder::{CHUNK_META_ENTRY_SIZE, FOOTER_SIZE, XORB_MAGIC};
use tokio_util::sync::CancellationToken;

use crate::{ReadError, ReadStoreLayout as StoreLayout};

type FileIndexLookupCell = tokio::sync::OnceCell<FileIndexLookupSession>;
type SharedFileIndexLookup = Arc<FileIndexLookupCell>;
type Result<T> = crate::Result<T>;

struct SequenceResolveContext<'a> {
    store: &'a CachingStore,
    router: &'a StoreLayout,
    cache: &'a LocalCache,
    shard_readers: &'a Mutex<HashMap<MerkleHash, Arc<ShardReader>>>,
    xorb_chunks: &'a Mutex<HashMap<MerkleHash, Arc<Vec<XorbChunkSequenceEntry>>>>,
    file_index_lookup: &'a FileIndexLookupCell,
    source: ChunkSequenceSourceKind,
    strict: bool,
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    Ok(())
}

/// Resolves file hashes to their reconstruction terms, batching shard
/// downloads and caching results.
pub struct TermResolver {
    store: CachingStore,
    router: StoreLayout,
    cache: Arc<LocalCache>,
    concurrency: usize,
}

impl TermResolver {
    /// Create a new resolver.
    ///
    /// `concurrency` controls the maximum number of concurrent metadata
    /// downloads (file-index lookups + shard fetches). Defaults to 8 in
    /// the diff pipeline.
    pub fn new(
        store: CachingStore,
        router: StoreLayout,
        cache: Arc<LocalCache>,
        concurrency: usize,
    ) -> Self {
        Self {
            store,
            router,
            cache,
            concurrency,
        }
    }

    /// Resolve a batch of file hashes to their reconstruction terms.
    ///
    /// `file_hashes` is a list of `(file_hash, optional_shard_hint)` pairs.
    /// When a shard hint is present, that shard is tried first before
    /// falling back to the file-index lookup.
    ///
    /// Each unique shard is downloaded at most once within the batch.
    /// Concurrent downloads are bounded by the configured concurrency limit.
    ///
    /// On per-file failure (missing file-index or shard), a `warn!` is
    /// logged and the file is omitted from the result map. The caller
    /// handles graceful degradation.
    pub async fn resolve_batch(
        &self,
        file_hashes: &[(MerkleHash, Option<MerkleHash>)],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<HashMap<MerkleHash, Vec<FileDataSequenceEntry>>> {
        if file_hashes.is_empty() {
            return Ok(HashMap::new());
        }

        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let shard_readers: Arc<Mutex<HashMap<MerkleHash, Arc<ShardReader>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let results: Arc<Mutex<HashMap<MerkleHash, Vec<FileDataSequenceEntry>>>> =
            Arc::new(Mutex::new(HashMap::with_capacity(file_hashes.len())));
        let file_index_lookup: SharedFileIndexLookup = Arc::new(FileIndexLookupCell::new());

        let mut handles = Vec::with_capacity(file_hashes.len());

        for &(file_hash, shard_hint) in file_hashes {
            check_cancelled(cancel)?;

            let semaphore = Arc::clone(&semaphore);
            let shard_readers = Arc::clone(&shard_readers);
            let results = Arc::clone(&results);
            let cancel = cancel.clone();
            let store = self.store.clone();
            let router = self.router.clone();
            let cache = Arc::clone(&self.cache);
            let file_index_lookup = Arc::clone(&file_index_lookup);

            let handle = tokio::spawn(async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .map_err(|_| ReadError::Cancelled)?;
                check_cancelled(&cancel)?;

                match resolve_single(
                    &store,
                    &router,
                    &cache,
                    &shard_readers,
                    &file_index_lookup,
                    file_hash,
                    shard_hint,
                )
                .await
                {
                    Ok(segments) => {
                        results.lock().await.insert(file_hash, segments);
                    }
                    Err(e) => {
                        // NotFound is expected for staged-but-unpushed files
                        // (file-index doesn't exist on the remote yet). Use
                        // debug! to avoid noisy warnings during `git diff --cached`.
                        if matches!(&e, ReadError::NotFound { .. }) {
                            debug!(
                                file_hash = %file_hash.hex(),
                                err = %e,
                                "failed to resolve reconstruction terms"
                            );
                        } else {
                            warn!(
                                file_hash = %file_hash.hex(),
                                err = %e,
                                "failed to resolve reconstruction terms"
                            );
                        }
                    }
                }
                Ok::<(), ReadError>(())
            });

            handles.push(handle);
        }

        // Await all tasks, propagating cancellation.
        for handle in handles {
            check_cancelled(cancel)?;
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(ReadError::Cancelled)) => return Err(ReadError::Cancelled),
                Ok(Err(e)) => {
                    warn!(err = %e, "unexpected error in term resolution task");
                }
                Err(e) => {
                    warn!(err = %e, "term resolution task panicked");
                }
            }
        }

        close_file_index_lookup(file_index_lookup).await;

        let map = match Arc::try_unwrap(results) {
            Ok(mutex) => mutex.into_inner(),
            Err(arc) => arc.lock().await.clone(),
        };
        Ok(map)
    }

    /// Resolve a batch of file hashes to ordered chunk sequences.
    ///
    /// Uses the same lookup and caching path as [`Self::resolve_batch`],
    /// then expands each file's reconstruction terms through xorb metadata
    /// so callers can compare actual chunk hashes.
    pub async fn resolve_sequences_batch(
        &self,
        files: &[(MerkleHash, Option<MerkleHash>, u64)],
        source: ChunkSequenceSourceKind,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<HashMap<MerkleHash, ChunkSequence>> {
        self.resolve_sequences_batch_with_mode(files, source, cancel, false)
            .await
    }

    /// Resolve a batch of file hashes to ordered chunk sequences.
    ///
    /// Unlike [`Self::resolve_sequences_batch`], this returns an error
    /// when any requested Crab pointer cannot be resolved. Use it for
    /// callers that must not degrade to pointer-text or git-native diffs.
    pub async fn resolve_sequences_batch_strict(
        &self,
        files: &[(MerkleHash, Option<MerkleHash>, u64)],
        source: ChunkSequenceSourceKind,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<HashMap<MerkleHash, ChunkSequence>> {
        self.resolve_sequences_batch_with_mode(files, source, cancel, true)
            .await
    }

    async fn resolve_sequences_batch_with_mode(
        &self,
        files: &[(MerkleHash, Option<MerkleHash>, u64)],
        source: ChunkSequenceSourceKind,
        cancel: &tokio_util::sync::CancellationToken,
        strict: bool,
    ) -> Result<HashMap<MerkleHash, ChunkSequence>> {
        if files.is_empty() {
            return Ok(HashMap::new());
        }

        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let shard_readers: Arc<Mutex<HashMap<MerkleHash, Arc<ShardReader>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let xorb_chunks: Arc<Mutex<HashMap<MerkleHash, Arc<Vec<XorbChunkSequenceEntry>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let results: Arc<Mutex<HashMap<MerkleHash, ChunkSequence>>> =
            Arc::new(Mutex::new(HashMap::with_capacity(files.len())));
        let file_index_lookup: SharedFileIndexLookup = Arc::new(FileIndexLookupCell::new());

        let mut handles = Vec::with_capacity(files.len());

        for &(file_hash, shard_hint, file_size) in files {
            check_cancelled(cancel)?;

            let semaphore = Arc::clone(&semaphore);
            let shard_readers = Arc::clone(&shard_readers);
            let xorb_chunks = Arc::clone(&xorb_chunks);
            let results = Arc::clone(&results);
            let cancel = cancel.clone();
            let store = self.store.clone();
            let router = self.router.clone();
            let cache = Arc::clone(&self.cache);
            let file_index_lookup = Arc::clone(&file_index_lookup);

            let handle = tokio::spawn(async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .map_err(|_| ReadError::Cancelled)?;
                check_cancelled(&cancel)?;

                let context = SequenceResolveContext {
                    store: &store,
                    router: &router,
                    cache: &cache,
                    shard_readers: &shard_readers,
                    xorb_chunks: &xorb_chunks,
                    file_index_lookup: &file_index_lookup,
                    source,
                    strict,
                };

                match resolve_sequence_single(&context, file_hash, shard_hint, file_size).await {
                    Ok(sequence) => {
                        results.lock().await.insert(file_hash, sequence);
                    }
                    Err(e) => {
                        if strict {
                            return Err(e);
                        }
                        if matches!(&e, ReadError::NotFound { .. }) {
                            debug!(
                                file_hash = %file_hash.hex(),
                                err = %e,
                                "failed to resolve chunk sequence"
                            );
                        } else {
                            warn!(
                                file_hash = %file_hash.hex(),
                                err = %e,
                                "failed to resolve chunk sequence"
                            );
                        }
                    }
                }
                Ok::<(), ReadError>(())
            });

            handles.push(handle);
        }

        let mut first_error = None;
        for handle in handles {
            check_cancelled(cancel)?;
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(ReadError::Cancelled)) => return Err(ReadError::Cancelled),
                Ok(Err(e)) => {
                    if strict {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                        continue;
                    }
                    warn!(err = %e, "unexpected error in chunk sequence resolution task");
                }
                Err(e) => {
                    if strict {
                        if first_error.is_none() {
                            first_error = Some(ReadError::internal(format!(
                                "chunk sequence resolution task panicked: {e}"
                            )));
                        }
                        continue;
                    }
                    warn!(err = %e, "chunk sequence resolution task panicked");
                }
            }
        }

        close_file_index_lookup(file_index_lookup).await;
        if let Some(error) = first_error {
            return Err(error);
        }

        let map = match Arc::try_unwrap(results) {
            Ok(mutex) => mutex.into_inner(),
            Err(arc) => arc.lock().await.clone(),
        };
        Ok(map)
    }
}

/// Resolve a single file hash to its reconstruction terms.
///
/// Resolution chain:
/// 1. If `shard_hint` is present, try loading that shard first.
/// 2. Otherwise, GET the file-index entry to find the shard hash.
/// 3. Download the shard (or hit cache / in-memory dedup map).
/// 4. Extract `MDBFileInfo.segments` via `ShardReader::get_file_info()`.
async fn resolve_single(
    store: &CachingStore,
    router: &StoreLayout,
    cache: &LocalCache,
    shard_readers: &Mutex<HashMap<MerkleHash, Arc<ShardReader>>>,
    file_index_lookup: &FileIndexLookupCell,
    file_hash: MerkleHash,
    shard_hint: Option<MerkleHash>,
) -> Result<Vec<FileDataSequenceEntry>> {
    // Try shard hint first if available.
    if let Some(hint_hash) = shard_hint {
        match try_shard(cache, store, router, shard_readers, &hint_hash, &file_hash).await {
            Ok(segments) => return Ok(segments),
            Err(e) => {
                debug!(
                    file_hash = %file_hash.hex(),
                    shard_hint = %hint_hash.hex(),
                    err = %e,
                    "shard hint miss, falling back to file-index"
                );
            }
        }
    }

    // Resolve shard hash via file-index.
    let shard_hash = resolve_file_index(file_index_lookup, store, router, &file_hash).await?;

    // Download shard and extract file info.
    try_shard(cache, store, router, shard_readers, &shard_hash, &file_hash).await
}

async fn resolve_sequence_single(
    context: &SequenceResolveContext<'_>,
    file_hash: MerkleHash,
    shard_hint: Option<MerkleHash>,
    file_size: u64,
) -> Result<ChunkSequence> {
    if let Some(hint_hash) = shard_hint {
        match try_sequence_shard(context, &hint_hash, &file_hash, file_size).await {
            Ok(sequence) => return Ok(sequence),
            Err(e) => {
                if context.strict && !sequence_hint_error_allows_fallback(&e) {
                    return Err(e);
                }
                debug!(
                    file_hash = %file_hash.hex(),
                    shard_hint = %hint_hash.hex(),
                    err = %e,
                    "shard hint miss, falling back to file-index for chunk sequence"
                );
            }
        }
    }

    let shard_hash = resolve_file_index(
        context.file_index_lookup,
        context.store,
        context.router,
        &file_hash,
    )
    .await?;
    try_sequence_shard(context, &shard_hash, &file_hash, file_size).await
}

fn sequence_hint_error_allows_fallback(error: &ReadError) -> bool {
    matches!(
        error,
        ReadError::NotFound { .. }
            | ReadError::Storage(crab_storage::StorageError::NotFound { .. })
    )
}

/// Look up the shard hash for a file via the per-repo `file_index_db`.
///
/// Lazily opens one read-only file-index session for the whole diff batch.
async fn resolve_file_index(
    file_index_lookup: &FileIndexLookupCell,
    store: &CachingStore,
    router: &StoreLayout,
    file_hash: &MerkleHash,
) -> Result<MerkleHash> {
    let session = file_index_lookup
        .get_or_try_init(|| {
            let origin = store.origin().clone();
            let repo_prefix = router.repo_prefix().to_owned();
            async move { FileIndexLookupSession::open_for_storage(&origin, &repo_prefix).await }
        })
        .await?;

    match session.lookup(file_hash).await? {
        Some(shard_hash) => Ok(shard_hash),
        None => Err(ReadError::NotFound {
            path: format!("file_index:{}", file_hash.hex()),
        }),
    }
}

async fn close_file_index_lookup(file_index_lookup: SharedFileIndexLookup) {
    let Ok(file_index_lookup) = Arc::try_unwrap(file_index_lookup) else {
        warn!("diff file-index lookup session still referenced after task join");
        return;
    };

    let Some(session) = file_index_lookup.into_inner() else {
        return;
    };

    if let Err(e) = session.close().await {
        warn!(err = %e, "diff file-index lookup session close failed");
    }
}

/// Try to get file reconstruction terms from a specific shard.
///
/// Downloads the shard if not already cached (disk or in-memory dedup map),
/// then queries for the file hash.
async fn try_shard(
    cache: &LocalCache,
    store: &CachingStore,
    router: &StoreLayout,
    shard_readers: &Mutex<HashMap<MerkleHash, Arc<ShardReader>>>,
    shard_hash: &MerkleHash,
    file_hash: &MerkleHash,
) -> Result<Vec<FileDataSequenceEntry>> {
    let reader = get_or_download_shard(cache, store, router, shard_readers, shard_hash).await?;

    let file_info = reader
        .get_file_info(file_hash)?
        .ok_or_else(|| ReadError::NotFound {
            path: format!(
                "file {} not found in shard {}",
                file_hash.hex(),
                shard_hash.hex()
            ),
        })?;

    Ok(file_info.segments)
}

async fn try_sequence_shard(
    context: &SequenceResolveContext<'_>,
    shard_hash: &MerkleHash,
    file_hash: &MerkleHash,
    file_size: u64,
) -> Result<ChunkSequence> {
    let reader = get_or_download_shard(
        context.cache,
        context.store,
        context.router,
        context.shard_readers,
        shard_hash,
    )
    .await?;

    let file_info = reader
        .get_file_info(file_hash)?
        .ok_or_else(|| ReadError::NotFound {
            path: format!(
                "file {} not found in shard {}",
                file_hash.hex(),
                shard_hash.hex()
            ),
        })?;

    let mut spans = Vec::new();
    let mut file_offset = 0u64;
    for term in file_info.segments {
        let chunks = resolve_term_chunks(
            context.store,
            context.router,
            context.xorb_chunks,
            &reader,
            shard_hash,
            &term,
        )
        .await?;
        append_term_spans(&mut spans, &mut file_offset, &term, &chunks, shard_hash)?;
    }

    if file_offset != file_size {
        return Err(ReadError::CorruptObject {
            path: format!("shard:{}", shard_hash.hex()),
            reason: format!(
                "expanded chunk sequence has {file_offset} bytes, pointer declares {file_size}"
            ),
        });
    }

    Ok(ChunkSequence {
        source: context.source,
        file_hash: *file_hash,
        file_size,
        spans,
    })
}

async fn resolve_term_chunks(
    store: &CachingStore,
    router: &StoreLayout,
    xorb_chunks: &Mutex<HashMap<MerkleHash, Arc<Vec<XorbChunkSequenceEntry>>>>,
    reader: &ShardReader,
    shard_hash: &MerkleHash,
    term: &FileDataSequenceEntry,
) -> Result<Arc<Vec<XorbChunkSequenceEntry>>> {
    let xorb_hash = term.xorb_hash;
    if let Some(xorb_info) = reader.get_xorb_info(&xorb_hash)?
        && term_range_is_valid(term, xorb_info.chunks.len())
        && term_bytes_match(term, &xorb_info.chunks)?
    {
        return Ok(Arc::new(xorb_info.chunks));
    }

    debug!(
        shard_hash = %shard_hash.hex(),
        xorb_hash = %xorb_hash.hex(),
        start = term.chunk_index_start,
        end = term.chunk_index_end,
        "falling back to xorb object metadata for chunk sequence"
    );
    get_or_fetch_xorb_chunks(store, router, xorb_chunks, &xorb_hash).await
}

fn append_term_spans(
    spans: &mut Vec<ChunkSpan>,
    file_offset: &mut u64,
    term: &FileDataSequenceEntry,
    chunks: &[XorbChunkSequenceEntry],
    shard_hash: &MerkleHash,
) -> Result<()> {
    let start = usize::try_from(term.chunk_index_start).map_err(|_| ReadError::CorruptObject {
        path: format!("shard:{}", shard_hash.hex()),
        reason: "file term chunk start overflows usize".to_owned(),
    })?;
    let end = usize::try_from(term.chunk_index_end).map_err(|_| ReadError::CorruptObject {
        path: format!("shard:{}", shard_hash.hex()),
        reason: "file term chunk end overflows usize".to_owned(),
    })?;
    if start > end || end > chunks.len() {
        return Err(ReadError::CorruptObject {
            path: format!("shard:{}", shard_hash.hex()),
            reason: format!(
                "file term chunk range {}..{} outside xorb {} length {}",
                term.chunk_index_start,
                term.chunk_index_end,
                term.xorb_hash.hex(),
                chunks.len()
            ),
        });
    }

    let mut term_bytes = 0u64;
    for (offset, chunk) in chunks[start..end].iter().enumerate() {
        let len = u64::from(chunk.unpacked_segment_bytes);
        let xorb_chunk_index = term
            .chunk_index_start
            .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        spans.push(ChunkSpan {
            chunk_hash: chunk.chunk_hash,
            offset: *file_offset,
            len,
            origin: ChunkOrigin {
                xorb_hash: Some(term.xorb_hash),
                xorb_chunk_index: Some(xorb_chunk_index),
            },
        });
        *file_offset = file_offset.saturating_add(len);
        term_bytes = term_bytes.saturating_add(len);
    }

    if term_bytes != u64::from(term.unpacked_segment_bytes) {
        return Err(ReadError::CorruptObject {
            path: format!("shard:{}", shard_hash.hex()),
            reason: format!(
                "file term byte count {} does not match expanded chunks {}",
                term.unpacked_segment_bytes, term_bytes
            ),
        });
    }

    Ok(())
}

fn term_range_is_valid(term: &FileDataSequenceEntry, len: usize) -> bool {
    let Ok(start) = usize::try_from(term.chunk_index_start) else {
        return false;
    };
    let Ok(end) = usize::try_from(term.chunk_index_end) else {
        return false;
    };
    start <= end && end <= len
}

fn term_bytes_match(
    term: &FileDataSequenceEntry,
    chunks: &[XorbChunkSequenceEntry],
) -> Result<bool> {
    let start = usize::try_from(term.chunk_index_start).map_err(|_| ReadError::CorruptObject {
        path: format!("xorb:{}", term.xorb_hash.hex()),
        reason: "file term chunk start overflows usize".to_owned(),
    })?;
    let end = usize::try_from(term.chunk_index_end).map_err(|_| ReadError::CorruptObject {
        path: format!("xorb:{}", term.xorb_hash.hex()),
        reason: "file term chunk end overflows usize".to_owned(),
    })?;
    let bytes: u64 = chunks[start..end]
        .iter()
        .map(|chunk| u64::from(chunk.unpacked_segment_bytes))
        .sum();
    Ok(bytes == u64::from(term.unpacked_segment_bytes))
}

async fn get_or_fetch_xorb_chunks(
    store: &CachingStore,
    router: &StoreLayout,
    xorb_chunks: &Mutex<HashMap<MerkleHash, Arc<Vec<XorbChunkSequenceEntry>>>>,
    xorb_hash: &MerkleHash,
) -> Result<Arc<Vec<XorbChunkSequenceEntry>>> {
    {
        let cache = xorb_chunks.lock().await;
        if let Some(chunks) = cache.get(xorb_hash) {
            return Ok(Arc::clone(chunks));
        }
    }

    let chunks = Arc::new(fetch_xorb_chunks_from_object(store, router, xorb_hash).await?);
    let mut cache = xorb_chunks.lock().await;
    Ok(Arc::clone(
        cache
            .entry(*xorb_hash)
            .or_insert_with(|| Arc::clone(&chunks)),
    ))
}

async fn fetch_xorb_chunks_from_object(
    store: &CachingStore,
    router: &StoreLayout,
    xorb_hash_value: &MerkleHash,
) -> Result<Vec<XorbChunkSequenceEntry>> {
    let path = router.xorb_path(xorb_hash_value);
    match fetch_xorb_chunks_from_cached_ranges(store, &path, xorb_hash_value).await {
        Ok(chunks) => Ok(chunks),
        Err(e @ ReadError::CorruptObject { .. }) => {
            warn!(
                xorb_hash = %xorb_hash_value.hex(),
                error = %e,
                "cached xorb metadata failed verification, evicting and retrying origin once"
            );
            store
                .local_cache()
                .evict(&CacheKey::Xorb(*xorb_hash_value))
                .await?;
            fetch_xorb_chunks_from_origin_ranges(store.origin(), &path, xorb_hash_value).await
        }
        Err(e) => Err(e),
    }
}

async fn fetch_xorb_chunks_from_cached_ranges(
    store: &CachingStore,
    path: &ObjectPath,
    xorb_hash_value: &MerkleHash,
) -> Result<Vec<XorbChunkSequenceEntry>> {
    let meta = store.head(path).await?;
    let object_len = meta.size;
    let footer_len = u64::try_from(FOOTER_SIZE).unwrap_or(u64::MAX);
    if object_len < footer_len {
        return Err(ReadError::CorruptObject {
            path: path.to_string(),
            reason: "xorb too small for footer".to_owned(),
        });
    }

    let footer = store
        .range_get(path, object_len - footer_len..object_len)
        .await?;
    let layout = parse_xorb_metadata_layout(path.as_ref(), object_len, &footer)?;

    let metadata = store
        .range_get(
            path,
            layout.meta_offset..layout.meta_offset + layout.metadata_len,
        )
        .await?;
    parse_xorb_metadata_entries(path.as_ref(), xorb_hash_value, layout.num_chunks, &metadata)
}

async fn fetch_xorb_chunks_from_origin_ranges(
    store: &Store,
    path: &ObjectPath,
    xorb_hash_value: &MerkleHash,
) -> Result<Vec<XorbChunkSequenceEntry>> {
    let meta = store.head(path).await?;
    let object_len = meta.size;
    let footer_len = u64::try_from(FOOTER_SIZE).unwrap_or(u64::MAX);
    if object_len < footer_len {
        return Err(ReadError::CorruptObject {
            path: path.to_string(),
            reason: "xorb too small for footer".to_owned(),
        });
    }

    let footer = store
        .range_get(path, object_len - footer_len..object_len)
        .await?;
    let layout = parse_xorb_metadata_layout(path.as_ref(), object_len, &footer)?;
    let metadata = store
        .range_get(
            path,
            layout.meta_offset..layout.meta_offset + layout.metadata_len,
        )
        .await?;
    parse_xorb_metadata_entries(path.as_ref(), xorb_hash_value, layout.num_chunks, &metadata)
}

struct XorbMetadataLayout {
    num_chunks: u32,
    meta_offset: u64,
    metadata_len: u64,
}

fn parse_xorb_metadata_layout(
    path: &str,
    object_len: u64,
    footer: &[u8],
) -> Result<XorbMetadataLayout> {
    let footer_len = u64::try_from(FOOTER_SIZE).unwrap_or(u64::MAX);
    if footer.len() != FOOTER_SIZE || &footer[FOOTER_SIZE - XORB_MAGIC.len()..] != XORB_MAGIC {
        return Err(ReadError::CorruptObject {
            path: path.to_owned(),
            reason: "invalid xorb footer".to_owned(),
        });
    }

    let num_chunks =
        u32::from_le_bytes(
            footer[0..4]
                .try_into()
                .map_err(|_| ReadError::CorruptObject {
                    path: path.to_owned(),
                    reason: "bad xorb chunk count".to_owned(),
                })?,
        );
    let meta_offset =
        u64::from_le_bytes(
            footer[4..12]
                .try_into()
                .map_err(|_| ReadError::CorruptObject {
                    path: path.to_owned(),
                    reason: "bad xorb metadata offset".to_owned(),
                })?,
        );
    let metadata_len = u64::from(num_chunks)
        .checked_mul(u64::try_from(CHUNK_META_ENTRY_SIZE).unwrap_or(u64::MAX))
        .ok_or_else(|| ReadError::CorruptObject {
            path: path.to_owned(),
            reason: "xorb metadata length overflow".to_owned(),
        })?;
    if meta_offset
        .checked_add(metadata_len)
        .and_then(|value| value.checked_add(footer_len))
        != Some(object_len)
    {
        return Err(ReadError::CorruptObject {
            path: path.to_owned(),
            reason: "xorb metadata region size mismatch".to_owned(),
        });
    }

    Ok(XorbMetadataLayout {
        num_chunks,
        meta_offset,
        metadata_len,
    })
}

fn parse_xorb_metadata_entries(
    path: &str,
    expected_xorb_hash: &MerkleHash,
    num_chunks: u32,
    metadata: &[u8],
) -> Result<Vec<XorbChunkSequenceEntry>> {
    let expected_len = usize::try_from(num_chunks)
        .ok()
        .and_then(|count| count.checked_mul(CHUNK_META_ENTRY_SIZE))
        .ok_or_else(|| ReadError::CorruptObject {
            path: path.to_owned(),
            reason: "xorb metadata length overflow".to_owned(),
        })?;
    if metadata.len() != expected_len {
        return Err(ReadError::CorruptObject {
            path: path.to_owned(),
            reason: "xorb metadata range length mismatch".to_owned(),
        });
    }

    let mut entries = Vec::with_capacity(num_chunks as usize);
    let mut hash_pairs = Vec::with_capacity(num_chunks as usize);
    let mut offset = 0usize;
    let mut uncompressed_offset = 0u32;
    for _ in 0..num_chunks {
        let chunk_hash = MerkleHash::from(
            <[u8; 32]>::try_from(&metadata[offset..offset + 32]).map_err(|_| {
                ReadError::CorruptObject {
                    path: path.to_owned(),
                    reason: "bad xorb chunk hash".to_owned(),
                }
            })?,
        );
        offset += 32;
        offset += 4; // compressed data offset
        offset += 4; // compressed byte length
        let uncompressed_len =
            u32::from_le_bytes(metadata[offset..offset + 4].try_into().map_err(|_| {
                ReadError::CorruptObject {
                    path: path.to_owned(),
                    reason: "bad xorb uncompressed chunk length".to_owned(),
                }
            })?);
        offset += 4;
        offset += 1; // compression scheme

        entries.push(XorbChunkSequenceEntry::new(
            chunk_hash,
            uncompressed_len,
            uncompressed_offset,
        ));
        hash_pairs.push((chunk_hash, u64::from(uncompressed_len)));
        uncompressed_offset = uncompressed_offset.saturating_add(uncompressed_len);
    }

    let actual_xorb_hash = xorb_hash(&hash_pairs);
    if actual_xorb_hash != *expected_xorb_hash {
        return Err(ReadError::CorruptObject {
            path: path.to_owned(),
            reason: format!(
                "xorb metadata hash mismatch: expected {}, got {}",
                expected_xorb_hash.hex(),
                actual_xorb_hash.hex()
            ),
        });
    }

    Ok(entries)
}

/// Get a shard reader, checking the in-memory dedup map first, then the
/// unified `LocalCache` (which handles disk cache + hash verification),
/// then downloading from the store.
async fn get_or_download_shard(
    cache: &LocalCache,
    store: &CachingStore,
    router: &StoreLayout,
    shard_readers: &Mutex<HashMap<MerkleHash, Arc<ShardReader>>>,
    shard_hash: &MerkleHash,
) -> Result<Arc<ShardReader>> {
    // Check in-memory dedup map first.
    {
        let readers = shard_readers.lock().await;
        if let Some(reader) = readers.get(shard_hash) {
            return Ok(Arc::clone(reader));
        }
    }

    // Download via LocalCache (handles disk cache + store download).
    let key = CacheKey::Shard(*shard_hash);
    let origin = store.origin().clone();
    let obj_path = router.shard_path(shard_hash);
    let hash = *shard_hash;

    let data = cache
        .get_or_fetch_with(&key, || {
            let origin = origin;
            let obj_path = obj_path;
            async move {
                debug!(shard_hash = %hash.hex(), "downloading shard");
                let (data, _) = origin.get_with_etag(&obj_path).await?;
                Ok::<_, ReadError>(data)
            }
        })
        .await?;

    let reader = Arc::new(ShardReader::from_bytes(data, *shard_hash));

    // Store in dedup map for other files in this batch.
    {
        let mut readers = shard_readers.lock().await;
        readers.insert(*shard_hash, Arc::clone(&reader));
    }

    Ok(reader)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Mutex as StdMutex, MutexGuard};

    use super::*;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    static CACHE_ENV_LOCK: StdMutex<()> = StdMutex::new(());

    struct CacheDirGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl CacheDirGuard {
        fn new(path: &Path) -> Self {
            let lock = CACHE_ENV_LOCK.lock().unwrap();
            let previous = std::env::var("CRAB_CACHE_DIR").ok();
            // SAFETY: tests that mutate CRAB_CACHE_DIR hold CACHE_ENV_LOCK.
            unsafe { std::env::set_var("CRAB_CACHE_DIR", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for CacheDirGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => {
                    // SAFETY: CACHE_ENV_LOCK is held for the lifetime of this guard.
                    unsafe { std::env::set_var("CRAB_CACHE_DIR", value) };
                }
                None => {
                    // SAFETY: CACHE_ENV_LOCK is held for the lifetime of this guard.
                    unsafe { std::env::remove_var("CRAB_CACHE_DIR") };
                }
            }
        }
    }

    fn chunk(seed: u8, size: usize) -> Chunk {
        Chunk::new(bytes::Bytes::from(vec![seed; size]))
    }

    fn metadata_slice(xorb: &crab_xet::xorb::builder::XorbResult) -> (u32, &[u8]) {
        let len = xorb.bytes.len();
        let footer = &xorb.bytes[len - FOOTER_SIZE..];
        let num_chunks = u32::from_le_bytes(footer[0..4].try_into().unwrap());
        let meta_offset = u64::from_le_bytes(footer[4..12].try_into().unwrap()) as usize;
        (num_chunks, &xorb.bytes[meta_offset..len - FOOTER_SIZE])
    }

    #[test]
    fn parses_ordered_xorb_footer_metadata() {
        let mut builder = XorbBuilder::new();
        let first = chunk(1, 1024);
        let second = chunk(2, 2048);
        builder.push(&first, RunId(0)).unwrap();
        builder.push(&second, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);
        let (num_chunks, metadata) = metadata_slice(&xorb);

        let entries = parse_xorb_metadata_entries("xorb", &xorb.hash, num_chunks, metadata)
            .expect("parse xorb metadata");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].chunk_hash, first.hash);
        assert_eq!(entries[0].unpacked_segment_bytes, 1024);
        assert_eq!(entries[1].chunk_hash, second.hash);
        assert_eq!(entries[1].unpacked_segment_bytes, 2048);
    }

    #[test]
    fn xorb_footer_metadata_rejects_wrong_hash() {
        let mut builder = XorbBuilder::new();
        builder.push(&chunk(1, 1024), RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);
        let (num_chunks, metadata) = metadata_slice(&xorb);

        let err = parse_xorb_metadata_entries(
            "xorb",
            &MerkleHash::from([0xFE; 32]),
            num_chunks,
            metadata,
        )
        .expect_err("hash mismatch");

        assert!(matches!(err, ReadError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn fetch_xorb_chunks_repairs_corrupt_cached_range_from_origin() {
        use std::sync::Arc;

        use object_store::memory::InMemory;

        use crab_cache_store::CacheConfig;
        use crab_storage::{Store, StoreLayout};

        let cache_root = tempfile::tempdir().expect("cache root");
        let _cache_guard = CacheDirGuard::new(cache_root.path());

        let mut builder = XorbBuilder::new();
        let first = chunk(1, 1024);
        let second = chunk(2, 2048);
        builder.push(&first, RunId(0)).unwrap();
        builder.push(&second, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);

        let origin = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(origin.clone(), "org/repo".to_owned());
        let store = CachingStore::new(origin, &CacheConfig::default()).unwrap();
        let path = router.xorb_path(&xorb.hash);
        store.origin().put(&path, xorb.bytes.clone()).await.unwrap();

        let mut corrupt = xorb.bytes.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        let key = CacheKey::Xorb(xorb.hash);
        store
            .local_cache()
            .put_unchecked_for_test(&key, &corrupt)
            .await
            .unwrap();

        let entries = fetch_xorb_chunks_from_object(&store, &router, &xorb.hash)
            .await
            .expect("corrupt cached xorb range should retry origin");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].chunk_hash, first.hash);
        assert_eq!(entries[0].unpacked_segment_bytes, 1024);
        assert_eq!(entries[1].chunk_hash, second.hash);
        assert_eq!(entries[1].unpacked_segment_bytes, 2048);
        assert!(
            !store.local_cache().contains(&key).await,
            "corrupt local xorb should be evicted after origin repair"
        );
    }
}
