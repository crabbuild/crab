//! Protocol-neutral upload-pack admission and object selection.

use std::collections::{HashMap, HashSet, VecDeque};

use bstr::ByteSlice;
use crab_metadata::git_visibility::GitVisibilityIndex;
use crab_remote_git::{
    CorruptionStage, Error as RemoteGitError, OperationContext, OperationKind, RemoteGitObject,
    RemoteGitRepository, RepositoryRef, RepositoryStateError,
};
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::{ReadError, Result};

const OBJECT_BATCH_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TraversalDepth {
    Absolute(u32),
    RelativeBoundary,
    Relative(u32),
}

/// Git object type accepted by the `object:type` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadPackObjectType {
    /// Annotated tag object.
    Tag,
    /// Commit object.
    Commit,
    /// Tree object.
    Tree,
    /// Blob object, including symlink contents.
    Blob,
}

impl UploadPackObjectType {
    fn parse(value: &str) -> std::result::Result<Self, UploadPackFilterError> {
        match value {
            "tag" => Ok(Self::Tag),
            "commit" => Ok(Self::Commit),
            "tree" => Ok(Self::Tree),
            "blob" => Ok(Self::Blob),
            _ => Err(UploadPackFilterError::Unsupported(format!(
                "object:type={value}"
            ))),
        }
    }

    fn matches(self, kind: gix_object::Kind) -> bool {
        matches!(
            (self, kind),
            (Self::Tag, gix_object::Kind::Tag)
                | (Self::Commit, gix_object::Kind::Commit)
                | (Self::Tree, gix_object::Kind::Tree)
                | (Self::Blob, gix_object::Kind::Blob)
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
        }
    }
}

/// A syntactically valid Git filter that the local upload-pack producer can
/// evaluate without downloading a complete canonical pack.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UploadPackFilter {
    /// Transfer the complete reachable object closure.
    #[default]
    None,
    /// Omit all non-explicitly-requested blobs.
    BlobNone,
    /// Omit blobs whose size is at least the limit.
    BlobLimit(u64),
    /// Retain only objects of one Git object type, while still traversing
    /// omitted commits and trees to find matching descendants.
    ObjectType(UploadPackObjectType),
    /// Retain trees and blobs strictly above this tree-relative depth.
    TreeDepth(u32),
    /// Retain blobs selected by a Git sparse-checkout specification stored in
    /// the referenced, generation-authorized blob.
    Sparse { oid: ObjectId },
    /// Retain objects accepted by every nested filter.
    Combine(Vec<UploadPackFilter>),
}

/// Error returned before planning when a filter is not in the published
/// support matrix or violates its bounded grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadPackFilterError {
    /// The filter uses a known Git form that this profile does not accept.
    #[error("unsupported filter: {0}")]
    Unsupported(String),
    /// The filter has malformed syntax or exceeds a parser bound.
    #[error("invalid filter: {0}")]
    Invalid(String),
}

const MAX_FILTER_BYTES: usize = 4 * 1024;
const MAX_COMBINE_DEPTH: usize = 8;
const MAX_COMBINE_FILTERS: usize = 16;

/// Parse one Git filter-spec into the canonical planner AST.
pub fn parse_upload_pack_filter(
    spec: &str,
) -> std::result::Result<UploadPackFilter, UploadPackFilterError> {
    if spec.is_empty() || spec.len() > MAX_FILTER_BYTES {
        return Err(UploadPackFilterError::Invalid(
            "filter is empty or exceeds the size bound".to_owned(),
        ));
    }
    parse_filter_component(spec, 0)
}

fn parse_filter_component(
    spec: &str,
    depth: usize,
) -> std::result::Result<UploadPackFilter, UploadPackFilterError> {
    if depth > MAX_COMBINE_DEPTH {
        return Err(UploadPackFilterError::Invalid(
            "combine filter nesting exceeds the bound".to_owned(),
        ));
    }
    if let Some(raw) = spec.strip_prefix("combine:") {
        if raw.is_empty() {
            return Err(UploadPackFilterError::Invalid(
                "combine filter has no members".to_owned(),
            ));
        }
        let mut members = Vec::new();
        for encoded in raw.split('+') {
            if encoded.is_empty() {
                return Err(UploadPackFilterError::Invalid(
                    "combine filter contains an empty member".to_owned(),
                ));
            }
            let member = percent_decode_filter(encoded)?;
            members.push(parse_filter_component(&member, depth + 1)?);
            if members.len() > MAX_COMBINE_FILTERS {
                return Err(UploadPackFilterError::Invalid(
                    "combine filter contains too many members".to_owned(),
                ));
            }
        }
        return Ok(combine_filters(members));
    }

    let spec = percent_decode_filter(spec)?;
    if spec.starts_with("combine:") {
        return parse_filter_component(&spec, depth + 1);
    }
    if spec == "blob:none" {
        return Ok(UploadPackFilter::BlobNone);
    }
    if let Some(value) = spec.strip_prefix("blob:limit=") {
        return Ok(UploadPackFilter::BlobLimit(parse_scaled_limit(value)?));
    }
    if let Some(value) = spec.strip_prefix("object:type=") {
        return Ok(UploadPackFilter::ObjectType(UploadPackObjectType::parse(
            value,
        )?));
    }
    if let Some(value) = spec.strip_prefix("tree:") {
        let depth = parse_decimal::<u32>(value, "tree depth")?;
        return Ok(UploadPackFilter::TreeDepth(depth));
    }
    if let Some(value) = spec.strip_prefix("sparse:oid=") {
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UploadPackFilterError::Invalid(
                "sparse:oid requires a full SHA-1 object ID".to_owned(),
            ));
        }
        let oid = ObjectId::from_hex(value.as_bytes()).map_err(|_| {
            UploadPackFilterError::Invalid("sparse:oid contains an invalid object ID".to_owned())
        })?;
        return Ok(UploadPackFilter::Sparse { oid });
    }
    Err(UploadPackFilterError::Unsupported(spec))
}

