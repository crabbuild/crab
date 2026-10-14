//! Descriptor-safe file adapter for shared content ingestion.

use crate::ingest::{
    IngestSession, READ_BUF_SIZE, StreamStageHooks, StreamStageProgress, StreamStageResult,
};
use crate::{Result, StagingArea, StagingError as CrabError};
use bytes::BytesMut;
use crab_xet::hash::MerkleHash;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    Ok(())
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Git-index stat fields captured while the staged bytes were verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedIndexStat {
    pub stat: gix_index::entry::Stat,
    pub len: u64,
}

/// Whole-file hash and stat captured by one descriptor-safe verified read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedFileHash {
    pub hash: [u8; 32],
    pub size: u64,
    pub index_stat: VerifiedIndexStat,
}

impl VerifiedIndexStat {
    pub fn from_path_no_follow(path: &Path) -> Option<Self> {
        let metadata = gix_index::fs::Metadata::from_path_no_follow(path).ok()?;
        let stat = gix_index::entry::Stat::from_fs(&metadata).ok()?;
        Some(Self {
            stat,
            len: metadata.len(),
        })
    }

    pub fn from_file(file: &std::fs::File) -> Option<Self> {
        let metadata = gix_index::fs::Metadata::from_file(file).ok()?;
        let stat = gix_index::entry::Stat::from_fs(&metadata).ok()?;
        Some(Self {
            stat,
            len: metadata.len(),
        })
    }
}

async fn open_regular_file_no_follow(path: &Path) -> Result<std::fs::File> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || open_regular_file_no_follow_sync(&path))
        .await
        .map_err(|error| CrabError::Internal(format!("no-follow open task failed: {error}")))?
}

#[cfg(windows)]
fn open_regular_file_no_follow_sync(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    // Inspect and read through one no-follow handle. A path-only check followed
    // by File::open would allow a reparse point to replace the checked file.
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path).map_err(CrabError::Io)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "staging refuses reparse points".to_owned(),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "staging requires a regular file".to_owned(),
        });
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_regular_file_no_follow_sync(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if path.symlink_metadata()?.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "staging refuses symbolic links".to_owned(),
        });
    }

    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return CrabError::Configuration {
                key: path.display().to_string(),
                origin: "staging refuses symbolic links".to_owned(),
            };
        }
        CrabError::Io(error)
    })?;
    if !file.metadata()?.file_type().is_file() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "staging requires a regular file".to_owned(),
        });
    }
    Ok(file)
}

/// Stream a local file into the staging area with bounded memory.
pub async fn stage_file_streaming(
    abs_path: &Path,
    repo_root: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
) -> Result<StreamStageResult> {
    stage_file_streaming_inner(
        abs_path,
        repo_root,
        None,
        None,
        staging,
        progress,
        cancel,
        StreamStageHooks::default(),
    )
    .await
}

/// Stream a local file into staging while recording a caller-provided repo path.
pub async fn stage_file_streaming_as(
    abs_path: &Path,
    repo_root: &Path,
    repo_path: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
) -> Result<StreamStageResult> {
    stage_file_streaming_inner(
        abs_path,
        repo_root,
        Some(repo_path),
        None,
        staging,
        progress,
        cancel,
        StreamStageHooks::default(),
    )
    .await
}

