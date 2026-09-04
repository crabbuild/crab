use crab_xet::hash::MerkleHash;

/// Validation failures for byte-preserving repository paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PathError {
    /// Git paths cannot contain NUL bytes.
    #[error("path contains a NUL byte")]
    Nul,
    /// Non-root paths cannot contain empty components.
    #[error("path contains an empty component")]
    EmptyComponent,
    /// One tree-entry component cannot contain a slash.
    #[error("path component contains a slash")]
    SlashInComponent,
}

/// Validation failures for opaque repository continuation cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CursorError {
    /// Cursor bytes do not match a supported canonical encoding.
    #[error("cursor is malformed or uses an unsupported schema")]
    Malformed,
    /// Cursor belongs to another snapshot, directory, or page shape.
    #[error("cursor does not belong to this repository operation")]
    ContextMismatch,
}

/// Validated repository-state failures that do not expose storage placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RepositoryStateError {
    /// A non-empty manifest did not resolve symbolic `HEAD`.
    #[error("repository HEAD does not resolve")]
    HeadDoesNotResolve,
    /// A manifest ref or symbolic `HEAD` violates Git ref-name rules.
    #[error("repository manifest contains an invalid reference name")]
    InvalidReference,
    /// A peeled-target entry does not correspond to a current reference.
    #[error("repository manifest contains an orphan peeled reference")]
    OrphanPeeledReference,
    /// The pinned pack inventory contains a duplicate pack identity.
    #[error("pack inventory contains a duplicate pack")]
    DuplicatePack,
    /// A committed content-addressed identity could not be decoded.
    #[error("repository metadata contains an invalid content identity")]
    InvalidContentIdentity,
    /// Manifest, inventory, and locator could not form one committed view.
    #[error("repository metadata does not form one committed generation")]
    InconsistentGeneration,
    /// The immutable Git object visibility proof does not match the manifest refs.
    #[error("Git object visibility proof does not match repository refs")]
    VisibilityProofMismatch,
}

/// Revision validation and reachability failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RevisionError {
    /// Reference input was empty or malformed.
    #[error("revision reference is invalid")]
    InvalidReference,
    /// Hex input is shorter than a complete SHA-1 object ID.
    #[error("abbreviated object IDs are unsupported")]
    AbbreviatedObjectId,
    /// Object-ID input is not exactly 40 hexadecimal SHA-1 digits.
    #[error("object ID is malformed")]
    MalformedObjectId,
    /// More than one complete reference matched an unqualified name.
    #[error("revision reference is ambiguous")]
    AmbiguousReference,
    /// No current reference or reachable commit matched.
    #[error("revision was not found")]
    NotFound,
    /// A direct commit is retained but not reachable from current refs.
    #[error("revision is not reachable from current references")]
    NotReachable,
    /// Tag peeling produced a cycle.
    #[error("annotated tag chain contains a cycle")]
    TagCycle,
    /// Tag peeling exceeded its configured depth.
    #[error("annotated tag chain exceeds its configured depth")]
    TagDepth,
    /// Resolved target is not a commit.
    #[error("revision does not resolve to a commit")]
    NotCommit,
}

/// Stage at which verified repository bytes were found corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CorruptionStage {
    /// Immutable commit-graph acceleration metadata.
    #[error("split commit graph")]
    CommitGraph,
    /// Exact object-locator metadata.
    #[error("object locator")]
    Locator,
    /// Immutable pack inventory.
    #[error("pack inventory")]
    Inventory,
    /// Pack-entry location or framing.
    #[error("pack entry")]
    PackEntry,
    /// Immutable pack index.
    #[error("pack index")]
    PackIndex,
    /// Delta ancestry or reconstruction.
    #[error("delta chain")]
    Delta,
    /// Parsed commit bytes.
    #[error("commit")]
    Commit,
    /// Parsed tree bytes.
    #[error("tree")]
    Tree,
    /// Parsed annotated-tag bytes.
    #[error("annotated tag")]
    Tag,
}

