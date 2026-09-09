use bytes::Bytes;

use crate::remote_error::remote_error;
use crate::{Error, ErrorKind, GitPath, ObjectId, Result};

/// Exact actor bytes and timestamp from a Git commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: Bytes,
    pub email: Bytes,
    pub seconds: i64,
    pub offset_seconds: i32,
}

impl Signature {
    fn from_owner(value: crab_remote_git::Signature) -> Self {
        Self {
            name: value.name,
            email: value.email,
            seconds: value.seconds,
            offset_seconds: value.offset_seconds,
        }
    }
}

/// A preserved cryptographic-signature header without verification claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHeader {
    pub name: Bytes,
    pub value: Bytes,
}

/// Verified commit metadata with original message and identity bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub id: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    pub encoding: Option<Bytes>,
    pub message: Bytes,
    pub signature_headers: Vec<SignatureHeader>,
}

impl Commit {
    pub(crate) fn from_owner(value: crab_remote_git::Commit) -> Result<Self> {
        Ok(Self {
            id: ObjectId::from_owner(value.oid)?,
            tree: ObjectId::from_owner(value.tree)?,
            parents: value
                .parents
                .into_iter()
                .map(ObjectId::from_owner)
                .collect::<Result<_>>()?,
            author: Signature::from_owner(value.author),
            committer: Signature::from_owner(value.committer),
            encoding: value.encoding,
            message: value.message,
            signature_headers: value
                .signature_headers
                .into_iter()
                .map(|header| SignatureHeader {
                    name: header.name,
                    value: header.value,
                })
                .collect(),
        })
    }
}

/// Supported Git tree modes without following symlinks or submodules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryMode {
    Tree,
    Regular,
    Executable,
    Symlink,
    Submodule,
}

#[cfg(feature = "write")]
impl EntryMode {
    pub(crate) fn tree_mode(self) -> Result<gix_object::tree::EntryMode> {
        use gix_object::tree::EntryKind;
        match self {
            Self::Regular => Ok(EntryKind::Blob.into()),
            Self::Executable => Ok(EntryKind::BlobExecutable.into()),
            Self::Symlink => Ok(EntryKind::Link.into()),
            Self::Tree | Self::Submodule => Err(Error::new(
                ErrorKind::InvalidInput,
                "streamed file edits require a blob or symlink mode",
            )),
        }
    }
}

/// One immediate entry of a verified Git tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: GitPath,
    pub id: ObjectId,
    pub mode: EntryMode,
}

impl TreeEntry {
    pub(crate) fn from_owner(value: crab_remote_git::TreeEntry) -> Result<Self> {
        use crab_remote_git::EntryMode as M;
        Ok(Self {
            path: GitPath::new(value.path.as_bytes().to_vec()).map_err(|source| {
                Error::with_source(
                    ErrorKind::Corruption,
                    "tree contains an unsupported path",
                    source,
                )
            })?,
            id: ObjectId::from_owner(value.oid)?,
            mode: match value.mode {
                M::Tree => EntryMode::Tree,
                M::Regular => EntryMode::Regular,
                M::Executable => EntryMode::Executable,
                M::Symlink => EntryMode::Symlink,
                M::Submodule => EntryMode::Submodule,
            },
        })
    }
}

/// An opaque in-process continuation bound to a repository generation and commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor {
    identity: crab_remote_git::RepositoryIdentity,
    generation: u64,
    commit: ObjectId,
    inner: crab_remote_git::PageCursor,
}

/// A nonempty page-size request with an optional preceding continuation.
#[derive(Clone, Debug)]
pub struct PageRequest {
    limit: usize,
    after: Option<PageCursor>,
}

