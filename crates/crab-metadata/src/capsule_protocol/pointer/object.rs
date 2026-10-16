//! Copy-on-write checkpoint catalog roots and immutable dependency pages.
use super::*;
use crate::pages::object::{Builder, Reference};
use crate::pages::{Kind, Rows};

#[cfg(feature = "storage")]
mod reader;
#[cfg(feature = "storage")]
pub(in crate::capsule_protocol) use reader::CatalogPages;

const MAGIC: &[u8; 8] = b"CRBCAT02";
pub(in crate::capsule_protocol) const MAX_DESCRIPTOR_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::capsule_protocol) struct Descriptor {
    version: u32,
    pub(super) files: Option<Reference>,
    pub(super) shards: Option<Reference>,
    pub(super) xorbs: Option<Reference>,
    pub(super) chunks: Option<Reference>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShardRow {
    pub(super) encoded_size: u64,
    pub(super) xorbs: Option<Reference>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct XorbRow {
    pub(super) encoded_size: u64,
    pub(super) body_digest: [u8; 32],
    pub(super) chunks: Reference,
}

impl Descriptor {
    pub(in crate::capsule_protocol) fn validate(&self) -> Result<()> {
        if self.version != 2 {
            return Err(corrupt("unsupported object catalog version"));
        }
        for (root, kind) in [
            (&self.files, Kind::CatalogFiles),
            (&self.shards, Kind::CatalogShards),
            (&self.xorbs, Kind::CatalogXorbs),
            (&self.chunks, Kind::CatalogChunks),
        ] {
            if let Some(root) = root {
                root.validate(kind)?;
            }
        }
        Ok(())
    }

    pub(in crate::capsule_protocol) fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let mut bytes = json(self)?;
        bytes.extend_from_slice(MAGIC);
        if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES {
            return Err(contract_error(
                "object catalog descriptor exceeds its byte limit",
            ));
        }
        Ok(bytes.into())
    }

    #[cfg(feature = "storage")]
    pub(in crate::capsule_protocol) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < MAGIC.len()
            || bytes.len() as u64 > MAX_DESCRIPTOR_BYTES
            || bytes.get(bytes.len() - MAGIC.len()..) != Some(MAGIC.as_slice())
        {
            return Err(corrupt("object catalog descriptor framing is invalid"));
        }
        let descriptor: Self = from_json(&bytes[..bytes.len() - MAGIC.len()])?;
        if descriptor.encode()?.as_ref() != bytes {
            return Err(corrupt("object catalog descriptor is not canonical"));
        }
        Ok(descriptor)
    }

    pub(in crate::capsule_protocol) fn build(
        catalog: &PointerCatalog,
    ) -> Result<(Self, BTreeMap<[u8; 32], Bytes>)> {
        catalog.validate(true, false)?;
        admit_catalog_rows(std::slice::from_ref(catalog), MAX_CATALOG_BYTES)?;
        let mut writer = Builder::default();
        let mut xorbs = Vec::new();
        let mut chunks = BTreeMap::new();
        for (identity, entry) in &catalog.xorbs {
            let identity = hash(identity)?;
            let rows = xorb_chunks(entry, &identity, &mut chunks)?;
            let root = writer
                .tree(Kind::CatalogXorbChunks, rows)?
                .ok_or_else(|| contract_error("object catalog xorb contains no chunks"))?;
            let row = XorbRow {
                encoded_size: entry.encoded_size,
                body_digest: hash(&entry.body_digest)?,
                chunks: root,
            };
            xorbs.push((identity.to_vec(), json(&row)?));
        }
        let mut shards = Vec::new();
        for (identity, entry) in &catalog.shards {
            let nested = shard_xorbs(entry)?;
            let row = ShardRow {
                encoded_size: entry.encoded_size,
                xorbs: writer.tree(Kind::CatalogShardXorbs, nested)?,
            };
            shards.push((hash(identity)?.to_vec(), json(&row)?));
        }
        let descriptor = Self {
            version: 2,
            files: writer.tree(Kind::CatalogFiles, file_rows(catalog)?)?,
            shards: writer.tree(Kind::CatalogShards, shards)?,
            xorbs: writer.tree(Kind::CatalogXorbs, xorbs)?,
            chunks: writer.tree(Kind::CatalogChunks, chunks.into_iter().collect())?,
        };
        let root = descriptor.encode()?;
        writer.objects.insert(*blake3::hash(&root).as_bytes(), root);
        let bytes = writer.objects.values().try_fold(0u64, |sum, body| {
            sum.checked_add(body.len() as u64)
                .ok_or_else(|| contract_error("object catalog byte count overflows"))
        })?;
        if bytes > MAX_CATALOG_BYTES {
            return Err(contract_error(
                "initial object catalog exceeds its byte limit",
            ));
        }
        Ok((descriptor, writer.objects))
    }
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| MetadataError::PointerCatalogRecord { source })
}

