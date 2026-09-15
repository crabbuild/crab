use std::path::PathBuf;

use bytes::Bytes;
use crab_cell_runtime::{CellId, Control, ControlState};
use crab_storage::{CellStorageLayout, StorageError};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::source::StagedSource;
use super::{SemanticSummary, sqlite_error};

const SCHEMA_VERSION: u32 = 3;
const PAGE_ENTRIES: usize = 256;
const MAX_PAGE_BYTES: u64 = 256 * 1024;
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const MAX_COMPLETE_BYTES: u64 = 64 * 1024;
const PAGE_QUEUE: usize = 4;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceEvidence {
    schema_version: u32,
    operation: String,
    repository: String,
    cell: String,
    objects: u64,
    bytes: u64,
    semantic: SemanticSummary,
    pages: Vec<String>,
    source_digest: String,
}

impl SourceEvidence {
    pub(super) fn source_digest(&self) -> &str {
        &self.source_digest
    }

    fn validate(&self) -> crate::Result<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.operation != canonical_uuid(&self.operation)?
            || self.repository != canonical_uuid(&self.repository)?
            || !canonical_digest(&self.cell)
            || self.pages.is_empty() != (self.objects == 0)
            || self.pages.len() > 8_192
            || self.pages.iter().any(|digest| !canonical_digest(digest))
            || !canonical_digest(&self.semantic.digest)
            || self.source_digest != derive_source_digest(self)
        {
            return Err(crate::Error::Config(
                "legacy repository source evidence is invalid",
            ));
        }
        Ok(())
    }

    fn encode(&self) -> crate::Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(crate::Error::Config(
                "legacy repository source evidence exceeds its limit",
            ));
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct CompleteEvidence {
    schema_version: u32,
    operation: String,
    repository: String,
    cell: String,
    source_digest: String,
    semantic: SemanticSummary,
    incarnation: String,
    root_digest: String,
    txid: u64,
    checksum: String,
    commit_sequence: u64,
}

impl CompleteEvidence {
    pub(super) fn new(
        operation: Uuid,
        repository: Uuid,
        cell: CellId,
        source_digest: &str,
        semantic: SemanticSummary,
        control: &Control,
    ) -> crate::Result<Self> {
        let root = control
            .root
            .as_ref()
            .ok_or(crate::Error::Config("imported control has no root"))?;
        let evidence = Self {
            schema_version: SCHEMA_VERSION,
            operation: operation.hyphenated().to_string(),
            repository: repository.hyphenated().to_string(),
            cell: hex(cell.as_bytes()),
            source_digest: source_digest.to_owned(),
            semantic,
            incarnation: hex(control.incarnation.as_bytes()),
            root_digest: hex(root.digest.as_bytes()),
            txid: root.txid,
            checksum: format!("{:016x}", root.checksum),
            commit_sequence: root.commit_sequence,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub(super) fn semantic(&self) -> &SemanticSummary {
        &self.semantic
    }

    pub(super) fn verify_control(&self, control: &Control) -> crate::Result<()> {
        let root = control
            .root
            .as_ref()
            .ok_or(crate::Error::Config("completed import control has no root"))?;
        if self.cell != hex(control.cell.as_bytes())
            || self.incarnation != hex(control.incarnation.as_bytes())
            || self.root_digest != hex(root.digest.as_bytes())
            || self.txid != root.txid
            || self.checksum != format!("{:016x}", root.checksum)
            || self.commit_sequence != root.commit_sequence
            || control.state == ControlState::Tombstoned
        {
            return Err(crate::Error::Config(
                "completed import evidence differs from Cell control",
            ));
        }
        Ok(())
    }

    fn validate(&self) -> crate::Result<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.operation != canonical_uuid(&self.operation)?
            || self.repository != canonical_uuid(&self.repository)?
            || !canonical_digest(&self.cell)
            || !canonical_digest(&self.source_digest)
            || !canonical_digest(&self.semantic.digest)
            || self.incarnation.len() != 32
            || !self.incarnation.bytes().all(is_lower_hex)
            || !canonical_digest(&self.root_digest)
            || self.checksum.len() != 16
            || !self.checksum.bytes().all(is_lower_hex)
            || self.txid == 0
        {
            return Err(crate::Error::Config(
                "legacy repository completion evidence is invalid",
            ));
        }
        Ok(())
    }

    pub(super) fn encode(&self) -> crate::Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > MAX_COMPLETE_BYTES {
            return Err(crate::Error::Config(
                "legacy repository completion evidence exceeds its limit",
            ));
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> crate::Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        if value.encode()? != bytes {
            return Err(crate::Error::Config(
                "legacy repository completion evidence is not canonical",
            ));
        }
        Ok(value)
    }
}

#[derive(Serialize)]
struct InventoryPage {
    schema_version: u32,
    entries: Vec<InventoryEntry>,
}

#[derive(Serialize)]
struct InventoryEntry {
    path: String,
    size: u64,
    etag: Option<String>,
    version: Option<String>,
    digest: String,
    kind: String,
}

struct EncodedPage {
    digest: String,
    bytes: Vec<u8>,
}

pub(super) async fn publish_source(
    layout: &CellStorageLayout,
    cell: CellId,
    operation: Uuid,
    repository: Uuid,
    source: &StagedSource,
) -> crate::Result<SourceEvidence> {
    let (sender, mut receiver) = mpsc::channel(PAGE_QUEUE);
    let database = source.database().to_path_buf();
    let producer = tokio::task::spawn_blocking(move || encode_pages(database, sender));
    let mut pages = Vec::new();
    while let Some(page) = receiver.recv().await {
        let page = page?;
        let path = layout.migration_path(
            cell.as_bytes(),
            &operation.into_bytes(),
            &format!("inventory/{}.json", page.digest),
        );
        create_exact(layout, &path, &page.bytes, MAX_PAGE_BYTES).await?;
        pages.push(page.digest);
    }
    producer.await??;
    let mut evidence = SourceEvidence {
        schema_version: SCHEMA_VERSION,
        operation: operation.hyphenated().to_string(),
        repository: repository.hyphenated().to_string(),
        cell: hex(cell.as_bytes()),
        objects: source.objects,
        bytes: source.bytes,
        semantic: source.semantic.clone(),
        pages,
        source_digest: String::new(),
    };
    evidence.source_digest = derive_source_digest(&evidence);
    let encoded = evidence.encode()?;
    let path = layout.migration_path(cell.as_bytes(), &operation.into_bytes(), "source.json");
    create_exact(layout, &path, &encoded, MAX_SOURCE_BYTES).await?;
    Ok(evidence)
}

pub(super) async fn publish_complete(
    layout: &CellStorageLayout,
    cell: CellId,
    operation: Uuid,
    complete: &CompleteEvidence,
) -> crate::Result<()> {
    let encoded = complete.encode()?;
    let path = layout.migration_path(cell.as_bytes(), &operation.into_bytes(), "complete.json");
    create_exact(layout, &path, &encoded, MAX_COMPLETE_BYTES).await
}

pub(super) async fn load_complete(
    layout: &CellStorageLayout,
    cell: CellId,
    operation: Uuid,
    repository: Uuid,
) -> crate::Result<Option<CompleteEvidence>> {
    let path = layout.migration_path(cell.as_bytes(), &operation.into_bytes(), "complete.json");
    let bytes = match layout
        .store()
        .get_with_etag_bounded(&path, MAX_COMPLETE_BYTES)
        .await
    {
        Ok((bytes, _)) => bytes,
        Err(StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let evidence = CompleteEvidence::decode(&bytes)?;
    if evidence.operation != operation.hyphenated().to_string()
        || evidence.repository != repository.hyphenated().to_string()
        || evidence.cell != hex(cell.as_bytes())
    {
        return Err(crate::Error::Config(
            "legacy repository completion evidence has the wrong scope",
        ));
    }
    Ok(Some(evidence))
}

fn encode_pages(
    database: PathBuf,
    sender: mpsc::Sender<crate::Result<EncodedPage>>,
) -> crate::Result<()> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite_error)?;
    let mut statement = connection
        .prepare("SELECT path, size, etag, version, digest, kind FROM source_objects ORDER BY path")
        .map_err(sqlite_error)?;
    let mut rows = statement.query([]).map_err(sqlite_error)?;
    let mut entries = Vec::with_capacity(PAGE_ENTRIES);
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        let size: i64 = row.get(1).map_err(sqlite_error)?;
        let digest: Vec<u8> = row.get(4).map_err(sqlite_error)?;
        if digest.len() != 32 {
            return Err(crate::Error::Config(
                "legacy repository inventory digest is invalid",
            ));
        }
        entries.push(InventoryEntry {
            path: row.get(0).map_err(sqlite_error)?,
            size: u64::try_from(size)
                .map_err(|_| crate::Error::Config("legacy repository object size is invalid"))?,
            etag: row.get(2).map_err(sqlite_error)?,
            version: row.get(3).map_err(sqlite_error)?,
            digest: hex(&digest),
            kind: row.get(5).map_err(sqlite_error)?,
        });
        if entries.len() == PAGE_ENTRIES {
            send_page(&sender, std::mem::take(&mut entries))?;
        }
    }
    if !entries.is_empty() {
        send_page(&sender, entries)?;
    }
    Ok(())
}

