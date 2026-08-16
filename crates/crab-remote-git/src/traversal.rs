use bytes::Bytes;
use futures_util::Stream;
use gix_hash::ObjectId;
use std::pin::Pin;

use crate::{Commit, CursorError, EntryKind, EntryMode, Error, GitPath, Result, TreeEntry};

const CURSOR_VERSION: u8 = 1;
const DIRECTORY_CURSOR_KIND: u8 = 1;
const HISTORY_CURSOR_KIND: u8 = 2;
const PATH_HISTORY_CURSOR_KIND: u8 = 3;
const DIRECTORY_CURSOR_FIXED_BYTES: usize = 2 + 20 + 20 + 8 + 4 + 4;
const HISTORY_CURSOR_BYTES: usize = 2 + 20 + 1 + 8;
const PATH_HISTORY_CURSOR_FIXED_BYTES: usize = HISTORY_CURSOR_BYTES + 4;
const MAX_CURSOR_BYTES: usize = 64 * 1024;

/// Opaque continuation state owned and validated by the caller boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageCursor(Bytes);

impl PageCursor {
    /// Validate and own an opaque cursor payload returned by this crate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCursor`] for malformed or unsupported bytes.
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Result<Self> {
        let cursor = Self(bytes.into());
        match cursor.0.get(1) {
            Some(&DIRECTORY_CURSOR_KIND) => {
                cursor.decode_directory()?;
            }
            Some(&HISTORY_CURSOR_KIND) => {
                cursor.decode_history()?;
            }
            Some(&PATH_HISTORY_CURSOR_KIND) => {
                cursor.decode_path_history()?;
            }
            _ => return Err(malformed_cursor()),
        }
        Ok(cursor)
    }

