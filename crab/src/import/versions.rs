//! Version-aware listing for `crab import`.
//!
//! The import pipeline needs two things that plain
//! [`object_store::ObjectStore`] doesn't give us:
//!
//! 1. A quick **sample** of a source prefix that answers "is this
//!    bucket versioned?" without pulling the entire key space. The
//!    detect stage uses this to pick between flat and versioned
//!    import modes.
//! 2. A **full enumeration** over every version of every key —
//!    optionally bounded by `--since` / `--until`, or collapsed to
//!    the live state at a single `--at` timestamp. The enumerate
//!    stage streams records from here into the resume journal.
//!
//! Object-store's unified API intentionally doesn't expose
//! version listing (S3 `ListObjectVersions`, GCS
//! `objects.list?versions=true`, Azure `List Blobs include=versions`
//! are all backend-specific). We model them behind a small trait
//! so the detect + enumerate stages stay cloud-agnostic.
//!
//! # Provider scope
//!
//! `LocalVersionedList` is a real, working implementation — tests
//! and `file://` imports rely on it end-to-end. `FlatObjectStoreList`
//! is also real: it lists the current live state from any
//! [`object_store::ObjectStore`] and backs flat cloud imports,
//! including S3-compatible stores such as RustFS.
//!
//! `S3VersionedList` uses S3 `ListObjectVersions`,
//! `GcsVersionedList` uses GCS `objects.list?versions=true`, and
//! `AzureVersionedList` uses Azure Blob `List Blobs
//! include=versions,deleted` when the matching provider feature is
//! enabled.
//!
//! # Callback-driven enumerate
//!
//! `enumerate` / `enumerate_at` take a `FnMut(VersionRecord)`
//! callback rather than returning a stream. This matches the
//! shape of the journal's existing row-iter path
//! (`Journal::iter_entries_sorted_by_time`) and sidesteps the
//! lifetime gymnastics of returning a cloud-backed stream through
//! a boxed trait object. The callback can short-circuit by
//! returning `Err(_)`, which we propagate to the caller.

use std::collections::BTreeMap;
#[cfg(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure"))]
use std::collections::HashSet;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectMeta, ObjectStore};

use crate::core::error::{CrabError, Result};
use crab_storage::map_object_store_error;

/// One version of one object.
///
/// Flat-mode entries (buckets without versioning) reuse this type
/// with `version_id = ""` and `is_delete_marker = false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRecord {
    /// Key relative to the source prefix. Slash-separated; does
    /// not start with `/`.
    pub key: String,
    /// Cloud-assigned version id. Empty string for flat mode.
    pub version_id: String,
    /// Size in bytes. Zero for delete markers.
    pub size: u64,
    /// Cloud etag, if exposed by the backend.
    pub etag: Option<String>,
    /// Last-modified timestamp in epoch seconds (UTC).
    pub last_modified: i64,
    /// True for S3 / Azure delete markers and GCS tombstones.
    /// These surface as git deletions during assemble.
    pub is_delete_marker: bool,
}

/// Output of [`VersionedList::sample`].
///
/// The detect stage classifies a source as versioned if
/// `total_versions > unique_keys` (more than one version per key
/// observed) or `has_delete_markers` (tombstones can't exist in a
/// flat bucket). Either condition alone is sufficient.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionSample {
    /// Total version records returned, capped at the sample limit.
    pub total_versions: usize,
    /// Distinct keys observed across those records.
    pub unique_keys: usize,
    /// Any delete marker / tombstone present in the sample.
    pub has_delete_markers: bool,
    /// The records themselves — retained so callers can inspect
    /// specific versions if needed (e.g. for dry-run preview).
    pub records: Vec<VersionRecord>,
}

impl VersionSample {
    /// True when the sample signals the source prefix is a
    /// versioned bucket: more than one version per key or any
    /// delete marker.
    #[must_use]
    pub fn is_versioned(&self) -> bool {
        self.has_delete_markers || self.total_versions > self.unique_keys
    }
}

/// Version-aware listing for a single source prefix.
///
/// Implementors are cheap to clone — they hold configuration
/// (prefix, credentials) rather than cached state. The trait is
/// object-safe via `#[async_trait]` so the detect + enumerate
/// stages can hold a `Box<dyn VersionedList>` when the concrete
/// backend isn't known at compile time.
#[async_trait]
pub trait VersionedList: Send + Sync {
    /// Returns up to `limit` version records from the head of the
    /// prefix. Used only by the detect stage; callers should pass
    /// ~1 000 and branch on [`VersionSample::is_versioned`].
    async fn sample(&self, limit: usize) -> Result<VersionSample>;

    /// Invokes `callback` once per version record in the prefix,
    /// filtered to `[since, until]` inclusive when either bound is
    /// set. Records are delivered in an implementation-defined
    /// order — window planning re-sorts by
    /// `(last_modified, key, version_id)` before commit emission,
    /// so upstream callers must not assume a particular stream
    /// order here.
    ///
    /// Returning `Err(_)` from the callback short-circuits the
    /// walk.
    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()>;

    /// Invokes `callback` once per key with the version live at
    /// timestamp `at` (epoch seconds). Delete markers are applied
    /// server- or client-side and do not reach the callback — a
    /// key whose live version at `at` is a delete marker is
    /// omitted entirely. Used by `--at` single-snapshot mode.
    async fn enumerate_at(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()>;
}

/// Concrete per-backend `VersionedList` implementations.
///
/// Exposed as an enum so detect can construct it from an
/// `ObjectUrl` without juggling `Box<dyn Trait>` in the common
/// path. Dispatch still goes through the trait so extension points
/// stay uniform.
#[derive(Clone)]
pub enum VersionedListImpl {
    Local(LocalVersionedList),
    FlatObjectStore(FlatObjectStoreList),
    S3(S3VersionedList),
    Gcs(GcsVersionedList),
    Azure(AzureVersionedList),
}

#[async_trait]
impl VersionedList for VersionedListImpl {
    async fn sample(&self, limit: usize) -> Result<VersionSample> {
        match self {
            Self::Local(inner) => inner.sample(limit).await,
            Self::FlatObjectStore(inner) => inner.sample(limit).await,
            Self::S3(inner) => inner.sample(limit).await,
            Self::Gcs(inner) => inner.sample(limit).await,
            Self::Azure(inner) => inner.sample(limit).await,
        }
    }

    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        match self {
            Self::Local(inner) => inner.enumerate(since, until, callback).await,
            Self::FlatObjectStore(inner) => inner.enumerate(since, until, callback).await,
            Self::S3(inner) => inner.enumerate(since, until, callback).await,
            Self::Gcs(inner) => inner.enumerate(since, until, callback).await,
            Self::Azure(inner) => inner.enumerate(since, until, callback).await,
        }
    }

    async fn enumerate_at(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        match self {
            Self::Local(inner) => inner.enumerate_at(at, callback).await,
            Self::FlatObjectStore(inner) => inner.enumerate_at(at, callback).await,
            Self::S3(inner) => inner.enumerate_at(at, callback).await,
            Self::Gcs(inner) => inner.enumerate_at(at, callback).await,
            Self::Azure(inner) => inner.enumerate_at(at, callback).await,
        }
    }
}

