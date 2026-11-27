//! Exact object graph of a self-contained pack imported into an empty repository.
//!
//! Objects arrive in any order from [`crate::pack_stream`]. Each object passes
//! the same structural rules as receive validation; link targets receive
//! compact ordinals on first sight so the graph never stores repeated object
//! IDs. Completion proves every linked object is present with the linked kind.

use std::collections::HashMap;

use gix_hash::ObjectId;
use gix_object::Kind;

use crate::pointer_detect::PointerKind;
use crate::shared_visibility::{self, SharedVisibility};

/// Bounds for one imported graph.
#[derive(Clone, Copy, Debug)]
pub struct GraphLimits {
    pub max_objects: usize,
    /// Bounds distinct per-object links retained for visibility.
    pub max_links: usize,
}

/// Failures proving an imported graph.
#[derive(Debug, thiserror::Error)]
pub enum ImportGraphError {
    #[error("invalid object {oid}: {reason}")]
    Invalid { oid: ObjectId, reason: &'static str },
    #[error("cannot parse Git object {oid}")]
    Parse {
        oid: ObjectId,
        #[source]
        source: gix_object::decode::Error,
    },
    #[error("invalid tree path in {oid}")]
    Path {
        oid: ObjectId,
        #[source]
        source: gix_validate::path::component::Error,
    },
    #[error("invalid annotated tag name in {oid}")]
    TagName {
        oid: ObjectId,
        #[source]
        source: gix_validate::tag::name::Error,
    },
    #[error("object {oid} has kind {actual}, expected {expected}")]
    Kind {
        oid: ObjectId,
        expected: Kind,
        actual: Kind,
    },
    #[error("object {oid} is referenced but not in the pack")]
    Missing { oid: ObjectId },
    #[error("object {oid} appears twice")]
    Duplicate { oid: ObjectId },
    #[error("imported graph exceeds its {0} limit")]
    Limit(&'static str),
    #[error("imported graph visibility failed")]
    Visibility(#[from] shared_visibility::Error),
}

type Result<T> = std::result::Result<T, ImportGraphError>;

struct Node {
    oid: ObjectId,
    kind: Option<Kind>,
    // The first kind a link required; a later object must match it.
    expected: Option<Kind>,
    links: Box<[u32]>,
}

/// Graph under construction while a pack resolves.
pub struct ImportGraph {
    limits: GraphLimits,
    positions: HashMap<ObjectId, u32>,
    nodes: Vec<Node>,
    links: usize,
    pointers: Vec<(ObjectId, PointerKind)>,
}

impl ImportGraph {
    #[must_use]
    pub fn new(limits: GraphLimits) -> Self {
        Self {
            limits,
            positions: HashMap::new(),
            nodes: Vec::new(),
            links: 0,
            pointers: Vec::new(),
        }
    }

    /// Admit one object whose identity was computed from `data`.
    pub fn add(&mut self, oid: ObjectId, kind: Kind, data: &[u8]) -> Result<()> {
        let structure = crate::object_check::inspect(kind, data, false)
            .map_err(|violation| violation_error(oid, violation))?;
        let position = self.admit(oid)?;
        let node = &mut self.nodes[position as usize];
        if node.kind.is_some() {
            return Err(ImportGraphError::Duplicate { oid });
        }
        if let Some(expected) = node.expected
            && expected != kind
        {
            return Err(ImportGraphError::Kind {
                oid,
                expected,
                actual: kind,
            });
        }
        node.kind = Some(kind);
        let mut links = Vec::with_capacity(structure.links.len());
        for (target, expected) in structure.links {
            let position = self.admit(target)?;
            let target = &mut self.nodes[position as usize];
            match (target.kind, target.expected) {
                (Some(actual), _) | (None, Some(actual)) if actual != expected => {
                    return Err(ImportGraphError::Kind {
                        oid: target.oid,
                        expected,
                        actual,
                    });
                }
                (None, None) => target.expected = Some(expected),
                _ => {}
            }
            links.push(position);
        }
        links.sort_unstable();
        links.dedup();
        self.links = self
            .links
            .checked_add(links.len())
            .filter(|links| *links <= self.limits.max_links)
            .ok_or(ImportGraphError::Limit("links"))?;
        self.nodes[position as usize].links = links.into_boxed_slice();
        if !matches!(structure.pointer, PointerKind::NotAPointer) {
            self.pointers.push((oid, structure.pointer));
        }
        Ok(())
    }

    /// Prove every linked object is present and freeze the graph.
    pub fn finish(self) -> Result<CompleteGraph> {
        let mut kinds = Vec::with_capacity(self.nodes.len());
        let mut objects = Vec::with_capacity(self.nodes.len());
        let mut links = Vec::with_capacity(self.nodes.len());
        for node in self.nodes {
            kinds.push(
                node.kind
                    .ok_or(ImportGraphError::Missing { oid: node.oid })?,
            );
            objects.push(node.oid);
            links.push(node.links);
        }
        Ok(CompleteGraph {
            positions: self.positions,
            objects,
            kinds,
            links,
            pointers: self.pointers,
        })
    }

    fn admit(&mut self, oid: ObjectId) -> Result<u32> {
        if let Some(position) = self.positions.get(&oid) {
            return Ok(*position);
        }
        if self.nodes.len() >= self.limits.max_objects.min(u32::MAX as usize) {
            return Err(ImportGraphError::Limit("objects"));
        }
        let position = self.nodes.len() as u32;
        self.positions.insert(oid, position);
        self.nodes.push(Node {
            oid,
            kind: None,
            expected: None,
            links: Box::new([]),
        });
        Ok(position)
    }
}

/// A complete, closed graph of verified objects.
pub struct CompleteGraph {
    positions: HashMap<ObjectId, u32>,
    objects: Vec<ObjectId>,
    kinds: Vec<Kind>,
    links: Vec<Box<[u32]>>,
    pointers: Vec<(ObjectId, PointerKind)>,
}

impl CompleteGraph {
    /// Number of distinct objects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Kind of a present object.
    #[must_use]
    pub fn kind(&self, oid: &ObjectId) -> Option<Kind> {
        self.positions
            .get(oid)
            .map(|position| self.kinds[*position as usize])
    }

    /// Follow annotated tags to the first non-tag object.
    pub fn peel(&self, oid: ObjectId) -> Result<ObjectId> {
        let mut current = oid;
        for _ in 0..=self.objects.len() {
            let position = *self
                .positions
                .get(&current)
                .ok_or(ImportGraphError::Missing { oid: current })?;
            if self.kinds[position as usize] != Kind::Tag {
                return Ok(current);
            }
            // A tag has exactly one link: its target.
            let target =
                self.links[position as usize]
                    .first()
                    .ok_or(ImportGraphError::Invalid {
                        oid: current,
                        reason: "tag has no target",
                    })?;
            current = self.objects[*target as usize];
        }
        Err(ImportGraphError::Invalid {
            oid,
            reason: "tag cycle",
        })
    }

    /// Whether the first-parent history of a ref tip contains no merge commit.
    ///
    /// Annotated tags peel first; a non-commit target is trivially linear.
    pub fn is_linear(&self, tip: ObjectId) -> Result<bool> {
        let peeled = self.peel(tip)?;
        let mut current = *self
            .positions
            .get(&peeled)
            .ok_or(ImportGraphError::Missing { oid: peeled })?;
        // Hash-linked history is acyclic; the bound only guards a corrupt graph.
        for _ in 0..=self.objects.len() {
            if self.kinds[current as usize] != Kind::Commit {
                return Ok(true);
            }
            let mut parents = self.links[current as usize]
                .iter()
                .filter(|link| self.kinds[**link as usize] == Kind::Commit);
            let Some(parent) = parents.next() else {
                return Ok(true);
            };
            if parents.next().is_some() {
                return Ok(false);
            }
            current = *parent;
        }
        Err(ImportGraphError::Invalid {
            oid: tip,
            reason: "commit cycle",
        })
    }

    /// Pointer files found in imported blobs.
    #[must_use]
    pub fn pointers(&self) -> &[(ObjectId, PointerKind)] {
        &self.pointers
    }

    /// Exact closures for `roots`, in order, sharing one sorted dictionary.
    pub fn visibility(
        self,
        roots: &[ObjectId],
        limits: shared_visibility::Limits,
        check: impl Fn() -> bool,
    ) -> Result<SharedVisibility> {
        let roots = roots
            .iter()
            .map(|oid| {
                self.positions
                    .get(oid)
                    .copied()
                    .ok_or(ImportGraphError::Missing { oid: *oid })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(shared_visibility::from_graph(
            self.objects,
            self.links,
            &roots,
            limits,
            check,
        )?)
    }
}

fn violation_error(oid: ObjectId, violation: crate::object_check::Violation) -> ImportGraphError {
    use crate::object_check::Violation;
    match violation {
        Violation::Parse(source) => ImportGraphError::Parse { oid, source },
        Violation::Invalid(reason) => ImportGraphError::Invalid { oid, reason },
        Violation::Path(source) => ImportGraphError::Path { oid, source },
        Violation::TagName(source) => ImportGraphError::TagName { oid, source },
    }
}

#[cfg(test)]
mod tests;