    /// Return the exact opaque cursor payload.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn directory(
        commit: ObjectId,
        tree: ObjectId,
        path: &GitPath,
        limit: usize,
        last_name: &[u8],
    ) -> Result<Self> {
        let path_len = u32::try_from(path.as_bytes().len()).map_err(|_| Error::LimitExceeded {
            limit: "cursor bytes",
            actual: path.as_bytes().len() as u64,
            maximum: u32::MAX as u64,
        })?;
        let name_len = u32::try_from(last_name.len()).map_err(|_| Error::LimitExceeded {
            limit: "cursor bytes",
            actual: last_name.len() as u64,
            maximum: u32::MAX as u64,
        })?;
        let length = DIRECTORY_CURSOR_FIXED_BYTES
            .checked_add(path.as_bytes().len())
            .and_then(|value| value.checked_add(last_name.len()))
            .ok_or(Error::LimitExceeded {
                limit: "cursor bytes",
                actual: u64::MAX,
                maximum: MAX_CURSOR_BYTES as u64,
            })?;
        if length > MAX_CURSOR_BYTES {
            return Err(Error::LimitExceeded {
                limit: "cursor bytes",
                actual: length as u64,
                maximum: MAX_CURSOR_BYTES as u64,
            });
        }
        let limit = u64::try_from(limit).map_err(|_| Error::LimitExceeded {
            limit: "page result limit",
            actual: u64::MAX,
            maximum: u64::MAX,
        })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|source| Error::Allocation {
                requested: length,
                source,
            })?;
        bytes.extend_from_slice(&[CURSOR_VERSION, DIRECTORY_CURSOR_KIND]);
        bytes.extend_from_slice(commit.as_bytes());
        bytes.extend_from_slice(tree.as_bytes());
        bytes.extend_from_slice(&limit.to_be_bytes());
        bytes.extend_from_slice(&path_len.to_be_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&name_len.to_be_bytes());
        bytes.extend_from_slice(last_name);
        Ok(Self(Bytes::from(bytes)))
    }

    pub(crate) fn decode_directory(&self) -> Result<DirectoryCursor<'_>> {
        if self.0.len() < DIRECTORY_CURSOR_FIXED_BYTES || self.0.len() > MAX_CURSOR_BYTES {
            return Err(malformed_cursor());
        }
        let bytes = self.0.as_ref();
        if bytes[..2] != [CURSOR_VERSION, DIRECTORY_CURSOR_KIND] {
            return Err(malformed_cursor());
        }
        let commit = object_id(&bytes[2..22])?;
        let tree = object_id(&bytes[22..42])?;
        let limit = u64::from_be_bytes(array(&bytes[42..50])?);
        let path_len = u32::from_be_bytes(array(&bytes[50..54])?) as usize;
        let path_end = 54usize.checked_add(path_len).ok_or_else(malformed_cursor)?;
        let name_len_end = path_end.checked_add(4).ok_or_else(malformed_cursor)?;
        let name_len_bytes = bytes
            .get(path_end..name_len_end)
            .ok_or_else(malformed_cursor)?;
        let name_len = u32::from_be_bytes(array(name_len_bytes)?) as usize;
        let name_end = name_len_end
            .checked_add(name_len)
            .ok_or_else(malformed_cursor)?;
        if name_end != bytes.len() || name_len == 0 {
            return Err(malformed_cursor());
        }
        let path = bytes.get(54..path_end).ok_or_else(malformed_cursor)?;
        GitPath::new(Bytes::copy_from_slice(path)).map_err(|_| malformed_cursor())?;
        let last_name = bytes
            .get(name_len_end..name_end)
            .ok_or_else(malformed_cursor)?;
        if last_name.contains(&0) || last_name.contains(&b'/') {
            return Err(malformed_cursor());
        }
        Ok(DirectoryCursor {
            commit,
            tree,
            limit,
            path,
            last_name,
        })
    }

    pub(crate) fn history(start: ObjectId, mode: HistoryTraversal, skip: u64) -> Self {
        let mut bytes = Vec::with_capacity(HISTORY_CURSOR_BYTES);
        bytes.extend_from_slice(&[CURSOR_VERSION, HISTORY_CURSOR_KIND]);
        bytes.extend_from_slice(start.as_bytes());
        bytes.push(mode as u8);
        bytes.extend_from_slice(&skip.to_be_bytes());
        Self(Bytes::from(bytes))
    }

    pub(crate) fn decode_history(&self) -> Result<HistoryCursor> {
        if self.0.len() != HISTORY_CURSOR_BYTES
            || self.0[..2] != [CURSOR_VERSION, HISTORY_CURSOR_KIND]
        {
            return Err(malformed_cursor());
        }
        let start = object_id(&self.0[2..22])?;
        let mode = HistoryTraversal::try_from(self.0[22])?;
        let skip = u64::from_be_bytes(array(&self.0[23..31])?);
        Ok(HistoryCursor { start, mode, skip })
    }

    pub(crate) fn path_history(
        start: ObjectId,
        mode: HistoryTraversal,
        path: &GitPath,
        skip: u64,
    ) -> Result<Self> {
        let path_len = u32::try_from(path.as_bytes().len()).map_err(|_| Error::LimitExceeded {
            limit: "cursor bytes",
            actual: path.as_bytes().len() as u64,
            maximum: MAX_CURSOR_BYTES as u64,
        })?;
        let length = PATH_HISTORY_CURSOR_FIXED_BYTES
            .checked_add(path.as_bytes().len())
            .ok_or(Error::LimitExceeded {
                limit: "cursor bytes",
                actual: u64::MAX,
                maximum: MAX_CURSOR_BYTES as u64,
            })?;
        if length > MAX_CURSOR_BYTES {
            return Err(Error::LimitExceeded {
                limit: "cursor bytes",
                actual: length as u64,
                maximum: MAX_CURSOR_BYTES as u64,
            });
        }
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&[CURSOR_VERSION, PATH_HISTORY_CURSOR_KIND]);
        bytes.extend_from_slice(start.as_bytes());
        bytes.push(mode as u8);
        bytes.extend_from_slice(&skip.to_be_bytes());
        bytes.extend_from_slice(&path_len.to_be_bytes());
        bytes.extend_from_slice(path.as_bytes());
        Ok(Self(Bytes::from(bytes)))
    }

    pub(crate) fn decode_path_history(&self) -> Result<PathHistoryCursor<'_>> {
        if self.0.len() < PATH_HISTORY_CURSOR_FIXED_BYTES
            || self.0.len() > MAX_CURSOR_BYTES
            || self.0[..2] != [CURSOR_VERSION, PATH_HISTORY_CURSOR_KIND]
        {
            return Err(malformed_cursor());
        }
        let start = object_id(&self.0[2..22])?;
        let mode = HistoryTraversal::try_from(self.0[22])?;
        let skip = u64::from_be_bytes(array(&self.0[23..31])?);
        let path_len = u32::from_be_bytes(array(&self.0[31..35])?) as usize;
        let end = PATH_HISTORY_CURSOR_FIXED_BYTES
            .checked_add(path_len)
            .ok_or_else(malformed_cursor)?;
        if end != self.0.len() {
            return Err(malformed_cursor());
        }
        let path = &self.0[35..end];
        GitPath::new(Bytes::copy_from_slice(path)).map_err(|_| malformed_cursor())?;
        Ok(PathHistoryCursor {
            start,
            mode,
            skip,
            path,
        })
    }
}