fn parse_scaled_limit(value: &str) -> std::result::Result<u64, UploadPackFilterError> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k') => (&value[..value.len() - 1], 1024u64),
        Some(b'm') => (&value[..value.len() - 1], 1024u64.saturating_pow(2)),
        Some(b'g') => (&value[..value.len() - 1], 1024u64.saturating_pow(3)),
        _ => (value, 1),
    };
    let number = parse_decimal::<u64>(digits, "blob size limit")?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| UploadPackFilterError::Invalid("blob size limit overflows".to_owned()))
}

fn parse_decimal<T>(value: &str, field: &str) -> std::result::Result<T, UploadPackFilterError>
where
    T: std::str::FromStr,
{
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(UploadPackFilterError::Invalid(format!(
            "{field} must be a non-negative decimal"
        )));
    }
    value
        .parse::<T>()
        .map_err(|_| UploadPackFilterError::Invalid(format!("{field} exceeds the numeric bound")))
}

fn percent_decode_filter(value: &str) -> std::result::Result<String, UploadPackFilterError> {
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err(UploadPackFilterError::Invalid(
                "percent escape is truncated".to_owned(),
            ));
        }
        let high = hex_value(bytes[index + 1]).ok_or_else(|| {
            UploadPackFilterError::Invalid("percent escape contains non-hex digits".to_owned())
        })?;
        let low = hex_value(bytes[index + 2]).ok_or_else(|| {
            UploadPackFilterError::Invalid("percent escape contains non-hex digits".to_owned())
        })?;
        decoded.push(high << 4 | low);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| {
        UploadPackFilterError::Invalid("percent-decoded filter is not UTF-8".to_owned())
    })
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Combine repeated `filter` arguments using Git's intersection semantics.
#[must_use]
pub fn combine_upload_pack_filters(filters: Vec<UploadPackFilter>) -> UploadPackFilter {
    combine_filters(filters)
}

fn combine_filters(filters: Vec<UploadPackFilter>) -> UploadPackFilter {
    let mut flattened = Vec::new();
    for filter in filters {
        match filter {
            UploadPackFilter::None => {}
            UploadPackFilter::Combine(members) => flattened.extend(members),
            filter => flattened.push(filter),
        }
    }
    match flattened.len() {
        0 => UploadPackFilter::None,
        1 => flattened
            .into_iter()
            .next()
            .unwrap_or(UploadPackFilter::None),
        _ => UploadPackFilter::Combine(flattened),
    }
}

impl UploadPackFilter {
    /// Return the stable protocol spelling used by telemetry and reports.
    #[must_use]
    pub fn canonical_spec(&self) -> String {
        match self {
            Self::None => "none".to_owned(),
            Self::BlobNone => "blob:none".to_owned(),
            Self::BlobLimit(limit) => format!("blob:limit={limit}"),
            Self::ObjectType(kind) => format!("object:type={}", kind.as_str()),
            Self::TreeDepth(depth) => format!("tree:{depth}"),
            Self::Sparse { oid } => format!("sparse:oid={oid}"),
            Self::Combine(filters) => {
                let value = filters
                    .iter()
                    .map(Self::canonical_spec)
                    .collect::<Vec<_>>()
                    .join("+");
                format!("combine:{value}")
            }
        }
    }

    fn contains_blob_none(&self) -> bool {
        match self {
            Self::BlobNone => true,
            Self::Combine(filters) => filters.iter().any(Self::contains_blob_none),
            _ => false,
        }
    }
}

/// Semantic inputs collected from one protocol-v2 fetch request.
#[derive(Debug, Clone, Default)]
pub struct UploadPackRequest {
    /// Object IDs requested by the client.
    pub wants: Vec<ObjectId>,
    /// Object IDs the client already has.
    pub haves: Vec<ObjectId>,
    /// Existing shallow boundary commits.
    pub shallow: Vec<ObjectId>,
    /// Requested depth from each want.
    pub deepen: Option<u32>,
    /// Whether the requested depth extends the existing shallow boundary.
    pub deepen_relative: bool,
    /// Whether annotated tags pointing at transferred commits should be added.
    pub include_tags: bool,
    /// Object filtering policy.
    pub filter: UploadPackFilter,
}

/// Resulting object set and shallow updates for one fetch response.
#[derive(Debug, Clone)]
pub struct PackPlan {
    /// Object IDs admitted from the requested visible generation.
    pub wants: Vec<ObjectId>,
    /// Common objects proven to be in the visible generation and already held by the client.
    pub common_haves: Vec<ObjectId>,
    /// Canonical filter selected for the response.
    pub filter: UploadPackFilter,
    /// Whether visible annotated tags were included in the selection.
    pub include_tags: bool,
    /// Deduplicated object IDs in deterministic selection order.
    pub object_ids: Vec<ObjectId>,
    /// External bases that would be required by a thin pack; the first producer is self-contained.
    pub required_bases: Vec<ObjectId>,
    /// Commits that become shallow boundaries in this response.
    pub shallow: Vec<ObjectId>,
    /// Existing shallow commits whose parents are included in this response.
    pub unshallow: Vec<ObjectId>,
}

/// Build an admitted pack plan from one generation-pinned repository.
pub async fn plan_upload_pack(
    repository: &RemoteGitRepository,
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    request: &UploadPackRequest,
    cancellation: &CancellationToken,
) -> Result<PackPlan> {
    authorize_wants(visibility, visible_ref_names, &request.wants)?;

    if request.deepen_relative && request.shallow.is_empty() {
        return Err(ReadError::Internal(
            "relative deepening requires a current shallow boundary".to_owned(),
        ));
    }
    let operation = repository
        .operation(OperationKind::UploadPack, cancellation)
        .await?;
    let result = plan_with_operation(
        repository,
        &operation,
        visible_ref_names,
        visibility,
        request,
        cancellation,
    )
    .await;
    match operation.finish(result).await {
        Err(RemoteGitError::AuthorizationDenied) => Err(ReadError::UnauthorizedObject),
        result => result.map_err(ReadError::from),
    }
}

