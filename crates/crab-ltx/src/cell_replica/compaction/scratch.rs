use std::{io, path::Path, path::PathBuf, sync::Arc};

use bytes::Bytes;
use crab_storage::{CellObjectKind, MultipartUploadSource, StorageError};

use super::CellReplica;
use crate::{CrabError, Host, Result};

const MULTIPART_BYTES: usize = 8 << 20;

pub(super) async fn upload(
    replica: &CellReplica,
    source: &Path,
    digest: &[u8; 32],
    kind: CellObjectKind,
) -> Result<()> {
    let size = replica.host.filesystem.file_len(source)?;
    let path =
        replica
            .layout
            .incarnation_object_path(&replica.cell, &replica.incarnation, digest, kind);
    let upload: Arc<dyn MultipartUploadSource> = Arc::new(HostUploadSource {
        host: replica.host.clone(),
        path: source.to_owned(),
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let _permit = replica.host.io_permit().await?;
    replica
        .layout
        .store()
        .put_multipart_source_retry(&path, upload, size, *digest, MULTIPART_BYTES, &cancel, None)
        .await?;
    Ok(())
}

struct HostUploadSource {
    host: Host,
    path: PathBuf,
}

#[async_trait::async_trait]
impl MultipartUploadSource for HostUploadSource {
    async fn byte_len(&self) -> crab_storage::Result<u64> {
        let filesystem = Arc::clone(&self.host.filesystem);
        let path = self.path.clone();
        self.host
            .run(move || filesystem.file_len(&path))
            .await
            .map_err(storage_read_error)?
            .map_err(|error| StorageError::ReadRejected {
                source: Box::new(error),
            })
    }

    async fn read_exact(&self, offset: u64, length: usize) -> crab_storage::Result<Bytes> {
        let filesystem = Arc::clone(&self.host.filesystem);
        let path = self.path.clone();
        self.host
            .run(move || {
                let mut file = filesystem.open(&path)?;
                file.read_exact_at(offset, length).map(Bytes::from)
            })
            .await
            .map_err(storage_read_error)?
            .map_err(|error| StorageError::ReadRejected {
                source: Box::new(error),
            })
    }
}

fn storage_read_error(error: CrabError) -> StorageError {
    StorageError::ReadRejected {
        source: Box::new(error),
    }
}

pub(super) struct ScratchFiles {
    filesystem: Arc<dyn crate::environment::FileSystem>,
    directory: PathBuf,
    paths: Vec<PathBuf>,
}

impl ScratchFiles {
    pub(super) fn new(host: &Host, directory: &Path) -> Self {
        Self {
            filesystem: Arc::clone(&host.filesystem),
            directory: directory.to_owned(),
            paths: Vec::new(),
        }
    }

    pub(super) fn create(&mut self, label: &str) -> Result<PathBuf> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..16 {
            let path = self.directory.join(format!(
                ".crab-compaction-{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match self.filesystem.create(&path) {
                Ok(file) => {
                    drop(file);
                    self.paths.push(path.clone());
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "compaction scratch namespace exhausted",
        )
        .into())
    }
}

impl Drop for ScratchFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = self.filesystem.remove_file(path);
        }
    }
}
