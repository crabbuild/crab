//! Exact Git ref planning and bounded incoming graph validation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use bstr::ByteSlice;
use gix_hash::ObjectId;
use gix_object::Kind;

use crate::{
    incoming_pack::{BaseObject, IncomingPack, IncomingPackError},
    pointer_detect::PointerKind,
};

type Result<T> = std::result::Result<T, ReceivePlanError>;
type SourceError = Box<dyn std::error::Error + Send + Sync>;

mod visibility;
pub use visibility::{RefVisibility, VisibilitySource, plan_visibility};

/// One exact ref comparison and replacement; `None` represents absence/deletion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
}

/// Server policy for a ref; native Git sends no separate force-push flag.
#[derive(Clone, Copy)]
pub struct RefPolicy {
    pub allow_delete: bool,
    pub allow_non_fast_forward: bool,
}

/// Bounds for one validation, including ancestry walks and trusted frontier lookups.
#[derive(Clone, Copy)]
pub struct GraphLimits {
    pub max_ref_updates: usize,
    pub max_graph_steps: usize,
    pub max_object_bytes: usize,
    pub max_read_bytes: u64,
}

/// Generation-pinned, authorized access to the committed repository.
///
/// `trusted_kind` may return a kind only when a committed visibility proof covers
/// that object's complete Git closure. Mere presence in a pack or locator is not
/// sufficient. Return `None` when no such proof exists; validation then reads and
/// traverses the object. Both methods must bound allocations and I/O deadlines.
pub trait GraphSource {
    fn trusted_kind(&mut self, oid: &ObjectId) -> std::result::Result<Option<Kind>, SourceError>;
    fn read(&mut self, oid: &ObjectId) -> std::result::Result<Option<BaseObject>, SourceError>;
}

/// A recognized pointer whose backing content needs separate publication proof.
#[derive(Debug)]
pub struct PointerDependency {
    pub blob: ObjectId,
    pub pointer: PointerKind,
}

/// Candidate refs after exact comparisons and graph checks, without publication.
#[derive(Debug)]
pub struct ValidatedRefUpdates {
    refs: BTreeMap<String, ObjectId>,
    peeled: BTreeMap<String, ObjectId>,
    pointers: Vec<PointerDependency>,
}
impl ValidatedRefUpdates {
    /// Returns the complete candidate ref map.
    pub fn refs(&self) -> &BTreeMap<String, ObjectId> {
        &self.refs
    }
    /// Returns peeled targets for changed refs whose targets are annotated tags.
    pub fn peeled(&self) -> &BTreeMap<String, ObjectId> {
        &self.peeled
    }
    /// Returns pointers in inspected objects, including imported thin-pack bases.
    pub fn pointers(&self) -> &[PointerDependency] {
        &self.pointers
    }
}