fn authorize_wants(
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    wants: &[ObjectId],
) -> Result<()> {
    for want in wants {
        if !visibility.contains_for_refs(
            visible_ref_names.iter().map(String::as_str),
            &want.to_hex().to_string(),
        ) {
            return Err(ReadError::UnauthorizedObject);
        }
    }
    Ok(())
}

async fn plan_with_operation(
    repository: &RemoteGitRepository,
    operation: &OperationContext,
    visible_ref_names: &[String],
    visibility: &GitVisibilityIndex,
    request: &UploadPackRequest,
    cancellation: &CancellationToken,
) -> crab_remote_git::Result<PackPlan> {
    let maximum_objects = operation.max_logical_objects();
    if let Some(plan) = plan_from_visibility(
        &repository.refs().entries,
        visible_ref_names,
        visibility,
        request,
        maximum_objects,
    )? {
        tracing::debug!(
            planned_objects = plan.object_ids.len(),
            "planned full ref closure from visibility proof"
        );
        return Ok(plan);
    }

    let common_haves = request
        .haves
        .iter()
        .filter(|oid| {
            visibility.contains_for_refs(
                visible_ref_names.iter().map(String::as_str),
                &oid.to_hex().to_string(),
            )
        })
        .copied()
        .collect::<HashSet<_>>();
    let existing_shallow = request.shallow.iter().copied().collect::<HashSet<_>>();
    let deduplicate_by_oid = request.deepen.is_none()
        && !request.deepen_relative
        && !filter_requires_traversal_context(&request.filter);
    let sparse_matchers =
        prepare_sparse_matchers(operation, visibility, visible_ref_names, &request.filter).await?;
    let mut queue = VecDeque::new();
    let mut queued = HashSet::new();
    let roots = request.wants.iter().copied().collect::<HashSet<_>>();
    let initial_depth = if request.deepen_relative {
        TraversalDepth::RelativeBoundary
    } else {
        TraversalDepth::Absolute(0)
    };
    for oid in &request.wants {
        enqueue(
            QueueItem {
                oid: *oid,
                depth: initial_depth,
                tree_depth: 0,
                path: Vec::new(),
                follow_children: true,
                known_kind: None,
            },
            &mut queue,
            &mut queued,
            maximum_objects,
            deduplicate_by_oid,
        )?;
    }

    let mut selected = HashSet::new();
    let mut object_ids = Vec::new();
    let mut shallow = HashSet::new();
    let mut unshallow = HashSet::new();

    while !queue.is_empty() {
        if cancellation.is_cancelled() {
            return Err(RemoteGitError::Cancelled);
        }
        let mut batch = Vec::with_capacity(OBJECT_BATCH_SIZE);
        while batch.len() < OBJECT_BATCH_SIZE {
            let Some(item) = queue.pop_front() else {
                break;
            };
            if common_haves.contains(&item.oid) && request.deepen.is_none() {
                continue;
            }
            batch.push(item);
        }
        if batch.is_empty() {
            continue;
        }

        let batch_oids = admit_batch(visibility, visible_ref_names, &batch)?;

        let needs_blob_metadata = filter_requires_blob_size(&request.filter);
        let batch_objects = if needs_blob_metadata {
            Vec::new()
        } else {
            let objects = operation.read_objects(&batch_oids).await?;
            if objects.len() != batch.len() {
                return Err(RemoteGitError::InternalInvariant {
                    invariant: "batched upload-pack reads changed request order or cardinality",
                });
            }
            objects
        };

        for (index, item) in batch.into_iter().enumerate() {
            let object = if needs_blob_metadata
                && item.known_kind == Some(gix_object::Kind::Blob)
                && !roots.contains(&item.oid)
            {
                let metadata = operation.read_object_metadata(item.oid).await?;
                if !filter_accepts_metadata(
                    &request.filter,
                    metadata.kind,
                    metadata.size,
                    &item,
                    &sparse_matchers,
                ) {
                    continue;
                }
                operation.read_object(item.oid).await?
            } else if needs_blob_metadata {
                operation.read_object(item.oid).await?
            } else {
                batch_objects
                    .get(index)
                    .cloned()
                    .ok_or(RemoteGitError::InternalInvariant {
                        invariant: "batched upload-pack read is missing an object",
                    })?
            };
            let include = !common_haves.contains(&item.oid)
                && (roots.contains(&item.oid)
                    || filter_accepts(&request.filter, &object, &item, &sparse_matchers));
            if include && selected.insert(item.oid) {
                object_ids.push(item.oid);
            }
            enqueue_children(
                &object,
                &item,
                request,
                maximum_objects,
                &existing_shallow,
                &sparse_matchers,
                &mut queue,
                &mut queued,
                &mut shallow,
                &mut unshallow,
                cancellation,
                deduplicate_by_oid,
            )
            .await?;
        }
    }

    if request.include_tags {
        for reference in &repository.refs().entries {
            if !reference.name.starts_with("refs/tags/")
                || !visible_ref_names.iter().any(|name| name == &reference.name)
                || !reference.peeled.is_some_and(|oid| selected.contains(&oid))
            {
                continue;
            }
            let mut tag_oid = reference.target;
            loop {
                ensure_visible_objects(visibility, visible_ref_names, &[tag_oid])?;
                if selected.contains(&tag_oid) {
                    break;
                }
                if u64::try_from(selected.len()).unwrap_or(u64::MAX) >= maximum_objects {
                    return Err(RemoteGitError::LimitExceeded {
                        limit: "upload-pack traversal objects",
                        actual: u64::try_from(selected.len())
                            .unwrap_or(u64::MAX)
                            .saturating_add(1),
                        maximum: maximum_objects,
                    });
                }
                let tag = operation.read_object(tag_oid).await?;
                let tag_item = QueueItem {
                    oid: tag_oid,
                    depth: TraversalDepth::Absolute(0),
                    tree_depth: 0,
                    path: Vec::new(),
                    follow_children: false,
                    known_kind: Some(gix_object::Kind::Tag),
                };
                if filter_accepts(&request.filter, &tag, &tag_item, &sparse_matchers) {
                    selected.insert(tag_oid);
                    object_ids.push(tag_oid);
                }
                if tag.kind != gix_object::Kind::Tag {
                    break;
                }
                let tag = gix_object::TagRef::from_bytes(&tag.data, gix_hash::Kind::Sha1).map_err(
                    |_| RemoteGitError::Corrupt {
                        stage: CorruptionStage::Tag,
                    },
                )?;
                if tag.target_kind != gix_object::Kind::Tag {
                    break;
                }
                tag_oid = tag.target();
            }
        }
    }

    let mut shallow = shallow.into_iter().collect::<Vec<_>>();
    shallow.sort_unstable_by_key(|oid| oid.to_hex().to_string());
    let mut unshallow = unshallow.into_iter().collect::<Vec<_>>();
    unshallow.sort_unstable_by_key(|oid| oid.to_hex().to_string());
    Ok(PackPlan {
        wants: request.wants.clone(),
        common_haves: {
            let mut haves = common_haves.iter().copied().collect::<Vec<_>>();
            haves.sort_unstable_by_key(|oid| oid.to_hex().to_string());
            haves
        },
        filter: request.filter.clone(),
        include_tags: request.include_tags,
        object_ids,
        required_bases: Vec::new(),
        shallow,
        unshallow,
    })
}

