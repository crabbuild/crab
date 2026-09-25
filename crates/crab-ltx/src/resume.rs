//! Local resume records: the sidecars that make a closed database reusable.
//!
//! A resumed session continues the capture lineage of the file it opens, so the
//! database must hold exactly the state one published position names. The record
//! proves structure (page count, aggregate checksum, dense page checksums); it
//! never proves ownership, which stays with the runtime's own resume record and
//! the authoritative control.

use std::path::{Path, PathBuf};

use crate::{CHECKSUM_FLAG, CrabError, Position, Result};

const CONTINUATION_MAGIC: [u8; 8] = *b"CRABLTC1";
const CONTINUATION_VERSION: u32 = 1;
const CONTINUATION_LEN: usize = 36;
const CHECKSUM_FILE_SUFFIX: &str = ".crab-ltx-checksums";
const CONTINUATION_FILE_SUFFIX: &str = ".crab-ltx-continuation";

/// One capture session's continuation, as recorded beside its database.
pub(crate) struct Continuation {
    pub(crate) position: Position,
    pub(crate) page_size: u32,
    pub(crate) pages: u32,
}

impl Continuation {
    pub(crate) fn encode(&self) -> [u8; CONTINUATION_LEN] {
        let mut bytes = [0u8; CONTINUATION_LEN];
        bytes[..8].copy_from_slice(&CONTINUATION_MAGIC);
        bytes[8..12].copy_from_slice(&CONTINUATION_VERSION.to_be_bytes());
        bytes[12..20].copy_from_slice(&self.position.txid.to_be_bytes());
        bytes[20..28].copy_from_slice(&self.position.checksum.to_be_bytes());
        bytes[28..32].copy_from_slice(&self.page_size.to_be_bytes());
        bytes[32..36].copy_from_slice(&self.pages.to_be_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != CONTINUATION_LEN || bytes[..8] != CONTINUATION_MAGIC {
            return Err(CrabError::InvalidState("unrecognized capture continuation"));
        }
        let version = u32::from_be_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| CrabError::InvalidState("capture continuation version"))?,
        );
        if version != CONTINUATION_VERSION {
            return Err(CrabError::InvalidState(
                "unsupported capture continuation version",
            ));
        }
        let position = Position {
            txid: u64::from_be_bytes(
                bytes[12..20]
                    .try_into()
                    .map_err(|_| CrabError::InvalidState("capture continuation txid"))?,
            ),
            checksum: u64::from_be_bytes(
                bytes[20..28]
                    .try_into()
                    .map_err(|_| CrabError::InvalidState("capture continuation checksum"))?,
            ),
        };
        let page_size = u32::from_be_bytes(
            bytes[28..32]
                .try_into()
                .map_err(|_| CrabError::InvalidState("capture continuation page size"))?,
        );
        let pages = u32::from_be_bytes(
            bytes[32..36]
                .try_into()
                .map_err(|_| CrabError::InvalidState("capture continuation page count"))?,
        );
        if !crate::ltx::is_valid_page_size(page_size)
            || pages == 0
            || position.txid == 0
            || position.checksum & CHECKSUM_FLAG == 0
        {
            return Err(CrabError::InvalidState("invalid capture continuation"));
        }
        Ok(Self {
            position,
            page_size,
            pages,
        })
    }
}

/// Returns the dense page-checksum sidecar for a database file.
pub(crate) fn checksum_path(database: &Path) -> PathBuf {
    suffixed(database, CHECKSUM_FILE_SUFFIX)
}

pub(crate) fn continuation_path(database: &Path) -> PathBuf {
    suffixed(database, CONTINUATION_FILE_SUFFIX)
}

fn suffixed(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    path.into()
}

pub(crate) fn wal_path(database: &Path) -> PathBuf {
    suffixed(database, "-wal")
}

fn journal_path(database: &Path) -> PathBuf {
    suffixed(database, "-journal")
}

/// Reads the continuation recorded beside `database`.
pub(crate) fn read_continuation(host: &crate::Host, database: &Path) -> Result<Continuation> {
    let path = continuation_path(database);
    let mut file = host.filesystem.open(&path)?;
    if file.file_len()? != CONTINUATION_LEN as u64 {
        return Err(CrabError::InvalidState("capture continuation length"));
    }
    Continuation::decode(&file.read_exact_at(0, CONTINUATION_LEN)?)
}

fn remove_if_present(host: &crate::Host, path: &Path) -> Result<()> {
    match host.filesystem.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Writes the dense checksum sidecar that a later open seeds from.
pub(crate) fn write_checksums(
    host: &crate::Host,
    database: &Path,
    checksums: &crate::pages::PageChecksums,
) -> Result<()> {
    let destination = checksum_path(database);
    let scratch = scratch_path(&destination)?;
    let mut file = host.filesystem.create(&scratch)?;
    let written = checksums
        .write_dense(file.as_mut())
        .and_then(|()| file.sync_all().map_err(Into::into));
    drop(file);
    if let Err(error) = written {
        let _ = host.filesystem.remove_file(&scratch);
        return Err(error);
    }
    // The sidecar is a cache of the live index, so replacing it is safe only
    // after the new file is complete and durable.
    let installed = remove_if_present(host, &destination).and_then(|()| {
        host.filesystem
            .persist_file_new(&scratch, &destination)
            .map_err(Into::into)
    });
    if installed.is_err() {
        let _ = host.filesystem.remove_file(&scratch);
    }
    installed
}

/// Writes the continuation that a later [`crate::Db::open_resumed`] continues.
pub(crate) fn write_continuation(
    host: &crate::Host,
    database: &Path,
    continuation: &Continuation,
) -> Result<()> {
    let path = continuation_path(database);
    remove_if_present(host, &path)?;
    host.filesystem.persist_new(&path, &continuation.encode())?;
    Ok(())
}

fn scratch_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let filename = destination
        .file_name()
        .ok_or(CrabError::InvalidState("missing checksum filename"))?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut name = filename.to_owned();
    name.push(format!(
        ".crab-resume-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    Ok(parent.join(name))
}

/// Requires a database that a clean close left checkpointed and complete.
fn require_checkpointed(host: &crate::Host, database: &Path) -> Result<()> {
    let wal = wal_path(database);
    match host.filesystem.file_len(&wal) {
        Ok(0) => {}
        Ok(_) => {
            return Err(CrabError::InvalidState(
                "resume requires a checkpointed database",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if host.filesystem.exists(&journal_path(database))? {
        return Err(CrabError::InvalidState(
            "resume requires a database without a rollback journal",
        ));
    }
    Ok(())
}

/// Moves a resumable database and both sidecars onto a fresh path.
///
/// The caller must already have matched the runtime's resume record against the
/// authoritative control: after this returns, the origin objects are never read.
pub(crate) fn move_resumed(source: &Path, destination: &Path, host: &crate::Host) -> Result<()> {
    require_checkpointed(host, source)?;
    let files = [source, &checksum_path(source), &continuation_path(source)];
    let installed = [
        destination,
        &checksum_path(destination),
        &continuation_path(destination),
    ];
    files
        .iter()
        .zip(installed)
        .try_for_each(|(from, to)| host.filesystem.rename(from, to).map_err(Into::into))
}

/// Removes a resumable database and both sidecars; missing files are ignored.
pub(crate) fn discard_resumed(database: &Path, host: &crate::Host) -> Result<()> {
    remove_if_present(host, &continuation_path(database))?;
    remove_if_present(host, &checksum_path(database))?;
    remove_if_present(host, database)
}