// ── LocalVersionedList ──────────────────────────────────────────

/// Filesystem-backed versioned list.
///
/// Local filesystems don't carry versioning, so every method
/// reports "non-versioned" — `sample` returns exactly one record
/// per file, `enumerate` walks the tree once and emits the live
/// state, and `enumerate_at` is identical to `enumerate` (mtime
/// is the only timestamp the filesystem tracks, so the "live
/// state at `at`" is the live state).
///
/// Used for `file://` sources and as the backbone of the import
/// test suite. Intentionally a real implementation — several
/// integration tests rely on end-to-end behavior with no cloud
/// dependency.
#[derive(Debug, Clone)]
pub struct LocalVersionedList {
    root: PathBuf,
}

impl LocalVersionedList {
    /// Builds a local listing rooted at `root`.
    ///
    /// `root` must be an existing directory; otherwise enumerate
    /// calls return [`CrabError::NotFound`]. The root is treated
    /// as the source prefix: emitted keys are relative to `root`,
    /// slash-separated, and never start with `/`.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Walks the tree once, invoking `on_record` for each regular
    /// file. Directories, symlinks, and other non-regular entries
    /// are skipped silently — they'd fail the pointer-write step
    /// later anyway, and git doesn't model them.
    fn walk(&self, on_record: &mut dyn FnMut(VersionRecord) -> Result<()>) -> Result<()> {
        if !self.root.exists() {
            return Err(CrabError::NotFound {
                path: self.root.to_string_lossy().into_owned(),
            });
        }
        if !self.root.is_dir() {
            return Err(CrabError::Internal(format!(
                "LocalVersionedList root is not a directory: {}",
                self.root.display()
            )));
        }

        // Explicit stack rather than recursion — avoids deep-path
        // stack overflows on pathological trees and keeps the
        // walker allocation-cheap.
        let mut stack: Vec<PathBuf> = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let read = std::fs::read_dir(&dir).map_err(|e| {
                CrabError::Internal(format!(
                    "LocalVersionedList read_dir({}): {e}",
                    dir.display()
                ))
            })?;
            for entry in read {
                let entry = entry.map_err(|e| {
                    CrabError::Internal(format!(
                        "LocalVersionedList dir entry in {}: {e}",
                        dir.display()
                    ))
                })?;
                let path = entry.path();
                let Ok(metadata) = entry.metadata() else {
                    // Racy removal between read_dir and metadata:
                    // skip the vanished entry rather than failing
                    // the whole walk.
                    continue;
                };
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }

                let key = relative_key(&self.root, &path)?;
                let size = metadata.len();
                let last_modified = metadata
                    .modified()
                    .ok()
                    .and_then(system_time_to_epoch_secs)
                    .unwrap_or(0);

                on_record(VersionRecord {
                    key,
                    version_id: String::new(),
                    size,
                    etag: None,
                    last_modified,
                    is_delete_marker: false,
                })?;
            }
        }
        Ok(())
    }
}

fn relative_key(root: &StdPath, full: &StdPath) -> Result<String> {
    let rel = full.strip_prefix(root).map_err(|e| {
        CrabError::Internal(format!(
            "LocalVersionedList strip_prefix({}, {}): {e}",
            full.display(),
            root.display()
        ))
    })?;
    // Normalize to forward slashes regardless of host OS so the
    // emitted keys match the cloud-native convention. Non-UTF-8
    // path components are rejected — git tree entries can't hold
    // arbitrary bytes as paths, and surfacing the issue here is
    // cleaner than failing deep inside assemble.
    let mut out = String::with_capacity(rel.as_os_str().len());
    for (i, component) in rel.components().enumerate() {
        let part = component.as_os_str().to_str().ok_or_else(|| {
            CrabError::Internal(format!(
                "LocalVersionedList non-UTF-8 path component under {}",
                root.display()
            ))
        })?;
        if i > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    Ok(out)
}

fn system_time_to_epoch_secs(t: SystemTime) -> Option<i64> {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

#[async_trait]
impl VersionedList for LocalVersionedList {
    async fn sample(&self, limit: usize) -> Result<VersionSample> {
        let mut records: Vec<VersionRecord> = Vec::new();
        {
            let records_ref = &mut records;
            let walk_result = self.walk(&mut |rec| {
                if records_ref.len() >= limit {
                    // Short-circuit: we've hit the sample ceiling.
                    // A dedicated error keeps the walker structure
                    // simple; we translate it to `Ok(())` below.
                    return Err(CrabError::Cancelled);
                }
                records_ref.push(rec);
                Ok(())
            });
            match walk_result {
                // Either a full walk completed, or the sentinel we
                // emit above to bail out at the sample ceiling.
                // Any Cancelled surfacing inside `walk` can only
                // be our own — the sync walker never observes
                // cancellation tokens — so swallowing it here is
                // safe.
                Ok(()) | Err(CrabError::Cancelled) => {}
                Err(other) => return Err(other),
            }
        }

        // Local filesystem can't represent multiple versions per
        // path or delete markers, so the sample is, by definition,
        // non-versioned.
        let unique_keys = records.len();
        Ok(VersionSample {
            total_versions: records.len(),
            unique_keys,
            has_delete_markers: false,
            records,
        })
    }

    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.walk(&mut |rec| {
            if let Some(min) = since
                && rec.last_modified < min
            {
                return Ok(());
            }
            if let Some(max) = until
                && rec.last_modified > max
            {
                return Ok(());
            }
            callback(rec)
        })
    }

    async fn enumerate_at(
        &self,
        _at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        // Local filesystems don't carry history, so "live state at
        // `at`" collapses to "the current state". The `_at`
        // argument is accepted for API uniformity with cloud
        // backends.
        self.walk(callback)
    }
}

/// Flat current-state listing over any [`ObjectStore`].
///
/// Used for non-versioned cloud imports and S3-compatible stores
/// such as RustFS. Version history is intentionally out of scope
/// for this lister: it emits exactly the live objects returned by
/// the backend's ordinary `list` API, with empty version ids and
/// no delete markers.
#[derive(Clone)]
pub struct FlatObjectStoreList {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl FlatObjectStoreList {
    /// Builds a current-state lister for `prefix`.
    ///
    /// The prefix is normalized as a directory prefix. For example,
    /// `data/models` lists `data/models/...` and will not import
    /// adjacent keys such as `data/models-v2/...`.
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            store,
            prefix: normalize_object_prefix(prefix.into()),
        }
    }

    fn list_prefix_path(&self) -> Option<ObjectPath> {
        if self.prefix.is_empty() {
            None
        } else {
            Some(ObjectPath::from(format!("{}/", self.prefix)))
        }
    }

    fn record_from_meta(&self, meta: ObjectMeta) -> Option<VersionRecord> {
        let full = meta.location.to_string();
        let key = if self.prefix.is_empty() {
            full
        } else {
            let dir_prefix = format!("{}/", self.prefix);
            full.strip_prefix(&dir_prefix)?.to_owned()
        };

        if key.is_empty() {
            return None;
        }

        Some(VersionRecord {
            key,
            version_id: String::new(),
            size: meta.size,
            etag: meta.e_tag,
            last_modified: meta.last_modified.timestamp(),
            is_delete_marker: false,
        })
    }

    async fn walk_current(
        &self,
        limit: Option<usize>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<usize> {
        let prefix = self.list_prefix_path();
        let mut stream = self.store.list(prefix.as_ref());
        let mut seen = 0usize;

        while let Some(meta) = stream
            .try_next()
            .await
            .map_err(|e| CrabError::from(map_object_store_error(e, &self.prefix)))?
        {
            let Some(record) = self.record_from_meta(meta) else {
                continue;
            };
            callback(record)?;
            seen = seen.saturating_add(1);
            if limit.is_some_and(|max| seen >= max) {
                break;
            }
        }

        Ok(seen)
    }
}

fn normalize_object_prefix(mut prefix: String) -> String {
    while prefix.starts_with('/') {
        prefix.remove(0);
    }
    while prefix.ends_with('/') {
        prefix.pop();
    }
    prefix
}

#[cfg(any(feature = "tier-s3", feature = "tier-gcs", feature = "tier-azure"))]
fn sample_from_records(records: Vec<VersionRecord>) -> VersionSample {
    let unique_keys = records
        .iter()
        .map(|record| record.key.as_str())
        .collect::<HashSet<_>>()
        .len();
    let has_delete_markers = records.iter().any(|record| record.is_delete_marker);
    VersionSample {
        total_versions: records.len(),
        unique_keys,
        has_delete_markers,
        records,
    }
}

fn object_relative_key(prefix: &str, full_key: &str) -> Option<String> {
    if prefix.is_empty() {
        return (!full_key.is_empty()).then(|| full_key.to_owned());
    }

    let dir_prefix = format!("{prefix}/");
    full_key
        .strip_prefix(&dir_prefix)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
}

fn snapshot_records_at(records: Vec<VersionRecord>, at: i64) -> Vec<VersionRecord> {
    let mut latest: BTreeMap<String, VersionRecord> = BTreeMap::new();
    for record in records {
        if record.last_modified > at {
            continue;
        }
        match latest.get(&record.key) {
            Some(existing) if !is_newer_snapshot_record(&record, existing) => {}
            _ => {
                latest.insert(record.key.clone(), record);
            }
        }
    }

    latest
        .into_values()
        .filter(|record| !record.is_delete_marker)
        .collect()
}

fn is_newer_snapshot_record(candidate: &VersionRecord, existing: &VersionRecord) -> bool {
    (candidate.last_modified, candidate.version_id.as_str())
        > (existing.last_modified, existing.version_id.as_str())
}

#[async_trait]
impl VersionedList for FlatObjectStoreList {
    async fn sample(&self, limit: usize) -> Result<VersionSample> {
        let mut records = Vec::new();
        self.walk_current(Some(limit), &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        let unique_keys = records.len();
        Ok(VersionSample {
            total_versions: records.len(),
            unique_keys,
            has_delete_markers: false,
            records,
        })
    }

    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.walk_current(None, &mut |record| {
            if let Some(min) = since
                && record.last_modified < min
            {
                return Ok(());
            }
            if let Some(max) = until
                && record.last_modified > max
            {
                return Ok(());
            }
            callback(record)
        })
        .await?;

        Ok(())
    }

    async fn enumerate_at(
        &self,
        _at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate(None, None, callback).await
    }
}

// ── S3VersionedList ─────────────────────────────────────────────

/// S3 versioned listing.
#[derive(Debug, Clone)]
pub struct S3VersionedList {
    #[cfg_attr(not(feature = "tier-s3"), allow(dead_code))]
    bucket: String,
    #[cfg_attr(not(feature = "tier-s3"), allow(dead_code))]
    prefix: String,
}

impl S3VersionedList {
    /// Builds a versioned listing for the given bucket and prefix.
    #[must_use]
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: normalize_object_prefix(prefix.into()),
        }
    }

    #[cfg(feature = "tier-s3")]
    fn list_prefix(&self) -> Option<String> {
        if self.prefix.is_empty() {
            None
        } else {
            Some(format!("{}/", self.prefix))
        }
    }
}

