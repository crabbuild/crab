use super::Error;
use bytes::{Buf, BufMut, Bytes};
use crab_remote_git::GitPath;
use gix_hash::ObjectId;

mod regex_mask;

const MAGIC: &[u8; 8] = b"CRABIDX3";
const OID_BYTES: usize = 20;
const CHECKSUM: usize = 32;
const MASK_BYTES: [usize; 4] = [128, 512, 2048, 8192];
const HEADER: usize = MAGIC.len() + OID_BYTES + 1;
pub(super) const OVERHEAD: usize = HEADER + CHECKSUM;
const ROW_HEADER: usize = 4 + OID_BYTES + 1;

#[derive(Clone)]
pub(crate) struct Mask {
    bits: Bytes,
}

impl Mask {
    pub(super) fn from_text(text: &str) -> Self {
        // Keep small files cheap while giving larger files enough bits to
        // avoid saturating their filter and reading many unrelated blobs.
        let tier = match text.len() {
            0..=1024 => 0,
            1025..=4096 => 1,
            4097..=16384 => 2,
            _ => 3,
        };
        Self::from_runs(tier, [text])
    }

    pub(super) fn from_symbols<'a>(symbols: impl IntoIterator<Item = &'a str>) -> Self {
        Self::from_runs(1, symbols)
    }

    fn from_runs<'a>(tier: usize, runs: impl IntoIterator<Item = &'a str>) -> Self {
        let mut bits = vec![0; MASK_BYTES[tier]];
        for run in runs {
            insert(&mut bits, run);
        }
        Self {
            bits: Bytes::from(bits),
        }
    }

    fn tier(&self) -> usize {
        // Mask widths are consecutive powers of four, starting at 128 bytes.
        (self.bits.len().ilog2() as usize - 7) / 2
    }

    pub(crate) fn may_contain(&self, query: &QueryMask) -> bool {
        query.0[self.tier()]
            .iter()
            .all(|&(byte, needed)| self.bits[byte] & needed == needed)
    }
}

pub(crate) struct QueryMask([Vec<(usize, u8)>; MASK_BYTES.len()]);

impl QueryMask {
    fn empty_bits() -> Box<[u8]> {
        vec![0; MASK_BYTES[MASK_BYTES.len() - 1]].into_boxed_slice()
    }

    fn from_bits(bits: &[u8]) -> Self {
        Self(MASK_BYTES.map(|size| {
            let mut folded = vec![0; size];
            // Shorter filters take the same hash modulo a smaller size.
            // Fold once per query, retaining only the bytes it must check.
            for chunk in bits.chunks_exact(size) {
                for (target, source) in folded.iter_mut().zip(chunk) {
                    *target |= source;
                }
            }
            folded
                .into_iter()
                .enumerate()
                .filter(|&(_, byte)| byte != 0)
                .collect()
        }))
    }
}

fn insert(bits: &mut [u8], text: &str) {
    for window in text.as_bytes().windows(3) {
        let gram = u32::from_le_bytes([
            window[0].to_ascii_lowercase(),
            window[1].to_ascii_lowercase(),
            window[2].to_ascii_lowercase(),
            0,
        ]);
        let mixed = gram.wrapping_mul(0x9e3779b1);
        let hash = (mixed ^ (mixed >> 16)).wrapping_mul(0x85ebca6b);
        for hash in [hash, hash.rotate_left(11)] {
            let bit = hash as usize & (bits.len() * 8 - 1);
            bits[bit / 8] |= 1 << (bit % 8);
        }
    }
}

pub(crate) struct Row {
    pub(crate) path: GitPath,
    pub(crate) oid: ObjectId,
    pub(crate) filter: Option<Mask>,
    pub(crate) symbols: Option<Mask>,
}

impl Row {
    pub(super) fn encoded_size(&self) -> usize {
        ROW_HEADER
            + self.path.as_bytes().len()
            + [&self.filter, &self.symbols]
                .into_iter()
                .flatten()
                .map(|mask| mask.bits.len())
                .sum::<usize>()
    }
}

pub(crate) struct Index {
    pub(crate) tree: ObjectId,
    pub(crate) rows: Option<Vec<Row>>,
    size: usize,
    pub(super) weight: usize,
}

impl Index {
    pub(super) fn new(tree: ObjectId, mut rows: Option<Vec<Row>>, size: usize) -> Self {
        if let Some(rows) = &mut rows {
            rows.shrink_to_fit();
        }
        let weight = size
            + rows
                .as_ref()
                .map_or(0, |rows| rows.capacity() * size_of::<Row>());
        Self {
            tree,
            rows,
            size,
            weight,
        }
    }

