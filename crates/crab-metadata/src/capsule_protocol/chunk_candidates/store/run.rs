use super::*;
use crate::pages::{Page, PageRef, Writer};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Run {
    pub(super) hash: [u8; 32],
    pub(super) bytes: u64,
    pub(super) records: PageRef,
    pub(super) filter: PageRef,
}

impl Run {
    pub(super) fn validate(&self) -> Result<()> {
        if self.bytes == 0 || self.bytes > crate::capsule_protocol::MAX_CATALOG_BYTES {
            return Err(corrupt("capsule hint run length exceeds its bound"));
        }
        self.records.validate(KIND, self.bytes)?;
        filter::validate_shape(&self.filter, self.bytes, self.records.count)?;
        if self
            .records
            .offset
            .checked_add(u64::from(self.records.bytes))
            != Some(self.bytes)
        {
            return Err(corrupt("capsule hint directory does not finish its run"));
        }
        Ok(())
    }

    pub(super) fn level(&self) -> usize {
        (u64::BITS - self.bytes.leading_zeros() - 1) as usize
    }

    pub(super) fn path(&self, prefix: &Path) -> Path {
        let hash = blake3::Hash::from_bytes(self.hash).to_hex().to_string();
        prefix.clone().join(&hash[..2]).join(hash)
    }

    pub(super) fn build(rows: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<(Self, Bytes)> {
        let maximum = crate::capsule_protocol::MAX_CATALOG_BYTES;
        let bytes = rows.iter().try_fold(0u64, |bytes, (key, value)| {
            bytes.checked_add((key.len() + value.len() + 4) as u64)
        });
        if bytes.is_none_or(|bytes| bytes > maximum) {
            return Err(corrupt("capsule hint run input exceeds its byte bound"));
        }
        let mut writer = Writer::default();
        let filter = writer.tree(
            Kind::CapsuleChunkFilters,
            filter::build(rows.keys().map(Vec::as_slice))?,
        )?;
        let records = writer.tree(
            KIND,
            rows.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )?;
        let run = Self {
            hash: *blake3::hash(&writer.bytes).as_bytes(),
            bytes: writer.bytes.len() as u64,
            records,
            filter,
        };
        run.validate()?;
        Ok((run, Bytes::from(writer.bytes)))
    }

    pub(super) async fn read(
        &self,
        store: &Store,
        prefix: &Path,
        sequence: u64,
        work: &mut PageWork,
    ) -> Result<Rows> {
        self.validate()?;
        work.admit_object(self.hash, self.bytes)?;
        let path = self.path(prefix);
        let (body, _) = store.get_with_etag_bounded(&path, self.bytes).await?;
        if body.len() as u64 != self.bytes || blake3::hash(&body).as_bytes() != &self.hash {
            return Err(corrupt("capsule hint run content identity differs"));
        }
        let rows = scan(&body, &self.records, KIND, sequence, work, path.as_ref())?;
        let actual = scan(
            &body,
            &self.filter,
            Kind::CapsuleChunkFilters,
            sequence,
            work,
            path.as_ref(),
        )?;
        let expected = filter::build(rows.iter().map(|(key, _)| key.as_slice()))?;
        if actual != expected {
            return Err(corrupt(
                "capsule hint filter differs from its authenticated records",
            ));
        }
        Ok(rows)
    }

    pub(super) async fn filter_candidates(
        &self,
        store: &Store,
        path: &Path,
        cache: &PageCache,
        layer: usize,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<bool>> {
        filter::candidates(self, store, path, cache, layer, keys, work).await
    }
}

fn scan(
    body: &[u8],
    root: &PageRef,
    kind: Kind,
    sequence: u64,
    work: &mut PageWork,
    path: &str,
) -> Result<Rows> {
    let mut pending = vec![root.clone()];
    let mut rows = Vec::new();
    while let Some(reference) = pending.pop() {
        reference.validate(kind, body.len() as u64)?;
        let end = reference
            .offset
            .checked_add(u64::from(reference.bytes))
            .ok_or_else(|| corrupt("capsule hint page range overflows"))?;
        let bytes = body
            .get(reference.offset as usize..end as usize)
            .ok_or_else(|| corrupt("capsule hint page escapes its run"))?;
        match crate::pages::decode(bytes, &reference, kind, body.len() as u64, path)? {
            Page::Leaf(values) => {
                for (key, value) in &values {
                    if kind == KIND {
                        row_value(key, value, sequence)?;
                    }
                    work.admit_materialized(2 * (key.len() + value.len() + 96) as u64)?;
                }
                rows.extend(values);
            }
            Page::Branch(children) => pending.extend(children.into_iter().rev()),
        }
    }
    if rows.len() != root.count as usize || rows.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(corrupt(
            "capsule hint run coverage differs from its directory",
        ));
    }
    Ok(rows)
}

pub(super) fn row_value<'a>(
    key: &[u8],
    value: &'a [u8],
    maximum_sequence: u64,
) -> Result<(u64, &'a [u8])> {
    let prefix: [u8; 8] = value
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| corrupt("capsule hint row has no sequence"))?;
    let sequence = u64::from_le_bytes(prefix);
    if sequence == 0 || sequence > maximum_sequence {
        return Err(corrupt("capsule hint row sequence escapes its root"));
    }
    let raw = &value[8..];
    validate_row(key, raw)?;
    Ok((sequence, raw))
}

pub(super) fn merge(
    rows: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    incoming: Rows,
    sequence: u64,
) -> Result<()> {
    for (key, value) in incoming {
        let (incoming_sequence, incoming_raw) = row_value(&key, &value, sequence)?;
        if let Some(prior) = rows.get(&key) {
            let (prior_sequence, prior_raw) = row_value(&key, prior, sequence)?;
            if (key[0] != 0x06 || prior_sequence == incoming_sequence) && prior_raw != incoming_raw
            {
                return Err(corrupt(
                    "capsule hint compaction has conflicting identities",
                ));
            }
            if prior_sequence >= incoming_sequence {
                continue;
            }
        }
        rows.insert(key, value);
    }
    Ok(())
}
