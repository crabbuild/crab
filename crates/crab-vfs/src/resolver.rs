//! VFS resolver: merges the base snapshot with the overlay layer.
//!
//! The resolver is the single source of truth for path lookups in the
//! mounted filesystem. It checks the overlay first (local writes take
//! precedence), then falls back to the base snapshot. This mirrors
//! artifact-fs's `Resolver` pattern.
//!
//! The overlay is accessed through the [`OverlayLookup`] trait so the
//! resolver can compile and be tested before the real `OverlayStore`
//! (task 51) lands.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use tracing::{debug, trace};

use crate::core::error::{CrabError, Result};
use crate::snapshot::{BaseNode, NodeType, SnapshotStore};

// ---------------------------------------------------------------------------
// Overlay abstraction (placeholder until task 51 lands)
// ---------------------------------------------------------------------------

/// The kind of mutation tracked by the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Create,
    Modify,
    Delete,
    Rename,
    Mkdir,
    Symlink,
}

/// A single entry in the overlay store.
///
/// This is the minimal shape needed by the resolver. The real
/// `OverlayStore` (task 51) will own the canonical definition and
/// implement [`OverlayLookup`].
#[derive(Debug, Clone)]
pub struct OverlayEntry {
    pub path: String,
    pub kind: OverlayKind,
    pub mode: u32,
    pub size: u64,
    pub mtime_ns: i64,
    pub node_type: NodeType,
}

impl OverlayEntry {
    /// Whether this entry represents a deletion marker.
    pub fn is_deleted(&self) -> bool {
        self.kind == OverlayKind::Delete
    }
}

/// Trait for looking up overlay entries.
///
/// The real `OverlayStore` will implement this. For now the resolver
/// accepts `Option<Arc<dyn OverlayLookup>>` so it works without an
/// overlay at all.
pub trait OverlayLookup: Send + Sync {
    /// Look up a single path in the overlay.
    fn get(&self, path: &str) -> Option<OverlayEntry>;

    /// List overlay entries whose paths are immediate children of
    /// `parent_path`, or have `parent_path` as a prefix (for deletion
    /// filtering in `readdir`).
    fn list_by_prefix(&self, parent_path: &str) -> Vec<OverlayEntry>;

