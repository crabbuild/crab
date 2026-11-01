//! Authenticated blocks make negative discovery independent of data-page fanout.
use super::*;

const BLOCK_BYTES: usize = 64;
const BITS_PER_KEY: usize = 16;
const PROBES: usize = 8;

fn probe(key: &[u8], blocks: u32) -> (u32, [usize; PROBES]) {
    let hash = blake3::derive_key("crab capsule chunk filter v2", key);
    let block = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) % blocks;
    let bits = std::array::from_fn(|index| {
        usize::from(u16::from_le_bytes([
            hash[4 + 2 * index],
            hash[5 + 2 * index],
        ])) % (BLOCK_BYTES * 8)
    });
    (block, bits)
}

fn blocks(count: usize) -> Result<u32> {
    count
        .checked_mul(BITS_PER_KEY)
        .map(|bits| bits.div_ceil(BLOCK_BYTES * 8).max(1))
        .and_then(|blocks| u32::try_from(blocks).ok())
        .ok_or_else(|| corrupt("capsule hint filter size overflows"))
}

pub(super) fn build<'a>(keys: impl ExactSizeIterator<Item = &'a [u8]>) -> Result<Rows> {
    let blocks = blocks(keys.len())?;
    let mut values = vec![[0_u8; BLOCK_BYTES]; blocks as usize];
    for key in keys {
        let (block, bits) = probe(key, blocks);
        for bit in bits {
            values[block as usize][bit / 8] |= 1 << (bit % 8);
        }
    }
    Ok(values
        .into_iter()
        .enumerate()
        .map(|(index, value)| ((index as u32).to_be_bytes().to_vec(), value.to_vec()))
        .collect())
}

pub(super) async fn candidates(
    run: &run::Run,
    store: &Store,
    path: &Path,
    cache: &PageCache,
    layer: usize,
    keys: &[Vec<u8>],
    work: &mut PageWork,
) -> Result<Vec<bool>> {
    let blocks = run.filter.count;
    work.admit_materialized((keys.len() as u64).saturating_mul(128))?;
    let positions = keys
        .iter()
        .map(|key| probe(key, blocks))
        .collect::<Vec<_>>();
    let needed = positions
        .iter()
        .map(|(block, _)| block.to_be_bytes().to_vec())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let values = cache
        .lookup(
            store,
            path,
            layer,
            0..run.bytes,
            &run.filter,
            Kind::CapsuleChunkFilters,
            &needed,
            work,
        )
        .await?;
    let mut loaded = BTreeMap::new();
    for (key, value) in needed.into_iter().zip(values) {
        let value = value
            .filter(|value| value.len() == BLOCK_BYTES)
            .ok_or_else(|| corrupt("capsule hint filter block is absent or malformed"))?;
        loaded.insert(key, value);
    }
    positions
        .into_iter()
        .map(|(block, bits)| {
            let value = loaded
                .get(block.to_be_bytes().as_slice())
                .ok_or_else(|| corrupt("capsule hint filter omitted a requested block"))?;
            Ok(bits
                .into_iter()
                .all(|bit| value[bit / 8] & (1 << (bit % 8)) != 0))
        })
        .collect()
}

pub(super) fn validate_shape(filter: &crate::pages::PageRef, bytes: u64, count: u32) -> Result<()> {
    filter.validate(Kind::CapsuleChunkFilters, bytes)?;
    if filter.count != blocks(count as usize)?
        || filter.first != 0_u32.to_be_bytes()
        || filter.last != (filter.count - 1).to_be_bytes()
    {
        return Err(corrupt("capsule hint filter block coverage is incomplete"));
    }
    Ok(())
}
