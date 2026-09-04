//! Non-mutating, bounded-detail inspection shared by product diagnostics.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::catalog::classify_family;
use crate::private_fs::{EntryStat, PinnedRoot, check_cancelled, run_blocking};
use crate::{CacheCatalog, CacheCatalogStats, CacheError, Result};

const MAX_ISSUES: usize = 64;
const FAMILIES: &[&str] = &[
    "chunk",
    "decoded-range",
    "xorb",
    "shard",
    "manifest",
    "stage",
    "chunk-index",
    "xorb-index",
    "bloom",
    "shard-hint",
    "catalog",
    "lock",
    "temporary",
    "other",
    "directory",
];

/// Observed linked entries, separating logical file lengths from allocated blocks.
#[derive(Debug, Default, Serialize)]
pub struct CacheUsage {
    pub files: u64,
    pub directories: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

impl CacheUsage {
    fn add(&mut self, entry: EntryStat) -> Result<()> {
        let add = |a: u64, b| {
            a.checked_add(b)
                .ok_or_else(|| CacheError::Internal("cache inspection total overflow".into()))
        };
        if entry.is_directory {
            self.directories = add(self.directories, 1)?;
        } else {
            self.files = add(self.files, 1)?;
            self.logical_bytes = add(self.logical_bytes, entry.file.size)?;
        }
        self.allocated_bytes = add(self.allocated_bytes, entry.allocated_bytes)?;
        Ok(())
    }
}

/// Per-family observed usage; incomplete scans retain their measured lower bounds.
#[derive(Debug, Serialize)]
pub struct CacheFamilyHealth {
    #[serde(flatten)]
    pub usage: CacheUsage,
    pub complete: bool,
    pub issues: u64,
}

impl Default for CacheFamilyHealth {
    fn default() -> Self {
        Self {
            usage: CacheUsage::default(),
            complete: true,
            issues: 0,
        }
    }
}

/// Presence and readability of the configured root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRootState {
    Missing,
    Present,
    Unavailable,
}

/// Catalog row totals are separate from the live filesystem inventory.
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CacheCatalogHealth {
    Missing,
    Readable { stats: CacheCatalogStats },
    Unavailable,
}

/// Classification for actionable inspection failures, not a repair authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheIssueKind {
    UnsafePath,
    Busy,
    Corrupt,
    Io,
    Unavailable,
}

/// A bounded diagnostic detail retaining the original typed failure.
#[derive(Debug, Serialize)]
pub struct CacheHealthIssue {
    pub family: Option<&'static str>,
    pub path: String,
    pub kind: CacheIssueKind,
    #[serde(serialize_with = "serialize_error")]
    pub error: CacheError,
}

fn serialize_error<S: serde::Serializer>(
    error: &CacheError,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(error)
}

fn serialize_path<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(&path.display())
}

/// A read-only cache report; filesystem counters are not an atomic snapshot or integrity proof.
#[derive(Debug, Serialize)]
pub struct CacheHealthReport {
    #[serde(serialize_with = "serialize_path")]
    pub root: PathBuf,
    pub root_state: CacheRootState,
    /// Configured disk-retention cap; `None` means unlimited, not unknown.
    pub budget_bytes: Option<u64>,
    pub observed: CacheUsage,
    pub scan_complete: bool,
    pub over_budget: Option<bool>,
    pub families: BTreeMap<&'static str, CacheFamilyHealth>,
    pub catalog: CacheCatalogHealth,
    pub issues: Vec<CacheHealthIssue>,
    pub omitted_issues: u64,
}

impl CacheHealthReport {
    fn new(root: PathBuf, budget_bytes: Option<u64>) -> Self {
        Self {
            root,
            root_state: CacheRootState::Missing,
            budget_bytes,
            observed: CacheUsage::default(),
            scan_complete: true,
            over_budget: Some(false),
            families: FAMILIES
                .iter()
                .map(|family| (*family, CacheFamilyHealth::default()))
                .collect(),
            catalog: CacheCatalogHealth::Missing,
            issues: Vec::new(),
            omitted_issues: 0,
        }
    }