fn send_page(
    sender: &mpsc::Sender<crate::Result<EncodedPage>>,
    entries: Vec<InventoryEntry>,
) -> crate::Result<()> {
    let bytes = serde_json::to_vec(&InventoryPage {
        schema_version: SCHEMA_VERSION,
        entries,
    })?;
    if bytes.len() as u64 > MAX_PAGE_BYTES {
        return Err(crate::Error::Config(
            "legacy repository inventory page exceeds its limit",
        ));
    }
    let page = EncodedPage {
        digest: blake3::hash(&bytes).to_hex().to_string(),
        bytes,
    };
    sender
        .blocking_send(Ok(page))
        .map_err(|_| crate::Error::Config("legacy repository evidence uploader stopped"))
}

async fn create_exact(
    layout: &CellStorageLayout,
    path: &object_store::path::Path,
    bytes: &[u8],
    max_bytes: u64,
) -> crate::Result<()> {
    match layout
        .store()
        .create_strict(path, Bytes::copy_from_slice(bytes))
        .await
    {
        Ok(()) => Ok(()),
        Err(StorageError::StateConflict { .. }) => {
            let (current, _) = layout
                .store()
                .get_with_etag_bounded(path, max_bytes)
                .await?;
            if current.as_ref() == bytes {
                Ok(())
            } else {
                Err(crate::Error::Config(
                    "legacy repository import operation was reused for different evidence",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn derive_source_digest(evidence: &SourceEvidence) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.import.source.v2\0");
    hasher.update(evidence.repository.as_bytes());
    hasher.update(evidence.cell.as_bytes());
    hasher.update(&evidence.objects.to_be_bytes());
    hasher.update(&evidence.bytes.to_be_bytes());
    hasher.update(evidence.semantic.digest.as_bytes());
    for page in &evidence.pages {
        hasher.update(page.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn canonical_uuid(value: &str) -> crate::Result<String> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.hyphenated().to_string())
        .map_err(|_| crate::Error::Config("legacy repository evidence UUID is invalid"))
}

fn canonical_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(is_lower_hex)
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}
