//! Captured logical catalog reduction with file precedence and deferred closure checks.
use super::relations::Record as Relation;
use super::spill::{Cursor, Reducer, Row, Run};
use super::*;
use crate::capsule_protocol::{Capsule, CapsuleControl, CheckpointCatalog};
use crab_storage::{Store, StoreLayout, scratch::ScratchSpace};

const MAGIC: &[u8; 8] = b"CRBCLG01";
const HEADER: usize = 45;

pub(super) struct Entry {
    key: [u8; 33],
    rank: u64,
    value: Bytes,
}

impl Entry {
    fn external_object(self) -> Result<PointerCatalogObject> {
        let hash = hex(&self.key[1..])?;
        let size = u64::from_be_bytes(
            self.value[..8]
                .try_into()
                .map_err(|_| corrupt("object size is truncated"))?,
        );
        let object = match self.key[0] {
            1 => PointerCatalogObject::Shard {
                hash,
                entry: ShardCatalogEntry::new(
                    size,
                    self.value[8..]
                        .as_chunks::<32>()
                        .0
                        .iter()
                        .map(|value| hex(value))
                        .collect::<Result<_>>()?,
                ),
            },
            2 => PointerCatalogObject::Xorb {
                hash,
                entry: XorbCatalogEntry::new(
                    size,
                    hex(&self.value[8..40])?,
                    self.value[40..]
                        .as_chunks::<36>()
                        .0
                        .iter()
                        .map(|chunk| {
                            Ok(XorbChunkEntry::new(
                                hex(&chunk[..32])?,
                                u32::from_be_bytes(
                                    chunk[32..]
                                        .try_into()
                                        .map_err(|_| corrupt("chunk size is truncated"))?,
                                ),
                            ))
                        })
                        .collect::<Result<_>>()?,
                ),
            },
            _ => return Err(corrupt("logical catalog object kind differs")),
        };
        Ok(object)
    }
    fn encode_rows<'a>(rows: impl ExactSizeIterator<Item = &'a Self>) -> Result<Bytes> {
        let count = rows.len();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(count as u32).to_be_bytes());
        for row in rows {
            row.validate()?;
            bytes.extend_from_slice(&row.key);
            bytes.extend_from_slice(&row.rank.to_be_bytes());
            let size = u32::try_from(row.value.len())
                .map_err(|_| corrupt("logical spill value exceeds its length field"))?;
            bytes.extend_from_slice(&size.to_be_bytes());
            bytes.extend_from_slice(&row.value);
        }
        if count == 0 || count > 4096 || bytes.len() as u64 > Self::MAX_BYTES {
            return Err(corrupt("logical catalog spill block exceeds its bound"));
        }
        Ok(bytes.into())
    }
    fn new(kind: u8, identity: &str, rank: u64, value: Vec<u8>) -> Result<Self> {
        let mut key = [0; 33];
        key[0] = kind;
        key[1..].copy_from_slice(&hash(identity)?);
        let row = Self {
            key,
            rank,
            value: value.into(),
        };
        row.validate()?;
        Ok(row)
    }

    fn validate(&self) -> Result<()> {
        let value = &self.value;
        let valid = match self.key[0] {
            0 => value.len() == 40,
            1 => {
                value.len() >= 8
                    && (value.len() - 8).is_multiple_of(32)
                    && value[..8] != [0; 8]
                    && value[8..]
                        .as_chunks::<32>()
                        .0
                        .windows(2)
                        .all(|pair| pair[0] < pair[1])
            }
            2 => {
                value.len() >= 76
                    && (value.len() - 40).is_multiple_of(36)
                    && value[..8] != [0; 8]
                    && value[40..]
                        .as_chunks::<36>()
                        .0
                        .iter()
                        .all(|chunk| chunk[32..] != [0; 4])
            }
            _ => false,
        };
        if !valid || self.bytes() as u64 > Self::MAX_BYTES - 12 {
            return Err(corrupt("logical catalog spill value has an invalid shape"));
        }
        Ok(())
    }
}