#[cfg(feature = "storage")]
fn from_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| MetadataError::PointerCatalogRecord { source })
}

fn file_rows(catalog: &PointerCatalog) -> Result<Rows> {
    catalog
        .files
        .iter()
        .map(|(identity, entry)| {
            let mut value = entry.size.to_be_bytes().to_vec();
            value.extend_from_slice(&hash(&entry.shard_hash)?);
            Ok((hash(identity)?.to_vec(), value))
        })
        .collect()
}

fn shard_xorbs(entry: &ShardCatalogEntry) -> Result<Rows> {
    entry
        .xorb_hashes
        .iter()
        .map(|identity| Ok((hash(identity)?.to_vec(), Vec::new())))
        .collect()
}

fn xorb_chunks(
    entry: &XorbCatalogEntry,
    identity: &[u8; 32],
    chunks: &mut BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Rows> {
    entry
        .chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let index = u32::try_from(index)
                .map_err(|_| contract_error("object xorb ordinal exceeds its bound"))?;
            let key = hash(&chunk.hash)?;
            let mut value = identity.to_vec();
            value.extend_from_slice(&index.to_be_bytes());
            value.extend_from_slice(&chunk.uncompressed_size.to_be_bytes());
            // Catalog iteration is text-ordered; the lowest xorb then its first
            // occurrence owns the reverse placement, matching publication deltas.
            chunks.entry(key.to_vec()).or_insert(value);
            let mut value = key.to_vec();
            value.extend_from_slice(&chunk.uncompressed_size.to_be_bytes());
            Ok((index.to_be_bytes().to_vec(), value))
        })
        .collect()
}

#[cfg(feature = "storage")]
pub(super) fn file_from_bytes(value: &[u8]) -> Result<FileCatalogEntry> {
    if value.len() != 40 {
        return Err(corrupt("catalog file binding length differs"));
    }
    let size = u64::from_be_bytes(
        value[..8]
            .try_into()
            .map_err(|_| corrupt("catalog file size is truncated"))?,
    );
    Ok(FileCatalogEntry::new(size, hex(&value[8..])?))
}

#[cfg(feature = "storage")]
pub(super) fn shard_from_rows(encoded_size: u64, rows: Rows) -> Result<ShardCatalogEntry> {
    if encoded_size == 0 {
        return Err(corrupt("catalog shard is empty"));
    }
    let xorbs = rows
        .into_iter()
        .map(|(key, value)| {
            if !value.is_empty() {
                return Err(corrupt("catalog shard closure has unexpected row data"));
            }
            hex(&key)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ShardCatalogEntry::new(encoded_size, xorbs))
}

#[cfg(feature = "storage")]
pub(super) fn xorb_from_rows(
    encoded_size: u64,
    digest: [u8; 32],
    rows: Rows,
) -> Result<XorbCatalogEntry> {
    if encoded_size == 0 || rows.is_empty() {
        return Err(corrupt("catalog xorb is empty"));
    }
    let chunks = rows
        .into_iter()
        .enumerate()
        .map(|(index, (key, value))| {
            let index = u32::try_from(index)
                .map_err(|_| corrupt("catalog chunk ordinal exceeds its bound"))?;
            if key != index.to_be_bytes() || value.len() != 36 {
                return Err(corrupt("catalog chunk position or size differs"));
            }
            let size = u32::from_be_bytes(
                value[32..]
                    .try_into()
                    .map_err(|_| corrupt("catalog chunk size is truncated"))?,
            );
            if size == 0 {
                return Err(corrupt("catalog contains an empty chunk"));
            }
            Ok(XorbChunkEntry::new(hex(&value[..32])?, size))
        })
        .collect::<Result<_>>()?;
    Ok(XorbCatalogEntry::new(encoded_size, hex(&digest)?, chunks))
}

#[cfg(all(test, feature = "storage"))]
mod tests;
