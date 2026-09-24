//! Uploading prepared LTX artifacts, indexes, bodies, and bundles.
//!
//! Every write path here streams a prepared root to object storage under the
//! replica's retry policy, and the capture sources below adapt a pinned local
//! file into the multipart interface the store expects.

use super::*;

impl CellReplica {
    async fn put_object(
        &self,
        digest: &[u8; 32],
        kind: CellObjectKind,
        bytes: Vec<u8>,
    ) -> Result<()> {
        self.put_object_bytes(digest, kind, Bytes::from(bytes))
            .await
    }

    pub(super) async fn put_object_bytes(
        &self,
        digest: &[u8; 32],
        kind: CellObjectKind,
        bytes: Bytes,
    ) -> Result<()> {
        if *blake3::hash(&bytes).as_bytes() != *digest {
            return Err(CrabError::ChecksumMismatch);
        }
        let _permit = self.host.io_permit().await?;
        let path = self
            .layout
            .incarnation_object_path(&self.cell, &self.incarnation, digest, kind);
        self.layout.store().put(&path, bytes.clone()).await?;
        if matches!(kind, CellObjectKind::Root | CellObjectKind::Directory) {
            cache::insert(
                &self.layout,
                &self.cell,
                &self.incarnation,
                *digest,
                kind,
                bytes.to_vec().into(),
            )?;
        }
        Ok(())
    }