impl Row for Entry {
    type Key = [u8; 33];
    const TARGET_BYTES: usize = 512 * 1024;
    const MAX_BYTES: u64 = MAX_CATALOG_BYTES + 128;
    fn key(&self) -> &[u8; 33] {
        &self.key
    }
    fn bytes(&self) -> usize {
        HEADER.saturating_add(self.value.len())
    }
    fn merge(&mut self, other: Self) -> Result<()> {
        let identity = hex(&self.key[1..])?;
        if self.key[0] == 0 {
            let size = |bytes: &[u8]| -> Result<u64> {
                Ok(u64::from_be_bytes(
                    bytes[..8]
                        .try_into()
                        .map_err(|_| corrupt("file size is truncated"))?,
                ))
            };
            validate_binding_size(&identity, size(&self.value)?, size(&other.value)?)?;
            if other.rank > self.rank {
                *self = other;
            }
        } else {
            let kind = if self.key[0] == 1 { "shard" } else { "xorb" };
            validate_consistent(Some(&self.value), &other.value, &identity, kind)?;
        }
        Ok(())
    }
    fn encode(rows: &[Self]) -> Result<Bytes> {
        Self::encode_rows(rows.iter())
    }
    fn decode(bytes: Bytes) -> Result<Vec<Self>> {
        if bytes.len() < 12 || bytes.get(..8) != Some(MAGIC.as_slice()) {
            return Err(corrupt("logical catalog spill framing differs"));
        }
        let count = u32::from_be_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| corrupt("logical spill count is truncated"))?,
        ) as usize;
        if count == 0 || count > 4096 {
            return Err(corrupt("logical spill count exceeds its bound"));
        }
        let mut rows = Vec::with_capacity(count);
        let mut offset = 12;
        for _ in 0..count {
            let header = bytes
                .get(offset..offset + HEADER)
                .ok_or_else(|| corrupt("logical spill header is truncated"))?;
            let key = header[..33]
                .try_into()
                .map_err(|_| corrupt("logical spill key is truncated"))?;
            let rank = u64::from_be_bytes(
                header[33..41]
                    .try_into()
                    .map_err(|_| corrupt("logical spill rank is truncated"))?,
            );
            let length = u32::from_be_bytes(
                header[41..]
                    .try_into()
                    .map_err(|_| corrupt("logical spill size is truncated"))?,
            ) as usize;
            offset += HEADER;
            let end = offset
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| corrupt("logical spill value escapes its block"))?;
            let row = Self {
                key,
                rank,
                value: bytes.slice(offset..end),
            };
            row.validate()?;
            rows.push(row);
            offset = end;
        }
        if offset != bytes.len() {
            return Err(corrupt("logical spill block has trailing bytes"));
        }
        Ok(rows)
    }
}

pub(in crate::capsule_protocol) struct Inspector {
    space: Option<ScratchSpace>,
    rows: Reducer<Entry>,
    maximum: u64,
}