fn plan_from_visibility(
    references: &[RepositoryRef],
    visible_ref_names: &[String],
    visibility: &GitVisibilityIndex,
    request: &UploadPackRequest,
    maximum_objects: u64,
) -> crab_remote_git::Result<Option<PackPlan>> {
    if request.wants.is_empty()
        || !request.haves.is_empty()
        || !request.shallow.is_empty()
        || request.deepen.is_some()
        || request.deepen_relative
        || request.include_tags
        || !matches!(request.filter, UploadPackFilter::None)
    {
        return Ok(None);
    }

    let visible = visible_ref_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let selected_refs = request
        .wants
        .iter()
        .map(|want| {
            references
                .iter()
                .find(|reference| {
                    visible.contains(reference.name.as_str()) && reference.target == *want
                })
                .map(|reference| reference.name.as_str())
        })
        .collect::<Option<Vec<_>>>();
    let Some(selected_refs) = selected_refs else {
        return Ok(None);
    };

    for (reference_name, want) in selected_refs.iter().zip(&request.wants) {
        let target = want.to_hex().to_string();
        if visibility
            .refs
            .get(*reference_name)
            .is_none_or(|objects| objects.binary_search(&target).is_err())
        {
            return Err(RemoteGitError::RepositoryState {
                reason: RepositoryStateError::VisibilityProofMismatch,
            });
        }
    }

    let objects = visibility.objects_for_refs(selected_refs.iter().copied());
    let actual = u64::try_from(objects.len()).unwrap_or(u64::MAX);
    if actual > maximum_objects {
        return Err(RemoteGitError::LimitExceeded {
            limit: "upload-pack planned objects",
            actual,
            maximum: maximum_objects,
        });
    }
    let object_ids = objects
        .into_iter()
        .map(|oid| {
            ObjectId::from_hex(oid.as_bytes()).map_err(|_| RemoteGitError::RepositoryState {
                reason: RepositoryStateError::VisibilityProofMismatch,
            })
        })
        .collect::<crab_remote_git::Result<Vec<_>>>()?;

    Ok(Some(PackPlan {
        wants: request.wants.clone(),
        common_haves: Vec::new(),
        filter: request.filter.clone(),
        include_tags: false,
        object_ids,
        required_bases: Vec::new(),
        shallow: Vec::new(),
        unshallow: Vec::new(),
    }))
}

