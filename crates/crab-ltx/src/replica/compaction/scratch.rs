use std::{io, path::Path, path::PathBuf, sync::Arc};

use bytes::Bytes;
use crab_storage::{MultipartUploadSource, StorageError};

use super::CellReplica;
use crate::{CellObjectKind, CrabError, Host, Result};

pub(crate) async fn upload(
    replica: &CellReplica,
    source: &Path,
    digest: &[u8; 32],
    kind: CellObjectKind,
) -> Result<()> {
    let size = replica.host.filesystem.file_len(source)?;
    let upload: Arc<dyn MultipartUploadSource> = Arc::new(HostUploadSource {
        host: replica.host.clone(),
        path: source.to_owned(),
    });
    upload_source(replica, upload, size, digest, kind).await
}

pub(crate) async fn upload_source(
    replica: &CellReplica,
    upload: Arc<dyn MultipartUploadSource>,
    size: u64,
    digest: &[u8; 32],
    kind: CellObjectKind,
) -> Result<()> {
    let path =
        replica
            .layout
            .incarnation_object_path(&replica.cell, &replica.incarnation, digest, kind);
    let _permit = replica.host.io_permit().await?;
    super::super::upload::put_source(replica, &path, upload, size, *digest).await?;
    replica.cost.record(size);
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
