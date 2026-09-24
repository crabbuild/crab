use serde::{Deserialize, Serialize};

use crate::{CrabError, Limits, Result, SegmentInfo};

use super::{MAX_SEGMENT_PAGES, ROOT_BYTES, SEGMENT_PAGE_BYTES};

use crate::hex::encode_hex;

#[derive(Clone)]
pub(super) struct RootDocument {
    pub cell: [u8; 32],
    pub checksum: u64,
    pub commit_sequence: u64,
    pub database_pages: u32,
    pub directory_digest: [u8; 32],
    pub directory_height: u32,
    pub incarnation: [u8; 16],
    pub page_size: u32,
    pub schema: u32,
    pub segment_pages: Vec<[u8; 32]>,
    pub txid: u64,
}

#[derive(Clone)]
pub(super) struct SegmentDescriptor {
    pub info: SegmentInfo,
    pub index_digest: [u8; 32],
    pub index_length: u64,
    object_digest: [u8; 32],
    offset: u64,
    length: u64,
    level: u8,
}

impl SegmentDescriptor {
    pub(super) fn native(info: SegmentInfo, index_digest: [u8; 32], index_length: u64) -> Self {
        let object_digest = info.blake3;
        let length = info.size_bytes;
        Self {
            info,
            index_digest,
            index_length,
            object_digest,
            offset: 0,
            length,
            level: 0,
        }
    }

    pub(super) fn bundled(
        info: SegmentInfo,
        index_digest: [u8; 32],
        index_length: u64,
        object_digest: [u8; 32],
        offset: u64,
    ) -> Self {
        let length = info.size_bytes;
        Self {
            info,
            index_digest,
            index_length,
            object_digest,
            offset,
            length,
            level: 0,
        }
    }

    pub(super) fn with_level(mut self, level: u8) -> Self {
        self.level = level;
        self
    }

    pub(super) const fn level(&self) -> u8 {
        self.level
    }

    pub(super) const fn object_digest(&self) -> [u8; 32] {
        self.object_digest
    }

    pub(super) const fn offset(&self) -> u64 {
        self.offset
    }

    #[cfg(test)]
    pub(super) const fn length(&self) -> u64 {
        self.length
    }

    pub(super) fn object_kind(&self) -> crate::CellObjectKind {
        if self.object_digest == self.info.blake3 {
            crate::CellObjectKind::Ltx
        } else {
            crate::CellObjectKind::Bundle
        }
    }

    pub(super) fn object_extent(&self) -> ([u8; 32], u64, u64, crate::CellObjectKind) {
        (
            self.object_digest,
            self.offset,
            self.length,
            self.object_kind(),
        )
    }

