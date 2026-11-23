use super::Error;
use bytes::{Buf, BufMut, Bytes};
use crab_remote_git::GitPath;
use gix_hash::ObjectId;

const MAGIC: &[u8; 8] = b"CRABIDX1";
const OID_BYTES: usize = 20;
const CHECKSUM: usize = 32;
const MASK_BYTES: usize = 256;
const HEADER: usize = MAGIC.len() + OID_BYTES + 1;
pub(super) const OVERHEAD: usize = HEADER + CHECKSUM;
const ROW_HEADER: usize = 4 + OID_BYTES + 1;

#[derive(Clone)]
pub(crate) struct Mask {
    bits: [u8; MASK_BYTES],
    ascii: bool,
}

impl Mask {
    pub(super) fn from_text(text: &str) -> Self {
        let mut bits = [0; MASK_BYTES];
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
                let bit = hash as usize % (MASK_BYTES * 8);
                bits[bit / 8] |= 1 << (bit % 8);
            }
        }
        Self {
            bits,
            ascii: text.is_ascii(),
        }
    }

    pub(crate) fn query(text: &str, case_sensitive: bool) -> Option<Self> {
        (text.len() >= 3 && (case_sensitive || text.is_ascii())).then(|| Self::from_text(text))
    }

    pub(crate) fn may_contain(&self, query: &Self, case_sensitive: bool) -> bool {
        // Regex case folding can match an ASCII query to non-ASCII text.
        // Those files remain candidates unless the search is case-sensitive.
        (!case_sensitive && !self.ascii)
            || query
                .bits
                .iter()
                .zip(self.bits)
                .all(|(needed, found)| needed & found == *needed)
    }
}

pub(crate) struct Row {
    pub(crate) path: GitPath,
    pub(crate) oid: ObjectId,
    pub(crate) filter: Option<Mask>,
}

impl Row {
    pub(super) fn encoded_size(&self) -> usize {
        ROW_HEADER + self.path.as_bytes().len() + self.filter.as_ref().map_or(0, |_| MASK_BYTES)
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
                out.put_u8(match &row.filter {
                    None => 0,
                    Some(mask) => {
                        if mask.ascii {
                            1
                        } else {
                            2
                        }
                    }
                });
                out.extend(path);
                if let Some(mask) = &row.filter {
                    out.extend(mask.bits);
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
        while !data.is_empty() {
            if data.len() < ROW_HEADER {
                return Err(Error::Invalid);
            }
            let len = data.get_u32_le() as usize;
            let oid = oid(&data.split_to(OID_BYTES))?;
            let flags = data.get_u8();
            let mask_bytes = if flags == 0 { 0 } else { MASK_BYTES };
            if flags > 2
                || len == 0
                || data
                    .len()
                    .checked_sub(len)
                    .is_none_or(|left| left < mask_bytes)
            {
                return Err(Error::Invalid);
            }
            let path = GitPath::new(data.split_to(len))?;
            if rows
                .last()
                .is_some_and(|row| row.path.as_bytes() >= path.as_bytes())
            {
                return Err(Error::Invalid);
            }
            let filter = if flags == 0 {
                None
            } else {
                let bits = data
                    .split_to(MASK_BYTES)
                    .as_ref()
                    .try_into()
                    .map_err(|_| Error::Invalid)?;
                Some(Mask {
                    bits,
                    ascii: flags == 1,
                })
            };
            if size + (rows.len() + 1) * size_of::<Row>() > super::CACHE_BYTES {
                return Err(Error::Invalid);
            }
            rows.push(Row { path, oid, filter });
        }
        Ok(Self::new(tree, Some(rows), size))
    }
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
        let path = GitPath::new(b"src/main.rs".as_slice()).unwrap();
        let index = Index::new(
            ObjectId::from([1; OID_BYTES]),
            Some(vec![Row {
                path,
                oid: ObjectId::from([2; OID_BYTES]),
                filter: Some(Mask::from_text("fn main() {}")),
            }]),
            OVERHEAD + ROW_HEADER + 11 + MASK_BYTES,
        );
        let encoded = index.encode().unwrap();
        let loaded = Index::decode(encoded.clone()).unwrap();
        assert_eq!(loaded.tree, index.tree);
        assert_eq!(loaded.rows.unwrap()[0].path.as_bytes(), b"src/main.rs");

        let mut damaged = encoded.to_vec();
        damaged[HEADER + ROW_HEADER] ^= 1;
        assert!(matches!(Index::decode(damaged.into()), Err(Error::Invalid)));
    }
}
