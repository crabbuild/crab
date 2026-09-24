//! Catalog head and page wire shapes with their hex codecs.

use super::*;
use crate::identity::nibble;

pub(super) struct ObservedHead {
    pub(super) head: CatalogHead,
    pub(super) token: ETag,
}

pub(super) struct CatalogHead {
    pub(super) revision: u64,
    pub(super) pages: Vec<Digest>,
}

impl CatalogHead {
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        if self.revision == 0 || self.pages.is_empty() || self.pages.len() > MAX_PAGES {
            return Err(Error::Catalog("invalid head bounds"));
        }
        let encoded = serde_json::to_vec(&RawHead {
            version: 1,
            revision: self.revision.to_string(),
            pages: self
                .pages
                .iter()
                .map(|digest| encode_hex(digest.as_bytes()))
                .collect(),
        })?;
        if encoded.len() as u64 > MAX_HEAD_BYTES {
            return Err(Error::Catalog("encoded head exceeds 32 KiB"));
        }
        Ok(encoded)
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let raw: RawHead = serde_json::from_slice(body)?;
        if raw.version != 1 || raw.pages.is_empty() || raw.pages.len() > MAX_PAGES {
            return Err(Error::Catalog("invalid head version or page count"));
        }
        let revision = canonical_u64(&raw.revision)?;
        let pages = raw
            .pages
            .iter()
            .map(|value| decode_fixed(value).map(Digest::from_bytes))
            .collect::<Result<Vec<_>>>()?;
        let mut unique = std::collections::HashSet::with_capacity(pages.len());
        if pages
            .iter()
            .any(|digest| !unique.insert(*digest.as_bytes()))
        {
            return Err(Error::Catalog("duplicate page digest"));
        }
        let head = Self { revision, pages };
        if head.encode()?.as_slice() != body {
            return Err(Error::Catalog("head JSON is not canonical"));
        }
        Ok(head)
    }
}

pub(super) struct CatalogPage {
    pub(super) entries: Vec<CatalogEntry>,
}

impl CatalogPage {
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        let entries = self.entries.iter().map(RawEntry::from).collect();
        Ok(serde_json::to_vec(&RawPage {
            version: 1,
            entries,
        })?)
    }

    pub(super) fn decode(body: &[u8]) -> Result<Self> {
        let raw: RawPage = serde_json::from_slice(body)?;
        if raw.version != 1 {
            return Err(Error::Catalog("unsupported page version"));
        }
        let entries = raw
            .entries
            .into_iter()
            .map(CatalogEntry::try_from)
            .collect::<Result<Vec<_>>>()?;
        let page = Self { entries };
        if page.encode()?.as_slice() != body {
            return Err(Error::Catalog("page JSON is not canonical"));
        }
        Ok(page)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawHead {
    version: u32,
    revision: String,
    pages: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPage {
    version: u32,
    entries: Vec<RawEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawEntry {
    cell: String,
    namespace: String,
    partition: String,
    role: CatalogRole,
    initial_code: String,
    initial_schema: u32,
}

impl From<&CatalogEntry> for RawEntry {
    fn from(entry: &CatalogEntry) -> Self {
        Self {
            cell: encode_hex(entry.cell.as_bytes()),
            namespace: encode_hex(entry.namespace.as_bytes()),
            partition: encode_hex(&entry.partition),
            role: entry.role,
            initial_code: encode_hex(entry.initial_code.as_bytes()),
            initial_schema: entry.initial_schema,
        }
    }
}

impl TryFrom<RawEntry> for CatalogEntry {
    type Error = Error;

    fn try_from(raw: RawEntry) -> Result<Self> {
        if raw.initial_schema == 0 {
            return Err(Error::Catalog("initial schema is zero"));
        }
        Ok(Self {
            cell: CellId::from_bytes(decode_fixed(&raw.cell)?),
            namespace: NamespaceId::from_bytes(decode_fixed(&raw.namespace)?),
            partition: decode_partition(&raw.partition)?,
            role: raw.role,
            initial_code: Digest::from_bytes(decode_fixed(&raw.initial_code)?),
            initial_schema: raw.initial_schema,
        })
    }
}

pub(super) fn canonical_u64(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Catalog("invalid decimal u64"))?;
    if parsed == 0 || parsed.to_string() != value {
        return Err(Error::Catalog("noncanonical decimal u64"));
    }
    Ok(parsed)
}

pub(super) fn decode_fixed<const N: usize>(value: &str) -> Result<[u8; N]> {
    let decoded = decode_hex(value)?;
    decoded
        .try_into()
        .map_err(|_| Error::Catalog("fixed hex length"))
}

pub(super) fn decode_partition(value: &str) -> Result<Vec<u8>> {
    if value.len() > 2_048 {
        return Err(Error::Catalog("partition exceeds 1024 bytes"));
    }
    decode_hex(value)
}

pub(super) fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(Error::Catalog("hex length is odd"));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = nibble(pair[0]).ok_or(Error::Catalog("hex is not lowercase"))?;
            let low = nibble(pair[1]).ok_or(Error::Catalog("hex is not lowercase"))?;
            Ok((high << 4) | low)
        })
        .collect()
}