/// Structural failures in an inflated pack entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InflatedEntryError {
    /// Encoded header size does not fit the current address space.
    #[error("entry header size is not addressable")]
    HeaderNotAddressable,
    /// Encoded header is larger than the locator range.
    #[error("entry header exceeds its locator range")]
    HeaderExceedsRange,
    /// The compressed stream did not reach a terminal state.
    #[error("compressed stream did not terminate")]
    StreamDidNotTerminate,
    /// Locator range contains bytes after the compressed stream.
    #[error("locator range contains trailing bytes")]
    TrailingBytes,
    /// Actual inflated bytes disagree with the entry header.
    #[error("inflated size does not match the entry header")]
    SizeMismatch,
}

pub use crab_git::delta::DeltaCorruption;

/// Safe operator diagnostic for repository-open failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RepositoryDiagnostic {
    /// The derived locator is absent and should be rebuilt.
    LocatorRebuildRequired,
    /// The manifest is ahead of a locator publication still in progress.
    LocatorPublicationInProgress,
    /// The committed immutable pack inventory failed validation.
    CorruptInventory,
    /// The selected object-store origin could not be read.
    OriginUnavailable,
}

/// Reasons a repository path cannot be attributed by line-level blame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BlameUnsupportedReason {
    /// The path is a tree, symbolic link, or submodule rather than a file.
    #[error("path is not an ordinary file")]
    EntryKind,
    /// The stored blob is a Crab large-file pointer.
    #[error("Crab pointer content requires artifact materialization")]
    CrabPointer,
    /// The stored blob is a Git LFS pointer.
    #[error("Git LFS pointer content requires artifact materialization")]
    LfsPointer,
    /// The blob contains NUL bytes and is classified as binary.
    #[error("binary content has no supported line attribution")]
    Binary,
    /// The blob is not valid UTF-8.
    #[error("non-UTF-8 content has no supported line attribution")]
    UnsupportedEncoding,
}

