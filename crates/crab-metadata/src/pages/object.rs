//! Content-addressed pages whose unchanged subtrees survive a new publication.
use super::*;
use bytes::Bytes;
use std::collections::BTreeMap;

#[cfg(feature = "storage")]
mod store;
#[cfg(feature = "storage")]
pub(crate) use store::ObjectPages;

const MAGIC: &[u8; 8] = b"CRABOP01";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reference {
    pub(crate) hash: [u8; 32],
    pub(crate) bytes: u32,
    pub(crate) height: u8,
    pub(crate) count: u32,
    pub(crate) first: Vec<u8>,
    pub(crate) last: Vec<u8>,
}

impl Reference {
    pub(crate) fn validate(&self, kind: Kind) -> Result<()> {
        if self.first.len() != kind.key_bytes()
            || self.last.len() != kind.key_bytes()
            || self.first > self.last
            || self.count == 0
            || self.height > MAX_HEIGHT
            || !(HEADER..=MAX_BYTES).contains(&(self.bytes as usize))
        {
            return Err(corruption("invalid object page reference"));
        }
        if kind.is_positional()
            && integer(&self.last)?
                .checked_sub(integer(&self.first)?)
                .and_then(|span| span.checked_add(1))
                != Some(self.count)
        {
            return Err(corruption("positional object page has incomplete coverage"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum Node {
    Leaf(Rows),
    Branch(Vec<Reference>),
}

pub(crate) struct Encoded {
    pub(crate) reference: Reference,
    pub(crate) bytes: Bytes,
}

#[derive(Default)]
pub(crate) struct Builder {
    pub(crate) objects: BTreeMap<[u8; 32], Bytes>,
}

impl Builder {
    pub(crate) fn tree(&mut self, kind: Kind, rows: Rows) -> Result<Option<Reference>> {
        if rows.is_empty() {
            return Ok(None);
        }
        let mut level = self.retain(leaves(kind, rows)?);
        while level.len() > 1 {
            level = self.retain(branches(kind, level)?);
        }
        Ok(level.pop())
    }

    fn retain(&mut self, encoded: Vec<Encoded>) -> Vec<Reference> {
        encoded
            .into_iter()
            .map(|page| {
                self.objects.insert(page.reference.hash, page.bytes);
                page.reference
            })
            .collect()
    }
}

fn encode(kind: Kind, node: Node) -> Result<Encoded> {
    let (height, count, first, last, entries) = match &node {
        Node::Leaf(rows) => {
            validate_rows(kind, rows)?;
            (
                0,
                u32::try_from(rows.len()).map_err(|_| corruption("leaf count overflows"))?,
                rows[0].0.clone(),
                rows[rows.len() - 1].0.clone(),
                rows.len(),
            )
        }
        Node::Branch(children) => {
            validate_children(kind, children)?;
            let first = &children[0];
            (
                first.height + 1,
                children.iter().try_fold(0u32, |total, child| {
                    total
                        .checked_add(child.count)
                        .ok_or_else(|| corruption("object directory count overflows"))
                })?,
                first.first.clone(),
                children[children.len() - 1].last.clone(),
                children.len(),
            )
        }
    };
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&[kind as u8, height]);
    put_u32(&mut bytes, entries)?;
    match node {
        Node::Leaf(rows) => {
            for (key, value) in rows {
                bytes.extend_from_slice(&key);
                put_u32(&mut bytes, value.len())?;
                bytes.extend_from_slice(&value);
            }
        }
        Node::Branch(children) => {
            for child in children {
                bytes.extend_from_slice(&child.hash);
                bytes.extend_from_slice(&child.bytes.to_le_bytes());
                bytes.push(child.height);
                bytes.extend_from_slice(&child.count.to_le_bytes());
                bytes.extend_from_slice(&child.first);
                bytes.extend_from_slice(&child.last);
            }
        }
    }
    if bytes.len() > MAX_BYTES {
        return Err(corruption("object page exceeds its byte limit"));
    }
    let reference = Reference {
        hash: *blake3::hash(&bytes).as_bytes(),
        bytes: bytes.len() as u32,
        height,
        count,
        first,
        last,
    };
    reference.validate(kind)?;
    Ok(Encoded {
        reference,
        bytes: bytes.into(),
    })
}

fn validate_children(kind: Kind, children: &[Reference]) -> Result<()> {
    let first = children
        .first()
        .ok_or_else(|| corruption("empty object directory"))?;
    if first.height >= MAX_HEIGHT {
        return Err(corruption("object directory exceeds its height limit"));
    }
    for child in children {
        child.validate(kind)?;
        if child.height != first.height {
            return Err(corruption("object directory has mixed child heights"));
        }
    }
    for pair in children.windows(2) {
        if pair[0].last >= pair[1].first
            || (kind.is_positional()
                && integer(&pair[0].last)?.checked_add(1) != Some(integer(&pair[1].first)?))
        {
            return Err(corruption(
                "object directory has overlapping or incomplete routing",
            ));
        }
    }
    Ok(())
}

fn leaves(kind: Kind, rows: Rows) -> Result<Vec<Encoded>> {
    validate_rows(kind, &rows)?;
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut size = HEADER;
    for row in rows {
        let row_bytes = kind
            .key_bytes()
            .checked_add(4)
            .and_then(|bytes| bytes.checked_add(row.1.len()))
            .filter(|bytes| *bytes <= MAX_BYTES - HEADER)
            .ok_or_else(|| corruption("object row exceeds its byte limit"))?;
        if !pending.is_empty() && size + row_bytes > TARGET_BYTES {
            output.push(encode(kind, Node::Leaf(std::mem::take(&mut pending)))?);
            size = HEADER;
        }
        size += row_bytes;
        pending.push(row);
    }
    if !pending.is_empty() {
        output.push(encode(kind, Node::Leaf(pending))?);
    }
    Ok(output)
}

fn branches(kind: Kind, children: Vec<Reference>) -> Result<Vec<Encoded>> {
    validate_children(kind, &children)?;
    let fanout = (TARGET_BYTES - HEADER) / (41 + 2 * kind.key_bytes());
    children
        .chunks(fanout)
        .map(|children| encode(kind, Node::Branch(children.to_vec())))
        .collect()
}

#[cfg(any(test, feature = "storage"))]
fn decode(bytes: &[u8], reference: &Reference, kind: Kind) -> Result<Node> {
    reference.validate(kind)?;
    if bytes.len() != reference.bytes as usize
        || blake3::hash(bytes).as_bytes() != &reference.hash
        || bytes.get(..8) != Some(MAGIC.as_slice())
        || bytes.get(8) != Some(&(kind as u8))
        || bytes.get(9) != Some(&reference.height)
    {
        return Err(corruption("object page identity or header differs"));
    }
    let mut input = Input::new(&bytes[10..], "object metadata page");
    let count = input.u32()? as usize;
    if count == 0 {
        return Err(corruption("empty object page"));
    }
    let node = if reference.height == 0 {
        if count > input.remaining() / (kind.key_bytes() + 4) {
            return Err(corruption("object leaf count exceeds encoded bytes"));
        }
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            let key = input.take(kind.key_bytes())?.to_vec();
            let size = input.u32()? as usize;
            rows.push((key, input.take(size)?.to_vec()));
        }
        validate_rows(kind, &rows)?;
        if count != reference.count as usize
            || rows[0].0 != reference.first
            || rows[rows.len() - 1].0 != reference.last
        {
            return Err(corruption("object leaf coverage differs"));
        }
        Node::Leaf(rows)
    } else {
        if count > input.remaining() / (41 + 2 * kind.key_bytes()) {
            return Err(corruption("object directory count exceeds encoded bytes"));
        }
        let mut children = Vec::with_capacity(count);
        for _ in 0..count {
            children.push(Reference {
                hash: input.array()?,
                bytes: input.u32()?,
                height: input.array::<1>()?[0],
                count: input.u32()?,
                first: input.take(kind.key_bytes())?.to_vec(),
                last: input.take(kind.key_bytes())?.to_vec(),
            });
        }
        validate_children(kind, &children)?;
        let total = children.iter().try_fold(0u32, |total, child| {
            total
                .checked_add(child.count)
                .ok_or_else(|| corruption("object directory count overflows"))
        })?;
        if children[0].height + 1 != reference.height
            || total != reference.count
            || children[0].first != reference.first
            || children[children.len() - 1].last != reference.last
        {
            return Err(corruption("object directory coverage differs"));
        }
        Node::Branch(children)
    };
    input.finish()?;
    Ok(node)
}

#[cfg(test)]
mod tests;