pub(crate) struct DirectoryCursor<'a> {
    pub(crate) commit: ObjectId,
    pub(crate) tree: ObjectId,
    pub(crate) limit: u64,
    pub(crate) path: &'a [u8],
    pub(crate) last_name: &'a [u8],
}

pub(crate) struct HistoryCursor {
    pub(crate) start: ObjectId,
    pub(crate) mode: HistoryTraversal,
    pub(crate) skip: u64,
}

pub(crate) struct PathHistoryCursor<'a> {
    pub(crate) start: ObjectId,
    pub(crate) mode: HistoryTraversal,
    pub(crate) skip: u64,
    pub(crate) path: &'a [u8],
}

fn object_id(bytes: &[u8]) -> Result<ObjectId> {
    let bytes: [u8; 20] = array(bytes)?;
    Ok(ObjectId::from(bytes))
}

fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| malformed_cursor())
}

fn malformed_cursor() -> Error {
    Error::InvalidCursor {
        reason: CursorError::Malformed,
    }
}

/// One bounded page of repository results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// Results in canonical repository order.
    pub items: Vec<T>,
    /// Continuation cursor when more results exist.
    pub next: Option<PageCursor>,
}

/// Validated request for one bounded page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    limit: usize,
    after: Option<PageCursor>,
}

/// Optional child metadata included in one bounded directory page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DirectoryMetadata {
    /// Do not read child objects; blob sizes remain absent.
    #[default]
    None,
    /// Batch-read blob-backed children in this page and include Git sizes.
    BlobSizes,
}

/// Parent traversal policy for repository and path history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HistoryTraversal {
    /// Follow only the first stored parent of each commit.
    FirstParent = 1,
    /// Visit every parent deterministically in stored parent order.
    AllParents = 2,
}

impl TryFrom<u8> for HistoryTraversal {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::FirstParent),
            2 => Ok(Self::AllParents),
            _ => Err(malformed_cursor()),
        }
    }
}

/// A verified symbolic-link entry and its unfollowed target bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symlink {
    /// Tree entry selecting the symbolic link.
    pub entry: TreeEntry,
    /// Exact committed link-target bytes.
    pub target: Bytes,
}

/// A Git submodule entry represented only by its gitlink commit ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    /// Tree entry selecting the submodule.
    pub entry: TreeEntry,
    /// Commit ID recorded by the gitlink, without fetching another repository.
    pub commit: ObjectId,
}