    pub(super) fn validate(&self, limits: Limits) -> Result<()> {
        let info = &self.info;
        if self.level > 9
            || info.max_txid < info.min_txid
            || info.database_pages == 0
            || !(512..=65536).contains(&info.page_size)
            || !info.page_size.is_power_of_two()
            || info.post_checksum & crate::CHECKSUM_FLAG == 0
            || info.size_bytes < 128
            || info.size_bytes > limits.max_file_bytes
            || self.index_length > u64::from(info.database_pages) * 60
            || self.length != info.size_bytes
            || self.offset.checked_add(self.length).is_none()
            || (self.object_kind() == crate::CellObjectKind::Ltx && self.offset != 0)
            || self
                .offset
                .checked_add(self.length)
                .is_none_or(|end| end > limits.max_plan_bytes)
            || u64::from(info.database_pages) * u64::from(info.page_size)
                > limits.max_database_bytes
        {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(())
    }

    pub(super) fn validate_published(&self, limits: Limits) -> Result<()> {
        self.validate(limits)?;
        if self.index_digest == [0; 32] || self.index_length == 0 || self.object_digest == [0; 32] {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootWire {
    cell: String,
    checksum: String,
    commit_sequence: String,
    database_pages: u32,
    directory_digest: String,
    directory_height: u32,
    incarnation: String,
    page_size: u32,
    schema: u32,
    segment_pages: Vec<String>,
    txid: String,
    version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentWire {
    blake3: String,
    database_pages: u32,
    index_digest: String,
    index_length: String,
    length: String,
    level: u8,
    max_txid: String,
    min_txid: String,
    object_digest: String,
    offset: String,
    page_size: u32,
    post_checksum: String,
    pre_checksum: String,
    size_bytes: String,
}

pub(super) fn encode_root(root: &RootDocument) -> Result<Vec<u8>> {
    if root.schema == 0
        || root.txid == 0
        || root.checksum & crate::CHECKSUM_FLAG == 0
        || root.commit_sequence > i64::MAX as u64
        || root.database_pages == 0
        || root.segment_pages.is_empty()
        || root.segment_pages.len() > MAX_SEGMENT_PAGES
    {
        return Err(CrabError::LTXCorrupted);
    }
    let bytes = serde_json::to_vec(&RootWire {
        cell: encode_hex(&root.cell),
        checksum: checksum(root.checksum),
        commit_sequence: root.commit_sequence.to_string(),
        database_pages: root.database_pages,
        directory_digest: encode_hex(&root.directory_digest),
        directory_height: root.directory_height,
        incarnation: encode_hex(&root.incarnation),
        page_size: root.page_size,
        schema: root.schema,
        segment_pages: root
            .segment_pages
            .iter()
            .map(|value| encode_hex(value))
            .collect(),
        txid: root.txid.to_string(),
        version: 1,
    })?;
    if bytes.len() as u64 > ROOT_BYTES {
        return Err(CrabError::Limit(crate::LimitKind::CellRootBytes));
    }
    Ok(bytes)
}

pub(super) fn decode_root(bytes: &[u8]) -> Result<RootDocument> {
    if bytes.len() as u64 > ROOT_BYTES {
        return Err(CrabError::Limit(crate::LimitKind::CellRootBytes));
    }
    let wire: RootWire = serde_json::from_slice(bytes)?;
    if wire.version != 1 {
        return Err(CrabError::LTXCorrupted);
    }
    let root = RootDocument {
        cell: parse_hex(&wire.cell)?,
        checksum: parse_checksum(&wire.checksum)?,
        commit_sequence: decimal(&wire.commit_sequence)?,
        database_pages: wire.database_pages,
        directory_digest: parse_hex(&wire.directory_digest)?,
        directory_height: wire.directory_height,
        incarnation: parse_hex(&wire.incarnation)?,
        page_size: wire.page_size,
        schema: wire.schema,
        segment_pages: wire
            .segment_pages
            .iter()
            .map(|value| parse_hex(value))
            .collect::<Result<Vec<_>>>()?,
        txid: decimal(&wire.txid)?,
    };
    if encode_root(&root)? != bytes {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(root)
}

pub(super) fn encode_segment_page(segments: &[SegmentDescriptor]) -> Result<Vec<u8>> {
    let wire = segments
        .iter()
        .map(|segment| SegmentWire {
            blake3: encode_hex(&segment.info.blake3),
            database_pages: segment.info.database_pages,
            index_digest: encode_hex(&segment.index_digest),
            index_length: segment.index_length.to_string(),
            length: segment.length.to_string(),
            level: segment.level,
            max_txid: segment.info.max_txid.to_string(),
            min_txid: segment.info.min_txid.to_string(),
            object_digest: encode_hex(&segment.object_digest),
            offset: segment.offset.to_string(),
            page_size: segment.info.page_size,
            post_checksum: checksum(segment.info.post_checksum),
            pre_checksum: checksum(segment.info.pre_checksum),
            size_bytes: segment.info.size_bytes.to_string(),
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&wire)?;
    if bytes.len() as u64 > SEGMENT_PAGE_BYTES {
        return Err(CrabError::Limit(crate::LimitKind::CellSegmentPageBytes));
    }
    Ok(bytes)
}

pub(super) fn decode_segment_page(bytes: &[u8]) -> Result<Vec<SegmentDescriptor>> {
    if bytes.len() as u64 > SEGMENT_PAGE_BYTES {
        return Err(CrabError::Limit(crate::LimitKind::CellSegmentPageBytes));
    }
    let wire: Vec<SegmentWire> = serde_json::from_slice(bytes)?;
    let segments = wire
        .into_iter()
        .map(|segment| {
            let object_digest = parse_hex(&segment.object_digest)?;
            Ok(SegmentDescriptor {
                info: SegmentInfo {
                    min_txid: decimal(&segment.min_txid)?,
                    max_txid: decimal(&segment.max_txid)?,
                    page_size: segment.page_size,
                    database_pages: segment.database_pages,
                    pre_checksum: parse_checksum(&segment.pre_checksum)?,
                    post_checksum: parse_checksum(&segment.post_checksum)?,
                    size_bytes: decimal(&segment.size_bytes)?,
                    blake3: parse_hex(&segment.blake3)?,
                },
                index_digest: parse_hex(&segment.index_digest)?,
                index_length: decimal(&segment.index_length)?,
                object_digest,
                offset: decimal(&segment.offset)?,
                length: decimal(&segment.length)?,
                level: segment.level,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if encode_segment_page(&segments)? != bytes {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(segments)
}

fn decimal(value: &str) -> Result<u64> {
    let parsed = value.parse::<u64>().map_err(|_| CrabError::LTXCorrupted)?;
    if parsed.to_string() != value {
        return Err(CrabError::LTXCorrupted);
    }
    Ok(parsed)
}

fn checksum(value: u64) -> String {
    format!("{value:016x}")
}

fn parse_checksum(value: &str) -> Result<u64> {
    if value.len() != 16
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(CrabError::LTXCorrupted);
    }
    u64::from_str_radix(value, 16).map_err(|_| CrabError::LTXCorrupted)
}

fn parse_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(CrabError::LTXCorrupted);
    }
    let mut output = [0; N];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        output[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(output)
}

fn nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CrabError::LTXCorrupted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> RootDocument {
        RootDocument {
            cell: [1; 32],
            checksum: crate::CHECKSUM_FLAG | 7,
            commit_sequence: 3,
            database_pages: 9,
            directory_digest: [2; 32],
            directory_height: 0,
            incarnation: [3; 16],
            page_size: 4096,
            schema: 1,
            segment_pages: vec![[4; 32]],
            txid: 2,
        }
    }

    #[test]
    fn encode_root_accepts_the_segment_page_ceiling() {
        let mut document = root();
        document.segment_pages = vec![[4; 32]; super::super::MAX_SEGMENT_PAGES];
        assert!(encode_root(&document).is_ok());
    }

    #[test]
    fn encode_root_rejects_more_than_the_segment_page_ceiling() {
        let mut document = root();
        document.segment_pages = vec![[4; 32]; super::super::MAX_SEGMENT_PAGES + 1];
        assert!(matches!(
            encode_root(&document),
            Err(CrabError::LTXCorrupted)
        ));
    }

    #[test]
    fn decode_root_rejects_a_body_past_the_root_bound() {
        let oversized = vec![b' '; super::super::ROOT_BYTES as usize + 1];
        assert!(matches!(
            decode_root(&oversized),
            Err(CrabError::Limit(crate::LimitKind::CellRootBytes))
        ));
    }

    #[test]
    fn decode_segment_page_rejects_a_body_past_the_page_bound() {
        let oversized = vec![b' '; super::super::SEGMENT_PAGE_BYTES as usize + 1];
        assert!(matches!(
            decode_segment_page(&oversized),
            Err(CrabError::Limit(crate::LimitKind::CellSegmentPageBytes))
        ));
    }

    #[test]
    fn root_codec_rejects_noncanonical_and_duplicate_fields() {
        let bytes = encode_root(&root()).unwrap();
        assert_eq!(decode_root(&bytes).unwrap().txid, 2);
        let text = String::from_utf8(bytes).unwrap();
        assert!(decode_root(text.replace("\"txid\":\"2\"", "\"txid\":\"02\"").as_bytes()).is_err());
        assert!(
            decode_root(
                text.replace("\"schema\":1", "\"schema\":1,\"schema\":1")
                    .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn maximum_descriptor_page_fits_the_wire_bound() {
        let info = SegmentInfo {
            min_txid: u64::MAX - 1,
            max_txid: u64::MAX,
            page_size: 65536,
            database_pages: u32::MAX,
            pre_checksum: u64::MAX,
            post_checksum: u64::MAX,
            size_bytes: u64::MAX,
            blake3: [1; 32],
        };
        let descriptor = SegmentDescriptor::native(info, [2; 32], u64::MAX);
        let one = encode_segment_page(std::slice::from_ref(&descriptor)).unwrap();
        let page = vec![descriptor; super::super::SEGMENTS_PER_PAGE];
        assert!(
            encode_segment_page(&page).is_ok(),
            "one maximum descriptor occupies {} bytes",
            one.len()
        );
    }

    #[test]
    fn bundled_descriptor_roundtrips_its_exact_extent() {
        let info = SegmentInfo {
            min_txid: 1,
            max_txid: 2,
            page_size: 4096,
            database_pages: 9,
            pre_checksum: 0,
            post_checksum: crate::CHECKSUM_FLAG | 7,
            size_bytes: 1024,
            blake3: [1; 32],
        };
        let descriptor = SegmentDescriptor::bundled(info, [2; 32], 60, [3; 32], 4096);
        let decoded = decode_segment_page(&encode_segment_page(&[descriptor]).unwrap()).unwrap();
        let decoded = decoded.first().unwrap();
        assert_eq!(decoded.object_kind(), crate::CellObjectKind::Bundle);
        assert_eq!(decoded.object_digest(), [3; 32]);
        assert_eq!(decoded.offset(), 4096);
        assert_eq!(decoded.length(), 1024);
    }
}
