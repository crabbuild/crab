//! Local resume records: the same-node wake that skips a restore.
//!
//! One record is written beside a database that a clean release left behind,
//! and it names exactly the published root that database still holds. It is an
//! accelerator, never authority: an activation still re-verifies the database
//! against the observed control before the Cell serves anything, and any
//! mismatch discards the local image and restores the exact root instead. Every
//! failure below therefore degrades one wake to a restore and never fails it.

use std::io;
use std::path::{Path, PathBuf};

use crate::control::Control;
use crate::identity::{CellId, Digest, IncarnationId};
use crate::{Error, Result};

const RECORD_MAGIC: [u8; 8] = *b"CRABRES1";
const RECORD_VERSION: u32 = 1;
const RECORD_SUFFIX: &str = ".resume";
const RECORD_HEADER_BYTES: usize = 154;
const MAX_DATABASE_NAME_BYTES: usize = 255;

/// Identity of the local database one clean release left resumable.
pub(crate) struct ResumeRecord {
    cell: CellId,
    incarnation: IncarnationId,
    schema: u32,
    code: Digest,
    root: crate::control::RootRef,
    database: String,
}

impl ResumeRecord {
    pub(crate) fn new(
        cell: CellId,
        incarnation: IncarnationId,
        schema: u32,
        code: Digest,
        root: crate::control::RootRef,
        database: &Path,
    ) -> Result<Self> {
        let name = database
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Control(
                "resume database path has no UTF-8 file name",
            ))?;
        if name.len() > MAX_DATABASE_NAME_BYTES {
            return Err(Error::Control("resume database file name is too long"));
        }
        Ok(Self {
            cell,
            incarnation,
            schema,
            code,
            root,
            database: name.to_owned(),
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(RECORD_HEADER_BYTES + self.database.len());
        bytes.extend_from_slice(&RECORD_MAGIC);
        bytes.extend_from_slice(&RECORD_VERSION.to_be_bytes());
        bytes.extend_from_slice(&self.schema.to_be_bytes());
        bytes.extend_from_slice(self.code.as_bytes());
        bytes.extend_from_slice(self.cell.as_bytes());
        bytes.extend_from_slice(self.incarnation.as_bytes());
        bytes.extend_from_slice(self.root.digest.as_bytes());
        bytes.extend_from_slice(&self.root.txid.to_be_bytes());
        bytes.extend_from_slice(&self.root.checksum.to_be_bytes());
        bytes.extend_from_slice(&self.root.commit_sequence.to_be_bytes());
        bytes.extend_from_slice(&(self.database.len() as u16).to_be_bytes());
        bytes.extend_from_slice(self.database.as_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < RECORD_HEADER_BYTES || bytes[..8] != RECORD_MAGIC {
            return Err(Error::Control("unrecognized Cell resume record"));
        }
        let version = u32::from_be_bytes(array(bytes, 8)?);
        if version != RECORD_VERSION {
            return Err(Error::Control("unsupported Cell resume record version"));
        }
        let name_len = usize::from(u16::from_be_bytes(array(bytes, 152)?));
        if name_len == 0
            || name_len > MAX_DATABASE_NAME_BYTES
            || bytes.len() != RECORD_HEADER_BYTES + name_len
        {
            return Err(Error::Control("Cell resume record length"));
        }
        let database = std::str::from_utf8(&bytes[RECORD_HEADER_BYTES..])?.to_owned();
        if database.contains(['/', '\\']) {
            return Err(Error::Control("Cell resume record escapes its directory"));
        }
        Ok(Self {
            schema: u32::from_be_bytes(array(bytes, 12)?),
            code: Digest::from_bytes(array(bytes, 16)?),
            cell: CellId::from_bytes(array(bytes, 48)?),
            incarnation: IncarnationId::from_bytes(array(bytes, 80)?),
            root: crate::control::RootRef {
                digest: Digest::from_bytes(array(bytes, 96)?),
                txid: u64::from_be_bytes(array(bytes, 128)?),
                checksum: u64::from_be_bytes(array(bytes, 136)?),
                commit_sequence: u64::from_be_bytes(array(bytes, 144)?),
            },
            database,
        })
    }

    /// Reports whether this record names exactly what the observed control names.
    ///
    /// The ownership epoch is deliberately absent: acquiring an idle Cell raises
    /// the epoch without touching the root, and every writer that did commit
    /// moves the root digest instead.
    pub(crate) fn matches(&self, control: &Control) -> bool {
        control.cell == self.cell
            && control.incarnation == self.incarnation
            && control.schema == self.schema
            && control.code == self.code
            && control.root.as_ref() == Some(&self.root)
    }

    fn database_path(&self, directory: &Path) -> PathBuf {
        directory.join(&self.database)
    }
}

fn array<const N: usize>(bytes: &[u8], start: usize) -> Result<[u8; N]> {
    bytes
        .get(start..start + N)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(Error::Control("Cell resume record length"))
}

/// Returns the resume record path that belongs to a database.
pub(crate) fn record_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(RECORD_SUFFIX);
    path.into()
}

/// Writes the record for a database that a clean release just closed.
pub(crate) fn write(record: &ResumeRecord, database: &Path) {
    let path = record_path(database);
    if let Err(error) = std::fs::write(&path, record.encode()) {
        tracing::debug!(error = %error, "Cell resume record was not written");
    }
}

fn remove_file(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(error = %error, "Cell resume artifact was not removed");
    }
}

/// Removes one record and everything it names.
pub(crate) fn discard(record: &Path, database: &Path, replica: &crab_ltx::CellReplica) {
    if let Err(error) = replica.discard_resumed(database) {
        tracing::debug!(error = %error, "Cell resume database was not discarded");
    }
    remove_file(record);
}

/// Consumes the record that still names the observed control's root.
///
/// Every other record in the directory is discarded with the database it names,
/// so a rejected candidate cannot accumulate. The returned database keeps its
/// files: the caller either opens them or discards them.
pub(crate) fn take_matching(
    destination: &Path,
    control: &Control,
    replica: &crab_ltx::CellReplica,
) -> Option<PathBuf> {
    let directory = destination.parent()?;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::debug!(error = %error, "Cell resume directory was not read");
            return None;
        }
    };
    let mut matched = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_record = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(RECORD_SUFFIX));
        if !is_record {
            continue;
        }
        let record = std::fs::read(&path)
            .ok()
            .and_then(|bytes| ResumeRecord::decode(&bytes).ok());
        let Some(record) = record else {
            discard(&path, &database_of_record(&path), replica);
            continue;
        };
        let database = record.database_path(directory);
        if record.matches(control) && matched.is_none() {
            remove_file(&path);
            matched = Some(database);
            continue;
        }
        discard(&path, &database, replica);
    }
    matched
}

fn database_of_record(record: &Path) -> PathBuf {
    let name = record
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    record.with_file_name(name.strip_suffix(RECORD_SUFFIX).unwrap_or_default())
}
