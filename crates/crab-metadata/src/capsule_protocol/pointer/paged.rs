//! Canonical pages for checkpoint catalogs and publication deltas.
use super::*;
use crate::pages::{Kind, PageRef, Rows, Writer};

const MAGIC: &[u8; 8] = b"CRBPCP01";
const TRAILER_BYTES: usize = 12;
const MAX_DESCRIPTOR_BYTES: usize = 4096;
const MAX_CATALOG_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Descriptor {
    version: u32,
    body_bytes: u64,
    body_hash: [u8; 32],
    files: Option<PageRef>,
    shards: Option<PageRef>,
    xorbs: Option<PageRef>,
    chunks: Option<PageRef>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShardRow {
    encoded_size: u64,
    xorbs: Option<PageRef>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct XorbRow {
    encoded_size: u64,
    body_digest: [u8; 32],
    chunks: PageRef,
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| MetadataError::PointerCatalogRecord { source })
}

fn from_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| MetadataError::PointerCatalogRecord { source })
}

fn hash(value: &str) -> Result<[u8; 32]> {
    let bytes = value.as_bytes();
    // Binary keys cannot distinguish hex aliases. Reject them instead of silently
    // merging two descriptors that were distinct in the caller's map.
    if bytes.len() != 64
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte))
    {
        return Err(contract_error(
            "catalog hashes must be lowercase canonical hex",
        ));
    }
    // Encode text order directly: Xet's raw hash bytes reverse each u64 word.
    // Using those bytes as keys would change the original BTreeMap precedence.
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        _ => byte - b'a' + 10,
    };
    Ok(std::array::from_fn(|index| {
        (digit(bytes[index * 2]) << 4) | digit(bytes[index * 2 + 1])
    }))
}

fn hex(bytes: &[u8]) -> Result<String> {
    if bytes.len() != 32 {
        return Err(corrupt("catalog hash has an invalid byte length"));
    }
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in bytes {
        value.push(DIGITS[(byte >> 4) as usize] as char);
        value.push(DIGITS[(byte & 15) as usize] as char);
    }
    Ok(value)
}

fn tree(writer: &mut Writer, kind: Kind, rows: Rows) -> Result<Option<PageRef>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let root = writer.tree(kind, rows)?;
    if writer.bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(contract_error("catalog exceeds its encoded byte bound"));
    }
    Ok(Some(root))
}