    pub(super) fn encode(&self) -> Result<Bytes, Error> {
        let mut out = Vec::with_capacity(self.size);
        out.extend(MAGIC);
        out.extend(self.tree.as_bytes());
        out.put_u8(u8::from(self.rows.is_some()));
        if let Some(rows) = &self.rows {
            for row in rows {
                let path = row.path.as_bytes();
                out.put_u32_le(u32::try_from(path.len()).map_err(|_| Error::Invalid)?);
                out.extend(row.oid.as_bytes());
                out.put_u8(flag(row.filter.as_ref()) | (flag(row.symbols.as_ref()) << 4));
                out.extend(path);
                for mask in [&row.filter, &row.symbols].into_iter().flatten() {
                    out.extend_from_slice(&mask.bits);
                }
            }
        }
        let sum = blake3::hash(&out);
        out.extend(sum.as_bytes());
        Ok(Bytes::from(out))
    }

    pub(super) fn decode(mut data: Bytes) -> Result<Self, Error> {
        let tree = tree(&data)?;
        let size = data.len();
        let checksum = data.split_off(size - CHECKSUM);
        if blake3::hash(&data) != *checksum.as_ref() {
            return Err(Error::Invalid);
        }
        data.advance(HEADER - 1);
        let status = data.get_u8();
        if status == 0 && data.is_empty() {
            return Ok(Self::new(tree, None, size));
        }
        if status != 1 {
            return Err(Error::Invalid);
        }
        let mut rows: Vec<Row> = Vec::new();
        let mut weight = size;
        while !data.is_empty() {
            if data.len() < ROW_HEADER {
                return Err(Error::Invalid);
            }
            let len = data.get_u32_le() as usize;
            let oid = oid(&data.split_to(OID_BYTES))?;
            let flags = data.get_u8();
            if len == 0 || len > data.len() {
                return Err(Error::Invalid);
            }
            let path = GitPath::new(data.split_to(len))?;
            if rows
                .last()
                .is_some_and(|row| row.path.as_bytes() >= path.as_bytes())
            {
                return Err(Error::Invalid);
            }
            let filter = read_mask(&mut data, flags & 0b1111)?;
            let symbols = read_mask(&mut data, flags >> 4)?;
            if symbols.is_some() && filter.is_none() {
                return Err(Error::Invalid);
            }
            weight += size_of::<Row>();
            if weight > super::CACHE_BYTES {
                return Err(Error::Invalid);
            }
            rows.push(Row {
                path,
                oid,
                filter,
                symbols,
            });
        }
        Ok(Self::new(tree, Some(rows), size))
    }
}

fn flag(mask: Option<&Mask>) -> u8 {
    mask.map_or(0, |mask| 1 + mask.tier() as u8)
}

fn read_mask(data: &mut Bytes, flag: u8) -> Result<Option<Mask>, Error> {
    if flag == 0 {
        return Ok(None);
    }
    let Some(&size) = MASK_BYTES.get(usize::from(flag - 1)) else {
        return Err(Error::Invalid);
    };
    if data.len() < size {
        return Err(Error::Invalid);
    }
    Ok(Some(Mask {
        bits: data.split_to(size),
    }))
}

pub(super) fn tree(data: &[u8]) -> Result<ObjectId, Error> {
    if data.len() < OVERHEAD || !data.starts_with(MAGIC) {
        return Err(Error::Invalid);
    }
    oid(&data[MAGIC.len()..HEADER - 1])
}

fn oid(data: &[u8]) -> Result<ObjectId, Error> {
    let bytes: [u8; OID_BYTES] = data.try_into().map_err(|_| Error::Invalid)?;
    Ok(ObjectId::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_format_rejects_corruption_before_using_a_filter() {
        let rows: Vec<_> = [0, 1024, 4096, 16384]
            .into_iter()
            .enumerate()
            .map(|(i, padding)| Row {
                path: GitPath::new(format!("src/main{i}.rs").into_bytes()).unwrap(),
                oid: ObjectId::from([2; OID_BYTES]),
                filter: Some(Mask::from_text(&format!(
                    "{}fn main() {{}}",
                    " ".repeat(padding)
                ))),
                symbols: Some(Mask::from_symbols(["main"])),
            })
            .collect();
        let size = OVERHEAD + rows.iter().map(Row::encoded_size).sum::<usize>();
        let index = Index::new(ObjectId::from([1; OID_BYTES]), Some(rows), size);
        let encoded = index.encode().unwrap();
        assert_eq!(encoded.len(), size);
        let loaded = Index::decode(encoded.clone()).unwrap();
        assert_eq!(loaded.tree, index.tree);
        let rows = loaded.rows.unwrap();
        let query = QueryMask::from_regex(&regex_syntax::parse("main").unwrap()).unwrap();
        for (i, (row, width)) in rows.iter().zip(MASK_BYTES).enumerate() {
            assert_eq!(row.path.as_bytes(), format!("src/main{i}.rs").as_bytes());
            let filter = row.filter.as_ref().unwrap();
            assert_eq!(filter.bits.len(), width);
            assert!(filter.may_contain(&query));
            assert!(row.symbols.as_ref().unwrap().may_contain(&query));
        }

        let mut damaged = encoded.to_vec();
        damaged[HEADER + ROW_HEADER] ^= 1;
        assert!(matches!(Index::decode(damaged.into()), Err(Error::Invalid)));
    }
}
