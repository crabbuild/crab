//! Authenticated bounded point and set reads with two-way ordinal identity checks.
use super::*;
use crate::pages::{PageCache, PageWork};
use crab_storage::Store;
use object_store::path::Path;

pub(crate) struct Reader<'a> {
    store: &'a Store,
    path: &'a Path,
    cache: &'a PageCache,
    source: std::ops::Range<u64>,
    descriptor: &'a Descriptor,
}
impl<'a> Reader<'a> {
    pub(crate) fn new(
        store: &'a Store,
        path: &'a Path,
        cache: &'a PageCache,
        offset: u64,
        body_bytes: u64,
        descriptor: &'a Descriptor,
    ) -> Result<Self> {
        descriptor.validate(body_bytes)?;
        let end = offset
            .checked_add(body_bytes)
            .ok_or_else(|| corrupt("shared visibility region overflows"))?;
        Ok(Self {
            store,
            path,
            cache,
            source: offset..end,
            descriptor,
        })
    }
    async fn rows(
        &self,
        root: Option<&PageRef>,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        match root {
            Some(root) => {
                self.cache
                    .lookup(
                        self.store,
                        self.path,
                        0,
                        self.source.clone(),
                        root,
                        kind,
                        keys,
                        work,
                    )
                    .await
            }
            None => Ok(vec![None; keys.len()]),
        }
    }
    async fn scan(
        &self,
        root: &PageRef,
        kind: Kind,
        work: &mut PageWork,
    ) -> Result<crate::pages::Rows> {
        self.cache
            .scan(
                self.store,
                self.path,
                0,
                self.source.clone(),
                root,
                kind,
                work,
            )
            .await
    }
    pub(crate) async fn positions(
        &self,
        oids: &[[u8; 20]],
        work: &mut PageWork,
    ) -> Result<Vec<Option<u32>>> {
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let rows = self
            .rows(
                self.descriptor.dictionary.as_ref(),
                Kind::VisibilityDictionary,
                &keys,
                work,
            )
            .await?;
        let positions = rows
            .into_iter()
            .map(|value| {
                value
                    .map(|value| {
                        let position = ordinal(&value)?;
                        if position as usize >= self.descriptor.object_count() {
                            return Err(corrupt("forward visibility ordinal exceeds dictionary"));
                        }
                        Ok(position)
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let keys = positions
            .iter()
            .flatten()
            .map(|position| position.to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        let reverse = self
            .rows(
                self.descriptor.ordinals.as_ref(),
                Kind::VisibilityOrdinals,
                &keys,
                work,
            )
            .await?;
        for ((oid, _), value) in oids
            .iter()
            .zip(&positions)
            .filter(|(_, position)| position.is_some())
            .zip(reverse)
        {
            if value.as_deref() != Some(oid.as_slice()) {
                return Err(corrupt("visibility ordinal does not bind its exact OID"));
            }
        }
        Ok(positions)
    }
    async fn oids(&self, positions: &[u32], work: &mut PageWork) -> Result<Vec<[u8; 20]>> {
        let keys = positions
            .iter()
            .map(|position| position.to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        let rows = self
            .rows(
                self.descriptor.ordinals.as_ref(),
                Kind::VisibilityOrdinals,
                &keys,
                work,
            )
            .await?;
        let oids = rows
            .into_iter()
            .map(|value| {
                value
                    .ok_or_else(|| corrupt("required reverse visibility ordinal is absent"))?
                    .try_into()
                    .map_err(|_| corrupt("reverse visibility OID has wrong length"))
            })
            .collect::<Result<Vec<[u8; 20]>>>()?;
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let rows = self
            .rows(
                self.descriptor.dictionary.as_ref(),
                Kind::VisibilityDictionary,
                &keys,
                work,
            )
            .await?;
        for (position, value) in positions.iter().zip(rows) {
            if value.as_deref() != Some(position.to_be_bytes().as_slice()) {
                return Err(corrupt(
                    "reverse visibility OID does not bind its exact ordinal",
                ));
            }
        }
        Ok(oids)
    }
    async fn pool(
        &self,
        ids: BTreeSet<u32>,
        work: &mut PageWork,
    ) -> Result<BTreeMap<u32, [u8; VISIBILITY_CHUNK_BYTES]>> {
        let keys = ids
            .iter()
            .map(|id| id.to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        let rows = self
            .rows(
                self.descriptor.chunks.as_ref(),
                Kind::VisibilityChunkPool,
                &keys,
                work,
            )
            .await?;
        ids.into_iter()
            .zip(rows)
            .map(|(id, value)| {
                Ok((
                    id,
                    value
                        .ok_or_else(|| corrupt("required visibility chunk is absent"))?
                        .try_into()
                        .map_err(|_| corrupt("visibility chunk has wrong width"))?,
                ))
            })
            .collect()
    }
    pub(crate) async fn lookup(
        &self,
        root: Option<&SetRoot>,
        oids: &[[u8; 20]],
        work: &mut PageWork,
    ) -> Result<Vec<bool>> {
        let Some(root) = root else {
            return Ok(vec![false; oids.len()]);
        };
        root.validate(self.descriptor, self.source.end - self.source.start)?;
        let positions = self.positions(oids, work).await?;
        match root.encoding {
            Encoding::Sparse => {
                let keys = positions
                    .iter()
                    .flatten()
                    .map(|position| position.to_be_bytes().to_vec())
                    .collect::<Vec<_>>();
                let values = self
                    .rows(Some(&root.root), Kind::VisibilitySparseSet, &keys, work)
                    .await?;
                let mut values = values.into_iter();
                positions
                    .into_iter()
                    .map(|position| match position {
                        None => Ok(false),
                        Some(_) => match values.next().expect("one sparse result per ordinal") {
                            None => Ok(false),
                            Some(value) if value.is_empty() => Ok(true),
                            _ => Err(corrupt("sparse visibility row has payload")),
                        },
                    })
                    .collect()
            }
            Encoding::Bitmap => {
                // Deduplicate table and pool keys before reading large values. Repeated
                // wants must not expand a shared1KiB table once per requested OID.
                let blocks = positions
                    .iter()
                    .flatten()
                    .map(|position| *position as usize / CHUNK_BITS / TABLE_SLOTS)
                    .collect::<BTreeSet<_>>();
                let keys = blocks
                    .iter()
                    .map(|block| (*block as u32).to_be_bytes().to_vec())
                    .collect::<Vec<_>>();
                let values = self
                    .rows(Some(&root.root), Kind::VisibilitySetChunks, &keys, work)
                    .await?;
                let tables = blocks
                    .into_iter()
                    .zip(values)
                    .map(|(block, value)| {
                        Ok((
                            block,
                            table_ids(
                                self.descriptor,
                                block,
                                &value.ok_or_else(|| {
                                    corrupt("required visibility chunk block is absent")
                                })?,
                            )?,
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                let ids = positions
                    .iter()
                    .flatten()
                    .map(|position| {
                        let chunk = *position as usize / CHUNK_BITS;
                        tables[&(chunk / TABLE_SLOTS)][chunk % TABLE_SLOTS]
                    })
                    .collect();
                let pool = self.pool(ids, work).await?;
                positions
                    .into_iter()
                    .map(|position| {
                        let Some(position) = position else {
                            return Ok(false);
                        };
                        let position = position as usize;
                        let chunk_index = position / CHUNK_BITS;
                        let chunk =
                            &pool[&tables[&(chunk_index / TABLE_SLOTS)][chunk_index % TABLE_SLOTS]];
                        validate_chunk(self.descriptor, chunk_index, chunk)?;
                        Ok(chunk[position % CHUNK_BITS / 8] & (1 << (position % 8)) != 0)
                    })
                    .collect()
            }
        }
    }
    pub(crate) async fn objects(
        &self,
        root: Option<&SetRoot>,
        work: &mut PageWork,
    ) -> Result<Vec<[u8; 20]>> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        root.validate(self.descriptor, self.source.end - self.source.start)?;
        let positions = match root.encoding {
            Encoding::Sparse => {
                let rows = self
                    .scan(&root.root, Kind::VisibilitySparseSet, work)
                    .await?;
                rows.into_iter()
                    .map(|(key, value)| {
                        let position = ordinal(&key)?;
                        if !value.is_empty() || position as usize >= self.descriptor.object_count()
                        {
                            return Err(corrupt("sparse visibility row is invalid"));
                        }
                        Ok(position)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            Encoding::Bitmap => {
                let rows = self
                    .scan(&root.root, Kind::VisibilitySetChunks, work)
                    .await?;
                let mut ids = Vec::new();
                for (block, (key, value)) in rows.into_iter().enumerate() {
                    if ordinal(&key)? as usize != block {
                        return Err(corrupt("visibility table block order differs"));
                    }
                    ids.extend(table_ids(self.descriptor, block, &value)?);
                }
                if ids.len() != self.descriptor.slots() {
                    return Err(corrupt("visibility chunk sequence is incomplete"));
                }
                let pool = self.pool(ids.iter().copied().collect(), work).await?;
                let mut positions = Vec::new();
                for (index, id) in ids.into_iter().enumerate() {
                    let chunk = &pool[&id];
                    validate_chunk(self.descriptor, index, chunk)?;
                    for (byte, bits) in chunk.iter().enumerate() {
                        let mut bits = *bits;
                        while bits != 0 {
                            positions.push(
                                (index * CHUNK_BITS + byte * 8 + bits.trailing_zeros() as usize)
                                    as u32,
                            );
                            if positions.len() > root.count as usize {
                                return Err(corrupt("visibility bits exceed advertised count"));
                            }
                            bits &= bits - 1;
                        }
                    }
                }
                positions
            }
        };
        if positions.len() != root.count as usize {
            return Err(corrupt("visibility bits differ from advertised count"));
        }
        self.oids(&positions, work).await
    }
}
fn validate_chunk(
    descriptor: &Descriptor,
    index: usize,
    chunk: &[u8; VISIBILITY_CHUNK_BYTES],
) -> Result<()> {
    let remainder = descriptor.object_count() % CHUNK_BITS;
    if index >= descriptor.slots() {
        return Err(corrupt("visibility chunk ordinal exceeds dictionary"));
    }
    if remainder != 0 && index + 1 == descriptor.slots() {
        let byte = remainder / 8;
        let bits = remainder % 8;
        if (bits != 0 && chunk[byte] >> bits != 0)
            || chunk[byte + usize::from(bits != 0)..]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(corrupt("visibility chunk has nonzero dictionary padding"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