impl PageRequest {
    /// Construct a page request with a non-zero result limit.
    pub fn new(limit: usize, after: Option<PageCursor>) -> crate::Result<Self> {
        if limit == 0 {
            return Err(crate::Error::InvalidLimit {
                name: "page result limit",
            });
        }
        Ok(Self { limit, after })
    }

    /// Return the maximum number of results in this page.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Return the optional opaque continuation cursor.
    #[must_use]
    pub const fn after(&self) -> Option<&PageCursor> {
        self.after.as_ref()
    }
}

/// Kind of semantic change between two tree snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Path exists only in the new snapshot.
    Added,
    /// Path exists only in the old snapshot.
    Deleted,
    /// Content identity changed without changing semantic kind.
    Modified,
    /// Exact Git mode changed.
    ModeChanged,
    /// Tree, blob, symlink, or submodule kind changed.
    TypeChanged,
}

/// One path-level tree comparison result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeChange {
    /// Exact repository path.
    pub path: GitPath,
    /// Semantic change classification.
    pub kind: ChangeKind,
    /// Entry in the old snapshot, when present.
    pub old: Option<TreeEntry>,
    /// Entry in the new snapshot, when present.
    pub new: Option<TreeEntry>,
}

/// One commit that changed an exact path under the selected parent policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHistoryEntry {
    /// Complete commit metadata for the change.
    pub commit: Commit,
    /// Semantic path change relative to the selected parent set.
    pub kind: ChangeKind,
}

/// Bounded comparison between two immutable commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// Old commit used as the comparison base.
    pub base: ObjectId,
    /// New commit used as the comparison target.
    pub head: ObjectId,
    /// Path changes in canonical Git byte order.
    pub changes: Vec<TreeChange>,
}

/// Reason textual diff hunks are present or intentionally unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffClassification {
    /// Both inputs were ordinary bounded text and hunks were computed.
    Text,
    /// At least one input was classified as binary.
    Binary,
    /// At least one input was a Crab pointer.
    CrabPointer,
    /// At least one input was a Git LFS pointer.
    LfsPointer,
    /// Input exceeded the configured textual-diff budget.
    TooLarge,
    /// Input bytes were not valid under the configured text policy.
    UnsupportedEncoding,
}

/// One owned textual diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// One-based old-file starting line.
    pub old_start: u64,
    /// Old-file line count.
    pub old_lines: u64,
    /// One-based new-file starting line.
    pub new_start: u64,
    /// New-file line count.
    pub new_lines: u64,
    /// Exact unified-diff hunk bytes.
    pub bytes: Bytes,
}

/// Bounded diff result for one repository path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Exact repository path.
    pub path: GitPath,
    /// Content classification governing whether hunks are available.
    pub classification: DiffClassification,
    /// Textual hunks, empty for non-text classifications.
    pub hunks: Vec<DiffHunk>,
}

/// One contiguous line range attributed by blame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameRange {
    /// One-based inclusive starting line.
    pub start: u64,
    /// Number of covered lines.
    pub lines: u64,
    /// Commit responsible for this range.
    pub commit: Commit,
    /// Path used in the responsible commit.
    pub source_path: GitPath,
}

/// Complete bounded blame result for one text blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blame {
    /// Snapshot commit used for attribution.
    pub commit: ObjectId,
    /// Exact requested path.
    pub path: GitPath,
    /// Complete contiguous attribution ranges.
    pub ranges: Vec<BlameRange>,
}

/// Entry yielded by an archive traversal without creating a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// Exact repository path.
    pub path: GitPath,
    /// Immutable object ID.
    pub oid: ObjectId,
    /// Exact Git mode.
    pub mode: EntryMode,
    /// Semantic entry kind.
    pub kind: EntryKind,
    /// Verified Git representation bytes for blobs and symlinks.
    pub bytes: Option<Bytes>,
}

/// Owned asynchronous archive traversal with operation cleanup on completion or drop.
pub type ArchiveStream = Pin<Box<dyn Stream<Item = Result<ArchiveEntry>> + Send + 'static>>;