#[cfg(not(feature = "tier-s3"))]
const S3_STUB_MSG: &str =
    "versioned listing requires aws-sdk-s3 (S3VersionedList is stubbed in V1)";

#[cfg(feature = "tier-s3")]
const S3_LIST_OBJECT_VERSIONS_PAGE_SIZE: i32 = 1_000;

#[async_trait]
impl VersionedList for S3VersionedList {
    async fn sample(&self, limit: usize) -> Result<VersionSample> {
        self.sample_impl(limit).await
    }

    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate_impl(since, until, callback).await
    }

    async fn enumerate_at(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate_at_impl(at, callback).await
    }
}

impl S3VersionedList {
    #[cfg(not(feature = "tier-s3"))]
    async fn sample_impl(&self, _limit: usize) -> Result<VersionSample> {
        Err(CrabError::Internal(S3_STUB_MSG.into()))
    }

    #[cfg(not(feature = "tier-s3"))]
    async fn enumerate_impl(
        &self,
        _since: Option<i64>,
        _until: Option<i64>,
        _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        Err(CrabError::Internal(S3_STUB_MSG.into()))
    }

    #[cfg(not(feature = "tier-s3"))]
    async fn enumerate_at_impl(
        &self,
        _at: i64,
        _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        Err(CrabError::Internal(S3_STUB_MSG.into()))
    }

