use std::io::Write;
use std::sync::Arc;

mod buffer;
mod cache_completion;
#[cfg(test)]
mod cache_completion_tests;
use buffer::ReconstructionBuffer;

use crab_cache_store::CachingStore;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use xet_client::cas_client::adaptive_concurrency::AdaptiveConcurrencyController;
use xet_client::cas_types::FileRange;
use xet_runtime::core::XetContext;
use xet_runtime::utils::adjustable_semaphore::AdjustableSemaphore;

use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use tokio_util::sync::CancellationToken;

use crate::{ReadError, ReadMetrics, Result, StoreClient, XorbAvailability};

const HYDRATE_CONCURRENCY_TAG: &str = "crab-read";

pub type ReadStoreLayout = crab_storage::StoreLayout<crab_storage::Store>;

/// Builder for the shared read-side cache/store/hydrator runtime.
pub struct ReadRuntimeBuilder {
    store: CachingStore,
    router: ReadStoreLayout,
    download_concurrency: usize,
    buffer_budget_bytes: u64,
    availability: Option<Arc<dyn XorbAvailability>>,
}

impl ReadRuntimeBuilder {
    /// Start a runtime with resolved storage and download concurrency.
    #[must_use]
    pub fn new(store: CachingStore, router: ReadStoreLayout, download_concurrency: usize) -> Self {
        Self {
            store,
            router,
            download_concurrency,
            buffer_budget_bytes: 256 * 1024 * 1024,
            availability: None,
        }
    }

    /// Bound decoded bytes queued ahead of reconstruction writers.
    #[must_use]
    pub fn with_buffer_budget(mut self, bytes: u64) -> Self {
        self.buffer_budget_bytes = bytes.max(1);
        self
    }

    #[must_use]
    pub fn with_availability(mut self, availability: Arc<dyn XorbAvailability>) -> Self {
        self.availability = Some(availability);
        self
    }

    /// Build the canonical shared hydrator.
    ///
    /// # Errors
    ///
    /// Returns an error when the Xet download controller cannot be created.
    pub fn build(self) -> Result<ShardHydrator> {
        let concurrency = fixed_hydrate_concurrency(self.download_concurrency)?;
        let local = self.store.local_cache();
        let directory = local.root().join("chunks");
        let chunk_cache = match crab_cache::XetChunkCacheHandle::open(&directory, local.max_bytes())
        {
            Ok(handle) => Some(handle.cache),
            Err(error) => {
                tracing::warn!(
                    family = "decoded-range",
                    operation = "open",
                    path = %directory.display(),
                    recovery = "use-verified-origin",
                    %error,
                    "local cache unavailable"
                );
                None
            }
        };
        Ok(ShardHydrator {
            store: self.store,
            router: self.router,
            concurrency,
            buffer_semaphore: AdjustableSemaphore::new(
                self.buffer_budget_bytes,
                (self.buffer_budget_bytes, self.buffer_budget_bytes),
            ),
            chunk_cache,
            metrics: None,
            availability: self.availability,
        })
    }
}

/// Read-side hydrator for reconstructing Crab pointers.
#[derive(Clone)]
pub struct ShardHydrator {
    store: CachingStore,
    router: ReadStoreLayout,
    concurrency: Arc<AdaptiveConcurrencyController>,
    buffer_semaphore: Arc<AdjustableSemaphore>,
    chunk_cache: Option<Arc<dyn xet_client::chunk_cache::ChunkCache>>,
    metrics: Option<Arc<dyn ReadMetrics>>,
    availability: Option<Arc<dyn XorbAvailability>>,
}