impl PageRequest {
    /// Validate a nonzero page size; operation budgets also constrain traversal.
    pub fn new(limit: usize, after: Option<PageCursor>) -> Result<Self> {
        if limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "page size must be nonzero",
            ));
        }
        Ok(Self { limit, after })
    }

    pub(crate) fn into_owner(
        self,
        identity: &crab_remote_git::RepositoryIdentity,
        generation: u64,
        commit: ObjectId,
    ) -> Result<crab_remote_git::PageRequest> {
        if self.after.as_ref().is_some_and(|cursor| {
            cursor.identity != *identity
                || cursor.generation != generation
                || cursor.commit != commit
        }) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cursor belongs to a different snapshot",
            ));
        }
        crab_remote_git::PageRequest::new(self.limit, self.after.map(|cursor| cursor.inner))
            .map_err(remote_error)
    }
}

/// One bounded page in canonical Git order.
#[derive(Clone, Debug)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<PageCursor>,
}

impl<T> Page<T> {
    pub(crate) fn from_owner<U>(
        page: crab_remote_git::Page<U>,
        identity: crab_remote_git::RepositoryIdentity,
        generation: u64,
        commit: ObjectId,
        convert: impl Fn(U) -> Result<T>,
    ) -> Result<Self> {
        Ok(Self {
            items: page.items.into_iter().map(convert).collect::<Result<_>>()?,
            next: page.next.map(|inner| PageCursor {
                identity,
                generation,
                commit,
                inner,
            }),
        })
    }
}

/// Parent traversal policy for commit history.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub enum HistoryTraversal {
    #[default]
    AllParents,
    FirstParent,
}

/// Classification explaining whether textual diff hunks are available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiffClassification {
    Text,
    Binary,
    CrabPointer,
    LfsPointer,
    TooLarge,
    UnsupportedEncoding,
}

/// Exact replacement-hunk bytes and their one-based line coordinates.
#[derive(Clone, Debug)]
pub struct DiffHunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    pub bytes: Bytes,
}

/// Bounded diff for one exact path; non-text classifications have no hunks.
#[derive(Clone, Debug)]
pub struct Diff {
    pub classification: DiffClassification,
    pub hunks: Vec<DiffHunk>,
}

impl Diff {
    pub(crate) fn from_owner(value: crab_remote_git::Diff) -> Self {
        use crab_remote_git::DiffClassification as C;
        Self {
            classification: match value.classification {
                C::Text => DiffClassification::Text,
                C::Binary => DiffClassification::Binary,
                C::CrabPointer => DiffClassification::CrabPointer,
                C::LfsPointer => DiffClassification::LfsPointer,
                C::TooLarge => DiffClassification::TooLarge,
                C::UnsupportedEncoding => DiffClassification::UnsupportedEncoding,
            },
            hunks: value
                .hunks
                .into_iter()
                .map(|hunk| DiffHunk {
                    old_start: hunk.old_start,
                    old_lines: hunk.old_lines,
                    new_start: hunk.new_start,
                    new_lines: hunk.new_lines,
                    bytes: hunk.bytes,
                })
                .collect(),
        }
    }
}

/// Contiguous, one-based line attribution in a verified text blob.
#[derive(Clone, Debug)]
pub struct BlameRange {
    pub start: u64,
    pub lines: u64,
    pub commit: Commit,
    pub source_path: GitPath,
}

/// Complete bounded attribution for the requested snapshot path.
#[derive(Clone, Debug)]
pub struct Blame {
    pub ranges: Vec<BlameRange>,
}

impl Blame {
    pub(crate) fn from_owner(value: crab_remote_git::Blame) -> Result<Self> {
        Ok(Self {
            ranges: value
                .ranges
                .into_iter()
                .map(|range| {
                    Ok(BlameRange {
                        start: range.start,
                        lines: range.lines,
                        commit: Commit::from_owner(range.commit)?,
                        source_path: GitPath::new(range.source_path.as_bytes().to_vec()).map_err(
                            |source| {
                                Error::with_source(
                                    ErrorKind::Corruption,
                                    "blame contains an unsupported path",
                                    source,
                                )
                            },
                        )?,
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}
