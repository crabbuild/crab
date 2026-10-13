//! Persistent authenticated ancestor sets; full intervals collapse shared subtrees.
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;

#[cfg(any(feature = "storage", test))]
use super::corrupt_at;
use super::{CommitGraphLayer, Result, corrupt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Reference {
    pub(super) hash: [u8; 32],
    pub(super) start: u32,
    pub(super) height: u8,
}

impl Reference {
    pub(super) fn end(self) -> u64 {
        u64::from(self.start) + (1u64 << self.height)
    }

    #[cfg(feature = "storage")]
    pub(super) fn contains(self, ordinal: u32) -> bool {
        ordinal >= self.start && u64::from(ordinal) < self.end()
    }

    pub(super) fn encode(self) -> Vec<u8> {
        let mut bytes = self.hash.to_vec();
        bytes.extend_from_slice(&self.start.to_le_bytes());
        bytes.push(self.height);
        bytes
    }

    #[cfg(any(feature = "storage", test))]
    pub(super) fn decode(bytes: &[u8], path: &str) -> Result<Self> {
        if bytes.len() != 37 {
            return corrupt_at(path, "invalid ancestry reference length");
        }
        let mut hash = [0; 32];
        hash.copy_from_slice(&bytes[..32]);
        let mut start = [0; 4];
        start.copy_from_slice(&bytes[32..36]);
        let reference = Self {
            hash,
            start: u32::from_le_bytes(start),
            height: bytes[36],
        };
        if reference.height > 32
            || u64::from(reference.start) & ((1u64 << reference.height) - 1) != 0
        {
            return corrupt_at(path, "invalid ancestry interval");
        }
        Ok(reference)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Node {
    pub(super) reference: Reference,
    pub(super) children: Option<[Reference; 2]>,
}

impl Node {
    fn new(start: u32, height: u8, children: Option<[Reference; 2]>) -> Self {
        let mut node = Self {
            reference: Reference {
                hash: [0; 32],
                start,
                height,
            },
            children,
        };
        node.reference.hash = *blake3::hash(&node.encode()).as_bytes();
        node
    }

    pub(super) fn encode(self) -> Vec<u8> {
        let mut bytes = vec![u8::from(self.children.is_some()), self.reference.height];
        bytes.extend_from_slice(&self.reference.start.to_le_bytes());
        if let Some(children) = self.children {
            for child in children {
                bytes.extend_from_slice(&child.encode());
            }
        }
        bytes
    }

    #[cfg(any(feature = "storage", test))]
    pub(super) fn decode(bytes: &[u8], expected: Reference, path: &str) -> Result<Self> {
        if !matches!(bytes.len(), 6 | 80) || *blake3::hash(bytes).as_bytes() != expected.hash {
            return corrupt_at(path, "ancestry node identity or length mismatch");
        }
        let children = match bytes[0] {
            0 if bytes.len() == 6 => None,
            1 if bytes.len() == 80 => Some([
                Reference::decode(&bytes[6..43], path)?,
                Reference::decode(&bytes[43..80], path)?,
            ]),
            _ => return corrupt_at(path, "invalid ancestry node kind"),
        };
        let mut start = [0; 4];
        start.copy_from_slice(&bytes[2..6]);
        if u32::from_le_bytes(start) != expected.start || bytes[1] != expected.height {
            return corrupt_at(path, "ancestry node disagrees with its reference");
        }
        if let Some([left, right]) = children {
            if expected.height == 0 {
                return corrupt_at(path, "ancestry branch has zero height");
            }
            let middle = u64::from(expected.start) + (1u64 << (expected.height - 1));
            if left.height >= expected.height
                || right.height >= expected.height
                || left.start < expected.start
                || left.end() > middle
                || u64::from(right.start) < middle
                || right.end() > expected.end()
                || (left.height == expected.height - 1
                    && right.height == left.height
                    && left.hash == Self::new(left.start, left.height, None).reference.hash
                    && right.hash == Self::new(right.start, right.height, None).reference.hash)
            {
                return corrupt_at(path, "invalid or noncanonical ancestry branch");
            }
        }
        Ok(Self {
            reference: expected,
            children,
        })
    }
}

pub(super) struct Index {
    pub(super) roots: Vec<Reference>,
    nodes: BTreeMap<[u8; 32], Node>,
    first_use: BTreeMap<[u8; 32], u32>,
}

impl Index {
    pub(super) fn build(layers: &[CommitGraphLayer]) -> Result<Self> {
        let mut index = Self {
            roots: Vec::new(),
            nodes: BTreeMap::new(),
            first_use: BTreeMap::new(),
        };
        for layer in layers {
            for record in &layer.records {
                let ordinal = u32::try_from(index.roots.len()).map_err(|_| {
                    crate::validation::corrupt_object("ancestry", "ordinal exceeds u32")
                })?;
                let mut root = None;
                for parent in &record.parents {
                    let parent_root =
                        index.roots.get(*parent as usize).copied().ok_or_else(|| {
                            crate::validation::corrupt_object(
                                "ancestry",
                                "parent is not earlier than child",
                            )
                        })?;
                    root = Some(match root {
                        None => parent_root,
                        Some(root) => index.union(root, parent_root)?,
                    });
                }
                let leaf = index.intern(Node::new(ordinal, 0, None));
                let root = match root {
                    None => leaf,
                    Some(root) => index.union(root, leaf)?,
                };
                index.roots.push(root);
            }
        }
        // Union can create transient nodes. Only closure reachable from committed
        // roots belongs in the on-disk directory; its first use fixes the layer.
        for (ordinal, root) in index.roots.iter().copied().enumerate() {
            let mut pending = vec![root];
            while let Some(reference) = pending.pop() {
                if index.first_use.contains_key(&reference.hash) {
                    continue;
                }
                index.first_use.insert(reference.hash, ordinal as u32);
                if let Some(children) = index.node(reference)?.children {
                    pending.extend(children);
                }
            }
        }
        Ok(index)
    }

    pub(super) fn rows(&self, start: u32, count: u32) -> Result<crate::pages::Rows> {
        let end = start
            .checked_add(count)
            .ok_or_else(|| crate::validation::corrupt_object("ancestry", "layer range overflow"))?;
        self.first_use
            .iter()
            .filter(|(_, ordinal)| **ordinal >= start && **ordinal < end)
            .map(|(hash, _)| {
                Ok((
                    hash.to_vec(),
                    self.nodes
                        .get(hash)
                        .ok_or_else(|| {
                            crate::validation::corrupt_object("ancestry", "node disappeared")
                        })?
                        .encode(),
                ))
            })
            .collect()
    }

    fn intern(&mut self, node: Node) -> Reference {
        self.nodes.entry(node.reference.hash).or_insert(node);
        node.reference
    }

    fn node(&self, reference: Reference) -> Result<Node> {
        let node = self.nodes.get(&reference.hash).copied().ok_or_else(|| {
            crate::validation::corrupt_object("ancestry", "required node is absent")
        })?;
        if node.reference != reference {
            return corrupt("ancestry reference changed");
        }
        Ok(node)
    }

    fn union(&mut self, left: Reference, right: Reference) -> Result<Reference> {
        if left == right {
            return Ok(left);
        }
        if left.start > right.start || (left.start == right.start && left.height < right.height) {
            return self.union(right, left);
        }
        let left_node = self.node(left)?;
        if left.start == right.start && left.height == right.height {
            let right_node = self.node(right)?;
            let (Some([a, b]), Some([c, d])) = (left_node.children, right_node.children) else {
                return Ok(if left_node.children.is_none() {
                    left
                } else {
                    right
                });
            };
            let first = self.union(a, c)?;
            let second = self.union(b, d)?;
            return self.branch(first, second);
        }
        if left.end() >= right.end() {
            let Some([first, second]) = left_node.children else {
                return Ok(left);
            };
            let middle = u64::from(left.start) + (1u64 << (left.height - 1));
            if u64::from(right.start) < middle {
                let first = self.union(first, right)?;
                return self.branch(first, second);
            }
            let second = self.union(second, right)?;
            return self.branch(first, second);
        }
        self.branch(left, right)
    }

    fn branch(&mut self, left: Reference, right: Reference) -> Result<Reference> {
        if left.start >= right.start || left.end() > u64::from(right.start) {
            return corrupt("ancestry branches overlap");
        }
        let last = u32::try_from(right.end() - 1)
            .map_err(|_| crate::validation::corrupt_object("ancestry", "interval exceeds u32"))?;
        let height = (32 - (left.start ^ last).leading_zeros()) as u8;
        let mask = u32::MAX.checked_shl(u32::from(height)).unwrap_or(0);
        let start = left.start & mask;
        let full = left.height + 1 == height
            && right.height == left.height
            && self.node(left)?.children.is_none()
            && self.node(right)?.children.is_none();
        Ok(self.intern(Node::new(start, height, (!full).then_some([left, right]))))
    }

    #[cfg(feature = "storage")]
    pub(super) fn validate_layers(
        &self,
        roots: &[Reference],
        rows: &BTreeMap<[u8; 32], (Vec<u8>, u32, u32)>,
    ) -> Result<()> {
        if self.roots != roots || self.first_use.len() != rows.len() {
            return corrupt("ancestry coverage differs from the verified DAG");
        }
        for hash in self.first_use.keys() {
            let node = self.nodes.get(hash).ok_or_else(|| {
                crate::validation::corrupt_object("ancestry", "verified node disappeared")
            })?;
            let first_use = self.first_use[hash];
            if rows.get(hash).is_none_or(|(bytes, start, count)| {
                *bytes != node.encode() || first_use < *start || first_use - *start >= *count
            }) {
                return corrupt("ancestry node differs from the verified DAG");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn members(&self, root: Reference) -> Result<BTreeSet<u32>> {
        let mut members = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(reference) = pending.pop() {
            match self.node(reference)?.children {
                Some(children) => pending.extend(children),
                None => members.extend(u64::from(reference.start)..reference.end()),
            }
        }
        Ok(members.into_iter().map(|value| value as u32).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_commit_graph::CommitGraphRecord;

    #[test]
    fn membership_storage_growth_is_bounded_for_linear_branch_and_merge_shapes() {
        for shape in 0..4 {
            let mut previous = None;
            for count in [1_000, 2_000, 4_000, 8_000] {
                let records = (0..count)
                    .map(|i| {
                        let parents = match shape {
                            0 if i > 0 => vec![i - 1],
                            1 if i >= 8 => vec![i - 8],
                            2 if i >= 2 => vec![(i / 2 - 1) * 2, (i / 2 - 1) * 2 + 1],
                            _ => Vec::new(),
                        };
                        CommitGraphRecord {
                            oid: [0; 20],
                            tree_oid: [0; 20],
                            commit_time: 0,
                            corrected_generation: 0,
                            parents,
                        }
                    })
                    .collect();
                let index = Index::build(&[CommitGraphLayer {
                    base_ordinal: 0,
                    records,
                }])
                .unwrap();
                let bytes = index
                    .first_use
                    .keys()
                    .map(|hash| index.nodes[hash].encode().len() + 36)
                    .sum::<usize>()
                    + index.roots.len() * 45;
                if let Some(previous) = previous {
                    assert!(
                        bytes <= previous * 3,
                        "shape {shape}, count {count}: {bytes} > 3 * {previous}"
                    );
                }
                previous = Some(bytes);
            }
        }
    }

    #[test]
    fn ancestry_nodes_reject_invalid_intervals_and_noncanonical_branch_shapes() {
        let left = Node::new(0, 0, None).reference;
        let right = Node::new(1, 0, None).reference;
        let reducible = Node::new(0, 1, Some([left, right]));
        let mut invalid = left.encode();
        invalid[36] = 33;
        assert!(Reference::decode(&invalid, "invalid").is_err());
        assert!(Node::decode(&reducible.encode(), reducible.reference, "reducible").is_err());
        let overlapping = Node::new(0, 1, Some([left, left]));
        assert!(Node::decode(&overlapping.encode(), overlapping.reference, "overlapping").is_err());
    }

    proptest::proptest! {
        #[test]
        fn persistent_sets_match_dags_with_arbitrary_distant_parent_edges(edges in proptest::collection::vec(proptest::collection::vec(0u16..u16::MAX, 0..5), 1..60)) {
            let records = edges.iter().enumerate().map(|(i, edges)| CommitGraphRecord {
                oid:[0;20], tree_oid:[0;20], commit_time:0, corrected_generation:0,
                parents: if i == 0 { Vec::new() } else { edges.iter().map(|edge| u32::from(*edge) % i as u32).collect() },
            }).collect::<Vec<_>>();
            let index = Index::build(&[CommitGraphLayer {base_ordinal:0, records:records.clone()}]).unwrap();
            for ordinal in 0..records.len() {
                let mut expected=BTreeSet::new(); let mut pending=vec![ordinal as u32];
                while let Some(current)=pending.pop() { if expected.insert(current) { pending.extend(records[current as usize].parents.iter().copied()); } }
                proptest::prop_assert_eq!(index.members(index.roots[ordinal]).unwrap(), expected);
            }
        }

        #[test]
        fn persistent_sets_match_full_dag_walk(selectors in proptest::collection::vec(0u8..8, 1..80)) {
            let records = selectors.iter().enumerate().map(|(i,s)| CommitGraphRecord {
                oid:[0;20],tree_oid:[0;20],commit_time:0,corrected_generation:0,
                parents: [(i>0 && s&1!=0).then_some(i.saturating_sub(1) as u32), (i>1 && s&2!=0).then_some(i.saturating_sub(2) as u32)].into_iter().flatten().collect(),
            }).collect::<Vec<_>>();
            let index = Index::build(&[CommitGraphLayer {base_ordinal:0, records:records.clone()}]).unwrap();
            for ordinal in 0..records.len() {
                let mut expected=BTreeSet::new(); let mut pending=vec![ordinal as u32];
                while let Some(current)=pending.pop() { if expected.insert(current) { pending.extend(records[current as usize].parents.iter().copied()); } }
                proptest::prop_assert_eq!(index.members(index.roots[ordinal]).unwrap(),expected);
            }
        }
    }
}