impl ShardHydrator {
    fn store_client(&self) -> StoreClient {
        let mut client = StoreClient::new(
            self.store.clone(),
            self.router.clone(),
            Arc::clone(&self.concurrency),
        );
        if let Some(metrics) = self.metrics.clone() {
            client = client.with_dyn_metrics(metrics);
        }
        if let Some(availability) = self.availability.clone() {
            client = client.with_availability(availability);
        }
        client
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
    pub fn with_availability(mut self, availability: Arc<dyn XorbAvailability>) -> Self {
        self.availability = Some(availability);
        self
    }

    /// Reconstruct a pointer into memory and verify its whole-file hash.
    pub async fn reconstruct_from_pointer(&self, pointer_bytes: &[u8]) -> Result<Vec<u8>> {
        let ptr = Pointer::parse(pointer_bytes)?;
        self.reconstruct_file(&ptr).await
    }

    /// Reconstruct a pointer into `dest` and verify its whole-file hash.
    pub async fn reconstruct_to_path(&self, ptr: &Pointer, dest: &std::path::Path) -> Result<u64> {
        let file = std::fs::File::create(dest)?;
        self.reconstruct_to_writer(ptr, file).await
    }

    /// Reconstruct a pointer into a blocking writer and verify its hash.
    pub async fn reconstruct_to_writer<W>(&self, ptr: &Pointer, writer: W) -> Result<u64>
    where
        W: Write + Send + 'static,
    {
        self.reconstruct_to_writer_with_cancel(ptr, writer, None, &CancellationToken::new())
            .await
    }

    /// Reconstruct into a writer with caller-owned cancellation and lookup lifetime.
    pub async fn reconstruct_to_writer_with_cancel<W>(
        &self,
        ptr: &Pointer,
        writer: W,
        file_index_lookup: Option<&SharedFileIndexLookup>,
        cancel: &CancellationToken,
    ) -> Result<u64>
    where
        W: Write + Send + 'static,
    {
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        let file_hash = MerkleHash::from(ptr.file_hash);
        let client = self.store_client_for_pointer(ptr, file_index_lookup);
        self.preflight_shard_coverage(&client, ptr).await?;

        let tap_state = Arc::new(std::sync::Mutex::new(GenericHasherTapState {
            writer: Some(writer),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
            expected_size: ptr.size,
            exceeded: false,
        }));
        let _writer_owner = GenericHasherTapOwner(Arc::clone(&tap_state));
        let writer = GenericHasherTap {
            shared: Arc::clone(&tap_state),
        };

        let operation_cancel = cancel.child_token();
        let _cancel_on_drop = operation_cancel.clone().drop_guard();
        let outcome = self
            .reconstruct_to_writer_unverified(client, file_hash, writer, None, &operation_cancel)
            .await;

        let (actual_hash, bytes_written) = {
            let mut guard = tap_state
                .lock()
                .map_err(|_| ReadError::internal("hasher tap poisoned"))?;
            let mut writer = guard.writer.take();
            if guard.exceeded {
                return Err(ReadError::CorruptObject {
                    path: file_hash.hex(),
                    reason: format!(
                        "reconstruction exceeds declared output size of {} bytes",
                        ptr.size
                    ),
                });
            }
            outcome?;
            if let Some(writer) = writer.as_mut() {
                // A final-flush failure follows written output, just like an
                // Xet writer failure; retain the same source and replay policy.
                writer.flush().map_err(|error| ReadError::Reconstruction {
                    file_hash: file_hash.hex(),
                    source: crate::error::ReconstructionError(error.into()),
                })?;
            }
            drop(writer);
            let hash: [u8; 32] = guard.hasher.finalize().into();
            (hash, guard.bytes_written)
        };

        if actual_hash != ptr.file_hash {
            return Err(ReadError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            });
        }
        if bytes_written != ptr.size {
            return Err(ReadError::CorruptObject {
                path: file_hash.hex(),
                reason: format!(
                    "reconstruction size mismatch: expected {}, got {}",
                    ptr.size, bytes_written
                ),
            });
        }

        Ok(bytes_written)
    }

    /// Reconstruct bytes covering `[start, end)` of the original file.
    pub async fn reconstruct_range_from_pointer(
        &self,
        pointer_bytes: &[u8],
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        let ptr = Pointer::parse(pointer_bytes)?;
        let client = self.store_client_for_pointer(&ptr, None);

        if end < start {
            return Err(ReadError::internal(format!(
                "reconstruct_range_from_pointer: end ({end}) < start ({start})"
            )));
        }

        let end = end.min(ptr.size);
        let start = start.min(ptr.size);
        if start >= end {
            return Ok(Vec::new());
        }

        let buffer = ReconstructionBuffer::new(end - start)?;
        let file_hash = MerkleHash::from(ptr.file_hash);
        let range = FileRange::new(start, end);
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();

        let outcome = self
            .reconstruct_to_writer_unverified(
                client,
                file_hash,
                buffer.writer(),
                Some(range),
                &cancel,
            )
            .await;
        buffer.finish(outcome, &file_hash)
    }

