//! File-backed pack installation with owned indexing and cleanup lifetimes.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{INDEX_PACK_TIMEOUT, InstalledPack, PackedFileData};
use crate::core::error::{CrabError, Result};
use crate::git::process;

/// Verify and install a downloaded pack without materializing its body in memory.
pub async fn install_pack_file_locally(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    install_file(
        pack_dir,
        pack_tmp_path,
        canonical_name,
        max_input_size,
        fsck_objects,
        None,
    )
    .await
}

/// Install a downloaded pack, stopping and reaping its indexer on deadline expiry.
pub async fn install_pack_file_locally_with_timeout(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    install_file(
        pack_dir,
        pack_tmp_path,
        canonical_name,
        max_input_size,
        fsck_objects,
        Some(INDEX_PACK_TIMEOUT),
    )
    .await
}

pub(crate) async fn install_generated_pack_file(
    directory: Arc<tempfile::TempDir>,
    pack: Arc<PackedFileData>,
    max_input_size: u64,
) -> Result<InstalledPack> {
    let git_sha1 = gix_hash::ObjectId::from_hex(pack.git_sha1.as_bytes()).map_err(|error| {
        CrabError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    let identity = crab_git::pack::VerifiedPackIdentity {
        git_sha1: git_sha1.as_bytes().try_into().map_err(|error| {
            CrabError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?,
        content_hash: pack.pack_blake3,
    };
    let error_pack_id = pack.pack_blake3_hex.clone();
    // Generation already checked both hashes. Retain its private source and
    // output directory through cancellation, so linking the source is safe.
    run_install(Some(INDEX_PACK_TIMEOUT), error_pack_id, move |cancel| {
        crab_git::pack::install_pack_file_from_path_with_indexer(
            directory.path(),
            pack.pack_path.as_ref(),
            &pack.pack_blake3_hex,
            max_input_size,
            true,
            Some(identity),
            |command, input| run_indexer(command, input, &cancel),
        )
    })
    .await
}

async fn install_file(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
    timeout: Option<Duration>,
) -> Result<InstalledPack> {
    let pack_dir = pack_dir.to_owned();
    let source = pack_tmp_path.to_owned();
    let canonical_name = canonical_name.to_owned();
    let error_pack_id = canonical_name.clone();
    run_install(timeout, error_pack_id, move |cancel| {
        crab_git::pack::install_pack_file_from_path_with_indexer(
            &pack_dir,
            &source,
            &canonical_name,
            max_input_size,
            fsck_objects,
            None,
            |command, input| run_indexer(command, input, &cancel),
        )
    })
    .await
}

/// Repair and install a thin pack, stopping and reaping its indexer on deadline expiry.
pub async fn install_thin_pack_file_locally_with_timeout(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    let pack_dir = pack_dir.to_owned();
    let source = pack_tmp_path.to_owned();
    let canonical_name = canonical_name.to_owned();
    let error_pack_id = canonical_name.clone();
    run_install(Some(INDEX_PACK_TIMEOUT), error_pack_id, move |cancel| {
        crab_git::pack::install_thin_pack_file_from_path_with_indexer(
            &pack_dir,
            &source,
            &canonical_name,
            max_input_size,
            fsck_objects,
            |command, input| run_indexer(command, input, &cancel),
        )
    })
    .await
}

async fn run_install(
    timeout: Option<Duration>,
    error_pack_id: String,
    operation: impl FnOnce(CancellationToken) -> crab_git::pack::Result<InstalledPack> + Send + 'static,
) -> Result<InstalledPack> {
    let cancel = CancellationToken::new();
    let _stop_on_drop = cancel.clone().drop_guard();
    let worker_cancel = cancel.clone();
    let mut task = tokio::task::spawn_blocking(move || operation(worker_cancel));
    let result = if let Some(timeout) = timeout {
        match tokio::time::timeout(timeout, &mut task).await {
            Ok(result) => result,
            Err(_) => {
                // A blocking task cannot be aborted. Its process owner cancels
                // the whole child tree and joins pipes before artifacts or the
                // caller's install fence can be released.
                cancel.cancel();
                match task
                    .await
                    .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
                {
                    Ok(_) | Err(crab_git::pack::PackError::Cancelled) => {
                        return Err(CrabError::Internal(format!(
                            "git index-pack exceeded timeout of {}s",
                            timeout.as_secs()
                        )));
                    }
                    Err(error) => return Err(install_error(error_pack_id, error)),
                }
            }
        }
    } else {
        task.await
    };
    result
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
        .map_err(|error| install_error(error_pack_id, error))
}

fn run_indexer(
    command: Command,
    input: Option<File>,
    cancel: &CancellationToken,
) -> crab_git::pack::Result<Output> {
    let result = process::run(
        command,
        cancel,
        Some(move |mut stdin| {
            if let Some(mut input) = input {
                std::io::copy(&mut input, &mut stdin)?;
            }
            Ok(())
        }),
        |stdout| Ok(process::capture_output(stdout, process::MAX_CAPTURE_BYTES)?),
    );
    match result {
        Ok(output) => Ok(Output {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }),
        Err(CrabError::Cancelled) => Err(crab_git::pack::PackError::Cancelled),
        Err(error) => Err(crab_git::pack::PackError::Io {
            context: "supervise git index-pack".to_owned(),
            source: std::io::Error::other(error),
        }),
    }
}

fn install_error(pack_id: String, error: crab_git::pack::PackError) -> CrabError {
    match error {
        crab_git::pack::PackError::ObjectFsckFailed { git_sha1, stderr } => {
            CrabError::FetchMalformedObject {
                pack_id,
                oid: git_sha1,
                kind: "pack".to_owned(),
                detail: stderr,
            }
        }
        error => CrabError::from(error),
    }
}

#[cfg(test)]
mod tests;