/// Stream an already-open regular file into staging under an explicit repository path.
///
/// The descriptor must permit reads and refer to `abs_path`. This avoids reopening
/// caller-owned temporary files while retaining the same path/descriptor validation.
pub async fn stage_file_streaming_from_descriptor_as(
    mut descriptor: std::fs::File,
    abs_path: &Path,
    repo_root: &Path,
    repo_path: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
) -> Result<StreamStageResult> {
    use std::io::{Seek as _, SeekFrom};

    descriptor.seek(SeekFrom::Start(0))?;
    stage_file_streaming_inner(
        abs_path,
        repo_root,
        Some(repo_path),
        Some(descriptor),
        staging,
        progress,
        cancel,
        StreamStageHooks::default(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn stage_file_streaming_with_hooks(
    abs_path: &Path,
    repo_root: &Path,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
    hooks: StreamStageHooks,
) -> Result<StreamStageResult> {
    stage_file_streaming_inner(
        abs_path, repo_root, None, None, staging, progress, cancel, hooks,
    )
    .await
}

async fn stage_file_streaming_inner(
    abs_path: &Path,
    repo_root: &Path,
    repo_path: Option<&Path>,
    descriptor: Option<std::fs::File>,
    staging: &StagingArea,
    progress: StreamStageProgress,
    cancel: &CancellationToken,
    hooks: StreamStageHooks,
) -> Result<StreamStageResult> {
    let start = Instant::now();
    let rel_path = repo_path.map_or_else(
        || {
            abs_path
                .strip_prefix(repo_root)
                .unwrap_or(abs_path)
                .to_path_buf()
        },
        Path::to_path_buf,
    );
    let descriptor = match descriptor {
        Some(descriptor) => descriptor,
        None => open_regular_file_no_follow(abs_path)
            .await
            .map_err(|error| with_path(abs_path, error))?,
    };
    let before_stream_stat =
        VerifiedIndexStat::from_file(&descriptor).ok_or_else(|| CrabError::Configuration {
            key: abs_path.display().to_string(),
            origin: "staging could not capture descriptor identity".to_owned(),
        })?;
    if VerifiedIndexStat::from_path_no_follow(abs_path) != Some(before_stream_stat) {
        return Err(CrabError::FileChangedDuringStaging {
            path: abs_path.display().to_string(),
            first_hash: "unread".to_owned(),
            second_hash: "unread".to_owned(),
            first_size: before_stream_stat.len,
            second_size: VerifiedIndexStat::from_path_no_follow(abs_path).map_or(0, |s| s.len),
        });
    }
    let prepared = staging.local_recipe_for_path(&rel_path)?;
    #[cfg(test)]
    let after_input_read = hooks.after_input_read.clone();
    let mut read_file = tokio::fs::File::from_std(descriptor.try_clone()?);
    let mut session =
        IngestSession::begin_with_hooks(staging, &rel_path, progress, cancel, prepared, hooks)
            .await?;
    let read = async {
        loop {
            check_cancelled(cancel)?;
            let mut bytes = BytesMut::with_capacity(READ_BUF_SIZE);
            if read_file.read_buf(&mut bytes).await? == 0 {
                break;
            }
            session.feed(bytes.freeze()).await?;
            #[cfg(test)]
            if let Some(after_read) = &after_input_read {
                after_read();
            }
        }
        Ok::<_, CrabError>(())
    }
    .await;
    if let Err(error) = read {
        session.abort().await?;
        return Err(with_path(abs_path, error));
    }
    let completed = session.finish_input().await?;
    let after = VerifiedIndexStat::from_file(&descriptor);
    if after != Some(before_stream_stat)
        || VerifiedIndexStat::from_path_no_follow(abs_path) != after
    {
        let first_hash = completed.file_hash();
        let first_size = completed.size();
        completed.abort().await?;
        let second = stream_hash_file(abs_path, None, cancel).await?;
        return Err(CrabError::FileChangedDuringStaging {
            path: abs_path.display().to_string(),
            first_hash: MerkleHash::from(first_hash).hex(),
            second_hash: MerkleHash::from(second.hash).hex(),
            first_size,
            second_size: second.size,
        });
    }
    let mut result = completed.seal().await?;
    result.abs_path = abs_path.to_path_buf();
    result.index_stat = after.filter(|stat| stat.len == result.size);
    result.duration_ms = duration_ms(start.elapsed());
    Ok(result)
}

pub async fn stream_hash_file(
    path: &Path,
    bytes_done: Option<&Arc<AtomicU64>>,
    cancel: &CancellationToken,
) -> Result<VerifiedFileHash> {
    let descriptor = open_regular_file_no_follow(path).await?;
    let before = VerifiedIndexStat::from_file(&descriptor);
    if VerifiedIndexStat::from_path_no_follow(path) != before {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "file path changed during descriptor-safe open".to_owned(),
        });
    }
    let mut file = tokio::fs::File::from_std(descriptor.try_clone()?);
    let mut buf = vec![0u8; READ_BUF_SIZE];
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;

    loop {
        check_cancelled(cancel)?;
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        if let Some(counter) = bytes_done {
            counter.fetch_add(n as u64, Relaxed);
        }
    }

    let after = VerifiedIndexStat::from_file(&descriptor);
    if before != after || VerifiedIndexStat::from_path_no_follow(path) != after {
        return Err(CrabError::FileChangedDuringStaging {
            path: path.display().to_string(),
            first_hash: MerkleHash::from(*hasher.finalize().as_bytes()).hex(),
            second_hash: "changed".to_owned(),
            first_size: total,
            second_size: after.map_or(0, |stat| stat.len),
        });
    }
    let index_stat = after.ok_or_else(|| CrabError::Configuration {
        key: path.display().to_string(),
        origin: "file stat unavailable after descriptor-safe hash".to_owned(),
    })?;
    Ok(VerifiedFileHash {
        hash: *hasher.finalize().as_bytes(),
        size: total,
        index_stat,
    })
}

fn with_path(path: &Path, err: CrabError) -> CrabError {
    match err {
        CrabError::Io(e) => CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        )),
        other => other,
    }
}