    pub(super) async fn put_objects(
        &self,
        kind: CellObjectKind,
        objects: Vec<([u8; 32], Vec<u8>)>,
    ) -> Result<()> {
        stream::iter(
            objects
                .into_iter()
                .map(|(digest, bytes)| async move { self.put_object(&digest, kind, bytes).await }),
        )
        .buffer_unordered(OBJECT_UPLOAD_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        Ok(())
    }

    pub(super) async fn upload_prepared_segment(&self, segment: PreparedSegment) -> Result<()> {
        let PreparedSegment {
            descriptor,
            index,
            body,
        } = segment;
        let body_upload = async {
            if descriptor.object_kind() != CellObjectKind::Ltx {
                return Ok(());
            }
            let AppendBody::Native(source) = body else {
                return Err(CrabError::InvalidState("native Cell body source missing"));
            };
            compaction::upload_source(
                self,
                source,
                descriptor.info.size_bytes,
                &descriptor.info.blake3,
                CellObjectKind::Ltx,
            )
            .await
        };
        let index_upload =
            self.put_object_bytes(&descriptor.index_digest, CellObjectKind::Index, index);
        futures_util::future::try_join(body_upload, index_upload).await?;
        Ok(())
    }

    pub(super) async fn put_bundle(&self, bundle: &crate::bundle::Bundle) -> Result<()> {
        let digest = bundle.digest();
        if bundle.len() > self.limits.max_plan_bytes {
            return Err(CrabError::Limit(crate::LimitKind::CellBundleBytes));
        }
        let path = self.layout.incarnation_object_path(
            &self.cell,
            &self.incarnation,
            &digest,
            CellObjectKind::Bundle,
        );
        let staged = self.layout.incarnation_staging_path(
            &self.cell,
            &self.incarnation,
            &digest,
            CellObjectKind::Bundle,
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let _permit = self.host.io_permit().await?;
        let upload = self
            .layout
            .store()
            .put_multipart_source_retry(
                &staged,
                bundle.upload_source(),
                bundle.len(),
                digest,
                MULTIPART_BYTES,
                &cancel,
                None,
            )
            .await;
        if let Err(error) = upload {
            return match cleanup_staged(self.layout.store(), &staged).await {
                Ok(()) => Err(error.into()),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
        let promotion = self
            .layout
            .store()
            .promote_staged_content_addressed_object(&staged, &path, digest, bundle.len())
            .await;
        match cleanup_staged(self.layout.store(), &staged).await {
            Err(error) => Err(error),
            Ok(()) => promotion.map(|_| ()).map_err(Into::into),
        }
    }
}

const MULTIPART_BYTES: usize = 8 << 20;

async fn cleanup_staged(
    store: &crab_storage::Store,
    path: &object_store::path::Path,
) -> Result<()> {
    match store.delete(path).await {
        Ok(()) | Err(crab_storage::StorageError::NotFound { .. }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) struct PinnedCapture {
    host: Host,
    file: Arc<Mutex<Box<dyn crate::environment::FileIo>>>,
    size: u64,
}

impl PinnedCapture {
    pub(super) async fn open(host: &Host, path: PathBuf, expected_size: u64) -> Result<Arc<Self>> {
        let source_host = host.clone();
        let filesystem = Arc::clone(&host.filesystem);
        host.run(move || {
            let file = filesystem.open(&path)?;
            if file.file_len()? != expected_size {
                return Err(CrabError::ChecksumMismatch);
            }
            Ok(Arc::new(Self {
                host: source_host,
                file: Arc::new(Mutex::new(file)),
                size: expected_size,
            }))
        })
        .await?
    }

    fn read_exact(&self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("capture file lock poisoned"))?;
        file.read_exact_at(offset, length)
    }
}

pub(super) struct PinnedCaptureReader {
    pub(super) source: Arc<PinnedCapture>,
    pub(super) offset: u64,
}

impl io::Read for PinnedCaptureReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.source.size.saturating_sub(self.offset);
        let length =
            usize::try_from(remaining.min(bytes.len() as u64)).map_err(io::Error::other)?;
        if length == 0 {
            return Ok(0);
        }
        let read = self.source.read_exact(self.offset, length)?;
        bytes[..length].copy_from_slice(&read);
        self.offset = self
            .offset
            .checked_add(length as u64)
            .ok_or_else(|| io::Error::other("capture offset overflow"))?;
        Ok(length)
    }
}

#[async_trait::async_trait]
impl crab_storage::MultipartUploadSource for PinnedCapture {
    async fn byte_len(&self) -> crab_storage::Result<u64> {
        let file = Arc::clone(&self.file);
        self.host
            .run(move || {
                let file = file
                    .lock()
                    .map_err(|_| io::Error::other("capture file lock poisoned"))?;
                file.file_len()
            })
            .await
            .map_err(pinned_storage_error)?
            .map_err(|error| crab_storage::StorageError::ReadRejected {
                source: Box::new(error),
            })
    }

    async fn read_exact(&self, offset: u64, length: usize) -> crab_storage::Result<Bytes> {
        let file = Arc::clone(&self.file);
        self.host
            .run(move || {
                let mut file = file
                    .lock()
                    .map_err(|_| io::Error::other("capture file lock poisoned"))?;
                file.read_exact_at(offset, length).map(Bytes::from)
            })
            .await
            .map_err(pinned_storage_error)?
            .map_err(|error| crab_storage::StorageError::ReadRejected {
                source: Box::new(error),
            })
    }
}

pub(super) async fn inspect_segment_source(
    replica: &CellReplica,
    source: Arc<PinnedCapture>,
    expected: &crate::SegmentInfo,
) -> Result<Vec<u8>> {
    let expected = expected.clone();
    replica
        .host
        .run(move || {
            let reader = PinnedCaptureReader { source, offset: 0 };
            let (file, size, digest, pages) = crate::ltx::inspect_reader_with_index(reader)?;
            if size != expected.size_bytes || digest != expected.blake3 {
                return Err(CrabError::ChecksumMismatch);
            }
            if crate::SegmentInfo::from_inspected(&file, size, digest) != expected {
                return Err(CrabError::ChecksumMismatch);
            }
            crate::paged::encode_index_from_pages(&pages)
        })
        .await?
}

fn pinned_storage_error(error: CrabError) -> crab_storage::StorageError {
    crab_storage::StorageError::ReadRejected {
        source: Box::new(error),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Read as _;

    use super::*;

    #[tokio::test]
    async fn pinned_capture_ignores_later_path_replacement() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("capture.ltx");
        let displaced = directory.path().join("original.ltx");
        let original = b"verified capture bytes";
        std::fs::write(&path, original).unwrap();
        let source = PinnedCapture::open(&Host::default(), path.clone(), original.len() as u64)
            .await
            .unwrap();

        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, b"replacement contents!").unwrap();

        let mut reader = PinnedCaptureReader { source, offset: 0 };
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).unwrap();
        assert_eq!(observed, original);
    }
}