/// Errors returned by remote Git object reads.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Repository cache identity is empty, oversized, or unsafe to diagnose.
    #[error("invalid repository identity component: {component}")]
    InvalidRepositoryIdentity { component: &'static str },

    /// A byte-preserving path did not satisfy repository path rules.
    #[error("invalid repository path: {reason}")]
    InvalidPath { reason: PathError },

    /// An opaque continuation cursor is malformed or was replayed elsewhere.
    #[error("invalid repository cursor: {reason}")]
    InvalidCursor { reason: CursorError },

    /// No tree entry exists at the requested repository path.
    #[error("repository path was not found")]
    PathNotFound,

    /// A dependent path component was not a tree.
    #[error("repository path component is {actual:?}, expected a tree")]
    PathComponentNotTree { actual: crate::EntryKind },

    /// A content read selected an entry that is not blob-backed.
    #[error("repository entry is {actual:?}, expected blob-backed content")]
    EntryNotBlob { actual: crate::EntryKind },

    /// A symbolic-link target read selected another entry kind.
    #[error("repository entry is {actual:?}, expected a symbolic link")]
    EntryNotSymlink { actual: crate::EntryKind },

    /// A submodule metadata read selected another entry kind.
    #[error("repository entry is {actual:?}, expected a submodule")]
    EntryNotSubmodule { actual: crate::EntryKind },

    /// A Git tree entry used a mode outside the supported exact set.
    #[error("unsupported Git tree-entry mode {raw:o}")]
    InvalidTreeMode { raw: u32 },

    /// Line-level blame is undefined for the selected content representation.
    #[error("repository path does not support blame: {reason}")]
    BlameUnsupported { reason: BlameUnsupportedReason },

    /// Validated repository metadata could not form a usable state.
    #[error("invalid repository state: {reason}")]
    RepositoryState { reason: RepositoryStateError },

    /// Repository uses a Git object format unsupported by this reader.
    #[error("unsupported Git object format")]
    UnsupportedObjectFormat,

    /// Repository contains no current references or commits.
    #[error("repository is empty")]
    EmptyRepository,

    /// Locator publication has not reached the required manifest generation.
    #[error(
        "repository object locator is not ready: observed generation {observed:?}, required {required}"
    )]
    RepositoryIndexing {
        observed: Option<u64>,
        required: u64,
    },

    /// Revision input could not be resolved under current repository policy.
    #[error("invalid repository revision: {reason}")]
    Revision { reason: RevisionError },

    /// A retained commit is no longer available in the current pack inventory.
    #[error("repository snapshot is unavailable in the current generation")]
    SnapshotUnavailable,

    /// Verified bytes had a different Git object kind than the operation needs.
    #[error("Git object {oid} has kind {actual:?}, expected {expected:?}")]
    ObjectKind {
        oid: gix_hash::ObjectId,
        expected: gix_object::Kind,
        actual: gix_object::Kind,
    },

    /// Commit payload parsing failed.
    #[error("failed to parse Git commit {oid}")]
    CommitParse {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_object::decode::Error,
    },

    /// Tree payload parsing failed.
    #[error("failed to parse Git tree {oid}")]
    TreeParse {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_object::decode::Error,
    },

    /// Annotated-tag payload parsing failed.
    #[error("failed to parse Git annotated tag {oid}")]
    TagParse {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_object::decode::Error,
    },

    /// Verified repository bytes violated a typed integrity invariant.
    #[error("remote Git data is corrupt at {stage}")]
    Corrupt { stage: CorruptionStage },

    /// The configured reader limit is invalid.
    #[error("remote Git reader limit {name} must be greater than zero")]
    InvalidLimit { name: &'static str },

    /// The requested object is not present in the pinned pack inventory.
    #[error("Git object {oid} is not present in the pinned remote inventory")]
    ObjectNotFound { oid: gix_hash::ObjectId },

    /// A packed entry did not match its locator CRC.
    #[error("packed entry for Git object {oid} failed CRC32 verification")]
    PackedEntryCrcMismatch { oid: gix_hash::ObjectId },

    /// A reconstructed object did not match its Git object ID.
    #[error("reconstructed Git object {oid} failed object-ID verification")]
    ObjectIdMismatch {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_object::data::verify::Error,
    },

    /// A retained cache value no longer matches its identity key.
    #[error("cached Git object {oid} failed object-ID verification")]
    CacheCorrupt {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_object::data::verify::Error,
    },

    /// A configured byte or depth budget was exceeded.
    #[error("remote Git read exceeded {limit}: requested {actual}, maximum {maximum}")]
    LimitExceeded {
        limit: &'static str,
        actual: u64,
        maximum: u64,
    },

    /// A bounded allocation could not be satisfied.
    #[error("failed to allocate {requested} bytes for remote Git decoding")]
    Allocation {
        requested: usize,
        #[source]
        source: std::collections::TryReserveError,
    },

    /// The operation was cancelled.
    #[error("remote Git read was cancelled")]
    Cancelled,

    /// The enclosing read-side policy denied an object before its bytes were exposed.
    #[error("remote Git request was denied by authorization policy")]
    AuthorizationDenied,

    /// The enclosing operation exceeded its wall-clock deadline.
    #[error("remote Git {operation} timed out")]
    Timeout { operation: &'static str },

    /// A Git pack entry header was malformed.
    #[error("failed to decode packed entry for Git object {oid}")]
    PackEntry {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_pack::data::entry::decode::Error,
    },

    /// A packed entry could not be inflated.
    #[error("failed to inflate packed entry for Git object {oid}")]
    Inflate {
        oid: gix_hash::ObjectId,
        #[source]
        source: gix_features::zlib::inflate::Error,
    },

    /// Inflated bytes did not match the packed entry declaration.
    #[error("packed entry for Git object {oid} has invalid compressed data: {reason}")]
    InvalidInflatedEntry {
        oid: gix_hash::ObjectId,
        reason: InflatedEntryError,
    },

    /// Delta instructions were malformed or unsafe.
    #[error("invalid delta for Git object {oid}: {reason}")]
    InvalidDelta {
        oid: gix_hash::ObjectId,
        reason: DeltaCorruption,
    },

    /// An OFS delta base was absent from the verified pack index.
    #[error("pack {pack_id} has no Git object at OFS-delta base offset {pack_offset}")]
    DeltaBaseNotFound {
        pack_id: MerkleHash,
        pack_offset: u64,
    },

    /// A pack index could not be parsed.
    #[error("failed to parse index for pack {pack_id}")]
    PackIndex {
        pack_id: MerkleHash,
        #[source]
        source: gix_pack::index::init::Error,
    },

    /// A blocking decode task failed.
    #[error("remote Git decode task failed")]
    DecodeTask {
        #[source]
        source: tokio::task::JoinError,
    },

    /// A coalesced immutable-object read failed for every waiting caller.
    #[error("shared remote Git object read failed")]
    SharedRead {
        #[source]
        source: std::sync::Arc<Error>,
    },

    /// A private lifecycle invariant was violated.
    #[error("remote Git internal invariant failed: {invariant}")]
    InternalInvariant { invariant: &'static str },

    /// Metadata access failed.
    #[error(transparent)]
    Metadata(#[from] crab_metadata::error::MetadataError),

    /// Canonical manifest loading or validation failed.
    #[error("failed to load canonical repository manifest")]
    Manifest {
        #[source]
        source: crab_metadata::error::MetadataError,
    },

    /// Immutable pack-inventory loading or validation failed.
    #[error("failed to load immutable repository pack inventory")]
    Inventory {
        #[source]
        source: crab_metadata::error::MetadataError,
    },

    /// Object storage access failed.
    #[error(transparent)]
    Storage(#[from] crab_storage::StorageError),

    /// Generated-artifact production coordination failed.
    #[error("generated response-pack coordination failed")]
    GeneratedPackLease {
        #[source]
        source: crate::GeneratedPackLeaseError,
    },

    /// A complete multi-pack response could not be consolidated.
    #[error("failed to consolidate complete Git response pack")]
    ResponsePackConsolidation {
        #[source]
        source: crab_git::repack::RepackError,
    },

    /// Closing the locator also failed after the read had already failed.
    #[error("remote Git read failed and its locator could not be closed")]
    CloseAfterFailure {
        #[source]
        operation: Box<Error>,
        close: crab_metadata::error::MetadataError,
    },
}

/// Result type for remote Git object reads.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn trace_category(&self) -> &'static str {
        match self {
            Self::InvalidRepositoryIdentity { .. }
            | Self::InvalidPath { .. }
            | Self::InvalidCursor { .. }
            | Self::InvalidLimit { .. }
            | Self::Revision {
                reason:
                    RevisionError::InvalidReference
                    | RevisionError::AbbreviatedObjectId
                    | RevisionError::MalformedObjectId
                    | RevisionError::AmbiguousReference,
            } => "invalid_request",
            Self::PathNotFound
            | Self::ObjectNotFound { .. }
            | Self::SnapshotUnavailable
            | Self::Revision {
                reason: RevisionError::NotFound | RevisionError::NotReachable,
            } => "not_found",
            Self::PathComponentNotTree { .. }
            | Self::EntryNotBlob { .. }
            | Self::EntryNotSymlink { .. }
            | Self::EntryNotSubmodule { .. }
            | Self::InvalidTreeMode { .. }
            | Self::BlameUnsupported { .. }
            | Self::UnsupportedObjectFormat
            | Self::Revision {
                reason: RevisionError::NotCommit,
            } => "unsupported",
            Self::EmptyRepository => "empty_repository",
            Self::RepositoryIndexing { .. } => "indexing",
            Self::LimitExceeded { .. } => "limit",
            Self::Allocation { .. } => "allocation",
            Self::Cancelled => "cancelled",
            Self::AuthorizationDenied => "authorization",
            Self::Timeout { .. } => "timeout",
            Self::RepositoryState { .. }
            | Self::ObjectKind { .. }
            | Self::CommitParse { .. }
            | Self::TreeParse { .. }
            | Self::TagParse { .. }
            | Self::Corrupt { .. }
            | Self::PackedEntryCrcMismatch { .. }
            | Self::ObjectIdMismatch { .. }
            | Self::CacheCorrupt { .. }
            | Self::PackEntry { .. }
            | Self::Inflate { .. }
            | Self::InvalidInflatedEntry { .. }
            | Self::InvalidDelta { .. }
            | Self::DeltaBaseNotFound { .. }
            | Self::PackIndex { .. }
            | Self::Revision {
                reason: RevisionError::TagCycle,
            } => "integrity",
            Self::DecodeTask { .. } | Self::SharedRead { .. } => "task",
            Self::InternalInvariant { .. } => "internal",
            Self::Metadata(_) | Self::Manifest { .. } | Self::Inventory { .. } => "metadata",
            Self::Storage(_) => "storage",
            Self::GeneratedPackLease { .. } => "coordination",
            Self::ResponsePackConsolidation { .. } => "integrity",
            Self::CloseAfterFailure { .. } => "close",
            Self::Revision {
                reason: RevisionError::TagDepth,
            } => "limit",
        }
    }

    /// Return a safe repository-open diagnostic without storage identifiers.
    #[must_use]
    pub fn repository_diagnostic(&self) -> Option<RepositoryDiagnostic> {
        match self {
            Self::RepositoryIndexing { observed: None, .. } => {
                Some(RepositoryDiagnostic::LocatorRebuildRequired)
            }
            Self::RepositoryIndexing {
                observed: Some(_), ..
            } => Some(RepositoryDiagnostic::LocatorPublicationInProgress),
            Self::Inventory {
                source:
                    crab_metadata::error::MetadataError::CorruptObject { .. }
                    | crab_metadata::error::MetadataError::Storage {
                        source: crab_storage::StorageError::CorruptObject { .. },
                    },
            } => Some(RepositoryDiagnostic::CorruptInventory),
            Self::Corrupt {
                stage: CorruptionStage::Inventory,
            }
            | Self::RepositoryState {
                reason:
                    RepositoryStateError::DuplicatePack | RepositoryStateError::InvalidContentIdentity,
            } => Some(RepositoryDiagnostic::CorruptInventory),
            Self::Storage(_)
            | Self::Manifest {
                source: crab_metadata::error::MetadataError::Storage { .. },
            }
            | Self::Inventory {
                source: crab_metadata::error::MetadataError::Storage { .. },
            } => Some(RepositoryDiagnostic::OriginUnavailable),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    fn oid() -> gix_hash::ObjectId {
        gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .expect("valid test OID")
    }

    #[test]
    fn gix_parse_source_survives_public_error_mapping() {
        let source = gix_object::CommitRef::from_bytes(b"invalid", gix_hash::Kind::Sha1)
            .expect_err("invalid commit");
        let error = Error::CommitParse { oid: oid(), source };
        assert!(error.source().is_some());
    }

    #[test]
    fn storage_source_chain_survives_public_error_mapping() {
        let source = std::io::Error::other("test transport failure");
        let error = Error::Storage(crab_storage::StorageError::Io { source });
        let source = error.source().expect("storage source");
        assert_eq!(source.to_string(), "test transport failure");
    }

    #[test]
    fn metadata_source_chain_survives_public_error_mapping() {
        let source = std::io::Error::other("test metadata failure");
        let error = Error::Metadata(crab_metadata::error::MetadataError::Io { source });
        let source = error.source().expect("metadata source");
        assert_eq!(source.to_string(), "test metadata failure");
    }

    #[tokio::test]
    async fn decode_task_source_survives_public_error_mapping() {
        let source = tokio::spawn(async { panic!("test decode task failure") })
            .await
            .expect_err("task must fail");
        let error = Error::DecodeTask { source };
        assert!(
            matches!(error.source(), Some(source) if source.to_string().contains("test decode task failure"))
        );
    }

    #[test]
    fn operation_source_and_typed_close_error_are_both_retained() {
        let operation = Error::Cancelled;
        let close = crab_metadata::error::MetadataError::Io {
            source: std::io::Error::other("test close failure"),
        };
        let error = Error::CloseAfterFailure {
            operation: Box::new(operation),
            close,
        };
        assert!(matches!(error.source(), Some(source) if source.to_string().contains("cancelled")));
        let Error::CloseAfterFailure { close, .. } = error else {
            panic!("wrong error variant");
        };
        assert!(matches!(
            close,
            crab_metadata::error::MetadataError::Io { ref source }
                if source.to_string() == "test close failure"
        ));
    }
}