/// A rejected plan leaves both the base refs and the quarantine unchanged.
#[derive(Debug, thiserror::Error)]
pub enum ReceivePlanError {
    #[error("invalid ref update for {name}: {reason}")]
    Ref { name: String, reason: &'static str },
    #[error("invalid Git ref name {name}")]
    RefName {
        name: String,
        #[source]
        source: gix_validate::reference::name::Error,
    },
    #[error("invalid annotated tag name in {oid}")]
    TagName {
        oid: ObjectId,
        #[source]
        source: gix_validate::tag::name::Error,
    },
    #[error("ref {name} has changed since advertisement")]
    Stale { name: String },
    #[error("non-fast-forward update rejected for {name}")]
    NonFastForward { name: String },
    #[error("object {oid} is missing")]
    Missing { oid: ObjectId },
    #[error("object {oid} has kind {actual}, expected {expected}")]
    Kind {
        oid: ObjectId,
        expected: Kind,
        actual: Kind,
    },
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
    #[error("repository object lookup failed for {oid}")]
    Source {
        oid: ObjectId,
        #[source]
        source: SourceError,
    },
    #[error("incoming object read failed")]
    Incoming(#[from] IncomingPackError),
    #[error("receive validation exceeds {0}")]
    Limit(&'static str),
    #[error("receive validation cancelled")]
    Cancelled,
}

#[derive(Clone)]
struct Node {
    kind: Kind,
    links: Vec<(ObjectId, Kind)>,
}
struct Validator<'a, S, C> {
    incoming: &'a IncomingPack,
    source: &'a mut S,
    nodes: HashMap<ObjectId, Node>,
    trusted: HashMap<ObjectId, Option<Kind>>,
    pointers: BTreeMap<ObjectId, PointerKind>,
    limits: GraphLimits,
    steps: usize,
    bytes: u64,
    cancelled: C,
}

/// Validates an atomic ref batch without rewriting objects or publishing any data.
///
/// The caller must recheck the same base refs under its writer locks before
/// publication, prove every returned pointer dependency, and bind the result to
/// the same pinned generation. Quarantine integrity alone is insufficient.
/// Object sources execute synchronously; run this operation on a blocking worker.
pub fn validate<S: GraphSource, C: Fn() -> bool>(
    incoming: &IncomingPack,
    base: &BTreeMap<String, ObjectId>,
    updates: &[RefUpdate],
    policy: impl Fn(&str) -> RefPolicy,
    source: &mut S,
    limits: GraphLimits,
    cancelled: C,
) -> Result<ValidatedRefUpdates> {
    if cancelled() {
        return Err(ReceivePlanError::Cancelled);
    }
    if updates.len() > limits.max_ref_updates {
        return Err(ReceivePlanError::Limit("ref updates"));
    }
    let mut names = BTreeSet::new();
    let mut refs = base.clone();
    for update in updates {
        let reject = |reason| ReceivePlanError::Ref {
            name: update.name.clone(),
            reason,
        };
        if !update.name.starts_with("refs/") {
            return Err(reject("a fully qualified Git ref is required"));
        }
        gix_validate::reference::name(update.name.as_bytes().as_bstr()).map_err(|source| {
            ReceivePlanError::RefName {
                name: update.name.clone(),
                source,
            }
        })?;
        if !names.insert(&update.name) {
            return Err(reject("duplicate destination"));
        }
        if update.old == update.new {
            return Err(reject("update does not change the ref"));
        }
        if update
            .old
            .into_iter()
            .chain(update.new)
            .any(|oid| oid.is_null())
        {
            return Err(reject("use absence instead of a zero object ID"));
        }
        if base.get(&update.name).copied() != update.old {
            return Err(ReceivePlanError::Stale {
                name: update.name.clone(),
            });
        }
        if update.new.is_none() && !policy(&update.name).allow_delete {
            return Err(reject("deletion is prohibited"));
        }
        match update.new {
            Some(oid) => {
                refs.insert(update.name.clone(), oid);
            }
            None => {
                refs.remove(&update.name);
            }
        }
    }
    // Git cannot store a ref and another ref nested beneath its name. Evaluate
    // the final map so an atomic delete-and-create can replace that namespace.
    for name in refs.keys() {
        for (index, _) in name.match_indices('/') {
            if refs.contains_key(&name[..index]) {
                return Err(ReceivePlanError::Ref {
                    name: name.clone(),
                    reason: "ref namespace conflicts with another ref",
                });
            }
        }
    }
    let mut validator = Validator::new(incoming, source, limits, cancelled);
    // Validate all received objects, including unreachable objects. Otherwise a
    // syntactically corrupt object could be published for a later ref update.
    for object in incoming.objects() {
        validator.load(object.oid)?;
    }
    let mut roots = incoming
        .objects()
        .map(|object| (object.oid, Some(object.kind)))
        .collect::<Vec<_>>();
    for update in updates {
        if let Some(oid) = update.new {
            let required = update
                .name
                .starts_with("refs/heads/")
                .then_some(Kind::Commit);
            roots.push((oid, required));
        }
    }
    validator.connected(roots)?;
    let mut peeled = BTreeMap::new();
    for update in updates {
        let Some(new) = update.new else { continue };
        if let Some(old) = update.old
            && !policy(&update.name).allow_non_fast_forward
            && (!update.name.starts_with("refs/heads/") || !validator.is_ancestor(old, new)?)
        {
            return Err(ReceivePlanError::NonFastForward {
                name: update.name.clone(),
            });
        }
        let mut current = new;
        let mut seen = HashSet::new();
        loop {
            validator.step()?;
            if !seen.insert(current) {
                return Err(ReceivePlanError::Invalid {
                    oid: current,
                    reason: "tag cycle",
                });
            }
            let node = validator.load(current)?;
            if node.kind != Kind::Tag {
                break;
            }
            current = node
                .links
                .first()
                .ok_or(ReceivePlanError::Invalid {
                    oid: current,
                    reason: "tag has no target",
                })?
                .0;
        }
        if current != new {
            peeled.insert(update.name.clone(), current);
        }
    }
    Ok(ValidatedRefUpdates {
        refs,
        peeled,
        pointers: validator
            .pointers
            .into_iter()
            .map(|(blob, pointer)| PointerDependency { blob, pointer })
            .collect(),
    })
}

impl<'a, S: GraphSource, C: Fn() -> bool> Validator<'a, S, C> {
    fn new(
        incoming: &'a IncomingPack,
        source: &'a mut S,
        limits: GraphLimits,
        cancelled: C,
    ) -> Self {
        Self {
            incoming,
            source,
            limits,
            cancelled,
            nodes: HashMap::new(),
            trusted: HashMap::new(),
            pointers: BTreeMap::new(),
            steps: 0,
            bytes: 0,
        }
    }

    fn trusted_kind(&mut self, oid: ObjectId) -> Result<Option<Kind>> {
        if let Some(kind) = self.trusted.get(&oid) {
            return Ok(*kind);
        }
        let kind = self
            .source
            .trusted_kind(&oid)
            .map_err(|source| ReceivePlanError::Source { oid, source })?;
        self.trusted.insert(oid, kind);
        Ok(kind)
    }

    fn step(&mut self) -> Result<()> {
        if (self.cancelled)() {
            return Err(ReceivePlanError::Cancelled);
        }
        self.steps = self
            .steps
            .checked_add(1)
            .filter(|steps| *steps <= self.limits.max_graph_steps)
            .ok_or(ReceivePlanError::Limit("graph steps"))?;
        Ok(())
    }
    fn load(&mut self, oid: ObjectId) -> Result<Node> {
        if let Some(node) = self.nodes.get(&oid) {
            return Ok(node.clone());
        }
        self.step()?;
        if let Some(object) = self.incoming.object(&oid) {
            if object.size > self.limits.max_object_bytes {
                return Err(ReceivePlanError::Limit("object bytes"));
            }
            if (object.size as u64) > self.limits.max_read_bytes.saturating_sub(self.bytes) {
                return Err(ReceivePlanError::Limit("total object bytes"));
            }
        }
        let object = match self.incoming.read_object(&oid)? {
            Some(object) => object,
            None => self
                .source
                .read(&oid)
                .map_err(|source| ReceivePlanError::Source { oid, source })?
                .ok_or(ReceivePlanError::Missing { oid })?,
        };
        if object.data.len() > self.limits.max_object_bytes {
            return Err(ReceivePlanError::Limit("object bytes"));
        }
        self.bytes = self
            .bytes
            .checked_add(object.data.len() as u64)
            .filter(|bytes| *bytes <= self.limits.max_read_bytes)
            .ok_or(ReceivePlanError::Limit("total object bytes"))?;
        if crate::incoming_pack::object_id(object.kind, &object.data) != oid {
            return Err(ReceivePlanError::Invalid {
                oid,
                reason: "object identity mismatch",
            });
        }
        let node = self.parse(oid, object)?;
        self.nodes.insert(oid, node.clone());
        Ok(node)
    }
    fn parse(&mut self, oid: ObjectId, object: BaseObject) -> Result<Node> {
        let mut links = Vec::new();
        let parse_error = |source| ReceivePlanError::Parse { oid, source };
        match object.kind {
            Kind::Commit => {
                if object.data.contains(&0) {
                    return Err(ReceivePlanError::Invalid {
                        oid,
                        reason: "NUL in commit",
                    });
                }
                let commit = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
                    .map_err(parse_error)?;
                commit.author().map_err(parse_error)?;
                commit.committer().map_err(parse_error)?;
                if commit.extra_headers.iter().any(|(name, _)| {
                    matches!(
                        name.as_bytes(),
                        b"tree" | b"parent" | b"author" | b"committer"
                    )
                }) {
                    return Err(ReceivePlanError::Invalid {
                        oid,
                        reason: "misplaced core commit header",
                    });
                }
                links.push((commit.tree(), Kind::Tree));
                links.extend(commit.parents().map(|parent| (parent, Kind::Commit)));
            }
            Kind::Tree => {
                let mut previous = None;
                let mut names = HashSet::new();
                for entry in gix_object::TreeRefIter::from_bytes(&object.data, gix_hash::Kind::Sha1)
                {
                    self.step()?;
                    let entry = entry.map_err(parse_error)?;
                    if previous.as_ref().is_some_and(|previous| previous >= &entry)
                        || !names.insert(entry.filename)
                    {
                        return Err(ReceivePlanError::Invalid {
                            oid,
                            reason: "unsorted or duplicate tree entries",
                        });
                    }
                    let kind = match entry.mode.value() {
                        0o040000 => Some(Kind::Tree),
                        0o100644 | 0o100755 | 0o120000 => Some(Kind::Blob),
                        0o160000 => None,
                        _ => {
                            return Err(ReceivePlanError::Invalid {
                                oid,
                                reason: "unsupported tree mode",
                            });
                        }
                    };
                    let mode = entry
                        .mode
                        .is_link()
                        .then_some(gix_validate::path::component::Mode::Symlink);
                    gix_validate::path::component(
                        entry.filename,
                        mode,
                        gix_validate::path::component::Options {
                            protect_windows: false,
                            protect_hfs: true,
                            protect_ntfs: true,
                        },
                    )
                    .map_err(|source| ReceivePlanError::Path { oid, source })?;
                    let target = entry.oid.to_owned();
                    if target.is_null() {
                        return Err(ReceivePlanError::Invalid {
                            oid,
                            reason: "null tree entry",
                        });
                    }
                    // Gitlinks identify commits in another repository, outside this closure.
                    if let Some(kind) = kind {
                        links.push((target, kind));
                    }
                    previous = Some(entry);
                }
            }
            Kind::Tag => {
                let tag = gix_object::TagRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
                    .map_err(parse_error)?;
                tag.tagger().map_err(parse_error)?;
                gix_validate::tag::name(tag.name)
                    .map_err(|source| ReceivePlanError::TagName { oid, source })?;
                links.push((tag.target(), tag.target_kind));
            }
            Kind::Blob => {
                let pointer = crate::classify(&object.data);
                if !matches!(pointer, PointerKind::NotAPointer) {
                    self.pointers.insert(oid, pointer);
                }
            }
        }
        if links.iter().any(|(oid, _)| oid.is_null()) {
            return Err(ReceivePlanError::Invalid {
                oid,
                reason: "null object link",
            });
        }
        Ok(Node {
            kind: object.kind,
            links,
        })
    }
    fn connected(&mut self, mut pending: Vec<(ObjectId, Option<Kind>)>) -> Result<()> {
        let mut visited = HashMap::new();
        while let Some((oid, expected)) = pending.pop() {
            self.step()?;
            let (kind, links) = if let Some(kind) = visited.get(&oid) {
                (*kind, None)
            } else {
                let trusted = self.trusted_kind(oid)?;
                match trusted {
                    Some(kind) => (kind, None),
                    None => {
                        let node = self.load(oid)?;
                        (node.kind, Some(node.links))
                    }
                }
            };
            if let Some(expected) = expected
                && expected != kind
            {
                return Err(ReceivePlanError::Kind {
                    oid,
                    expected,
                    actual: kind,
                });
            }
            visited.insert(oid, kind);
            if let Some(links) = links {
                pending.extend(links.into_iter().map(|(oid, kind)| (oid, Some(kind))));
            }
        }
        Ok(())
    }
    fn is_ancestor(&mut self, old: ObjectId, new: ObjectId) -> Result<bool> {
        let mut pending = vec![new];
        let mut seen = HashSet::new();
        while let Some(oid) = pending.pop() {
            self.step()?;
            if oid == old {
                return Ok(true);
            }
            if !seen.insert(oid) {
                continue;
            }
            let node = self.load(oid)?;
            if node.kind != Kind::Commit {
                return Err(ReceivePlanError::Kind {
                    oid,
                    expected: Kind::Commit,
                    actual: node.kind,
                });
            }
            pending.extend(
                node.links
                    .into_iter()
                    .filter_map(|(oid, kind)| (kind == Kind::Commit).then_some(oid)),
            );
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
