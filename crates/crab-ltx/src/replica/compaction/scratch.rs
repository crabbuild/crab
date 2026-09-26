use std::{io, path::Path, path::PathBuf, sync::Arc};

use bytes::Bytes;
use crab_storage::{MultipartUploadSource, StorageError};

use super::CellReplica;
use crate::{CellObjectKind, CrabError, Host, Result, environment::FileIo};

pub(super) async fn upload(
    replica: &CellReplica,
    scratch: &Arc<ScratchFiles>,
    source: &Path,
    size: u64,
    digest: &[u8; 32],
    kind: CellObjectKind,
) -> Result<()> {
    let upload: Arc<dyn MultipartUploadSource> = Arc::new(HostUploadSource {
        scratch: Arc::clone(scratch),
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
    scratch: Arc<ScratchFiles>,
    path: PathBuf,
}

#[async_trait::async_trait]
impl MultipartUploadSource for HostUploadSource {
    async fn byte_len(&self) -> crab_storage::Result<u64> {
        let scratch = Arc::clone(&self.scratch);
        let path = self.path.clone();
        self.scratch
            .host
            .run(move || scratch.host.filesystem.file_len(&path))
            .await
            .map_err(storage_read_error)?
            .map_err(|error| StorageError::ReadRejected {
                source: Box::new(error),
            })
    }

    async fn read_exact(&self, offset: u64, length: usize) -> crab_storage::Result<Bytes> {
        let scratch = Arc::clone(&self.scratch);
        let path = self.path.clone();
        self.scratch
            .host
            .run(move || {
                let mut file = scratch.host.filesystem.open(&path)?;
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
    pub(super) host: Host,
    runtime: tokio::runtime::Handle,
    cleaned: Option<tokio::sync::oneshot::Sender<()>>,
    directory: PathBuf,
    paths: Vec<PathBuf>,
}

impl ScratchFiles {
    pub(super) fn new(
        host: Host,
        directory: &Path,
        runtime: tokio::runtime::Handle,
        cleaned: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        Self {
            host,
            runtime,
            cleaned: Some(cleaned),
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
            match self.host.filesystem.create(&path) {
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

    pub(super) async fn open(self: &Arc<Self>, path: &Path) -> Result<Box<dyn FileIo>> {
        let scratch = Arc::clone(self);
        let path = path.to_owned();
        self.host
            .run(move || {
                let file = scratch.host.filesystem.open_rw(&path)?;
                Ok(Box::new(ScratchFile {
                    file,
                    _scratch: scratch,
                }) as Box<dyn FileIo>)
            })
            .await?
    }
}

// The file drops before its scratch owner. Dispatched jobs can outlive a
// canceled caller without closing admission or unlinking files they still use.
struct ScratchFile {
    file: Box<dyn FileIo>,
    _scratch: Arc<ScratchFiles>,
}

impl FileIo for ScratchFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all_at(offset, bytes)
    }
    fn read_exact_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.file.read_exact_at(offset, len)
    }
    fn sync_all(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
    fn file_len(&self) -> io::Result<u64> {
        self.file.file_len()
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

impl Drop for ScratchFiles {
    fn drop(&mut self) {
        let paths = std::mem::take(&mut self.paths);
        let host = self.host.clone();
        let cleaned = self.cleaned.take();
        // Keep dirty/recovery/scratch admission until the last file and queued
        // job have finished, then remove files through the same job ceiling.
        self.runtime.spawn(async move {
            let filesystem = Arc::clone(&host.filesystem);
            let _ = host
                .run(move || {
                    for path in paths {
                        let _ = filesystem.remove_file(&path);
                    }
                })
                .await;
            drop(host);
            if let Some(cleaned) = cleaned {
                let _ = cleaned.send(());
            }
        });
    }
}