    fn store_client_for_pointer(
        &self,
        ptr: &Pointer,
        file_index_lookup: Option<&SharedFileIndexLookup>,
    ) -> Arc<dyn xet_client::cas_client::Client> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let mut client = self.store_client();
        if let Some(lookup) = file_index_lookup {
            client = client.with_file_index_lookup(lookup.clone());
        }
        let client = match ptr.shard_hint {
            Some(hint) => client.with_shard_hint(file_hash, MerkleHash::from(hint)),
            None => client,
        };
        Arc::new(client)
    }

    async fn reconstruct_file(&self, ptr: &Pointer) -> Result<Vec<u8>> {
        let buffer = ReconstructionBuffer::new(ptr.size)?;
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let outcome = self
            .reconstruct_to_writer_with_cancel(ptr, buffer.writer(), None, &cancel)
            .await
            .map(|_| ());
        buffer.finish(outcome, &MerkleHash::from(ptr.file_hash))
    }

    async fn reconstruct_to_writer_unverified<W>(
        &self,
        client: Arc<dyn xet_client::cas_client::Client>,
        file_hash: MerkleHash,
        writer: W,
        range: Option<FileRange>,
        cancel: &CancellationToken,
    ) -> Result<()>
    where
        W: Write + Send + 'static,
    {
        let xet_context = XetContext::default()?;
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        let reconstructor =
            xet_data::file_reconstruction::FileReconstructor::new(&xet_context, &client, file_hash)
                .with_buffer_semaphore(Arc::clone(&self.buffer_semaphore))
                .with_cancellation_token(cancel.child_token());
        let reconstructor = match range {
            Some(range) => reconstructor.with_byte_range(range),
            None => reconstructor,
        };
        let cache_cancel = cancel.child_token();
        let _cancel_cache_on_drop = cache_cancel.clone().drop_guard();
        let (reconstructor, cache_done) = match self.chunk_cache.clone() {
            Some(cache) => {
                let (cache, completion) = cache_completion::track(cache, cache_cancel);
                (reconstructor.with_chunk_cache(cache), Some(completion))
            }
            None => (reconstructor, None),
        };

        if let Err(error) = reconstructor.reconstruct_to_writer(writer).await {
            let source = crate::error::ReconstructionError(error);
            if cancel.is_cancelled() || source.is_cancelled() {
                return Err(ReadError::Cancelled);
            }
            return Err(ReadError::Reconstruction {
                file_hash: file_hash.hex(),
                source,
            });
        }
        // Xet returns after output is written, but spawns cache puts without
        // joining them. Await this operation's cache owners so a completed
        // prefetch survives immediate runtime/process shutdown and reopening.
        if let Some(completion) = cache_done {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ReadError::Cancelled),
                _ = completion => {}
            }
        }
        Ok(())
    }

    async fn preflight_shard_coverage(
        &self,
        client: &Arc<dyn xet_client::cas_client::Client>,
        ptr: &Pointer,
    ) -> Result<()> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let Ok(Some((info, _))) = client.get_file_reconstruction_info(&file_hash).await else {
            return Ok(());
        };

        let covered = info
            .segments
            .iter()
            .try_fold(0_u64, |total, segment| {
                total.checked_add(u64::from(segment.unpacked_segment_bytes))
            })
            .ok_or_else(|| ReadError::CorruptObject {
                path: file_hash.hex(),
                reason: "reconstruction coverage size overflow".into(),
            })?;
        if covered >= ptr.size {
            return Ok(());
        }

        let example = info.segments.first().map_or_else(
            || (0, String::new()),
            |s| (s.chunk_index_start, s.xorb_hash.hex()),
        );

        Err(ReadError::IncompleteShardReconstruction {
            file_hash: file_hash.hex(),
            uncovered_chunks: 1,
            example_chunk_hash: example.1,
            example_chunk_index: example.0,
        })
    }
}

fn fixed_hydrate_concurrency(concurrency: usize) -> Result<Arc<AdaptiveConcurrencyController>> {
    let context = XetContext::default()?;
    Ok(AdaptiveConcurrencyController::new_fixed(
        context,
        HYDRATE_CONCURRENCY_TAG,
        concurrency,
    ))
}