impl Inspector {
    pub(in crate::capsule_protocol) fn new(maximum: u64) -> Self {
        Self {
            space: None,
            rows: Reducer::new(),
            maximum,
        }
    }
    async fn add(&mut self, row: Entry) -> Result<()> {
        if self.space.is_none() {
            self.space = Some(ScratchSpace::temporary(self.maximum.saturating_mul(16)).await?);
        }
        let space = self
            .space
            .as_ref()
            .ok_or_else(|| corrupt("logical spill has no workspace"))?;
        self.rows.add(space, row).await
    }
    pub(super) async fn file(
        &mut self,
        identity: &str,
        entry: FileCatalogEntry,
        rank: u64,
    ) -> Result<()> {
        let mut value = entry.size.to_be_bytes().to_vec();
        value.extend_from_slice(&hash(&entry.shard_hash)?);
        self.add(Entry::new(0, identity, rank, value)?).await
    }
    pub(super) async fn shard(
        &mut self,
        identity: &str,
        entry: ShardCatalogEntry,
        rank: u64,
    ) -> Result<()> {
        let mut value = entry.encoded_size.to_be_bytes().to_vec();
        for xorb in entry.xorb_hashes {
            value.extend_from_slice(&hash(&xorb)?);
        }
        self.add(Entry::new(1, identity, rank, value)?).await
    }
    pub(super) async fn xorb(
        &mut self,
        identity: &str,
        entry: XorbCatalogEntry,
        rank: u64,
    ) -> Result<()> {
        let mut value = entry.encoded_size.to_be_bytes().to_vec();
        value.extend_from_slice(&hash(&entry.body_digest)?);
        for chunk in entry.chunks {
            value.extend_from_slice(&hash(&chunk.hash)?);
            value.extend_from_slice(&chunk.uncompressed_size.to_be_bytes());
        }
        self.add(Entry::new(2, identity, rank, value)?).await
    }
    pub(in crate::capsule_protocol) async fn delta(
        &mut self,
        catalog: PointerCatalog,
        rank: u64,
    ) -> Result<()> {
        catalog.validate(false, false)?;
        for (hash, entry) in catalog.files {
            self.file(&hash, entry, rank).await?;
        }
        for (hash, entry) in catalog.shards {
            self.shard(&hash, entry, rank).await?;
        }
        for (hash, entry) in catalog.xorbs {
            self.xorb(&hash, entry, rank).await?;
        }
        Ok(())
    }
    pub(in crate::capsule_protocol) async fn finish(
        self,
        prior: Result<()>,
    ) -> Result<PointerCatalogInspection> {
        let Some(space) = self.space else {
            prior?;
            return Ok(PointerCatalogInspection {
                space: None,
                rows: Cursor::new(Run::default()),
                counts: PointerCatalogCounts::default(),
                payload_bytes: 0,
                checkpoint_catalog: (0, 0),
            });
        };
        let result: Result<(Run, PointerCatalogCounts, u64)> = async {
            prior?;
            let run = self.rows.finish(&space).await?;
            let mut cursor = Cursor::<Entry>::new(run.clone());
            let mut dependencies = Reducer::<Relation>::new();
            let mut counts = PointerCatalogCounts::default();
            let mut payload_bytes = 0_u64;
            while let Some(row) = cursor.next(&space).await? {
                let identity = &row.key[1..];
                let count = match row.key[0] {
                    0 => &mut counts.files,
                    1 => &mut counts.shards,
                    2 => &mut counts.xorbs,
                    _ => return Err(corrupt("logical catalog row kind differs")),
                };
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| corrupt("catalog count overflows"))?;
                if row.key[0] != 0 {
                    let size = u64::from_be_bytes(
                        row.value[..8]
                            .try_into()
                            .map_err(|_| corrupt("catalog payload size is truncated"))?,
                    );
                    payload_bytes = payload_bytes
                        .checked_add(size)
                        .ok_or_else(|| corrupt("catalog payload byte count overflows"))?;
                }
                match row.key[0] {
                    0 => {
                        dependencies
                            .add(&space, Relation::reference(0, &row.value[8..], false)?)
                            .await?
                    }
                    1 => {
                        dependencies
                            .add(&space, Relation::reference(0, identity, true)?)
                            .await?;
                        for xorb in row.value[8..].as_chunks::<32>().0 {
                            dependencies
                                .add(&space, Relation::reference(1, xorb, false)?)
                                .await?;
                        }
                    }
                    2 => {
                        dependencies
                            .add(&space, Relation::reference(1, identity, true)?)
                            .await?
                    }
                    _ => return Err(corrupt("logical catalog row kind differs")),
                }
            }
            let dependencies = dependencies.finish(&space).await?;
            let mut cursor = Cursor::<Relation>::new(dependencies.clone());
            while let Some(row) = cursor.next(&space).await? {
                row.validate()?;
            }
            dependencies.remove(&space).await?;
            Ok((run, counts, payload_bytes))
        }
        .await;
        match result {
            Ok((run, counts, payload_bytes)) => Ok(PointerCatalogInspection {
                space: Some(space),
                rows: Cursor::new(run),
                counts,
                payload_bytes,
                checkpoint_catalog: (0, 0),
            }),
            Err(error) => finish(Some(space), Err(error)).await,
        }
    }
}

/// One immutable external object authenticated by a complete captured catalog.
#[derive(Debug, PartialEq, Eq)]
pub enum PointerCatalogObject {
    Shard {
        hash: String,
        entry: ShardCatalogEntry,
    },
    Xorb {
        hash: String,
        entry: XorbCatalogEntry,
    },
}

/// Cardinalities of one complete, precedence-resolved captured catalog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerCatalogCounts {
    pub files: u64,
    pub shards: u64,
    pub xorbs: u64,
}

/// A complete catalog proof with bounded iteration over its external object descriptors.
///
/// The private spill is owned until `finish` drains and removes it. Dropping the
/// cursor retains cleanup behind blocking workers. No origin writes or recapture occur.
#[must_use]
pub struct PointerCatalogInspection {
    space: Option<ScratchSpace>,
    rows: Cursor<Entry>,
    counts: PointerCatalogCounts,
    payload_bytes: u64,
    checkpoint_catalog: (u64, u64),
}

impl PointerCatalogInspection {
    /// Return complete catalog cardinalities, independent of cursor consumption.
    #[must_use]
    pub fn counts(&self) -> PointerCatalogCounts {
        self.counts
    }