    /// List sorted immediate children after `after_name`.
    fn list_children_page(
        &self,
        parent_path: &str,
        after_name: Option<&str>,
        limit: usize,
    ) -> Vec<OverlayEntry> {
        let mut entries = self
            .list_by_prefix(parent_path)
            .into_iter()
            .filter(|entry| {
                let name = immediate_child_name(parent_path, &entry.path);
                let child_path = if parent_path.is_empty() {
                    name.clone()
                } else {
                    format!("{parent_path}/{name}")
                };
                entry.path == child_path
                    && after_name.is_none_or(|after_name| name.as_str() > after_name)
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        entries.truncate(limit);
        entries
    }

    /// Original base snapshot path for a metadata-only overlay entry.
    fn base_path(&self, _path: &str) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// ResolvedNode
// ---------------------------------------------------------------------------

/// The result of resolving a path: either a base snapshot node or an
/// overlay entry.
#[derive(Debug, Clone)]
pub enum ResolvedNode {
    Base(BaseNode),
    Overlay(OverlayEntry),
}

impl ResolvedNode {
    /// Normalized Unix file mode.
    pub fn mode(&self) -> u32 {
        match self {
            Self::Base(n) => normalize_mode(n.mode, n.node_type),
            Self::Overlay(e) => normalize_overlay_mode(e.mode, e.node_type),
        }
    }

    /// File size in bytes.
    pub fn size(&self) -> u64 {
        match self {
            Self::Base(n) => n.size,
            Self::Overlay(e) => e.size,
        }
    }

    /// Node type (file, dir, symlink).
    pub fn node_type(&self) -> NodeType {
        match self {
            Self::Base(n) => n.node_type,
            Self::Overlay(e) => e.node_type,
        }
    }

    /// Path relative to the repository root.
    pub fn path(&self) -> &str {
        match self {
            Self::Base(n) => &n.path,
            Self::Overlay(e) => &e.path,
        }
    }

    /// Whether this node came from the overlay.
    pub fn is_overlay(&self) -> bool {
        matches!(self, Self::Overlay(_))
    }
}

// ---------------------------------------------------------------------------
// Mode normalization
// ---------------------------------------------------------------------------

/// Ensure sane permission bits on a file mode.
///
/// Git tree entries for directories have mode `0o040000` which has zero
/// permission bits after masking. Directories need at least `0o755`,
/// regular files at least `0o644`, and symlinks get `0o120000`.
pub fn normalize_mode(mode: u32, node_type: NodeType) -> u32 {
    let perms = mode & 0o777;
    match node_type {
        NodeType::Dir => {
            if perms == 0 {
                0o040_755
            } else {
                // Ensure at least read+execute for owner and group.
                let fixed = perms | 0o755;
                (mode & !0o777) | fixed
            }
        }
        NodeType::Symlink => {
            // Symlinks are always 0o120777 on most systems.
            0o120_777
        }
        NodeType::File => {
            if perms == 0 {
                0o100_644
            } else {
                // Ensure at least read for owner and group.
                let fixed = perms | 0o644;
                (mode & !0o777) | fixed
            }
        }
    }
}

fn normalize_overlay_mode(mode: u32, node_type: NodeType) -> u32 {
    let perms = mode & 0o7777;
    match node_type {
        NodeType::Dir => 0o040_000 | perms,
        NodeType::File => 0o100_000 | perms,
        NodeType::Symlink => 0o120_777,
    }
}

// ---------------------------------------------------------------------------
// Readdir entry
// ---------------------------------------------------------------------------

/// A single directory entry returned by [`FuseResolver::readdir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaddirEntry {
    pub name: String,
    pub node_type: NodeType,
}

// ---------------------------------------------------------------------------
// FuseResolver
// ---------------------------------------------------------------------------

/// Merges the base snapshot with the overlay to present a unified view
/// of the filesystem tree.
pub struct FuseResolver {
    snapshot: Arc<SnapshotStore>,
    overlay: Option<Arc<dyn OverlayLookup>>,
    generation: AtomicI64,
    commit_time: AtomicI64,
}

impl FuseResolver {
    /// Create a new resolver.
    ///
    /// `overlay` may be `None` for a read-only mount (before the overlay
    /// store is wired in).
    pub fn new(
        snapshot: Arc<SnapshotStore>,
        overlay: Option<Arc<dyn OverlayLookup>>,
        generation: i64,
        commit_time: i64,
    ) -> Self {
        Self {
            snapshot,
            overlay,
            generation: AtomicI64::new(generation),
            commit_time: AtomicI64::new(commit_time),
        }
    }

    /// Current snapshot generation.
    pub fn generation(&self) -> i64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Atomically swap the generation (used by the refresh loop).
    pub fn set_generation(&self, generation: i64) {
        self.generation.store(generation, Ordering::Release);
    }

    /// HEAD commit timestamp (unix seconds) used as mtime for base files.
    pub fn commit_time(&self) -> i64 {
        self.commit_time.load(Ordering::Acquire)
    }

    /// Update the commit timestamp (used by the refresh loop).
    pub fn set_commit_time(&self, ts: i64) {
        self.commit_time.store(ts, Ordering::Release);
    }

    /// Resolve a path to either an overlay entry or a base node.
    ///
    /// Overlay takes precedence. Overlay deletion markers produce
    /// `NotFound`.
    pub fn resolve_path(&self, path: &str) -> Result<ResolvedNode> {
        let path = clean_path(path);

        // 1. Check overlay first (local writes take precedence).
        if let Some(ref ov) = self.overlay
            && let Some(entry) = ov.get(&path)
        {
            if entry.is_deleted() {
                trace!(path = %path, "overlay deletion marker");
                return Err(CrabError::NotFound {
                    path: path.into_owned(),
                });
            }
            if let Some(base_path) = ov.base_path(&path) {
                let current_gen = self.generation();
                let Some(base) = self.snapshot.get_node(current_gen, &base_path)? else {
                    return Err(CrabError::NotFound {
                        path: path.into_owned(),
                    });
                };
                trace!(path = %path, base_path, "resolved from moved base");
                return Ok(ResolvedNode::Base(moved_base_node(base, &path, &entry)));
            }
            trace!(path = %path, "resolved from overlay");
            return Ok(ResolvedNode::Overlay(entry));
        }

        // 2. Fall back to base snapshot.
        let current_gen = self.generation();
        if let Some(node) = self.snapshot.get_node(current_gen, &path)? {
            trace!(path = %path, generation = current_gen, "resolved from snapshot");
            return Ok(ResolvedNode::Base(node));
        }

        Err(CrabError::NotFound {
            path: path.into_owned(),
        })
    }

    /// Get file attributes for a path.
    ///
    /// Returns `(mode, size, node_type, mtime_unix_secs)`.
    pub fn getattr(&self, path: &str) -> Result<(u32, u64, NodeType, i64)> {
        let node = self.resolve_path(path)?;
        match &node {
            ResolvedNode::Overlay(e) => {
                let mode = normalize_overlay_mode(e.mode, e.node_type);
                // Overlay entries carry their own mtime.
                let mtime = e.mtime_ns / 1_000_000_000;
                Ok((mode, e.size, e.node_type, mtime))
            }
            ResolvedNode::Base(n) => {
                let mode = normalize_mode(n.mode, n.node_type);
                // Base files use the HEAD commit timestamp for stable mtime.
                let ct = self.commit_time();
                let mtime = if ct != 0 { ct } else { self.generation() };
                Ok((mode, n.size, n.node_type, mtime))
            }
        }
    }

    /// List directory entries, merging snapshot children with overlay
    /// mutations.
    ///
    /// Overlay deletions hide base entries. Overlay creates appear
    /// alongside base entries. Results are sorted by name.
    pub fn readdir(&self, path: &str) -> Result<Vec<ReaddirEntry>> {
        const PAGE_SIZE: usize = 512;

        let mut entries = Vec::new();
        let mut after_name = None;
        loop {
            let page = self.readdir_page(path, after_name.as_deref(), PAGE_SIZE)?;
            let page_len = page.len();
            after_name = page.last().map(|entry| entry.name.clone());
            entries.extend(page);
            if page_len < PAGE_SIZE {
                break;
            }
        }
        debug!(path, count = entries.len(), "readdir");
        Ok(entries)
    }

    /// List one sorted directory page after `after_name`.
    pub fn readdir_page(
        &self,
        path: &str,
        after_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReaddirEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let path = clean_path(path);
        let current_gen = self.generation();
        let batch_size = limit.max(64);
        let mut cursor = after_name.map(str::to_owned);
        let mut result = Vec::with_capacity(limit);

        loop {
            let base = self.snapshot.list_children_page(
                current_gen,
                &path,
                cursor.as_deref(),
                batch_size,
            )?;
            let overlay = self.overlay.as_ref().map_or_else(Vec::new, |overlay| {
                overlay.list_children_page(&path, cursor.as_deref(), batch_size)
            });
            if base.is_empty() && overlay.is_empty() {
                break;
            }

            let base_last = base.last().map(|entry| child_name_from_path(&entry.path));
            let overlay_last = overlay
                .last()
                .map(|entry| child_name_from_path(&entry.path));
            let base_exhausted = base.len() < batch_size;
            let overlay_exhausted = overlay.len() < batch_size;
            let frontier = match (base_exhausted, overlay_exhausted, base_last, overlay_last) {
                (true, true, Some(base), Some(overlay)) => base.max(overlay),
                (true, true, Some(last), None) | (true, true, None, Some(last)) => last,
                (false, true, Some(last), _) | (true, false, _, Some(last)) => last,
                (false, false, Some(base), Some(overlay)) => base.min(overlay),
                _ => break,
            };

            let mut merged = BTreeMap::new();
            for child in base {
                let name = child_name_from_path(&child.path);
                if name <= frontier && !is_macos_appledouble_name(&name) {
                    merged.insert(name, Some(child.node_type));
                }
            }
            for child in overlay {
                let name = child_name_from_path(&child.path);
                if name <= frontier && !is_macos_appledouble_name(&name) {
                    merged.insert(name, (!child.is_deleted()).then_some(child.node_type));
                }
            }
            for (name, node_type) in merged {
                if let Some(node_type) = node_type {
                    result.push(ReaddirEntry { name, node_type });
                    if result.len() == limit {
                        return Ok(result);
                    }
                }
            }

            cursor = Some(frontier);
            if base_exhausted && overlay_exhausted {
                break;
            }
        }

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize a path: strip leading `/`, collapse `//`, trim trailing `/`.
/// Returns a `Cow` to avoid allocation when the path is already clean
/// (the common case on the FUSE hot path).
fn clean_path(path: &str) -> Cow<'_, str> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Cow::Borrowed("");
    }
    // If trimming didn't change the string, return a borrow (zero alloc).
    if trimmed.len() == path.len() {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(trimmed.to_owned())
    }
}

/// Extract the filename component from a full path.
fn child_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Given a parent path and a child's full path, extract the immediate
/// child name (one level deep).
fn immediate_child_name(parent: &str, child_path: &str) -> String {
    let prefix = if parent.is_empty() {
        String::new()
    } else {
        format!("{parent}/")
    };

    let Some(rel) = child_path.strip_prefix(&prefix) else {
        return String::new();
    };

    if rel.is_empty() {
        return String::new();
    }

    // Take only the first path component.
    rel.split('/').next().unwrap_or_default().to_owned()
}

fn is_macos_appledouble_name(name: &str) -> bool {
    name.len() > 2 && name.starts_with("._")
}

fn moved_base_node(mut base: BaseNode, path: &str, entry: &OverlayEntry) -> BaseNode {
    path.clone_into(&mut base.path);
    base.mode = entry.mode;
    base.size = entry.size;
    base.node_type = entry.node_type;
    base
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_mode tests ---

    #[test]
    fn dir_with_zero_perms_gets_755() {
        // Git stores directories as 0o040000 — zero permission bits.
        let mode = normalize_mode(0o040_000, NodeType::Dir);
        assert_eq!(mode, 0o040_755);
    }

    #[test]
    fn dir_with_existing_perms_gets_at_least_755() {
        let mode = normalize_mode(0o040_700, NodeType::Dir);
        // 0o700 | 0o755 = 0o755
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn file_with_zero_perms_gets_644() {
        let mode = normalize_mode(0o100_000, NodeType::File);
        assert_eq!(mode, 0o100_644);
    }

    #[test]
    fn file_with_existing_perms_gets_at_least_644() {
        let mode = normalize_mode(0o100_755, NodeType::File);
        // 0o755 | 0o644 = 0o755
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn executable_file_preserves_exec_bit() {
        let mode = normalize_mode(0o100_755, NodeType::File);
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn overlay_file_preserves_exact_permission_bits() {
        let base = vec![];
        let overlay = vec![OverlayEntry {
            path: "private.txt".to_owned(),
            kind: OverlayKind::Modify,
            mode: 0o100600,
            size: 1,
            mtime_ns: 1_800_000_000_000_000_000,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&base, overlay);
        let (mode, _size, _node_type, _mtime) = resolver.getattr("private.txt").unwrap();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn symlink_always_gets_120777() {
        let mode = normalize_mode(0o120_000, NodeType::Symlink);
        assert_eq!(mode, 0o120_777);
    }

    // --- clean_path tests ---

    #[test]
    fn clean_path_strips_slashes() {
        assert_eq!(clean_path("/foo/bar/"), "foo/bar");
        assert_eq!(clean_path("/"), "");
        assert_eq!(clean_path("."), "");
        assert_eq!(clean_path(""), "");
        assert_eq!(clean_path("src/main.rs"), "src/main.rs");
    }

    // --- immediate_child_name tests ---

    #[test]
    fn immediate_child_at_root() {
        assert_eq!(immediate_child_name("", "src"), "src");
        assert_eq!(immediate_child_name("", "src/main.rs"), "src");
    }

    #[test]
    fn immediate_child_in_subdir() {
        assert_eq!(immediate_child_name("src", "src/main.rs"), "main.rs");
        assert_eq!(immediate_child_name("src", "src/utils/helper.rs"), "utils");
    }

    #[test]
    fn immediate_child_no_match() {
        assert_eq!(immediate_child_name("docs", "src/main.rs"), "");
    }

    // --- ResolvedNode accessors ---

    #[test]
    fn resolved_node_base_accessors() {
        let node = ResolvedNode::Base(BaseNode {
            path: "src/main.rs".to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: Some("abc123".to_owned()),
            pointer: None,
            size: 4096,
        });
        assert_eq!(node.size(), 4096);
        assert_eq!(node.node_type(), NodeType::File);
        assert_eq!(node.path(), "src/main.rs");
        assert!(!node.is_overlay());
    }

    #[test]
    fn resolved_node_overlay_accessors() {
        let node = ResolvedNode::Overlay(OverlayEntry {
            path: "new_file.txt".to_owned(),
            kind: OverlayKind::Create,
            mode: 0o100644,
            size: 100,
            mtime_ns: 1_700_000_000_000_000_000,
            node_type: NodeType::File,
        });
        assert_eq!(node.size(), 100);
        assert_eq!(node.node_type(), NodeType::File);
        assert_eq!(node.path(), "new_file.txt");
        assert!(node.is_overlay());
    }

    // --- FuseResolver with no overlay ---

    fn make_file_node(path: &str, size: u64) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: Some("abcd1234".to_owned()),
            pointer: None,
            size,
        }
    }

    fn make_dir_node(path: &str) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::Dir,
            mode: 0o040000,
            object_oid: None,
            pointer: None,
            size: 0,
        }
    }

    fn temp_resolver(nodes: &[BaseNode]) -> (tempfile::TempDir, FuseResolver) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("snapshot.sqlite");
        let store = SnapshotStore::open_or_create(&db_path).unwrap();
        store
            .publish_generation("aabbccdd", "refs/heads/main", nodes)
            .unwrap();
        let resolver = FuseResolver::new(
            Arc::new(store),
            None, // no overlay
            1,
            1_700_000_000, // commit time
        );
        (dir, resolver)
    }

    #[test]
    fn resolve_existing_file() {
        let (_dir, resolver) = temp_resolver(&[make_file_node("README.md", 50)]);
        let node = resolver.resolve_path("README.md").unwrap();
        assert_eq!(node.size(), 50);
        assert!(!node.is_overlay());
    }

    #[test]
    fn resolve_missing_file_returns_not_found() {
        let (_dir, resolver) = temp_resolver(&[make_file_node("README.md", 50)]);
        let err = resolver.resolve_path("nonexistent").unwrap_err();
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[test]
    fn resolve_strips_leading_slash() {
        let (_dir, resolver) = temp_resolver(&[make_file_node("src/main.rs", 100)]);
        let node = resolver.resolve_path("/src/main.rs").unwrap();
        assert_eq!(node.size(), 100);
    }

    #[test]
    fn getattr_base_file_uses_commit_time() {
        let (_dir, resolver) = temp_resolver(&[make_file_node("a.txt", 42)]);
        let (mode, size, node_type, mtime) = resolver.getattr("a.txt").unwrap();
        assert_eq!(size, 42);
        assert_eq!(node_type, NodeType::File);
        assert_eq!(mtime, 1_700_000_000);
        // File mode should be normalized.
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn getattr_preserves_unknown_blob_size_for_exact_lookup() {
        let (_dir, resolver) = temp_resolver(&[BaseNode {
            path: "a.txt".to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: Some("abcd1234".to_owned()),
            pointer: None,
            size: 0,
        }]);
        let (_mode, size, node_type, _mtime) = resolver.getattr("a.txt").unwrap();
        assert_eq!(size, 0);
        assert_eq!(node_type, NodeType::File);
    }

    #[test]
    fn readdir_returns_immediate_children() {
        let nodes = vec![
            make_dir_node("src"),
            make_file_node("src/main.rs", 100),
            make_file_node("src/lib.rs", 200),
            make_file_node("README.md", 50),
        ];
        let (_dir, resolver) = temp_resolver(&nodes);

        // Root listing.
        let root = resolver.readdir("").unwrap();
        let names: Vec<&str> = root.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"src"));
        assert!(names.contains(&"README.md"));
        assert!(!names.contains(&"main.rs"));

        // Subdir listing.
        let src = resolver.readdir("src").unwrap();
        let names: Vec<&str> = src.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"lib.rs"));
    }