struct GenericHasherTapState<W: Write> {
    writer: Option<W>,
    hasher: blake3::Hasher,
    bytes_written: u64,
    expected_size: u64,
    exceeded: bool,
}

impl<W: Write> GenericHasherTapState<W> {
    fn admit_write(&mut self, mut lengths: impl Iterator<Item = usize>) -> std::io::Result<()> {
        let total = lengths.try_fold(self.bytes_written, |total, len| {
            total.checked_add(len as u64)
        });
        if self.exceeded || total.is_none_or(|total| total > self.expected_size) {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "reconstructed output exceeds declared size",
            ));
        }
        Ok(())
    }
}

struct GenericHasherTapOwner<W: Write>(Arc<std::sync::Mutex<GenericHasherTapState<W>>>);

impl<W: Write> Drop for GenericHasherTapOwner<W> {
    fn drop(&mut self) {
        // Xet may retain tap clones after cancellation. Ownership stays here
        // so returning or dropping this future closes the actual destination.
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state.writer.take());
    }
}

struct GenericHasherTap<W: Write> {
    shared: Arc<std::sync::Mutex<GenericHasherTapState<W>>>,
}

fn hash_vectored_prefix(
    hasher: &mut blake3::Hasher,
    bufs: &[std::io::IoSlice<'_>],
    mut written: usize,
) {
    for buf in bufs {
        if written == 0 {
            break;
        }
        let take = written.min(buf.len());
        hasher.update(&buf[..take]);
        written -= take;
    }
}

impl<W: Write> Write for GenericHasherTap<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        guard.admit_write(std::iter::once(buf.len()))?;
        let written = guard
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: writer already taken"))?
            .write(buf)?;
        guard.hasher.update(&buf[..written]);
        guard.bytes_written += written as u64;
        Ok(written)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let lengths = bufs.iter().map(|buf| buf.len());
        guard.admit_write(lengths)?;
        let written = guard
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: writer already taken"))?
            .write_vectored(bufs)?;
        hash_vectored_prefix(&mut guard.hasher, bufs, written);
        guard.bytes_written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        match guard.writer.as_mut() {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{IoSlice, Write};
    use std::sync::{Arc, Mutex};

    use super::{GenericHasherTap, GenericHasherTapState};

    pub(super) async fn reconstruction_fixture(
        root: &std::path::Path,
        corrupt_origin: bool,
    ) -> (super::ShardHydrator, crab_types::pointer::Pointer, Vec<u8>) {
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};

        let data = vec![42; 128 * 1024];
        let chunk = crab_xet::xorb::format::Chunk::new(bytes::Bytes::from(data.clone()));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);
        let file_hash = *blake3::hash(&data).as_bytes();
        let mut shard = ShardWriter::new();
        shard
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, data.len()),
                chunks: vec![XorbChunkSequenceEntry::new(
                    chunk.hash,
                    data.len() as u32,
                    0,
                )],
            }))
            .unwrap();
        shard
            .add_file(MDBFileInfo {
                metadata: FileDataSequenceHeader::new(file_hash.into(), 1, false, false),
                segments: vec![FileDataSequenceEntry::new(
                    xorb.hash,
                    data.len() as u32,
                    0,
                    1,
                )],
                verification: vec![],
                metadata_ext: None,
            })
            .unwrap();
        let (shard_bytes, shard_hash) = shard.finalize().unwrap();
        let hydrator = runtime(root, 1024 * 1024);
        hydrator
            .store
            .origin()
            .put(&hydrator.router.shard_path(&shard_hash), shard_bytes.into())
            .await
            .unwrap();
        let mut xorb_bytes = xorb.bytes.to_vec();
        if corrupt_origin {
            *xorb_bytes.last_mut().unwrap() ^= 0xFF;
        }
        hydrator
            .store
            .origin()
            .put(&hydrator.router.xorb_path(&xorb.hash), xorb_bytes.into())
            .await
            .unwrap();
        let pointer = crab_types::pointer::Pointer {
            file_hash,
            size: data.len() as u64,
            shard_hint: Some(shard_hash.into()),
        };
        (hydrator, pointer, data)
    }

    fn source_of<'a, T: std::error::Error + 'static>(
        error: &'a (dyn std::error::Error + 'static),
    ) -> Option<&'a T> {
        let mut current = Some(error);
        while let Some(error) = current {
            if let Some(source) = error.downcast_ref::<T>() {
                return Some(source);
            }
            current = error.source();
        }
        None
    }

    #[derive(Debug, thiserror::Error)]
    #[error("restore denied for {path}")]
    struct RestoreDenied {
        path: String,
    }

    struct FailingAvailability {
        calls: std::sync::atomic::AtomicUsize,
        cancel: Option<tokio_util::sync::CancellationToken>,
        reports_cancelled: bool,
    }

    #[async_trait::async_trait]
    impl crate::XorbAvailability for FailingAvailability {
        async fn ensure_available(&self, path: &object_store::path::Path) -> crate::Result<()> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            if self.reports_cancelled {
                return Err(crate::ReadError::Cancelled);
            }
            Err(crate::ReadError::availability(RestoreDenied {
                path: path.to_string(),
            }))
        }
    }

    #[tokio::test]
    async fn reconstruction_preserves_origin_integrity_source() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), true).await;
        let error = hydrator
            .reconstruct_from_pointer(&pointer.serialize())
            .await
            .unwrap_err();
        let source = source_of::<crab_cache_store::CacheStoreError>(&error)
            .unwrap_or_else(|| panic!("missing origin-integrity source in {error:#?}"));
        assert!(matches!(
            source,
            crab_cache_store::CacheStoreError::OriginIntegrity { .. }
        ));
        assert!(source_of::<crab_cache::CacheError>(&error).is_some());
    }

    #[tokio::test]
    async fn reconstruction_preserves_availability_source() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let availability = Arc::new(FailingAvailability {
            calls: std::sync::atomic::AtomicUsize::new(0),
            cancel: None,
            reports_cancelled: false,
        });
        let hydrator = hydrator.with_availability(availability.clone());
        let error = hydrator
            .reconstruct_from_pointer(&pointer.serialize())
            .await
            .unwrap_err();
        let source = source_of::<RestoreDenied>(&error)
            .unwrap_or_else(|| panic!("missing availability source in {error:#?}"));
        assert!(source.path.contains("/xorbs/"));
        assert_eq!(
            availability.calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_reconstruction_failures_retain_availability_source_on_every_surface() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let availability = Arc::new(FailingAvailability {
            calls: std::sync::atomic::AtomicUsize::new(0),
            cancel: None,
            reports_cancelled: false,
        });
        let hydrator = hydrator.with_availability(availability);
        for attempt in 0..256 {
            let error = match attempt % 3 {
                0 => hydrator
                    .reconstruct_from_pointer(&pointer.serialize())
                    .await
                    .unwrap_err(),
                1 => hydrator
                    .reconstruct_to_writer(&pointer, std::io::sink())
                    .await
                    .unwrap_err(),
                _ => hydrator
                    .reconstruct_range_from_pointer(&pointer.serialize(), 0, 1024)
                    .await
                    .unwrap_err(),
            };
            assert!(
                source_of::<RestoreDenied>(&error).is_some(),
                "attempt {attempt} lost the availability source: {error:#?}"
            );
        }
    }

    struct FailingWriter(Arc<std::sync::atomic::AtomicBool>);

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "writer denied",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for FailingWriter {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct FailingFlush {
        calls: usize,
        fail_on: usize,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Write for FailingFlush {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.calls += 1;
            if self.calls == self.fail_on {
                return Err(std::io::Error::other("flush failed"));
            }
            Ok(())
        }
    }

    impl Drop for FailingFlush {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn upstream_and_final_flush_failures_share_writer_error_policy() {
        for fail_on in [1, 2] {
            let directory = tempfile::tempdir().unwrap();
            let (hydrator, pointer, _) =
                reconstruction_fixture(&directory.path().join("cache"), false).await;
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let writer = FailingFlush {
                calls: 0,
                fail_on,
                dropped: dropped.clone(),
            };
            let error = hydrator
                .reconstruct_to_writer(&pointer, writer)
                .await
                .unwrap_err();
            assert!(matches!(error, crate::ReadError::Reconstruction { .. }));
            assert_eq!(
                source_of::<std::io::Error>(&error).unwrap().to_string(),
                "flush failed"
            );
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn reconstruction_preserves_writer_error_and_releases_writer() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = hydrator
            .reconstruct_to_writer(&pointer, FailingWriter(dropped.clone()))
            .await
            .unwrap_err();
        assert_eq!(
            source_of::<std::io::Error>(&error).unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_during_availability_releases_writer() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let cancel = tokio_util::sync::CancellationToken::new();
        let availability = Arc::new(FailingAvailability {
            calls: std::sync::atomic::AtomicUsize::new(0),
            cancel: Some(cancel.clone()),
            reports_cancelled: false,
        });
        let hydrator = hydrator.with_availability(availability.clone());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = hydrator
            .reconstruct_to_writer_with_cancel(
                &pointer,
                FailingWriter(dropped.clone()),
                None,
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, crate::ReadError::Cancelled));
        assert_eq!(
            availability.calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_reconstruction_closes_writer_while_source_is_pending() {
        struct PendingAvailability(tokio::sync::Notify);

        #[async_trait::async_trait]
        impl crate::XorbAvailability for PendingAvailability {
            async fn ensure_available(&self, _: &object_store::path::Path) -> crate::Result<()> {
                self.0.notify_one();
                std::future::pending().await
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let availability = Arc::new(PendingAvailability(tokio::sync::Notify::new()));
        let hydrator = hydrator.with_availability(availability.clone());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut reconstruction =
            Box::pin(hydrator.reconstruct_to_writer(&pointer, FailingWriter(dropped.clone())));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut reconstruction => panic!("pending source unexpectedly completed: {result:?}"),
                () = availability.0.notified() => {}
            }
        }).await.unwrap();
        drop(reconstruction);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reconstruction_fixture_returns_byte_identical_content() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, original) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        assert_eq!(
            hydrator
                .reconstruct_from_pointer(&pointer.serialize())
                .await
                .unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn reconstructed_ranges_match_exact_clamped_lengths() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, original) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        for (start, end) in [
            (0, 1),
            (1, pointer.size - 1),
            (0, pointer.size),
            (pointer.size - 1, u64::MAX),
            (pointer.size, u64::MAX),
            (7, 7),
        ] {
            let bytes = hydrator
                .reconstruct_range_from_pointer(&pointer.serialize(), start, end)
                .await
                .unwrap();
            assert_eq!(
                bytes,
                original[start.min(pointer.size) as usize..end.min(pointer.size) as usize]
            );
        }
        assert!(
            hydrator
                .reconstruct_range_from_pointer(&pointer.serialize(), 8, 7)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn incomplete_range_output_is_not_success() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, mut pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        pointer.size += 5;
        let error = hydrator
            .reconstruct_range_from_pointer(&pointer.serialize(), 0, pointer.size)
            .await
            .unwrap_err();
        assert!(
            matches!(error, crate::ReadError::CorruptObject { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn oversized_reconstruction_is_an_integrity_failure_for_memory_and_streaming() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, mut pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        pointer.size -= 1;
        let memory_error = hydrator
            .reconstruct_from_pointer(&pointer.serialize())
            .await
            .unwrap_err();
        assert!(
            matches!(memory_error, crate::ReadError::CorruptObject { .. }),
            "{memory_error:?}"
        );
        let stream_error = hydrator
            .reconstruct_to_writer(&pointer, std::io::sink())
            .await
            .unwrap_err();
        assert!(
            matches!(stream_error, crate::ReadError::CorruptObject { .. }),
            "{stream_error:?}"
        );
    }

    #[tokio::test]
    async fn source_reported_cancellation_does_not_become_internal_failure() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let availability = Arc::new(FailingAvailability {
            calls: std::sync::atomic::AtomicUsize::new(0),
            cancel: None,
            reports_cancelled: true,
        });
        let hydrator = hydrator.with_availability(availability);
        let error = hydrator
            .reconstruct_from_pointer(&pointer.serialize())
            .await
            .unwrap_err();
        assert!(matches!(error, crate::ReadError::Cancelled));
    }

    fn runtime(root: &std::path::Path, max_bytes: u64) -> super::ShardHydrator {
        let origin = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let local = Arc::new(crab_cache::LocalCache::with_limits(
            root.to_owned(),
            max_bytes,
            Some(max_bytes),
        ));
        let caching = crab_cache_store::CachingStore::new_with_local_cache(
            origin.clone(),
            crab_cache_store::CacheConfig::default(),
            local,
        )
        .expect("caching store");
        let layout = super::ReadStoreLayout::new(origin, "runtime-test".into());
        super::ReadRuntimeBuilder::new(caching, layout, 2)
            .build()
            .expect("runtime")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_uses_object_cache_root_and_budget_without_caller_attachment() {
        use xet_client::cas_types::{ChunkRange, Key};

        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().join("private-cache");
        let budget = 1024 * 1024;
        let hydrator = runtime(&root, budget);
        let cache = hydrator
            .chunk_cache
            .clone()
            .expect("automatically attached cache");
        let range = ChunkRange::new(0, 1);
        let key = Key {
            prefix: "runtime".into(),
            hash: crab_xet::hash::MerkleHash::from([1, 2, 3, 4]),
        };
        cache.put(&key, &range, &[0, 3], b"abc").await.unwrap();
        drop(cache);
        drop(hydrator);

        let reopened = runtime(&root, budget);
        let cache = reopened.chunk_cache.clone().expect("reopened cache");
        assert_eq!(cache.get(&key, &range).await.unwrap().unwrap().data, b"abc");

        let oversized_key = Key {
            prefix: "runtime".into(),
            hash: crab_xet::hash::MerkleHash::from([5, 6, 7, 8]),
        };
        let oversized = vec![0; 2 * 1024 * 1024];
        cache
            .put(&oversized_key, &range, &[0, 2 * 1024 * 1024], &oversized)
            .await
            .unwrap();
        assert!(cache.get(&oversized_key, &range).await.unwrap().is_none());
        assert_eq!(
            crab_cache::xet_chunk_cache_stats(&root.join("chunks"))
                .await
                .unwrap()
                .entries,
            1
        );
    }

    #[test]
    fn unsafe_cache_does_not_prevent_runtime_construction() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().join("not-a-directory");
        std::fs::write(&root, b"untouched").unwrap();
        let hydrator = runtime(&root, 1024 * 1024);
        assert!(hydrator.chunk_cache.is_none());
        assert_eq!(std::fs::read(root).unwrap(), b"untouched");
    }

    #[test]
    fn conflicting_live_budget_disables_only_the_second_runtime_cache() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().join("private-cache");
        let first = runtime(&root, 1024 * 1024);

        let second = runtime(&root, 512 * 1024);

        assert!(first.chunk_cache.is_some());
        assert!(second.chunk_cache.is_none());
    }

    struct PartialVectoredWriter;

    impl Write for PartialVectoredWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len().min(5))
        }

        fn write_vectored(&mut self, _bufs: &[IoSlice<'_>]) -> std::io::Result<usize> {
            Ok(5)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn generic_hasher_tap_hashes_only_partial_vectored_write() {
        let shared = Arc::new(Mutex::new(GenericHasherTapState {
            writer: Some(PartialVectoredWriter),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
            expected_size: 7,
            exceeded: false,
        }));
        let mut tap = GenericHasherTap {
            shared: Arc::clone(&shared),
        };
        let bufs = [IoSlice::new(b"abc"), IoSlice::new(b"defg")];

        let written = tap.write_vectored(&bufs).expect("partial vectored write");
        let guard = shared.lock().expect("hasher state");
        let actual = *guard.hasher.finalize().as_bytes();

        assert_eq!(
            (written, guard.bytes_written, actual),
            (5, 5, *blake3::hash(b"abcde").as_bytes())
        );
    }

    #[test]
    fn hasher_tap_rejects_overrun_before_forwarding_bytes() {
        for vectored in [false, true] {
            let shared = Arc::new(Mutex::new(GenericHasherTapState {
                writer: Some(Vec::new()),
                hasher: blake3::Hasher::new(),
                bytes_written: 0,
                expected_size: 2,
                exceeded: false,
            }));
            let mut tap = GenericHasherTap {
                shared: Arc::clone(&shared),
            };
            tap.write_all(b"a").unwrap();
            let result = if vectored {
                tap.write_vectored(&[IoSlice::new(b"b"), IoSlice::new(b"c")])
            } else {
                tap.write(b"bc")
            };
            assert!(result.is_err());
            let state = shared.lock().unwrap();
            assert_eq!(state.writer.as_ref().unwrap(), b"a");
            assert_eq!(state.bytes_written, 1);
        }
    }
}