    #[cfg(feature = "tier-s3")]
    async fn sample_impl(&self, limit: usize) -> Result<VersionSample> {
        let mut records = Vec::new();
        self.walk_versions(Some(limit), None, None, &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        Ok(sample_from_records(records))
    }

    #[cfg(feature = "tier-s3")]
    async fn enumerate_impl(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.walk_versions(None, since, until, callback).await
    }

    #[cfg(feature = "tier-s3")]
    async fn enumerate_at_impl(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        let mut records = Vec::new();
        self.walk_versions(None, None, Some(at), &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        for record in snapshot_records_at(records, at) {
            callback(record)?;
        }
        Ok(())
    }

    #[cfg(feature = "tier-s3")]
    async fn walk_versions(
        &self,
        limit: Option<usize>,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        let client = build_s3_version_client().await;
        let mut key_marker: Option<String> = None;
        let mut version_id_marker: Option<String> = None;
        let mut seen = 0_usize;

        loop {
            let max_keys = match limit {
                Some(limit) => {
                    let remaining = limit.saturating_sub(seen);
                    if remaining == 0 {
                        return Ok(());
                    }
                    remaining.min(S3_LIST_OBJECT_VERSIONS_PAGE_SIZE as usize) as i32
                }
                None => S3_LIST_OBJECT_VERSIONS_PAGE_SIZE,
            };

            let mut request = client
                .list_object_versions()
                .bucket(self.bucket.clone())
                .max_keys(max_keys);
            if let Some(prefix) = self.list_prefix() {
                request = request.prefix(prefix);
            }
            if let Some(marker) = &key_marker {
                request = request.key_marker(marker.clone());
            }
            if let Some(marker) = &version_id_marker {
                request = request.version_id_marker(marker.clone());
            }

            let page = request
                .send()
                .await
                .map_err(|err| s3_version_error("list object versions", err))?;

            for record in s3_records_from_page(&self.prefix, &page)? {
                if let Some(min) = since
                    && record.last_modified < min
                {
                    continue;
                }
                if let Some(max) = until
                    && record.last_modified > max
                {
                    continue;
                }
                callback(record)?;
                seen += 1;
                if let Some(limit) = limit
                    && seen >= limit
                {
                    return Ok(());
                }
            }

            if !page.is_truncated().unwrap_or(false) {
                break;
            }
            key_marker = page.next_key_marker().map(ToOwned::to_owned);
            version_id_marker = page.next_version_id_marker().map(ToOwned::to_owned);
            if key_marker.is_none() && version_id_marker.is_none() {
                return Err(CrabError::Internal(
                    "S3 ListObjectVersions response was truncated without next markers".into(),
                ));
            }
        }

        Ok(())
    }
}

#[cfg(feature = "tier-s3")]
async fn build_s3_version_client() -> aws_sdk_s3::Client {
    let endpoint = crab_storage::s3_endpoint_from_env();
    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_owned());

    let mut config_loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region));
    if let Some(endpoint) = &endpoint {
        config_loader = config_loader.endpoint_url(endpoint.clone());
    }

    let config = config_loader.load().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&config)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());

    if let Some(endpoint) = endpoint {
        builder = builder.endpoint_url(endpoint);
        let force_path_style = crab_storage::s3_virtual_hosted_style_from_env()
            .is_none_or(|virtual_hosted| !virtual_hosted);
        builder = builder.force_path_style(force_path_style);
    } else if let Some(virtual_hosted) = crab_storage::s3_virtual_hosted_style_from_env() {
        builder = builder.force_path_style(!virtual_hosted);
    }

    aws_sdk_s3::Client::from_conf(builder.build())
}

#[cfg(feature = "tier-s3")]
fn s3_version_error(operation: &str, err: impl std::fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "import.s3.version_listing".into(),
        origin: format!("S3 {operation} failed: {err}"),
    }
}

#[cfg(feature = "tier-s3")]
fn s3_records_from_page(
    prefix: &str,
    page: &aws_sdk_s3::operation::list_object_versions::ListObjectVersionsOutput,
) -> Result<Vec<VersionRecord>> {
    let mut records = Vec::with_capacity(page.versions().len() + page.delete_markers().len());
    for version in page.versions() {
        if let Some(record) = s3_object_version_record(prefix, version)? {
            records.push(record);
        }
    }
    for marker in page.delete_markers() {
        if let Some(record) = s3_delete_marker_record(prefix, marker)? {
            records.push(record);
        }
    }
    Ok(records)
}

#[cfg(feature = "tier-s3")]
fn s3_object_version_record(
    prefix: &str,
    version: &aws_sdk_s3::types::ObjectVersion,
) -> Result<Option<VersionRecord>> {
    let full_key = version.key().ok_or_else(|| {
        CrabError::Internal("S3 ListObjectVersions returned an object version without key".into())
    })?;
    let Some(key) = s3_relative_key(prefix, full_key) else {
        return Ok(None);
    };
    let version_id = version.version_id().ok_or_else(|| {
        CrabError::Internal(format!(
            "S3 ListObjectVersions returned object {full_key} without version id"
        ))
    })?;
    let size = version.size().ok_or_else(|| {
        CrabError::Internal(format!(
            "S3 ListObjectVersions returned object {full_key} without size"
        ))
    })?;
    let size = u64::try_from(size).map_err(|_| {
        CrabError::Internal(format!(
            "S3 ListObjectVersions returned negative size for object {full_key}"
        ))
    })?;
    let last_modified = version.last_modified().ok_or_else(|| {
        CrabError::Internal(format!(
            "S3 ListObjectVersions returned object {full_key} without last_modified"
        ))
    })?;

    Ok(Some(VersionRecord {
        key,
        version_id: version_id.to_owned(),
        size,
        etag: version.e_tag().map(ToOwned::to_owned),
        last_modified: last_modified.secs(),
        is_delete_marker: false,
    }))
}

#[cfg(feature = "tier-s3")]
fn s3_delete_marker_record(
    prefix: &str,
    marker: &aws_sdk_s3::types::DeleteMarkerEntry,
) -> Result<Option<VersionRecord>> {
    let full_key = marker.key().ok_or_else(|| {
        CrabError::Internal("S3 ListObjectVersions returned a delete marker without key".into())
    })?;
    let Some(key) = s3_relative_key(prefix, full_key) else {
        return Ok(None);
    };
    let version_id = marker.version_id().ok_or_else(|| {
        CrabError::Internal(format!(
            "S3 ListObjectVersions returned delete marker {full_key} without version id"
        ))
    })?;
    let last_modified = marker.last_modified().ok_or_else(|| {
        CrabError::Internal(format!(
            "S3 ListObjectVersions returned delete marker {full_key} without last_modified"
        ))
    })?;

    Ok(Some(VersionRecord {
        key,
        version_id: version_id.to_owned(),
        size: 0,
        etag: None,
        last_modified: last_modified.secs(),
        is_delete_marker: true,
    }))
}

#[cfg(feature = "tier-s3")]
fn s3_relative_key(prefix: &str, full_key: &str) -> Option<String> {
    object_relative_key(prefix, full_key)
}

/// GCS versioned listing.
#[derive(Debug, Clone)]
pub struct GcsVersionedList {
    #[cfg_attr(not(feature = "tier-gcs"), allow(dead_code))]
    bucket: String,
    #[cfg_attr(not(feature = "tier-gcs"), allow(dead_code))]
    prefix: String,
}

impl GcsVersionedList {
    /// Builds a versioned listing for the given bucket and prefix.
    #[must_use]
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: normalize_object_prefix(prefix.into()),
        }
    }

    #[cfg(feature = "tier-gcs")]
    fn list_prefix(&self) -> Option<String> {
        if self.prefix.is_empty() {
            None
        } else {
            Some(format!("{}/", self.prefix))
        }
    }
}

#[cfg(not(feature = "tier-gcs"))]
const GCS_STUB_MSG: &str =
    "versioned listing requires google-cloud-storage (GcsVersionedList feature is disabled)";

#[cfg(feature = "tier-gcs")]
const GCS_LIST_OBJECTS_PAGE_SIZE: i32 = 1_000;

#[async_trait]
impl VersionedList for GcsVersionedList {
    async fn sample(&self, limit: usize) -> Result<VersionSample> {
        self.sample_impl(limit).await
    }

    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate_impl(since, until, callback).await
    }

    async fn enumerate_at(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate_at_impl(at, callback).await
    }
}

impl GcsVersionedList {
    #[cfg(not(feature = "tier-gcs"))]
    async fn sample_impl(&self, _limit: usize) -> Result<VersionSample> {
        Err(CrabError::Internal(GCS_STUB_MSG.into()))
    }

    #[cfg(not(feature = "tier-gcs"))]
    async fn enumerate_impl(
        &self,
        _since: Option<i64>,
        _until: Option<i64>,
        _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        Err(CrabError::Internal(GCS_STUB_MSG.into()))
    }

