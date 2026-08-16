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
    /// Resolved symbolic `HEAD`, absent only for an empty repository.
    pub head: Option<HeadReference>,
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