#[derive(Debug, Clone)]
struct QueueItem {
    oid: ObjectId,
    depth: TraversalDepth,
    tree_depth: u32,
    path: Vec<u8>,
    follow_children: bool,
    known_kind: Option<gix_object::Kind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum QueueKey {
    Object(ObjectId),
    Context {
        oid: ObjectId,
        depth: TraversalDepth,
        tree_depth: u32,
        path: Vec<u8>,
        follow_children: bool,
        known_kind: Option<gix_object::Kind>,
    },
}

struct SparseMatchers {
    patterns: HashMap<ObjectId, gix_ignore::Search>,
}

async fn prepare_sparse_matchers(
    operation: &OperationContext,
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    filter: &UploadPackFilter,
) -> crab_remote_git::Result<SparseMatchers> {
    let mut sparse_oids = Vec::new();
    collect_sparse_oids(filter, &mut sparse_oids);
    sparse_oids.sort_unstable();
    sparse_oids.dedup();

    let mut patterns = HashMap::with_capacity(sparse_oids.len());
    for oid in sparse_oids {
        ensure_visible_objects(visibility, visible_ref_names, &[oid])?;
        let object = operation.read_object(oid).await?;
        if object.kind != gix_object::Kind::Blob {
            return Err(RemoteGitError::InternalInvariant {
                invariant: "sparse filter specification is not a blob",
            });
        }
        let mut search = gix_ignore::Search::default();
        search.add_patterns_buffer(
            object.data.as_ref(),
            "sparse-checkout",
            None,
            gix_ignore::search::Ignore::default(),
        );
        patterns.insert(oid, search);
    }
    Ok(SparseMatchers { patterns })
}

fn collect_sparse_oids(filter: &UploadPackFilter, output: &mut Vec<ObjectId>) {
    match filter {
        UploadPackFilter::Sparse { oid } => output.push(*oid),
        UploadPackFilter::Combine(filters) => {
            for filter in filters {
                collect_sparse_oids(filter, output);
            }
        }
        _ => {}
    }
}

fn filter_accepts(
    filter: &UploadPackFilter,
    object: &RemoteGitObject,
    item: &QueueItem,
    sparse_matchers: &SparseMatchers,
) -> bool {
    filter_accepts_metadata(
        filter,
        object.kind,
        u64::try_from(object.data.len()).unwrap_or(u64::MAX),
        item,
        sparse_matchers,
    )
}

fn filter_accepts_metadata(
    filter: &UploadPackFilter,
    kind: gix_object::Kind,
    size: u64,
    item: &QueueItem,
    sparse_matchers: &SparseMatchers,
) -> bool {
    match filter {
        UploadPackFilter::None => true,
        UploadPackFilter::BlobNone => kind != gix_object::Kind::Blob,
        UploadPackFilter::BlobLimit(limit) => kind != gix_object::Kind::Blob || size < *limit,
        UploadPackFilter::ObjectType(expected) => expected.matches(kind),
        UploadPackFilter::TreeDepth(depth) => {
            !matches!(kind, gix_object::Kind::Tree | gix_object::Kind::Blob)
                || item.tree_depth < *depth
        }
        UploadPackFilter::Sparse { oid } => {
            kind != gix_object::Kind::Blob || sparse_path_matches(sparse_matchers, oid, &item.path)
        }
        UploadPackFilter::Combine(filters) => filters
            .iter()
            .all(|filter| filter_accepts_metadata(filter, kind, size, item, sparse_matchers)),
    }
}

fn filter_requires_blob_size(filter: &UploadPackFilter) -> bool {
    match filter {
        UploadPackFilter::BlobLimit(_) => true,
        UploadPackFilter::Combine(filters) => filters.iter().any(filter_requires_blob_size),
        _ => false,
    }
}

fn filter_requires_traversal_context(filter: &UploadPackFilter) -> bool {
    match filter {
        UploadPackFilter::TreeDepth(_) | UploadPackFilter::Sparse { .. } => true,
        UploadPackFilter::Combine(filters) => filters.iter().any(filter_requires_traversal_context),
        _ => false,
    }
}

fn ensure_visible_objects(
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    object_ids: &[ObjectId],
) -> crab_remote_git::Result<()> {
    if object_ids.iter().any(|oid| {
        !visibility.contains_for_refs(
            visible_ref_names.iter().map(String::as_str),
            &oid.to_hex().to_string(),
        )
    }) {
        return Err(RemoteGitError::AuthorizationDenied);
    }
    Ok(())
}

fn admit_batch(
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    batch: &[QueueItem],
) -> crab_remote_git::Result<Vec<ObjectId>> {
    let object_ids = batch.iter().map(|item| item.oid).collect::<Vec<_>>();
    ensure_visible_objects(visibility, visible_ref_names, &object_ids)?;
    Ok(object_ids)
}

#[expect(
    clippy::too_many_arguments,
    reason = "object traversal carries the bounded protocol policy explicitly"
)]
async fn enqueue_children(
    object: &RemoteGitObject,
    item: &QueueItem,
    request: &UploadPackRequest,
    maximum_objects: u64,
    existing_shallow: &HashSet<ObjectId>,
    sparse_matchers: &SparseMatchers,
    queue: &mut VecDeque<QueueItem>,
    queued: &mut HashSet<QueueKey>,
    shallow: &mut HashSet<ObjectId>,
    unshallow: &mut HashSet<ObjectId>,
    cancellation: &CancellationToken,
    deduplicate_by_oid: bool,
) -> crab_remote_git::Result<()> {
    if cancellation.is_cancelled() {
        return Err(RemoteGitError::Cancelled);
    }
    if !item.follow_children {
        return Ok(());
    }
    match object.kind {
        gix_object::Kind::Commit => {
            let commit = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
                .map_err(|_| RemoteGitError::Corrupt {
                    stage: CorruptionStage::Commit,
                })?;
            enqueue(
                QueueItem {
                    oid: commit.tree(),
                    depth: item.depth,
                    tree_depth: 0,
                    path: Vec::new(),
                    follow_children: true,
                    known_kind: Some(gix_object::Kind::Tree),
                },
                queue,
                queued,
                maximum_objects,
                deduplicate_by_oid,
            )?;
            match item.depth {
                TraversalDepth::RelativeBoundary => {
                    if existing_shallow.contains(&item.oid) {
                        unshallow.insert(item.oid);
                        for parent in commit.parents() {
                            enqueue(
                                QueueItem {
                                    oid: parent,
                                    depth: TraversalDepth::Relative(0),
                                    tree_depth: 0,
                                    path: Vec::new(),
                                    follow_children: true,
                                    known_kind: Some(gix_object::Kind::Commit),
                                },
                                queue,
                                queued,
                                maximum_objects,
                                deduplicate_by_oid,
                            )?;
                        }
                    } else {
                        for parent in commit.parents() {
                            enqueue(
                                QueueItem {
                                    oid: parent,
                                    depth: TraversalDepth::RelativeBoundary,
                                    tree_depth: 0,
                                    path: Vec::new(),
                                    follow_children: true,
                                    known_kind: Some(gix_object::Kind::Commit),
                                },
                                queue,
                                queued,
                                maximum_objects,
                                deduplicate_by_oid,
                            )?;
                        }
                    }
                }
                TraversalDepth::Absolute(distance) => {
                    if existing_shallow.contains(&item.oid) {
                        let Some(limit) = request.deepen else {
                            return Ok(());
                        };
                        if distance.saturating_add(1) >= limit {
                            return Ok(());
                        }
                        unshallow.insert(item.oid);
                    } else if let Some(limit) = request.deepen
                        && distance.saturating_add(1) >= limit
                    {
                        shallow.insert(item.oid);
                        return Ok(());
                    }
                    for parent in commit.parents() {
                        enqueue(
                            QueueItem {
                                oid: parent,
                                depth: TraversalDepth::Absolute(distance.saturating_add(1)),
                                tree_depth: 0,
                                path: Vec::new(),
                                follow_children: true,
                                known_kind: Some(gix_object::Kind::Commit),
                            },
                            queue,
                            queued,
                            maximum_objects,
                            deduplicate_by_oid,
                        )?;
                    }
                }
                TraversalDepth::Relative(distance) => {
                    if let Some(limit) = request.deepen
                        && distance.saturating_add(1) >= limit
                    {
                        shallow.insert(item.oid);
                        return Ok(());
                    }
                    for parent in commit.parents() {
                        enqueue(
                            QueueItem {
                                oid: parent,
                                depth: TraversalDepth::Relative(distance.saturating_add(1)),
                                tree_depth: 0,
                                path: Vec::new(),
                                follow_children: true,
                                known_kind: Some(gix_object::Kind::Commit),
                            },
                            queue,
                            queued,
                            maximum_objects,
                            deduplicate_by_oid,
                        )?;
                    }
                }
            }
        }
        gix_object::Kind::Tree => {
            let tree = gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
                .map_err(|_| RemoteGitError::Corrupt {
                    stage: CorruptionStage::Tree,
                })?;
            for entry in tree.entries {
                let mut path = item.path.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(entry.filename.as_ref());
                let tree_depth = item.tree_depth.saturating_add(1);
                let (follow_children, should_enqueue) = if entry.mode.is_tree() {
                    (
                        true,
                        filter_may_have_tree_descendant(&request.filter, tree_depth),
                    )
                } else if entry.mode.is_blob_or_symlink() {
                    (
                        false,
                        filter_may_include_blob(
                            &request.filter,
                            tree_depth,
                            &path,
                            sparse_matchers,
                        ),
                    )
                } else if entry.mode.is_commit() {
                    // A gitlink names a submodule commit object, but that
                    // object and its history belong to another repository.
                    (false, false)
                } else {
                    (false, false)
                };
                if should_enqueue {
                    enqueue(
                        QueueItem {
                            oid: entry.oid.to_owned(),
                            depth: item.depth,
                            tree_depth,
                            path,
                            follow_children,
                            known_kind: if entry.mode.is_tree() {
                                Some(gix_object::Kind::Tree)
                            } else if entry.mode.is_blob_or_symlink() {
                                Some(gix_object::Kind::Blob)
                            } else if entry.mode.is_commit() {
                                Some(gix_object::Kind::Commit)
                            } else {
                                None
                            },
                        },
                        queue,
                        queued,
                        maximum_objects,
                        deduplicate_by_oid,
                    )?;
                }
            }
        }
        gix_object::Kind::Tag => {
            let tag = gix_object::TagRef::from_bytes(&object.data, gix_hash::Kind::Sha1).map_err(
                |_| RemoteGitError::Corrupt {
                    stage: CorruptionStage::Tag,
                },
            )?;
            if tag.target_kind != gix_object::Kind::Blob || !request.filter.contains_blob_none() {
                enqueue(
                    QueueItem {
                        oid: tag.target(),
                        depth: item.depth,
                        tree_depth: 0,
                        path: Vec::new(),
                        follow_children: true,
                        known_kind: Some(tag.target_kind),
                    },
                    queue,
                    queued,
                    maximum_objects,
                    deduplicate_by_oid,
                )?;
            }
        }
        gix_object::Kind::Blob => {}
    }
    Ok(())
}