    #[cfg(not(feature = "tier-gcs"))]
    async fn enumerate_at_impl(
        &self,
        _at: i64,
        _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        Err(CrabError::Internal(GCS_STUB_MSG.into()))
    }

    #[cfg(feature = "tier-gcs")]
    async fn sample_impl(&self, limit: usize) -> Result<VersionSample> {
        let mut records = Vec::new();
        self.walk_versions(Some(limit), None, None, &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        Ok(sample_from_records(records))
    }

    #[cfg(feature = "tier-gcs")]
    async fn enumerate_impl(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.walk_versions(None, since, until, callback).await
    }

    #[cfg(feature = "tier-gcs")]
    async fn enumerate_at_impl(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        let mut records = Vec::new();
        self.walk_versions(None, None, Some(at), &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        for record in snapshot_records_at(records, at) {
            callback(record)?;
        }
        Ok(())
    }

    #[cfg(feature = "tier-gcs")]
    async fn walk_versions(
        &self,
        limit: Option<usize>,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        use google_cloud_storage::http::objects::list::ListObjectsRequest;

        let client = build_gcs_version_client().await?;
        let mut page_token: Option<String> = None;
        let mut seen = 0_usize;

        loop {
            let max_results = match limit {
                Some(limit) => {
                    let remaining = limit.saturating_sub(seen);
                    if remaining == 0 {
                        return Ok(());
                    }
                    remaining.min(GCS_LIST_OBJECTS_PAGE_SIZE as usize) as i32
                }
                None => GCS_LIST_OBJECTS_PAGE_SIZE,
            };

            let request = ListObjectsRequest {
                bucket: self.bucket.clone(),
                prefix: self.list_prefix(),
                versions: Some(true),
                max_results: Some(max_results),
                page_token,
                ..Default::default()
            };

            let page = client
                .list_objects(&request)
                .await
                .map_err(|err| gcs_version_error("list objects", err))?;

            for object in page.items.unwrap_or_default() {
                for record in gcs_records_from_object(&self.prefix, &object)? {
                    if let Some(min) = since
                        && record.last_modified < min
                    {
                        continue;
                    }
                    if let Some(max) = until
                        && record.last_modified > max
                    {
                        continue;
                    }
                    callback(record)?;
                    seen += 1;
                    if let Some(limit) = limit
                        && seen >= limit
                    {
                        return Ok(());
                    }
                }
            }

            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(())
    }
}

#[cfg(feature = "tier-gcs")]
async fn build_gcs_version_client() -> Result<google_cloud_storage::client::Client> {
    let config = google_cloud_storage::client::ClientConfig::default()
        .with_auth()
        .await
        .map_err(|err| gcs_version_error("load credentials", err))?;
    Ok(google_cloud_storage::client::Client::new(config))
}

#[cfg(feature = "tier-gcs")]
fn gcs_version_error(operation: &str, err: impl std::fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "import.gcs.version_listing".into(),
        origin: format!("GCS {operation} failed: {err}"),
    }
}

#[cfg(feature = "tier-gcs")]
fn gcs_records_from_object(
    prefix: &str,
    object: &google_cloud_storage::http::objects::Object,
) -> Result<Vec<VersionRecord>> {
    let Some(key) = object_relative_key(prefix, &object.name) else {
        return Ok(Vec::new());
    };
    if object.generation <= 0 {
        return Err(CrabError::Internal(format!(
            "GCS objects.list returned object {} without a positive generation",
            object.name
        )));
    }
    let size = u64::try_from(object.size).map_err(|_| {
        CrabError::Internal(format!(
            "GCS objects.list returned negative size for object {}",
            object.name
        ))
    })?;
    let content_time = object
        .time_created
        .or(object.updated)
        .or(object.time_deleted)
        .ok_or_else(|| {
            CrabError::Internal(format!(
                "GCS objects.list returned object {} without timeCreated, updated, or timeDeleted",
                object.name
            ))
        })?;
    let version_id = object.generation.to_string();
    let etag = (!object.etag.is_empty()).then(|| object.etag.clone());

    let mut records = vec![VersionRecord {
        key: key.clone(),
        version_id: version_id.clone(),
        size,
        etag,
        last_modified: content_time.unix_timestamp(),
        is_delete_marker: false,
    }];

    if let Some(deleted_at) = object.time_deleted {
        records.push(VersionRecord {
            key,
            version_id: format!("{version_id}#delete"),
            size: 0,
            etag: None,
            last_modified: deleted_at.unix_timestamp(),
            is_delete_marker: true,
        });
    }

    Ok(records)
}

/// Azure versioned listing.
#[derive(Debug, Clone)]
pub struct AzureVersionedList {
    #[cfg_attr(not(feature = "tier-azure"), allow(dead_code))]
    account: String,
    #[cfg_attr(not(feature = "tier-azure"), allow(dead_code))]
    container: String,
    #[cfg_attr(not(feature = "tier-azure"), allow(dead_code))]
    prefix: String,
}

impl AzureVersionedList {
    /// Builds a versioned listing for the given account, container,
    /// and object prefix.
    #[must_use]
    pub fn new(
        account: impl Into<String>,
        container: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Self {
        Self {
            account: account.into(),
            container: container.into(),
            prefix: normalize_object_prefix(prefix.into()),
        }
    }

    #[cfg(feature = "tier-azure")]
    fn list_prefix(&self) -> Option<String> {
        if self.prefix.is_empty() {
            None
        } else {
            Some(format!("{}/", self.prefix))
        }
    }
}

#[cfg(not(feature = "tier-azure"))]
const AZURE_STUB_MSG: &str =
    "versioned listing requires azure_storage_blobs (AzureVersionedList feature is disabled)";

#[cfg(feature = "tier-azure")]
const AZURE_LIST_BLOBS_PAGE_SIZE: u32 = 5_000;

#[async_trait]
impl VersionedList for AzureVersionedList {
    async fn sample(&self, limit: usize) -> Result<VersionSample> {
        self.sample_impl(limit).await
    }

    async fn enumerate(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate_impl(since, until, callback).await
    }

    async fn enumerate_at(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.enumerate_at_impl(at, callback).await
    }
}

impl AzureVersionedList {
    #[cfg(not(feature = "tier-azure"))]
    async fn sample_impl(&self, _limit: usize) -> Result<VersionSample> {
        Err(CrabError::Internal(AZURE_STUB_MSG.into()))
    }

    #[cfg(not(feature = "tier-azure"))]
    async fn enumerate_impl(
        &self,
        _since: Option<i64>,
        _until: Option<i64>,
        _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        Err(CrabError::Internal(AZURE_STUB_MSG.into()))
    }

    #[cfg(not(feature = "tier-azure"))]
    async fn enumerate_at_impl(
        &self,
        _at: i64,
        _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        Err(CrabError::Internal(AZURE_STUB_MSG.into()))
    }

    #[cfg(feature = "tier-azure")]
    async fn sample_impl(&self, limit: usize) -> Result<VersionSample> {
        let mut records = Vec::new();
        self.walk_versions(Some(limit), None, None, &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        Ok(sample_from_records(records))
    }

    #[cfg(feature = "tier-azure")]
    async fn enumerate_impl(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        self.walk_versions(None, since, until, callback).await
    }

    #[cfg(feature = "tier-azure")]
    async fn enumerate_at_impl(
        &self,
        at: i64,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        let mut records = Vec::new();
        self.walk_versions(None, None, Some(at), &mut |record| {
            records.push(record);
            Ok(())
        })
        .await?;

        for record in snapshot_records_at(records, at) {
            callback(record)?;
        }
        Ok(())
    }

    #[cfg(feature = "tier-azure")]
    async fn walk_versions(
        &self,
        limit: Option<usize>,
        since: Option<i64>,
        until: Option<i64>,
        callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
    ) -> Result<()> {
        use azure_core::prelude::{MaxResults, Prefix};

        let client = build_azure_container_client(&self.account, &self.container)?;
        let mut seen = 0_usize;

        let max_results = match limit {
            Some(limit) => {
                if limit == 0 {
                    return Ok(());
                }
                limit.min(AZURE_LIST_BLOBS_PAGE_SIZE as usize) as u32
            }
            None => AZURE_LIST_BLOBS_PAGE_SIZE,
        };
        let max_results = MaxResults::try_from(max_results).map_err(|err| {
            CrabError::Internal(format!("Azure List Blobs maxresults was invalid: {err}"))
        })?;

        let mut request = client
            .list_blobs()
            .include_versions(true)
            .include_deleted(true)
            .max_results(max_results);
        if let Some(prefix) = self.list_prefix() {
            request = request.prefix(Prefix::new(prefix));
        }

        let mut stream = request.into_stream();
        while let Some(page) = stream
            .try_next()
            .await
            .map_err(|err| azure_version_error("list blobs", err))?
        {
            for blob in page.blobs.blobs() {
                for record in azure_records_from_blob(&self.prefix, blob)? {
                    if let Some(min) = since
                        && record.last_modified < min
                    {
                        continue;
                    }
                    if let Some(max) = until
                        && record.last_modified > max
                    {
                        continue;
                    }
                    callback(record)?;
                    seen += 1;
                    if let Some(limit) = limit
                        && seen >= limit
                    {
                        return Ok(());
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(feature = "tier-azure")]
fn build_azure_container_client(
    account: &str,
    container: &str,
) -> Result<azure_storage_blobs::prelude::ContainerClient> {
    use azure_storage::StorageCredentials;
    use azure_storage_blobs::prelude::ClientBuilder;

    let key = std::env::var("AZURE_STORAGE_ACCESS_KEY")
        .or_else(|_| std::env::var("AZURE_STORAGE_KEY"))
        .map_err(|_| CrabError::Configuration {
            key: "AZURE_STORAGE_ACCESS_KEY".into(),
            origin: "environment".into(),
        })?;

    let credentials = StorageCredentials::access_key(account.to_owned(), key);
    let service_client = ClientBuilder::new(account.to_owned(), credentials);
    Ok(service_client.container_client(container.to_owned()))
}

#[cfg(feature = "tier-azure")]
fn azure_version_error(operation: &str, err: impl std::fmt::Display) -> CrabError {
    CrabError::Configuration {
        key: "import.azure.version_listing".into(),
        origin: format!("Azure {operation} failed: {err}"),
    }
}

#[cfg(feature = "tier-azure")]
fn azure_records_from_blob(
    prefix: &str,
    blob: &azure_storage_blobs::prelude::Blob,
) -> Result<Vec<VersionRecord>> {
    let Some(key) = object_relative_key(prefix, &blob.name) else {
        return Ok(Vec::new());
    };

    let deleted = blob.deleted.unwrap_or(false);
    let version_id = blob.version_id.clone();
    let Some(version_id) = version_id else {
        if deleted {
            return Ok(vec![VersionRecord {
                key,
                version_id: format!("{}#delete", blob.name),
                size: 0,
                etag: Some(blob.properties.etag.to_string()),
                last_modified: blob
                    .properties
                    .deleted_time
                    .unwrap_or(blob.properties.last_modified)
                    .unix_timestamp(),
                is_delete_marker: true,
            }]);
        }
        return Err(CrabError::Internal(format!(
            "Azure List Blobs returned blob {} without version id",
            blob.name
        )));
    };

    let mut records = vec![VersionRecord {
        key: key.clone(),
        version_id: version_id.clone(),
        size: blob.properties.content_length,
        etag: Some(blob.properties.etag.to_string()),
        last_modified: blob.properties.last_modified.unix_timestamp(),
        is_delete_marker: false,
    }];

    if deleted {
        records.push(VersionRecord {
            key,
            version_id: format!("{version_id}#delete"),
            size: 0,
            etag: Some(blob.properties.etag.to_string()),
            last_modified: blob
                .properties
                .deleted_time
                .unwrap_or(blob.properties.last_modified)
                .unix_timestamp(),
            is_delete_marker: true,
        });
    }

    Ok(records)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    use std::fs;

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStoreExt, PutPayload};
    use tempfile::TempDir;

    fn write_file(root: &StdPath, rel: &str, contents: &[u8]) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, contents).unwrap();
    }

    async fn seed_object(store: &Arc<dyn ObjectStore>, key: &str, body: &[u8]) {
        store
            .put(
                &ObjectPath::from(key.to_owned()),
                PutPayload::from(Bytes::copy_from_slice(body)),
            )
            .await
            .unwrap();
    }

    // --- LocalVersionedList: sample ---

    #[tokio::test]
    async fn local_sample_reports_unique_keys_and_non_versioned() {
        // Three distinct files → three records, non-versioned. Local
        // trees never surface delete markers or duplicate versions,
        // so `is_versioned` must stay false regardless of tree shape.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", b"alpha");
        write_file(tmp.path(), "nested/b.txt", b"beta");
        write_file(tmp.path(), "nested/c.txt", b"gamma");

        let list = LocalVersionedList::new(tmp.path().to_path_buf());
        let sample = list.sample(1000).await.unwrap();

        assert_eq!(sample.total_versions, 3);
        assert_eq!(sample.unique_keys, 3);
        assert!(!sample.has_delete_markers);
        assert!(!sample.is_versioned());

        let mut keys: Vec<String> = sample.records.iter().map(|r| r.key.clone()).collect();
        keys.sort();
        assert_eq!(keys, vec!["a.txt", "nested/b.txt", "nested/c.txt"]);
    }

    #[tokio::test]
    async fn local_sample_caps_at_limit() {
        // Sample limit must bound the record count even when more
        // files exist — the detect stage passes 1 000 and doesn't
        // want to wait for a million-file walk to finish.
        let tmp = TempDir::new().unwrap();
        for i in 0..10 {
            write_file(tmp.path(), &format!("f{i:02}.bin"), b"x");
        }

        let list = LocalVersionedList::new(tmp.path().to_path_buf());
        let sample = list.sample(3).await.unwrap();

        assert_eq!(sample.records.len(), 3);
        assert_eq!(sample.total_versions, 3);
        assert_eq!(sample.unique_keys, 3);
    }

    // --- LocalVersionedList: enumerate ---

    #[tokio::test]
    async fn local_enumerate_walks_the_whole_tree() {
        // Every file reachable under `root` must appear exactly
        // once in the callback stream. Order is
        // implementation-defined, so the assertion sorts.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", b"1");
        write_file(tmp.path(), "dir/a.txt", b"2");
        write_file(tmp.path(), "dir/sub/b.txt", b"3");

        let list = LocalVersionedList::new(tmp.path().to_path_buf());
        let mut got: Vec<String> = Vec::new();
        list.enumerate(None, None, &mut |rec| {
            got.push(rec.key);
            Ok(())
        })
        .await
        .unwrap();

        got.sort();
        assert_eq!(got, vec!["dir/a.txt", "dir/sub/b.txt", "top.txt"]);
    }

    #[tokio::test]
    async fn local_enumerate_honors_since_and_until_bounds() {
        // Time bounds filter records whose last_modified falls
        // outside the window. The filter is client-side for the
        // local backend, but the invariant matches the cloud
        // backends' contract.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", b"aa");
        write_file(tmp.path(), "b.txt", b"bb");
        let list = LocalVersionedList::new(tmp.path().to_path_buf());

        // Absurdly-high `since`: nothing survives.
        let mut count = 0;
        list.enumerate(Some(i64::MAX), None, &mut |_| {
            count += 1;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(count, 0);

        // Absurdly-low `until`: also nothing (0 sits before epoch
        // for our synthetic case only if mtimes are positive —
        // they are by construction).
        let mut count = 0;
        list.enumerate(None, Some(-1), &mut |_| {
            count += 1;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn local_enumerate_callback_error_short_circuits() {
        // A callback returning `Err(_)` must stop the walk and
        // surface the exact error to the caller — the enumerate
        // stage uses this to propagate cancellation and per-entry
        // failures without draining the remainder of the tree.
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            write_file(tmp.path(), &format!("f{i}.bin"), b"x");
        }

        let list = LocalVersionedList::new(tmp.path().to_path_buf());
        let mut seen = 0;
        let err = list
            .enumerate(None, None, &mut |_| {
                seen += 1;
                if seen >= 2 {
                    Err(CrabError::Cancelled)
                } else {
                    Ok(())
                }
            })
            .await
            .expect_err("callback error must surface");
        assert!(matches!(err, CrabError::Cancelled));
        assert_eq!(seen, 2, "walker should stop on the erroring call");
    }

    // --- LocalVersionedList: enumerate_at ---

    #[tokio::test]
    async fn local_enumerate_at_returns_live_state() {
        // Local filesystems have no history — `enumerate_at`
        // matches the tree's current contents regardless of the
        // requested timestamp.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "x.txt", b"x");
        write_file(tmp.path(), "y.txt", b"y");

        let list = LocalVersionedList::new(tmp.path().to_path_buf());
        let mut keys: Vec<String> = Vec::new();
        list.enumerate_at(1_700_000_000, &mut |rec| {
            keys.push(rec.key);
            Ok(())
        })
        .await
        .unwrap();

        keys.sort();
        assert_eq!(keys, vec!["x.txt", "y.txt"]);
    }

    #[tokio::test]
    async fn local_missing_root_is_not_found() {
        // A root that doesn't exist surfaces as NotFound so callers
        // can distinguish "bad path" from "permissions" / "I/O".
        let list = LocalVersionedList::new(PathBuf::from("/definitely/not/here/crab-test"));
        let err = list.sample(10).await.expect_err("missing root");
        assert!(
            matches!(err, CrabError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn flat_object_store_list_emits_source_prefix_relative_keys() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        seed_object(&store, "data/a.bin", b"a").await;
        seed_object(&store, "data/nested/b.bin", b"b").await;
        seed_object(&store, "data-v2/skip.bin", b"skip").await;

        let list = FlatObjectStoreList::new(store, "data");
        let mut keys = Vec::new();
        list.enumerate(None, None, &mut |record| {
            keys.push(record.key);
            Ok(())
        })
        .await
        .unwrap();

        keys.sort();
        assert_eq!(keys, vec!["a.bin", "nested/b.bin"]);
    }

    #[tokio::test]
    async fn flat_object_store_sample_is_non_versioned_and_capped() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for i in 0..5 {
            seed_object(&store, &format!("source/f{i}.bin"), b"x").await;
        }

        let list = FlatObjectStoreList::new(store, "source/");
        let sample = list.sample(3).await.unwrap();

        assert_eq!(sample.total_versions, 3);
        assert_eq!(sample.unique_keys, 3);
        assert!(!sample.has_delete_markers);
        assert!(!sample.is_versioned());
        assert!(
            sample
                .records
                .iter()
                .all(|record| record.version_id.is_empty())
        );
    }

    // --- VersionSample: detection rule ---

    #[test]
    fn sample_is_versioned_when_duplicates_or_delete_markers() {
        // Detection truth table: either condition flips the bit.
        let flat = VersionSample {
            total_versions: 3,
            unique_keys: 3,
            has_delete_markers: false,
            records: vec![],
        };
        assert!(!flat.is_versioned());

        let with_dupes = VersionSample {
            total_versions: 5,
            unique_keys: 3,
            has_delete_markers: false,
            records: vec![],
        };
        assert!(with_dupes.is_versioned());

        let with_tombstones = VersionSample {
            total_versions: 3,
            unique_keys: 3,
            has_delete_markers: true,
            records: vec![],
        };
        assert!(with_tombstones.is_versioned());
    }

    // Provider-backed history lists.

    #[cfg(not(feature = "tier-s3"))]
    #[tokio::test]
    async fn s3_stub_returns_internal_error() {
        let list = S3VersionedList::new("my-bucket", "prefix/");
        let err = list.sample(10).await.expect_err("stub must error");
        match err {
            CrabError::Internal(msg) => assert!(
                msg.contains("aws-sdk-s3"),
                "expected aws-sdk-s3 hint in message, got {msg:?}"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn object_relative_key_strips_only_directory_prefix() {
        assert_eq!(
            object_relative_key("prefix/data", "prefix/data/file.bin"),
            Some("file.bin".to_owned())
        );
        assert_eq!(
            object_relative_key("prefix/data", "prefix/data-namesake/file.bin"),
            None
        );
        assert_eq!(
            object_relative_key("", "top.bin"),
            Some("top.bin".to_owned())
        );
        assert_eq!(object_relative_key("", ""), None);
    }

    #[cfg(feature = "tier-s3")]
    #[test]
    fn s3_object_version_record_maps_sdk_fields() {
        let version = aws_sdk_s3::types::ObjectVersion::builder()
            .key("prefix/file.bin")
            .version_id("v2")
            .size(42)
            .e_tag("\"abc\"")
            .last_modified(aws_sdk_s3::primitives::DateTime::from_secs(1_700_000_000))
            .build();

        let record = s3_object_version_record("prefix", &version)
            .unwrap()
            .expect("record should be inside prefix");

        assert_eq!(
            record,
            VersionRecord {
                key: "file.bin".to_owned(),
                version_id: "v2".to_owned(),
                size: 42,
                etag: Some("\"abc\"".to_owned()),
                last_modified: 1_700_000_000,
                is_delete_marker: false,
            }
        );
    }

    #[cfg(feature = "tier-s3")]
    #[test]
    fn s3_delete_marker_record_maps_delete_marker() {
        let marker = aws_sdk_s3::types::DeleteMarkerEntry::builder()
            .key("prefix/deleted.bin")
            .version_id("delete-v1")
            .last_modified(aws_sdk_s3::primitives::DateTime::from_secs(1_700_000_010))
            .build();

        let record = s3_delete_marker_record("prefix", &marker)
            .unwrap()
            .expect("record should be inside prefix");

        assert_eq!(
            record,
            VersionRecord {
                key: "deleted.bin".to_owned(),
                version_id: "delete-v1".to_owned(),
                size: 0,
                etag: None,
                last_modified: 1_700_000_010,
                is_delete_marker: true,
            }
        );
    }

    #[test]
    fn snapshot_records_at_uses_latest_visible_non_delete_version() {
        let records = vec![
            VersionRecord {
                key: "a.bin".to_owned(),
                version_id: "a-old".to_owned(),
                size: 1,
                etag: None,
                last_modified: 10,
                is_delete_marker: false,
            },
            VersionRecord {
                key: "a.bin".to_owned(),
                version_id: "a-delete".to_owned(),
                size: 0,
                etag: None,
                last_modified: 20,
                is_delete_marker: true,
            },
            VersionRecord {
                key: "a.bin".to_owned(),
                version_id: "a-future".to_owned(),
                size: 3,
                etag: None,
                last_modified: 30,
                is_delete_marker: false,
            },
            VersionRecord {
                key: "b.bin".to_owned(),
                version_id: "b-live".to_owned(),
                size: 4,
                etag: None,
                last_modified: 15,
                is_delete_marker: false,
            },
        ];

        let snapshot = snapshot_records_at(records, 25);

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, "b.bin");
        assert_eq!(snapshot[0].version_id, "b-live");
    }

    #[cfg(feature = "tier-gcs")]
    #[test]
    fn gcs_object_record_maps_generation_and_delete_time() {
        let object: google_cloud_storage::http::objects::Object =
            serde_json::from_value(serde_json::json!({
                "selfLink": "https://storage.googleapis.com/storage/v1/b/my-bucket/o/prefix%2Ffile.bin",
                "mediaLink": "https://storage.googleapis.com/download/storage/v1/b/my-bucket/o/prefix%2Ffile.bin",
                "bucket": "my-bucket",
                "name": "prefix/file.bin",
                "id": "my-bucket/prefix/file.bin/123",
                "generation": "123",
                "metageneration": "1",
                "size": "42",
                "etag": "etag-value",
                "timeCreated": "2023-11-14T22:13:20Z",
                "timeDeleted": "2023-11-14T22:15:00Z"
            }))
            .unwrap();

        let records = gcs_records_from_object("prefix", &object).unwrap();

        assert_eq!(
            records,
            vec![
                VersionRecord {
                    key: "file.bin".to_owned(),
                    version_id: "123".to_owned(),
                    size: 42,
                    etag: Some("etag-value".to_owned()),
                    last_modified: 1_700_000_000,
                    is_delete_marker: false,
                },
                VersionRecord {
                    key: "file.bin".to_owned(),
                    version_id: "123#delete".to_owned(),
                    size: 0,
                    etag: None,
                    last_modified: 1_700_000_100,
                    is_delete_marker: true,
                },
            ]
        );
    }

    #[cfg(not(feature = "tier-gcs"))]
    #[tokio::test]
    async fn gcs_stub_returns_internal_error() {
        let list = GcsVersionedList::new("my-bucket", "prefix/");
        let err = list
            .enumerate(None, None, &mut |_| Ok(()))
            .await
            .expect_err("stub must error");
        match err {
            CrabError::Internal(msg) => assert!(
                msg.contains("GCS"),
                "expected GCS hint in message, got {msg:?}"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[cfg(feature = "tier-azure")]
    #[test]
    fn azure_blob_record_maps_version_and_deleted_state() {
        let blob: azure_storage_blobs::prelude::Blob = quick_xml::de::from_str(
            r#"
<Blob>
  <Name>prefix/file.bin</Name>
  <VersionId>2023-11-14T22:13:20.0000000Z</VersionId>
  <IsCurrentVersion>false</IsCurrentVersion>
  <Deleted>true</Deleted>
  <Properties>
    <Creation-Time>Tue, 14 Nov 2023 22:13:20 GMT</Creation-Time>
    <Last-Modified>Tue, 14 Nov 2023 22:13:20 GMT</Last-Modified>
    <Etag>0x8DBE55</Etag>
    <Content-Length>42</Content-Length>
    <Content-Type>application/octet-stream</Content-Type>
    <Content-Encoding />
    <Content-Language />
    <Content-Disposition />
    <BlobType>BlockBlob</BlobType>
    <DeletedTime>Tue, 14 Nov 2023 22:15:00 GMT</DeletedTime>
    <ServerEncrypted>true</ServerEncrypted>
  </Properties>
</Blob>
"#,
        )
        .unwrap();

        let records = azure_records_from_blob("prefix", &blob).unwrap();

        assert_eq!(
            records,
            vec![
                VersionRecord {
                    key: "file.bin".to_owned(),
                    version_id: "2023-11-14T22:13:20.0000000Z".to_owned(),
                    size: 42,
                    etag: Some("0x8DBE55".to_owned()),
                    last_modified: 1_700_000_000,
                    is_delete_marker: false,
                },
                VersionRecord {
                    key: "file.bin".to_owned(),
                    version_id: "2023-11-14T22:13:20.0000000Z#delete".to_owned(),
                    size: 0,
                    etag: Some("0x8DBE55".to_owned()),
                    last_modified: 1_700_000_100,
                    is_delete_marker: true,
                },
            ]
        );
    }

    #[cfg(not(feature = "tier-azure"))]
    #[tokio::test]
    async fn azure_stub_returns_internal_error() {
        let list = AzureVersionedList::new("my-account", "my-container", "prefix/");
        let err = list
            .enumerate_at(0, &mut |_| Ok(()))
            .await
            .expect_err("stub must error");
        match err {
            CrabError::Internal(msg) => assert!(
                msg.contains("Azure"),
                "expected Azure hint in message, got {msg:?}"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    // --- VersionedListImpl dispatch ---

    #[tokio::test]
    async fn versioned_list_impl_dispatches_to_local() {
        // The enum wrapper should forward to the Local variant
        // without introducing its own sampling logic.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "only.txt", b"hi");

        let wrapped: VersionedListImpl =
            VersionedListImpl::Local(LocalVersionedList::new(tmp.path().to_path_buf()));
        let sample = wrapped.sample(10).await.unwrap();
        assert_eq!(sample.records.len(), 1);
        assert_eq!(sample.records[0].key, "only.txt");
    }

    #[cfg(not(feature = "tier-s3"))]
    #[tokio::test]
    async fn versioned_list_impl_dispatches_to_s3_stub() {
        let wrapped: VersionedListImpl = VersionedListImpl::S3(S3VersionedList::new("b", ""));
        let err = wrapped.sample(10).await.expect_err("stub must error");
        assert!(matches!(err, CrabError::Internal(_)));
    }
}
