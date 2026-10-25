//! Verified ordinary Git blob reads for filesystem range access and promotion.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gix_hash::ObjectId;
use gix_object::{Find, FindHeader};

use crate::core::error::{CrabError, Result};
use crate::memory_cache::MemoryCache;

const MAX_BLOB_SIZE: usize = 100 * 1024 * 1024;
const CACHE_BYTES: usize = 128 * 1024 * 1024;
const CACHE_ENTRIES: usize = 4096;

/// Reads verified ordinary Git blobs with bounded immutable decoded reuse.
pub struct OdbReader {
    odb: Mutex<gix_odb::HandleArc>,
    git_dir: PathBuf,
    cache: Mutex<MemoryCache<ObjectId>>,
    fills: Arc<tokio::sync::Mutex<()>>,
    accepting: Mutex<bool>,
    reads: tokio_util::task::TaskTracker,
    #[cfg(test)]
    native_reads_started: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for OdbReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OdbReader")
            .field("git_dir", &self.git_dir)
            .finish_non_exhaustive()
    }
}

impl OdbReader {
    /// Open a repository's common object database without a second disk-blob cache.
    pub fn new(git_dir: &Path) -> Result<Self> {
        let objects = crab_git::discover::resolve_common_dir(git_dir).join("objects");
        if !objects.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Git object directory missing: {}", objects.display()),
            )
            .into());
        }
        let mut odb = gix_odb::at(objects)?.into_arc()?;
        // Native Git admits newly fetched promisor packs. Refreshing this fixed
        // gix slot map indefinitely would eventually exhaust its pack slots.
        odb.refresh_never();
        Ok(Self {
            odb: Mutex::new(odb),
            git_dir: git_dir.to_owned(),
            cache: Mutex::new(MemoryCache::new(CACHE_BYTES, CACHE_ENTRIES)),
            fills: Arc::new(tokio::sync::Mutex::new(())),
            accepting: Mutex::new(true),
            reads: tokio_util::task::TaskTracker::new(),
            #[cfg(test)]
            native_reads_started: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn read_blob(&self, oid_hex: &str) -> Result<Bytes> {
        let oid = parse_oid(oid_hex)?;
        if let Some(bytes) = self.cached(&oid) {
            return Ok(bytes);
        }
        let _read = self.admit_read()?;
        self.read_blob_id(oid)
    }

    pub(crate) async fn read_blob_async(self: &Arc<Self>, oid_hex: &str) -> Result<Bytes> {
        let oid = parse_oid(oid_hex)?;
        if let Some(bytes) = self.cached(&oid) {
            return Ok(bytes);
        }
        let admission = Arc::clone(&self.fills).lock_owned().await;
        let read = self.admit_read()?;
        let reader = Arc::clone(self);
        // Cancellation of the awaiting future must not release fill admission
        // while its blocking decode/fetch still owns buffers and Git state.
        tokio::task::spawn_blocking(move || {
            let _admission = admission;
            let _read = read;
            reader.read_blob_id(oid)
        })
        .await
        .map_err(CrabError::ReadTask)?
    }

    fn admit_read(&self) -> Result<tokio_util::task::task_tracker::TaskTrackerToken> {
        let accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*accepting {
            return Err(CrabError::Cancelled);
        }
        // Registration and closing share admission; shutdown cannot observe
        // an empty tracker just before a new blocking operation is registered.
        Ok(self.reads.token())
    }

    pub(crate) fn request_shutdown(&self) {
        let mut accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *accepting = false;
        self.reads.close();
    }

    pub(crate) async fn shutdown(&self) {
        self.request_shutdown();
        self.reads.wait().await;
    }

    pub(crate) async fn read_blob_range(
        self: &Arc<Self>,
        oid_hex: &str,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        let bytes = self.read_blob_async(oid_hex).await?;
        let start = offset.min(bytes.len() as u64) as usize;
        let len = (bytes.len() - start).min(size as usize);
        // Kernel replies must not keep an evicted complete blob allocation alive.
        Ok(Bytes::copy_from_slice(&bytes[start..start + len]))
    }

    pub(crate) async fn copy_blob(
        self: &Arc<Self>,
        oid: &str,
        mut output: tempfile::NamedTempFile,
    ) -> Result<(tempfile::NamedTempFile, u64)> {
        let bytes = self.read_blob_async(oid).await?;
        let read = self.admit_read()?;
        // The owned output and verified allocation stay with the tracked job.
        // Abandoning its waiter cannot publish or reuse a partially written path.
        tokio::task::spawn_blocking(move || {
            let _read = read;
            std::io::Write::write_all(output.as_file_mut(), &bytes)?;
            output.as_file().sync_all()?;
            Ok((output, bytes.len() as u64))
        })
        .await
        .map_err(CrabError::ReadTask)?
    }

    fn cached(&self, oid: &ObjectId) -> Option<Bytes> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(oid)
    }

    fn read_blob_id(&self, oid: ObjectId) -> Result<Bytes> {
        if let Some(bytes) = self.cached(&oid) {
            return Ok(bytes);
        }
        let bytes = match self.read_native(&oid)? {
            Some(bytes) => bytes,
            None => self.fetch_native(&oid)?,
        };
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(oid, &bytes);
        Ok(bytes)
    }

    fn read_native(&self, oid: &ObjectId) -> Result<Option<Bytes>> {
        #[cfg(test)]
        self.native_reads_started
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        let odb = self
            .odb
            .lock()
            .map_err(|_| CrabError::Internal("ODB mutex poisoned".into()))?;
        let Some(header) = odb.try_header(oid).map_err(|source| CrabError::GitObject {
            operation: format!("read header {oid}"),
            source,
        })?
        else {
            return Ok(None);
        };
        check_blob(oid, header.kind, header.size)?;
        let mut decoded = Vec::new();
        let Some(data) =
            odb.try_find(oid, &mut decoded)
                .map_err(|source| CrabError::GitObject {
                    operation: format!("decode {oid}"),
                    source,
                })?
        else {
            return Ok(None);
        };
        check_blob(oid, data.kind, data.data.len() as u64)?;
        data.verify_checksum(oid)
            .map_err(|source| CrabError::GitObject {
                operation: format!("verify {oid}"),
                source: Box::new(source),
            })?;
        Ok(Some(Bytes::copy_from_slice(data.data)))
    }

    fn fetch_native(&self, oid: &ObjectId) -> Result<Bytes> {
        let child = std::process::Command::new("git")
            .arg("--no-replace-objects")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["cat-file", "blob", &oid.to_string()])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let mut owned = BlobChild {
            child,
            waited: false,
        };
        let stdout = owned
            .child
            .stdout
            .take()
            .ok_or_else(|| CrabError::Internal("Git blob fetch has no stdout".into()))?;
        let mut bytes = Vec::new();
        stdout
            .take(MAX_BLOB_SIZE as u64 + 1)
            .read_to_end(&mut bytes)?;
        check_blob(oid, gix_object::Kind::Blob, bytes.len() as u64)?;
        let status = owned.child.wait()?;
        owned.waited = true;
        if !status.success() {
            return Err(CrabError::NotFound {
                path: format!("Git blob {oid}; cat-file exited {status}"),
            });
        }
        gix_object::Data::new(&bytes, gix_object::Kind::Blob, oid.kind())
            .verify_checksum(oid)
            .map_err(|source| CrabError::GitObject {
                operation: format!("verify fetched {oid}"),
                source: Box::new(source),
            })?;
        Ok(bytes.into())
    }
}

fn parse_oid(oid: &str) -> Result<ObjectId> {
    ObjectId::from_hex(oid.as_bytes()).map_err(|source| CrabError::GitObject {
        operation: "parse object ID".into(),
        source: Box::new(source),
    })
}

fn check_blob(oid: &ObjectId, kind: gix_object::Kind, size: u64) -> Result<()> {
    if kind != gix_object::Kind::Blob || size > MAX_BLOB_SIZE as u64 {
        return Err(CrabError::CorruptObject {
            path: oid.to_string(),
            reason: format!(
                "expected a blob at most {MAX_BLOB_SIZE} bytes; got {kind} with {size} bytes"
            ),
        });
    }
    Ok(())
}

struct BlobChild {
    child: std::process::Child,
    waited: bool,
}

impl Drop for BlobChild {
    fn drop(&mut self) {
        // Oversize, decode and I/O failures must reap the fetch process too.
        if !self.waited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test fixture setup and assertions")]
#[path = "blob_tests.rs"]
mod tests;