fn filter_may_include_blob(
    filter: &UploadPackFilter,
    tree_depth: u32,
    path: &[u8],
    sparse_matchers: &SparseMatchers,
) -> bool {
    match filter {
        UploadPackFilter::None | UploadPackFilter::BlobLimit(_) => true,
        UploadPackFilter::BlobNone => false,
        UploadPackFilter::ObjectType(kind) => *kind == UploadPackObjectType::Blob,
        UploadPackFilter::TreeDepth(depth) => tree_depth < *depth,
        UploadPackFilter::Sparse { oid } => sparse_path_matches(sparse_matchers, oid, path),
        UploadPackFilter::Combine(filters) => filters
            .iter()
            .all(|filter| filter_may_include_blob(filter, tree_depth, path, sparse_matchers)),
    }
}

fn filter_may_have_tree_descendant(filter: &UploadPackFilter, tree_depth: u32) -> bool {
    match filter {
        UploadPackFilter::None
        | UploadPackFilter::BlobNone
        | UploadPackFilter::BlobLimit(_)
        | UploadPackFilter::Sparse { .. } => true,
        UploadPackFilter::ObjectType(kind) => {
            matches!(
                kind,
                UploadPackObjectType::Blob | UploadPackObjectType::Tree
            )
        }
        UploadPackFilter::TreeDepth(depth) => tree_depth < *depth,
        UploadPackFilter::Combine(filters) => filters
            .iter()
            .all(|filter| filter_may_have_tree_descendant(filter, tree_depth)),
    }
}

