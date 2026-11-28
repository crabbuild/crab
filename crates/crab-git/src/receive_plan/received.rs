//! Objects received with one ref batch, read by validation before the repository.
//!
//! A pushed pack is quarantined as bytes that validation parses. A fetched pack
//! can instead be parsed once while it streams, retaining only structure, so
//! validation and visibility planning need neither its bytes nor local files.

use super::*;

/// Objects validation reads before the committed repository source.
///
/// Sealed: [`IncomingPack`] supplies quarantined bytes and [`VerifiedObjects`]
/// supplies structure parsed while a fetched pack streamed.
pub trait Received: private::Sealed {
    /// Identity and kind of every received object.
    fn objects(&self) -> impl Iterator<Item = (ObjectId, Kind)> + '_;

    /// Decoded size of a received object.
    fn size(&self, oid: &ObjectId) -> Option<usize>;

    /// A received object; `None` defers to the repository source.
    fn load(&self, oid: &ObjectId) -> Result<Option<ReceivedObject>>;
}

/// One received object as validation consumes it.
pub struct ReceivedObject(Inner);

pub(super) enum Inner {
    Bytes(BaseObject),
    Verified {
        node: Node,
        size: usize,
        pointer: PointerKind,
    },
}

impl ReceivedObject {
    pub(super) fn into_inner(self) -> Inner {
        self.0
    }
}

mod private {
    pub trait Sealed {}
    impl Sealed for crate::incoming_pack::IncomingPack {}
    impl Sealed for super::VerifiedObjects {}
}

impl Received for IncomingPack {
    fn objects(&self) -> impl Iterator<Item = (ObjectId, Kind)> + '_ {
        IncomingPack::objects(self).map(|object| (object.oid, object.kind))
    }

    fn size(&self, oid: &ObjectId) -> Option<usize> {
        self.object(oid).map(|object| object.size)
    }

    fn load(&self, oid: &ObjectId) -> Result<Option<ReceivedObject>> {
        Ok(self
            .read_object(oid)?
            .map(|object| ReceivedObject(Inner::Bytes(object))))
    }
}

/// Structure of a fetched pack's objects, parsed once with the receive rules.
///
/// [`Self::add`] must receive the exact bytes each identity was computed from,
/// as [`crate::pack_stream::PackResolver`] supplies them. Bytes are not retained;
/// commits keep no message, so commit observers cannot run over these objects.
pub struct VerifiedObjects {
    objects: HashMap<ObjectId, Verified>,
    steps: usize,
    max_steps: usize,
}

struct Verified {
    node: Node,
    size: usize,
    pointer: PointerKind,
}

impl VerifiedObjects {
    /// Retain at most `max_steps` objects and tree entries.
    #[must_use]
    pub fn new(max_steps: usize) -> Self {
        Self {
            objects: HashMap::new(),
            steps: 0,
            max_steps,
        }
    }

    /// Check one object's syntax and retain its links, tree entries, and pointer.
    pub fn add(&mut self, oid: ObjectId, kind: Kind, data: &[u8]) -> Result<()> {
        if self.objects.contains_key(&oid) {
            return Ok(());
        }
        let structure = crate::object_check::inspect(kind, data, true)
            .map_err(|violation| ReceivePlanError::violation(oid, violation))?;
        self.steps = self
            .steps
            .checked_add(1 + structure.entries.len())
            .filter(|steps| *steps <= self.max_steps)
            .ok_or(ReceivePlanError::Limit("graph steps"))?;
        let node = Node {
            kind,
            links: structure.links,
            tree: structure
                .entries
                .into_iter()
                .map(|entry| TreeEntry {
                    name: entry.name.to_vec(),
                    oid: entry.oid,
                    mode: entry.mode,
                })
                .collect(),
        };
        self.objects.insert(
            oid,
            Verified {
                node,
                size: data.len(),
                pointer: structure.pointer,
            },
        );
        Ok(())
    }

    /// Number of distinct objects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

impl Received for VerifiedObjects {
    fn objects(&self) -> impl Iterator<Item = (ObjectId, Kind)> + '_ {
        self.objects
            .iter()
            .map(|(oid, verified)| (*oid, verified.node.kind))
    }

    fn size(&self, oid: &ObjectId) -> Option<usize> {
        self.objects.get(oid).map(|verified| verified.size)
    }

    fn load(&self, oid: &ObjectId) -> Result<Option<ReceivedObject>> {
        Ok(self.objects.get(oid).map(|verified| {
            ReceivedObject(Inner::Verified {
                node: verified.node.clone(),
                size: verified.size,
                pointer: verified.pointer.clone(),
            })
        }))
    }
}