    /// Whether every attempted inspection was available; payload integrity is not checked.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.issues.is_empty() && self.omitted_issues == 0
    }

    fn issue(
        &mut self,
        relative: &Path,
        family: Option<&'static str>,
        error: CacheError,
        scan_failure: bool,
    ) -> Result<()> {
        if matches!(error, CacheError::Cancelled) {
            return Err(error);
        }
        let kind = match &error {
            CacheError::UnsafeRoot { .. } => CacheIssueKind::UnsafePath,
            CacheError::Io(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                CacheIssueKind::Busy
            }
            CacheError::Io(_) => CacheIssueKind::Io,
            CacheError::CorruptObject { .. } | CacheError::HashMismatch { .. } => {
                CacheIssueKind::Corrupt
            }
            CacheError::Index { source, .. } => match source.sqlite_error_code() {
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                    CacheIssueKind::Busy
                }
                Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
                    CacheIssueKind::Corrupt
                }
                _ => CacheIssueKind::Unavailable,
            },
            _ => CacheIssueKind::Unavailable,
        };
        if scan_failure {
            self.scan_complete = false;
        }
        // Raw chunks and range-key directories share three path components.
        // An unreadable entry there can hide either representation.
        // Mark every affected family incomplete without hiding unrelated rows.
        let shared_chunks = relative == Path::new("chunks")
            || (relative.starts_with("chunks") && relative.components().count() <= 3);
        for (name, stats) in &mut self.families {
            if family.is_none()
                || family == Some(*name)
                || (shared_chunks && matches!(*name, "chunk" | "decoded-range"))
                || (scan_failure && matches!(*name, "directory" | "temporary"))
                || (scan_failure
                    && family == Some("other")
                    && matches!(*name, "bloom" | "shard-hint"))
            {
                stats.issues = stats.issues.saturating_add(1);
                stats.complete &= !scan_failure;
            }
        }
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(CacheHealthIssue {
                family,
                path: relative.display().to_string(),
                kind,
                error,
            });
        } else {
            self.omitted_issues = self.omitted_issues.saturating_add(1);
        }
        Ok(())
    }
}

/// Inspect all linked entries and catalog totals without creating or repairing state.
///
/// Unsafe entries are reported without following links. Errors in one subtree
/// do not hide other families. Allocation includes directory blocks but not
/// unlinked open files; catalog reservations are reported separately. No payload
/// bodies or unrelated indexes are opened. The catalog is read as one snapshot,
/// and the shard-hint database receives bounded-detail schema and SQLite checks.
pub async fn inspect_cache(
    root: &Path,
    budget_bytes: impl Into<Option<u64>>,
    cancel: &CancellationToken,
) -> Result<CacheHealthReport> {
    let root = root.to_owned();
    let budget_bytes = budget_bytes.into();
    run_blocking(cancel, move |cancel| inspect(&root, budget_bytes, cancel)).await
}

fn inspect(
    root: &Path,
    budget_bytes: impl Into<Option<u64>>,
    cancel: &CancellationToken,
) -> Result<CacheHealthReport> {
    let budget_bytes = budget_bytes.into();
    let mut report = CacheHealthReport::new(root.to_owned(), budget_bytes);
    let pinned = match PinnedRoot::open(root) {
        Ok(pinned) => pinned,
        Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(report);
        }
        Err(error) => {
            report.root_state = CacheRootState::Unavailable;
            report.catalog = CacheCatalogHealth::Unavailable;
            report.issue(Path::new(""), None, error, true)?;
            report.over_budget = budget_bytes.is_none().then_some(false);
            return Ok(report);
        }
    };
    report.root_state = CacheRootState::Present;
    pinned.inspect_entries(&mut |path, entry| {
        check_cancelled(cancel)?;
        let family = if path.as_os_str().is_empty() {
            None
        } else {
            // Display-only classification preserves known ASCII ancestors
            // of non-UTF-8 leaves; this string never authorizes file access.
            Some(classify_family(&path.to_string_lossy()))
        };
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return report.issue(path, family, error, true),
        };
        let family = if entry.is_directory {
            "directory"
        } else {
            family.unwrap_or("other")
        };
        report.observed.add(entry)?;
        report
            .families
            .get_mut(family)
            .ok_or_else(|| CacheError::Internal("unregistered cache family".into()))?
            .usage
            .add(entry)
    })?;
    check_cancelled(cancel)?;
    // Reuse the scan's pinned root. Re-resolving the pathname here could read
    // the catalog from a replacement tree and mix two unrelated cache roots.
    report.catalog = match CacheCatalog::read_only_stats_at(&pinned, root) {
        Ok(Some(stats)) => CacheCatalogHealth::Readable { stats },
        Ok(None) => CacheCatalogHealth::Missing,
        Err(error) => {
            report.issue(Path::new(".catalog.sqlite"), Some("catalog"), error, false)?;
            CacheCatalogHealth::Unavailable
        }
    };
    check_cancelled(cancel)?;
    #[cfg(feature = "local-cache")]
    if let Err(error) = crate::shard_hints::inspect_database_at(&pinned, root, cancel) {
        report.issue(
            Path::new(crate::shard_hints::SHARD_HINTS_DATABASE),
            Some("shard-hint"),
            error,
            false,
        )?;
    }
    check_cancelled(cancel)?;
    report.over_budget = if budget_bytes.is_some_and(|max| report.observed.allocated_bytes > max) {
        Some(true)
    } else if budget_bytes.is_none() || report.scan_complete {
        Some(false)
    } else {
        None
    };
    Ok(report)
}

#[cfg(all(test, unix))]
mod tests;