fn sparse_path_matches(sparse_matchers: &SparseMatchers, oid: &ObjectId, path: &[u8]) -> bool {
    let Some(search) = sparse_matchers.patterns.get(oid) else {
        return false;
    };
    let mut selected = false;
    let mut prefix = Vec::with_capacity(path.len());
    for (index, component) in path.split(|byte| *byte == b'/').enumerate() {
        if index > 0 {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(component);
        let is_directory = prefix.len() < path.len();
        if let Some(matched) = search.pattern_matching_relative_path(
            prefix.as_bstr(),
            Some(is_directory),
            gix_ignore::glob::pattern::Case::Sensitive,
        ) {
            selected = !matched.pattern.is_negative();
        }
    }
    selected
}

fn enqueue(
    item: QueueItem,
    queue: &mut VecDeque<QueueItem>,
    queued: &mut HashSet<QueueKey>,
    maximum_objects: u64,
    deduplicate_by_oid: bool,
) -> crab_remote_git::Result<()> {
    let key = if deduplicate_by_oid {
        QueueKey::Object(item.oid)
    } else {
        QueueKey::Context {
            oid: item.oid,
            depth: item.depth,
            tree_depth: item.tree_depth,
            path: item.path.clone(),
            follow_children: item.follow_children,
            known_kind: item.known_kind,
        }
    };
    if queued.contains(&key) {
        return Ok(());
    }
    let actual = u64::try_from(queued.len()).unwrap_or(u64::MAX);
    if actual >= maximum_objects {
        return Err(RemoteGitError::LimitExceeded {
            limit: "upload-pack traversal objects",
            actual: actual.saturating_add(1),
            maximum: maximum_objects,
        });
    }
    queued.insert(key);
    queue.push_back(item);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crab_remote_git::RepositoryRef;

    use super::*;

    fn oid(value: char) -> ObjectId {
        ObjectId::from_hex(value.to_string().repeat(40).as_bytes()).expect("test object ID")
    }

    fn visibility() -> GitVisibilityIndex {
        GitVisibilityIndex::new(
            7,
            "a".repeat(64),
            [
                (
                    "refs/heads/main".to_owned(),
                    vec![oid('1').to_hex().to_string(), oid('3').to_hex().to_string()],
                ),
                (
                    "refs/heads/secret".to_owned(),
                    vec![oid('2').to_hex().to_string(), oid('3').to_hex().to_string()],
                ),
            ]
            .into_iter()
            .collect(),
        )
    }

    fn references() -> Vec<RepositoryRef> {
        vec![
            RepositoryRef {
                name: "refs/heads/main".to_owned(),
                target: oid('1'),
                peeled: None,
            },
            RepositoryRef {
                name: "refs/heads/secret".to_owned(),
                target: oid('2'),
                peeled: None,
            },
        ]
    }

    #[test]
    fn full_ref_visibility_plan_deduplicates_duplicate_wants() {
        let request = UploadPackRequest {
            wants: vec![oid('1'), oid('1')],
            ..UploadPackRequest::default()
        };
        let plan = plan_from_visibility(
            &references(),
            &["refs/heads/main".to_owned()],
            &visibility(),
            &request,
            10,
        )
        .expect("valid visibility closure")
        .expect("fresh full ref fetch should use the visibility plan");

        assert_eq!(plan.object_ids, [oid('1'), oid('3')]);
        assert_eq!(plan.wants, request.wants);
        assert!(plan.common_haves.is_empty());
        assert!(plan.shallow.is_empty());
        assert!(plan.unshallow.is_empty());
    }

    #[test]
    fn visibility_plan_falls_back_when_request_semantics_need_traversal() {
        let cases = [
            UploadPackRequest {
                wants: vec![oid('3')],
                ..UploadPackRequest::default()
            },
            UploadPackRequest {
                wants: vec![oid('1')],
                haves: vec![oid('3')],
                ..UploadPackRequest::default()
            },
            UploadPackRequest {
                wants: vec![oid('1')],
                shallow: vec![oid('3')],
                ..UploadPackRequest::default()
            },
            UploadPackRequest {
                wants: vec![oid('1')],
                deepen: Some(1),
                ..UploadPackRequest::default()
            },
            UploadPackRequest {
                wants: vec![oid('1')],
                include_tags: true,
                ..UploadPackRequest::default()
            },
            UploadPackRequest {
                wants: vec![oid('1')],
                filter: UploadPackFilter::BlobNone,
                ..UploadPackRequest::default()
            },
        ];

        for request in cases {
            assert!(
                plan_from_visibility(
                    &references(),
                    &["refs/heads/main".to_owned()],
                    &visibility(),
                    &request,
                    10,
                )
                .expect("fallback decision")
                .is_none()
            );
        }
    }

    #[test]
    fn visibility_plan_enforces_the_operation_object_bound() {
        let request = UploadPackRequest {
            wants: vec![oid('1')],
            ..UploadPackRequest::default()
        };
        let error = plan_from_visibility(
            &references(),
            &["refs/heads/main".to_owned()],
            &visibility(),
            &request,
            1,
        )
        .expect_err("two-object closure must exceed a one-object operation bound");

        assert!(matches!(
            error,
            RemoteGitError::LimitExceeded {
                actual: 2,
                maximum: 1,
                ..
            }
        ));
    }

    #[test]
    fn visibility_plan_rejects_a_closure_missing_its_ref_target() {
        let proof = GitVisibilityIndex::new(
            7,
            "a".repeat(64),
            [("refs/heads/main".to_owned(), vec![oid('3').to_string()])]
                .into_iter()
                .collect(),
        );
        let request = UploadPackRequest {
            wants: vec![oid('1')],
            ..UploadPackRequest::default()
        };
        let error = plan_from_visibility(
            &references(),
            &["refs/heads/main".to_owned()],
            &proof,
            &request,
            10,
        )
        .expect_err("a visibility closure must contain its exact ref target");

        assert!(matches!(error, RemoteGitError::RepositoryState { .. }));
    }

    #[test]
    fn oid_admission_accepts_visible_and_shared_objects() {
        let proof = visibility();
        authorize_wants(
            &proof,
            &["refs/heads/main".to_owned()],
            &[oid('1'), oid('3')],
        )
        .expect("visible and shared objects are authorized");
    }

    #[test]
    fn oid_admission_rejects_hidden_only_and_unknown_objects() {
        let proof = visibility();
        let error = authorize_wants(&proof, &["refs/heads/main".to_owned()], &[oid('2')])
            .expect_err("hidden-only object must not be authorized");
        assert!(error.to_string().contains("outside the visible generation"));

        assert!(authorize_wants(&proof, &["refs/heads/main".to_owned()], &[oid('f')]).is_err());
    }

    #[test]
    fn traversal_admission_rejects_an_unproven_batch_before_reader_access() {
        let proof = visibility();
        let batch = [
            QueueItem {
                oid: oid('1'),
                depth: TraversalDepth::Absolute(0),
                tree_depth: 0,
                path: Vec::new(),
                follow_children: true,
                known_kind: None,
            },
            QueueItem {
                oid: oid('2'),
                depth: TraversalDepth::Absolute(0),
                tree_depth: 0,
                path: Vec::new(),
                follow_children: true,
                known_kind: None,
            },
        ];
        let error = admit_batch(&proof, &["refs/heads/main".to_owned()], &batch)
            .expect_err("an unproven traversal child must be denied");
        assert!(matches!(error, RemoteGitError::AuthorizationDenied));
    }

    #[test]
    fn traversal_queue_rejects_objects_beyond_the_operation_bound() {
        let mut queue = VecDeque::new();
        let mut queued = HashSet::new();
        enqueue(
            QueueItem {
                oid: oid('1'),
                depth: TraversalDepth::Absolute(0),
                tree_depth: 0,
                path: Vec::new(),
                follow_children: true,
                known_kind: None,
            },
            &mut queue,
            &mut queued,
            1,
            false,
        )
        .expect("first object fits the bound");
        let error = enqueue(
            QueueItem {
                oid: oid('2'),
                depth: TraversalDepth::Absolute(0),
                tree_depth: 0,
                path: Vec::new(),
                follow_children: true,
                known_kind: None,
            },
            &mut queue,
            &mut queued,
            1,
            false,
        )
        .expect_err("second object must exceed the bound");
        assert!(matches!(error, RemoteGitError::LimitExceeded { .. }));
    }

    #[test]
    fn path_independent_filters_deduplicate_shared_objects() {
        let mut queue = VecDeque::new();
        let mut queued = HashSet::new();
        for path in [b"first/path".as_slice(), b"second/path".as_slice()] {
            enqueue(
                QueueItem {
                    oid: oid('1'),
                    depth: TraversalDepth::Absolute(0),
                    tree_depth: 2,
                    path: path.to_vec(),
                    follow_children: false,
                    known_kind: Some(gix_object::Kind::Blob),
                },
                &mut queue,
                &mut queued,
                1,
                true,
            )
            .expect("shared object should be queued once");
        }
        assert_eq!(queue.len(), 1);
        assert_eq!(queued.len(), 1);
    }

    #[test]
    fn path_and_depth_filters_retain_traversal_context() {
        assert!(!filter_requires_traversal_context(&UploadPackFilter::None));
        assert!(!filter_requires_traversal_context(
            &UploadPackFilter::BlobLimit(1)
        ));
        assert!(filter_requires_traversal_context(
            &UploadPackFilter::TreeDepth(1)
        ));
        assert!(filter_requires_traversal_context(
            &UploadPackFilter::Sparse { oid: oid('1') }
        ));
        assert!(filter_requires_traversal_context(
            &UploadPackFilter::Combine(vec![UploadPackFilter::TreeDepth(1)])
        ));
    }

    #[test]
    fn parses_and_canonicalizes_supported_filter_forms() {
        let oid = "0123456789012345678901234567890123456789";
        let cases = [
            ("blob:none", "blob:none"),
            ("blob:limit=1m", "blob:limit=1048576"),
            ("object:type=tree", "object:type=tree"),
            ("tree:2", "tree:2"),
            (
                "sparse:oid=0123456789012345678901234567890123456789",
                "sparse:oid=0123456789012345678901234567890123456789",
            ),
            ("combine:blob%3Anone+tree%3A2", "combine:blob:none+tree:2"),
        ];
        for (spec, canonical) in cases {
            assert_eq!(
                parse_upload_pack_filter(spec)
                    .expect("supported filter should parse")
                    .canonical_spec(),
                canonical,
                "filter {spec}"
            );
        }
        assert_eq!(oid.len(), 40);
    }

    #[test]
    fn blob_limit_omits_blobs_at_the_limit() {
        let item = QueueItem {
            oid: oid('1'),
            depth: TraversalDepth::Absolute(0),
            tree_depth: 0,
            path: Vec::new(),
            follow_children: false,
            known_kind: Some(gix_object::Kind::Blob),
        };
        let sparse_matchers = SparseMatchers {
            patterns: HashMap::new(),
        };

        assert!(filter_accepts_metadata(
            &UploadPackFilter::BlobLimit(4),
            gix_object::Kind::Blob,
            3,
            &item,
            &sparse_matchers,
        ));
        assert!(!filter_accepts_metadata(
            &UploadPackFilter::BlobLimit(4),
            gix_object::Kind::Blob,
            4,
            &item,
            &sparse_matchers,
        ));
    }

    #[tokio::test]
    async fn tree_gitlinks_are_not_dereferenced_as_superproject_objects() {
        let mut tree_data = b"160000 submodule\0".to_vec();
        tree_data.extend_from_slice(&[7; 20]);
        let object = RemoteGitObject {
            oid: oid('1'),
            kind: gix_object::Kind::Tree,
            data: bytes::Bytes::from(tree_data),
        };
        let item = QueueItem {
            oid: oid('1'),
            depth: TraversalDepth::Absolute(0),
            tree_depth: 0,
            path: Vec::new(),
            follow_children: true,
            known_kind: Some(gix_object::Kind::Tree),
        };
        let mut queue = VecDeque::new();
        let mut queued = HashSet::new();
        let mut shallow = HashSet::new();
        let mut unshallow = HashSet::new();
        enqueue_children(
            &object,
            &item,
            &UploadPackRequest::default(),
            10,
            &HashSet::new(),
            &SparseMatchers {
                patterns: HashMap::new(),
            },
            &mut queue,
            &mut queued,
            &mut shallow,
            &mut unshallow,
            &CancellationToken::new(),
            false,
        )
        .await
        .expect("gitlink traversal should succeed without a submodule object");
        assert!(queue.is_empty());
    }

    #[test]
    fn rejects_unbounded_or_unknown_filter_forms() {
        for spec in [
            "",
            "blob:depth=1",
            "blob:limit=1t",
            "tree:-1",
            "object:type=unknown",
            "sparse:oid=0123",
            "combine:",
            "combine:blob%3Anone+",
            "combine:blob%3A%zz",
        ] {
            assert!(
                parse_upload_pack_filter(spec).is_err(),
                "filter {spec} must be rejected"
            );
        }
    }

    #[test]
    fn sparse_directory_pattern_matches_descendant_files() {
        let mut search = gix_ignore::Search::default();
        search.add_patterns_buffer(
            b"nested/\n!nested/third.txt\n",
            "sparse-checkout",
            None,
            gix_ignore::search::Ignore::default(),
        );
        let oid = oid('1');
        let sparse_matchers = SparseMatchers {
            patterns: [(oid, search)].into_iter().collect(),
        };
        assert!(
            sparse_path_matches(&sparse_matchers, &oid, b"nested/other.txt"),
            "nested/ must match a descendant"
        );
        assert!(!sparse_path_matches(
            &sparse_matchers,
            &oid,
            b"nested/third.txt"
        ));
        assert!(!sparse_path_matches(&sparse_matchers, &oid, b"normal.bin"));
    }
}