    #[test]
    fn readdir_hides_macos_appledouble_sidecars() {
        let nodes = vec![
            make_file_node("visible.txt", 10),
            make_file_node("._visible.txt", 10),
        ];
        let overlay = vec![OverlayEntry {
            path: "._overlay.txt".to_owned(),
            kind: OverlayKind::Create,
            mode: 0o100644,
            size: 4,
            mtime_ns: 1_700_000_000_000_000_000,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&nodes, overlay);

        let entries = resolver.readdir("").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"visible.txt"));
        assert!(!names.contains(&"._visible.txt"));
        assert!(!names.contains(&"._overlay.txt"));
    }

    #[test]
    fn readdir_page_advances_across_overlay_deletion_runs() {
        let nodes = (0..130)
            .map(|index| make_file_node(&format!("file-{index:03}.bin"), 1))
            .collect::<Vec<_>>();
        let mut overlay = (0..80)
            .map(|index| OverlayEntry {
                path: format!("file-{index:03}.bin"),
                kind: OverlayKind::Delete,
                mode: 0o100644,
                size: 0,
                mtime_ns: 0,
                node_type: NodeType::File,
            })
            .collect::<Vec<_>>();
        overlay.push(OverlayEntry {
            path: "new.bin".to_owned(),
            kind: OverlayKind::Create,
            mode: 0o100644,
            size: 1,
            mtime_ns: 0,
            node_type: NodeType::File,
        });
        let (_dir, resolver) = temp_resolver_with_overlay(&nodes, overlay);
        let mut names = Vec::new();
        let mut after_name = None;

        loop {
            let page = resolver
                .readdir_page("", after_name.as_deref(), 32)
                .unwrap();
            let count = page.len();
            after_name = page.last().map(|entry| entry.name.clone());
            names.extend(page.into_iter().map(|entry| entry.name));
            if count < 32 {
                break;
            }
        }

        assert_eq!(names.len(), 51);
        assert_eq!(names.first().map(String::as_str), Some("file-080.bin"));
        assert_eq!(names.last().map(String::as_str), Some("new.bin"));
    }

    // --- FuseResolver with overlay ---

    /// A simple in-memory overlay for testing.
    struct TestOverlay {
        entries: Vec<OverlayEntry>,
    }

    impl OverlayLookup for TestOverlay {
        fn get(&self, path: &str) -> Option<OverlayEntry> {
            self.entries.iter().find(|e| e.path == path).cloned()
        }

        fn list_by_prefix(&self, parent_path: &str) -> Vec<OverlayEntry> {
            let prefix = if parent_path.is_empty() {
                String::new()
            } else {
                format!("{parent_path}/")
            };
            self.entries
                .iter()
                .filter(|e| {
                    if parent_path.is_empty() {
                        // Root: any entry that doesn't have a parent is a child,
                        // or any entry at all is under root.
                        true
                    } else {
                        e.path.starts_with(&prefix)
                    }
                })
                .cloned()
                .collect()
        }
    }

    fn temp_resolver_with_overlay(
        nodes: &[BaseNode],
        overlay_entries: Vec<OverlayEntry>,
    ) -> (tempfile::TempDir, FuseResolver) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("snapshot.sqlite");
        let store = SnapshotStore::open_or_create(&db_path).unwrap();
        store
            .publish_generation("aabbccdd", "refs/heads/main", nodes)
            .unwrap();
        let overlay = Arc::new(TestOverlay {
            entries: overlay_entries,
        });
        let resolver = FuseResolver::new(Arc::new(store), Some(overlay), 1, 1_700_000_000);
        (dir, resolver)
    }

    #[test]
    fn overlay_takes_precedence_over_base() {
        let base = vec![make_file_node("a.txt", 100)];
        let overlay = vec![OverlayEntry {
            path: "a.txt".to_owned(),
            kind: OverlayKind::Modify,
            mode: 0o100644,
            size: 999,
            mtime_ns: 1_700_000_000_000_000_000,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&base, overlay);
        let node = resolver.resolve_path("a.txt").unwrap();
        assert!(node.is_overlay());
        assert_eq!(node.size(), 999);
    }

    #[test]
    fn overlay_deletion_hides_base() {
        let base = vec![make_file_node("a.txt", 100)];
        let overlay = vec![OverlayEntry {
            path: "a.txt".to_owned(),
            kind: OverlayKind::Delete,
            mode: 0,
            size: 0,
            mtime_ns: 0,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&base, overlay);
        let err = resolver.resolve_path("a.txt").unwrap_err();
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[test]
    fn overlay_create_appears_in_readdir() {
        let base = vec![make_file_node("a.txt", 100)];
        let overlay = vec![OverlayEntry {
            path: "new.txt".to_owned(),
            kind: OverlayKind::Create,
            mode: 0o100644,
            size: 50,
            mtime_ns: 1_700_000_000_000_000_000,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&base, overlay);
        let entries = resolver.readdir("").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"new.txt"));
    }

    #[test]
    fn overlay_deletion_hides_in_readdir() {
        let base = vec![make_file_node("a.txt", 100), make_file_node("b.txt", 200)];
        let overlay = vec![OverlayEntry {
            path: "a.txt".to_owned(),
            kind: OverlayKind::Delete,
            mode: 0,
            size: 0,
            mtime_ns: 0,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&base, overlay);
        let entries = resolver.readdir("").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[test]
    fn getattr_overlay_uses_overlay_mtime() {
        let base = vec![make_file_node("a.txt", 100)];
        let overlay = vec![OverlayEntry {
            path: "a.txt".to_owned(),
            kind: OverlayKind::Modify,
            mode: 0o100755,
            size: 999,
            mtime_ns: 1_800_000_000_000_000_000,
            node_type: NodeType::File,
        }];
        let (_dir, resolver) = temp_resolver_with_overlay(&base, overlay);
        let (mode, size, _node_type, mtime) = resolver.getattr("a.txt").unwrap();
        assert_eq!(size, 999);
        assert_eq!(mtime, 1_800_000_000);
        // 0o755 | 0o644 = 0o755
        assert_eq!(mode & 0o777, 0o755);
    }
}