pub(super) fn encode(catalog: &PointerCatalog) -> Result<Bytes> {
    // Admit the unavoidable row payload before duplicating any caller-owned
    // lists. Page headers and directories are checked again after encoding.
    let minimum = catalog.xorbs.values().try_fold(0u64, |bytes, xorb| {
        bytes.checked_add((xorb.chunks.len() as u64).checked_mul(44)?)
    });
    let minimum = minimum.and_then(|minimum| {
        catalog.shards.values().try_fold(minimum, |bytes, shard| {
            bytes.checked_add((shard.xorb_hashes.len() as u64).checked_mul(36)?)
        })
    });
    let minimum =
        minimum.and_then(|bytes| bytes.checked_add((catalog.files.len() as u64).checked_mul(76)?));
    if minimum.is_none_or(|bytes| bytes > MAX_CATALOG_BYTES) {
        return Err(contract_error(
            "catalog rows exceed their encoded byte bound",
        ));
    }
    let mut writer = Writer::default();
    let mut chunks = BTreeMap::new();
    let mut xorbs = Vec::new();
    for (xorb_hash, entry) in &catalog.xorbs {
        let xorb_hash = hash(xorb_hash)?;
        let mut rows = Vec::new();
        for (index, chunk) in entry.chunks.iter().enumerate() {
            let index = u32::try_from(index)
                .map_err(|_| contract_error("xorb chunk position exceeds its bound"))?;
            let chunk_hash = hash(&chunk.hash)?;
            let mut value = chunk_hash.to_vec();
            value.extend_from_slice(&chunk.uncompressed_size.to_be_bytes());
            rows.push((index.to_be_bytes().to_vec(), value));
            // Match the original BTreeMap scan: lowest xorb identity wins, then
            // its first occurrence. Repeated chunks retain their full recipe.
            chunks.entry(chunk_hash.to_vec()).or_insert_with(|| {
                let mut value = xorb_hash.to_vec();
                value.extend_from_slice(&index.to_be_bytes());
                value.extend_from_slice(&chunk.uncompressed_size.to_be_bytes());
                value
            });
        }
        let chunks = tree(&mut writer, Kind::CatalogXorbChunks, rows)?
            .ok_or_else(|| contract_error("catalog xorb contains no chunks"))?;
        let row = XorbRow {
            encoded_size: entry.encoded_size,
            body_digest: hash(&entry.body_digest)?,
            chunks,
        };
        xorbs.push((xorb_hash.to_vec(), json(&row)?));
    }
    let xorbs = tree(&mut writer, Kind::CatalogXorbs, xorbs)?;
    let chunks = tree(
        &mut writer,
        Kind::CatalogChunks,
        chunks.into_iter().collect(),
    )?;
    let mut shards = Vec::new();
    for (shard_hash, entry) in &catalog.shards {
        let rows = entry
            .xorb_hashes
            .iter()
            .map(|value| Ok((hash(value)?.to_vec(), Vec::new())))
            .collect::<Result<_>>()?;
        let row = ShardRow {
            encoded_size: entry.encoded_size,
            xorbs: tree(&mut writer, Kind::CatalogShardXorbs, rows)?,
        };
        shards.push((hash(shard_hash)?.to_vec(), json(&row)?));
    }
    let shards = tree(&mut writer, Kind::CatalogShards, shards)?;
    let files = catalog
        .files
        .iter()
        .map(|(file_hash, entry)| {
            let mut value = entry.size.to_be_bytes().to_vec();
            value.extend_from_slice(&hash(&entry.shard_hash)?);
            Ok((hash(file_hash)?.to_vec(), value))
        })
        .collect::<Result<_>>()?;
    let files = tree(&mut writer, Kind::CatalogFiles, files)?;
    let descriptor = Descriptor {
        version: 1,
        body_bytes: writer.bytes.len() as u64,
        body_hash: *blake3::hash(&writer.bytes).as_bytes(),
        files,
        shards,
        xorbs,
        chunks,
    };
    let header = json(&descriptor)?;
    if header.len() > MAX_DESCRIPTOR_BYTES {
        return Err(contract_error("catalog descriptor exceeds its byte bound"));
    }
    writer.bytes.extend_from_slice(&header);
    crate::pages::put_u32(&mut writer.bytes, header.len())?;
    writer.bytes.extend_from_slice(MAGIC);
    if writer.bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(contract_error("catalog exceeds its encoded byte bound"));
    }
    Ok(writer.bytes.into())
}