    /// Return the declared encoded bytes of all distinct shard and xorb bodies.
    ///
    /// The total comes from the complete normalized capture, independent of
    /// iteration progress. It does not certify that those bodies are readable.
    #[must_use]
    pub fn encoded_payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    /// Return distinct checkpoint catalog objects and their encoded bytes authenticated by this capture.
    ///
    /// Captures without a checkpoint catalog return `(0, 0)`. Embedded run
    /// catalogs and external shard/xorb bodies are excluded. No I/O occurs.
    #[must_use]
    pub fn checkpoint_catalog_footprint(&self) -> (u64, u64) {
        self.checkpoint_catalog
    }

    async fn next_entry(&mut self) -> Result<Option<Entry>> {
        match &self.space {
            Some(space) => self.rows.next(space).await,
            None => Ok(None),
        }
    }

    /// Return the next shard or xorb in canonical hash order within each kind.
    ///
    /// Complete metadata validation precedes construction. One typed descriptor
    /// is returned at a time; its recipe may reach the admitted catalog bound.
    /// Cancelling a pending call leaves the next object available on resume.
    pub async fn next_object(&mut self) -> Result<Option<PointerCatalogObject>> {
        while let Some(row) = self.next_entry().await? {
            if row.key[0] == 0 {
                continue;
            }
            return row.external_object().map(Some);
        }
        Ok(None)
    }

    /// Collect the remaining shard identities without decoding object recipes, then close.
    pub async fn into_shard_hashes(mut self) -> Result<Vec<String>> {
        let result = async {
            let mut hashes = Vec::new();
            while let Some(row) = self.next_entry().await? {
                if row.key[0] == 1 {
                    hashes.push(hex(&row.key[1..])?);
                }
            }
            Ok(hashes)
        }
        .await;
        self.finish(result).await
    }

    /// Close the private spill after an operation, preserving its primary error.
    ///
    /// Await completion on success and returned-error paths. A cleanup failure
    /// becomes the result only when the operation itself succeeded.
    pub async fn finish<T, E: From<MetadataError>>(
        self,
        result: std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        finish(self.space, result).await
    }
}

async fn finish<T, E: From<MetadataError>>(
    space: Option<ScratchSpace>,
    result: std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let cleanup = match space {
        Some(space) => space.close().await.map_err(MetadataError::from),
        None => Ok(()),
    };
    if let (Err(_), Err(source)) = (&result, &cleanup) {
        tracing::warn!(%source, "logical catalog spill cleanup failed after operation error");
    }
    let result = result?;
    cleanup.map_err(E::from)?;
    Ok(result)
}

/// Validate one ordered captured catalog before opening its external object cursor.
///
/// Checkpoint rows stream through bounded spill; each packed delta retains its
/// existing format-bounded decoder. Conflicts and incomplete final dependencies
/// fail before a cursor is returned. Await cursor `finish` on every returned path.
pub async fn inspect_pointer_catalog(
    layout: &StoreLayout<Store>,
    checkpoint: Option<&CheckpointCatalog>,
    capsules: &[Capsule],
    controls: &[CapsuleControl],
    maximum: u64,
) -> Result<PointerCatalogInspection> {
    let maximum = maximum.min(MAX_CATALOG_BYTES);
    let mut inspector = Inspector::new(maximum);
    let mut checkpoint_catalog = (0, 0);
    let checked = async {
        if let Some(checkpoint) = checkpoint {
            checkpoint_catalog = checkpoint
                .reader(layout)?
                .inspect_into(layout.store(), maximum, &mut inspector)
                .await?;
        }
        let mut rank = 1u64;
        for capsule in capsules {
            if let Some(delta) = capsule.pointer_catalog_delta()? {
                inspector.delta(delta, rank).await?;
            }
            rank = rank
                .checked_add(1)
                .ok_or_else(|| corrupt("catalog capture rank overflows"))?;
        }
        for control in controls {
            if let Some(delta) = control.materialize_catalog(layout.store()).await? {
                inspector.delta(delta, rank).await?;
            }
            rank = rank
                .checked_add(1)
                .ok_or_else(|| corrupt("catalog capture rank overflows"))?;
        }
        Ok(())
    }
    .await;
    let mut inspection = inspector.finish(checked).await?;
    inspection.checkpoint_catalog = checkpoint_catalog;
    Ok(inspection)
}

mod grouped;
pub use grouped::{PointerCatalogEntry, PointerCatalogShardInspection};

#[cfg(test)]
mod tests;
