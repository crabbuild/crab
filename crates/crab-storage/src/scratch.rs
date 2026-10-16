//! Private bounded spill files with cancellation-safe blocking-I/O ownership.
use crate::{Result, StorageError};
use bytes::Bytes;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio_util::task::TaskTracker;

struct Directory {
    path: PathBuf,
    identity: [u8; 16],
    next: AtomicU64,
    live: AtomicU64,
    maximum: u64,
    closed: AtomicBool,
}

/// One immutable spill file, bound to its private workspace and verified bytes.
#[derive(Clone)]
pub struct ScratchFile {
    identity: [u8; 16],
    slot: u64,
    bytes: u64,
    hash: [u8; 32],
}

/// Caller-scoped local spill storage; operations retain ownership until their worker exits.
pub struct ScratchSpace {
    directory: Arc<Directory>,
    tasks: TaskTracker,
}

fn io(source: impl Into<std::io::Error>) -> StorageError {
    StorageError::Io {
        source: source.into(),
    }
}

fn worker(source: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    io(std::io::Error::other(source))
}

impl ScratchSpace {
    /// Create a private child of an existing caller-owned directory with a live-byte ceiling.
    ///
    /// The parent must remain private to the caller. Await `close` on normal and
    /// returned-error paths; dropping retains cleanup behind any in-flight worker.
    pub async fn create(parent: &Path, maximum: u64) -> Result<Self> {
        let parent = parent.to_owned();
        let handle = tokio::runtime::Handle::try_current().map_err(worker)?;
        let directory = handle
            .spawn_blocking(move || {
                let identity = rand::random::<[u8; 16]>();
                let name = blake3::hash(&identity).to_hex().to_string();
                let path = parent.join(format!("crab-spill-{name}"));
                let builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                let builder = {
                    use std::os::unix::fs::DirBuilderExt as _;
                    let mut builder = builder;
                    builder.mode(0o700);
                    builder
                };
                builder.create(&path).map_err(io)?;
                Ok::<_, StorageError>(Arc::new(Directory {
                    path,
                    identity,
                    next: AtomicU64::new(0),
                    live: AtomicU64::new(0),
                    maximum,
                    closed: AtomicBool::new(false),
                }))
            })
            .await
            .map_err(worker)??;
        Ok(Self {
            directory,
            tasks: TaskTracker::new(),
        })
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Directory) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let directory = self.directory.clone();
        let handle = tokio::runtime::Handle::try_current().map_err(worker)?;
        self.tasks
            .spawn_blocking_on(move || operation(&directory), &handle)
            .await
            .map_err(worker)?
    }

    /// Write one immutable file, admitting its bytes before creating it.
    pub async fn write(&self, bytes: Bytes) -> Result<ScratchFile> {
        self.blocking(move |directory| {
            let size = bytes.len() as u64;
            let slot = directory
                .next
                .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| io(std::io::Error::other("spill file count overflowed")))?;
            directory
                .live
                .try_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                    live.checked_add(size)
                        .filter(|total| *total <= directory.maximum)
                })
                .map_err(|live| {
                    io(std::io::Error::other(format!(
                        "spill workspace requires {} live bytes, exceeding {} capacity",
                        live.saturating_add(size),
                        directory.maximum
                    )))
                })?;
            let path = directory.path.join(format!("{slot:016x}"));
            let mut output = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(output) => output,
                Err(source) => {
                    directory.live.fetch_sub(size, Ordering::AcqRel);
                    return Err(io(source));
                }
            };
            if let Err(source) = output.write_all(&bytes) {
                drop(output);
                if std::fs::remove_file(&path).is_ok() {
                    directory.live.fetch_sub(size, Ordering::AcqRel);
                }
                return Err(io(source));
            }
            Ok(ScratchFile {
                identity: directory.identity,
                slot,
                bytes: size,
                hash: *blake3::hash(&bytes).as_bytes(),
            })
        })
        .await
    }

    /// Read one admitted file, rejecting changed lengths or content before returning bytes.
    pub async fn read(&self, file: &ScratchFile, maximum: u64) -> Result<Bytes> {
        let file = file.clone();
        self.blocking(move |directory| {
            let path = directory.file_path(&file)?;
            let input = File::open(&path).map_err(io)?;
            let size = input.metadata().map_err(io)?.len();
            if size != file.bytes || size > maximum {
                return Err(corrupt(
                    "spill file length exceeds its commitment or read limit",
                ));
            }
            let capacity = usize::try_from(size).map_err(worker)?;
            let limit = size
                .checked_add(1)
                .ok_or_else(|| corrupt("spill file length overflows"))?;
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(capacity).map_err(worker)?;
            input.take(limit).read_to_end(&mut bytes).map_err(io)?;
            if bytes.len() as u64 != size || blake3::hash(&bytes).as_bytes() != &file.hash {
                return Err(corrupt("spill file differs from its committed bytes"));
            }
            Ok(bytes.into())
        })
        .await
    }

    /// Delete one owned file and release its live-byte admission.
    pub async fn remove(&self, file: ScratchFile) -> Result<()> {
        self.blocking(move |directory| {
            std::fs::remove_file(directory.file_path(&file)?).map_err(io)?;
            directory.live.fetch_sub(file.bytes, Ordering::AcqRel);
            Ok(())
        })
        .await
    }

    /// Drain workers before removing the private directory; preserve cleanup errors.
    pub async fn close(self) -> Result<()> {
        self.tasks.close();
        self.tasks.wait().await;
        self.blocking(Directory::remove).await
    }
}

impl Directory {
    fn file_path(&self, file: &ScratchFile) -> Result<PathBuf> {
        if file.identity != self.identity {
            return Err(corrupt("spill file belongs to another workspace"));
        }
        Ok(self.path.join(format!("{:016x}", file.slot)))
    }

    fn remove(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => (),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => (),
            Err(source) => return Err(io(source)),
        }
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        // Every blocking operation owns this directory. Cleanup cannot race a
        // cancelled future's detached writer and resurrect an abandoned root.
        let path = self.path.clone();
        let spawned = std::thread::Builder::new()
            .name("crab-spill-cleanup".to_owned())
            .spawn(move || {
                if let Err(source) = std::fs::remove_dir_all(path)
                    && source.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(%source, "private spill cleanup failed");
                }
            });
        if let Err(source) = spawned {
            tracing::warn!(%source, "private spill cleanup worker could not start");
            if let Err(source) = self.remove() {
                tracing::warn!(%source, "private spill cleanup failed");
            }
        }
    }
}

fn corrupt(reason: &str) -> StorageError {
    StorageError::CorruptObject {
        path: "private spill file".to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests;