pub(super) fn decode(bytes: &[u8]) -> Result<PointerCatalog> {
    let descriptor = Descriptor::decode(bytes)?;
    let body = &bytes[..descriptor.body_bytes as usize];
    if blake3::hash(body).as_bytes() != &descriptor.body_hash {
        return Err(corrupt("catalog page body hash differs"));
    }
    let mut visited = BTreeMap::new();
    let mut scan = |root: Option<&PageRef>, kind| -> Result<Rows> {
        root.map(|root| crate::pages::scan(body, root, kind, "pointer catalog pages", &mut visited))
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let mut catalog = PointerCatalog::new();
    for (key, value) in scan(descriptor.files.as_ref(), Kind::CatalogFiles)? {
        if value.len() != 40 {
            return Err(corrupt("catalog file row has an invalid byte length"));
        }
        let size = u64::from_be_bytes(
            value[..8]
                .try_into()
                .map_err(|_| corrupt("file size is truncated"))?,
        );
        catalog.insert_file(hex(&key)?, FileCatalogEntry::new(size, hex(&value[8..])?))?;
    }
    for (key, value) in scan(descriptor.shards.as_ref(), Kind::CatalogShards)? {
        let row: ShardRow = from_json(&value)?;
        let hashes = scan(row.xorbs.as_ref(), Kind::CatalogShardXorbs)?
            .into_iter()
            .map(|(hash, value)| {
                if !value.is_empty() {
                    return Err(corrupt("shard xorb row has unexpected data"));
                }
                hex(&hash)
            })
            .collect::<Result<_>>()?;
        catalog.insert_shard(hex(&key)?, ShardCatalogEntry::new(row.encoded_size, hashes))?;
    }
    for (key, value) in scan(descriptor.xorbs.as_ref(), Kind::CatalogXorbs)? {
        let row: XorbRow = from_json(&value)?;
        let chunks = scan(Some(&row.chunks), Kind::CatalogXorbChunks)?
            .into_iter()
            .enumerate()
            .map(|(index, (key, value))| {
                if key != (index as u32).to_be_bytes() || value.len() != 36 {
                    return Err(corrupt("xorb chunk row has invalid position or size"));
                }
                let size = u32::from_be_bytes(
                    value[32..]
                        .try_into()
                        .map_err(|_| corrupt("chunk size is truncated"))?,
                );
                Ok(XorbChunkEntry::new(hex(&value[..32])?, size))
            })
            .collect::<Result<_>>()?;
        catalog.insert_xorb(
            hex(&key)?,
            XorbCatalogEntry::new(row.encoded_size, hex(&row.body_digest)?, chunks),
        )?;
    }
    // Rebuild the reverse chunk index from authenticated xorb order. This also
    // rejects unclaimed bytes, re-signed routing aliases and noncanonical rows.
    if encode(&catalog)?.as_ref() != bytes {
        return Err(corrupt("catalog pages are not canonical"));
    }
    Ok(catalog)
}

impl Descriptor {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < TRAILER_BYTES
            || bytes.len() as u64 > MAX_CATALOG_BYTES
            || bytes.get(bytes.len() - 8..) != Some(MAGIC.as_slice())
        {
            return Err(corrupt("catalog trailer or encoded size is invalid"));
        }
        let end = bytes.len() - TRAILER_BYTES;
        let length = u32::from_le_bytes(
            bytes[end..end + 4]
                .try_into()
                .map_err(|_| corrupt("catalog descriptor length is truncated"))?,
        ) as usize;
        if length == 0 || length > MAX_DESCRIPTOR_BYTES {
            return Err(corrupt("catalog descriptor exceeds its byte bound"));
        }
        let start = end
            .checked_sub(length)
            .ok_or_else(|| corrupt("catalog descriptor span is invalid"))?;
        let descriptor: Self = from_json(&bytes[start..end])?;
        if descriptor.body_bytes != start as u64 {
            return Err(corrupt("catalog body span differs from its descriptor"));
        }
        descriptor.validate(bytes.len() as u64)?;
        Ok(descriptor)
    }

    fn validate(&self, length: u64) -> Result<()> {
        let header = json(self)?;
        if self.version != 1
            || header.len() > MAX_DESCRIPTOR_BYTES
            || self
                .body_bytes
                .checked_add(header.len() as u64)
                .and_then(|n| n.checked_add(TRAILER_BYTES as u64))
                != Some(length)
            || length > MAX_CATALOG_BYTES
        {
            return Err(corrupt("catalog descriptor version or span is invalid"));
        }
        for (root, kind) in [
            (&self.files, Kind::CatalogFiles),
            (&self.shards, Kind::CatalogShards),
            (&self.xorbs, Kind::CatalogXorbs),
            (&self.chunks, Kind::CatalogChunks),
        ] {
            if let Some(root) = root {
                root.validate(kind, self.body_bytes)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(xorb_count: usize, chunks_per_xorb: usize) -> PointerCatalog {
        let mut catalog = PointerCatalog::new();
        let mut xorbs = Vec::new();
        for index in 0..xorb_count {
            let xorb = format!("{index:064x}");
            let chunks = (0..chunks_per_xorb)
                .map(|chunk| XorbChunkEntry::new(format!("{chunk:064x}"), 7))
                .collect();
            catalog
                .insert_xorb(
                    xorb.clone(),
                    XorbCatalogEntry::new(100, "a".repeat(64), chunks),
                )
                .unwrap();
            xorbs.push(xorb);
        }
        let shard = "b".repeat(64);
        catalog
            .insert_shard(shard.clone(), ShardCatalogEntry::new(50, xorbs))
            .unwrap();
        catalog
            .insert_file("c".repeat(64), FileCatalogEntry::new(7, shard))
            .unwrap();
        catalog
    }

    #[test]
    fn nested_chunk_and_shard_directories_round_trip_without_growing_the_descriptor() {
        for (xorbs, chunks) in [(1, 2000), (2000, 1)] {
            let catalog = catalog(xorbs, chunks);
            let bytes = catalog.encode().unwrap();
            let descriptor = Descriptor::decode(&bytes).unwrap();
            assert!(json(&descriptor).unwrap().len() < MAX_DESCRIPTOR_BYTES);
            assert_eq!(PointerCatalog::decode(&bytes).unwrap(), catalog);
        }
    }

    #[test]
    fn chunk_index_selects_first_occurrence_in_lowest_xorb() {
        let mut catalog = catalog(2, 3);
        let xorb = catalog.xorbs.get_mut(&"0".repeat(64)).unwrap();
        xorb.chunks.push(xorb.chunks[0].clone());
        let bytes = catalog.encode().unwrap();
        let descriptor = Descriptor::decode(&bytes).unwrap();
        let body = &bytes[..descriptor.body_bytes as usize];
        let rows = crate::pages::scan(
            body,
            &descriptor.chunks.unwrap(),
            Kind::CatalogChunks,
            "chunks",
            &mut BTreeMap::new(),
        )
        .unwrap();
        assert!(rows.iter().all(|(_, value)| value[..32] == [0; 32]));
        assert_eq!(&rows[0].1[32..36], &0u32.to_be_bytes());
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn catalogs_reject_tampering_and_truncated_descriptors() {
        let bytes = catalog(2, 3).encode().unwrap();
        let mut tampered = bytes.to_vec();
        tampered[14] ^= 1;
        assert!(PointerCatalog::decode(&tampered).is_err());
        for length in [0, 7, 11, bytes.len() - 1] {
            assert!(PointerCatalog::decode(&bytes[..length]).is_err());
        }
    }

    #[test]
    fn resigned_chunk_index_cannot_disagree_with_xorb_order() {
        let bytes = catalog(2, 3).encode().unwrap();
        let mut descriptor = Descriptor::decode(&bytes).unwrap();
        let root = descriptor.chunks.as_mut().unwrap();
        assert_eq!(root.height, 0);
        let mut body = bytes[..descriptor.body_bytes as usize].to_vec();
        let start = root.offset as usize;
        // First row: page header, key, value length, then xorb identity.
        body[start + 14 + 32 + 4] ^= 1;
        root.hash = *blake3::hash(&body[start..start + root.bytes as usize]).as_bytes();
        descriptor.body_hash = *blake3::hash(&body).as_bytes();
        let header = json(&descriptor).unwrap();
        body.extend_from_slice(&header);
        crate::pages::put_u32(&mut body, header.len()).unwrap();
        body.extend_from_slice(MAGIC);
        assert!(PointerCatalog::decode(&body).is_err());
    }

    #[test]
    fn empty_catalog_is_canonical_and_retired_json_is_rejected() {
        let catalog = PointerCatalog::new();
        assert_eq!(
            PointerCatalog::decode(&catalog.encode().unwrap()).unwrap(),
            catalog
        );
        assert!(PointerCatalog::decode(&serde_json::to_vec(&catalog).unwrap()).is_err());
    }

    #[test]
    fn binary_catalog_keys_reject_case_aliases() {
        let mut catalog = catalog(1, 1);
        let file = catalog.files.remove(&"c".repeat(64)).unwrap();
        catalog.files.insert("C".repeat(64), file);
        assert!(catalog.encode().is_err());
    }

    #[test]
    fn later_file_binding_preserves_immutable_descriptor_conflicts() {
        let mut base = catalog(1, 1);
        let file = "c".repeat(64);
        let shard = "d".repeat(64);
        let mut delta = PointerCatalog::new();
        delta
            .insert_shard(
                shard.clone(),
                ShardCatalogEntry::new(90, vec!["0".repeat(64)]),
            )
            .unwrap();
        delta
            .insert_file(file.clone(), FileCatalogEntry::new(7, shard.clone()))
            .unwrap();
        let delta = PointerCatalog::decode_delta(&delta.encode_delta().unwrap()).unwrap();
        base.apply(&delta).unwrap();
        let base = PointerCatalog::decode(&base.encode().unwrap()).unwrap();
        assert_eq!(base.files[&file].shard_hash(), shard);
        for replace_shard in [false, true] {
            let mut conflict = PointerCatalog::new();
            if replace_shard {
                conflict
                    .insert_shard(
                        shard.clone(),
                        ShardCatalogEntry::new(91, vec!["0".repeat(64)]),
                    )
                    .unwrap();
            } else {
                conflict
                    .insert_xorb(
                        "0".repeat(64),
                        XorbCatalogEntry::new(
                            101,
                            "a".repeat(64),
                            vec![XorbChunkEntry::new("0".repeat(64), 7)],
                        ),
                    )
                    .unwrap();
            }
            let conflict = PointerCatalog::decode_delta(&conflict.encode_delta().unwrap()).unwrap();
            assert!(base.clone().apply(&conflict).is_err());
        }
    }
}
