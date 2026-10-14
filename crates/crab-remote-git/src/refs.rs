use crate::{Error, RepositoryStateError, Result};
use gix_hash::ObjectId;

/// The repository's symbolic `HEAD` and its resolved object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadReference {
    /// Complete symbolic reference name, such as `refs/heads/main`.
    pub name: String,
    /// Object named by the symbolic reference in the pinned manifest.
    pub target: ObjectId,
}

/// One reference from a pinned repository manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRef {
    /// Complete reference name.
    pub name: String,
    /// Object directly named by the reference.
    pub target: ObjectId,
    /// Peeled commit recorded for an annotated tag, when available.
    pub peeled: Option<ObjectId>,
}

/// Complete reference state from one validated manifest generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryRefs {
    /// Resolved symbolic `HEAD`, absent when the repository is empty or HEAD
    /// points at an unborn branch.
    pub head: Option<HeadReference>,
    /// Symbolic `HEAD` target when its branch has no commit, even if other refs exist.
    pub unborn_head: Option<String>,
    /// References sorted by their complete bytewise names.
    pub entries: Vec<RepositoryRef>,
}

impl RepositoryRefs {
    /// Return a reference only when its complete name matches exactly.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&RepositoryRef> {
        self.entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Return whether this manifest describes an empty repository.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl RepositoryRefs {
    /// Parse complete captured ref state without opening Git payloads.
    ///
    /// The caller validates the authority envelope. This checks ref names,
    /// targets, peeling associations and HEAD; partial ref maps are invalid input.
    pub fn from_ref_state(
        head_name: &str,
        refs: &std::collections::BTreeMap<String, String>,
        peeled_refs: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        if crab_git::validate_push_refname(head_name).is_err() || !head_name.starts_with("refs/") {
            return Err(Error::RepositoryState {
                reason: RepositoryStateError::InvalidReference,
            });
        }
        if peeled_refs.keys().any(|name| !refs.contains_key(name)) {
            return Err(Error::RepositoryState {
                reason: RepositoryStateError::OrphanPeeledReference,
            });
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(refs.len())
            .map_err(|source| Error::Allocation {
                requested: refs
                    .len()
                    .saturating_mul(std::mem::size_of::<RepositoryRef>()),
                source,
            })?;
        for (name, value) in refs {
            if !name.starts_with("refs/") || crab_git::validate_push_refname(name).is_err() {
                return Err(Error::RepositoryState {
                    reason: RepositoryStateError::InvalidReference,
                });
            }
            let target = parse_oid(value)?;
            let peeled = peeled_refs
                .get(name)
                .map(|value| parse_oid(value))
                .transpose()?;
            entries.push(RepositoryRef {
                name: name.clone(),
                target,
                peeled,
            });
        }
        let (head, unborn_head) = if !refs.contains_key(head_name)
            && (entries.is_empty() || head_name.starts_with("refs/heads/"))
        {
            (None, Some(head_name.to_owned()))
        } else {
            let target = refs.get(head_name).ok_or(Error::RepositoryState {
                reason: RepositoryStateError::HeadDoesNotResolve,
            })?;
            (
                Some(HeadReference {
                    name: head_name.to_owned(),
                    target: parse_oid(target)?,
                }),
                None,
            )
        };
        Ok(Self {
            head,
            unborn_head,
            entries,
        })
    }
}

/// Parse refs from a validated manifest envelope and Git-state digest.
impl TryFrom<&crab_metadata::manifests::Manifest> for RepositoryRefs {
    type Error = crate::Error;

    fn try_from(manifest: &crab_metadata::manifests::Manifest) -> Result<Self> {
        Self::from_ref_state(&manifest.head, &manifest.refs, &manifest.peeled_refs)
    }
}

pub(crate) fn parse_oid(value: &str) -> Result<ObjectId> {
    ObjectId::from_hex(value.as_bytes()).map_err(|_| Error::RepositoryState {
        reason: RepositoryStateError::InvalidContentIdentity,
    })
}
