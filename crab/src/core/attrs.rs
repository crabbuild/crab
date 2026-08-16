//! Single `.gitattributes` reader wrapping `gix_attributes::Search`.
//!
//! This module is the canonical attributes lookup used by every crab
//! classifier that cares about `filter=crab` or `filter=lfs`:
//! `git/clean.rs`, `git/filter_process.rs`, `cmd/add.rs`,
//! `cmd/hydrate.rs`, `cmd/dehydrate.rs`, `cmd/status.rs`,
//! `lfs/status.rs`, and `lfs/migrate.rs`.
//!
//! Previously each of those files parsed `.gitattributes` on its own
//! with four subtly different implementations, producing user-visible
//! inconsistency: a file at `dir/model.bin` matched `*.bin` under
//! `clean.rs::glob_matches` but not under `cmd/add.rs::matches_any_tracked`.
//!
//! `AttrsReader` is the consolidated reader. It walks `.gitattributes`
//! files from the repo root plus any nested per-directory overrides,
//! then exposes `has_filter(rel_path, name)` for O(1) lookup. The
//! reader is cheap to construct (attribute parsing is memory-bound,
//! not I/O bound for typical repos) but still worth caching per
//! session — hot paths like the filter-process smudge loop can rebuild
//! the reader once at startup and reuse it for every request.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bstr::{BStr, ByteSlice};
use gix_attributes::Search;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_glob::pattern::Case;
use tracing::{debug, warn};

use crate::core::error::Result;

/// Cached attributes reader rooted at the repo worktree.
///
/// Holds a prebuilt `gix_attributes::Search` plus the `MetadataCollection`
/// that tracks which attribute names are known. `has_filter` uses an
/// `Outcome` initialized for the `filter` attribute to pull the current
/// value out of the search tree.
///
/// The `Search` is behind a `Mutex` because matching queries mutate
/// the `Outcome` buffer and share it with the caller. Contention is
/// not a concern — crab's classifiers call this one path at a time.
pub struct AttrsReader {
    search: Mutex<Search>,
    collection: MetadataCollection,
    case: Case,
    repo_root: PathBuf,
}

impl AttrsReader {
    /// Open and parse every `.gitattributes` file reachable from
    /// `repo_root`. Errors during parsing of an individual file are
    /// logged and skipped — we lean on `gix_attributes`'s own lenient
    /// behavior so a malformed `.gitattributes` in some nested
    /// directory never breaks `crab status` for the whole repo.
    pub fn open(repo_root: &Path) -> Result<Self> {
        let mut search = Search::default();
        let mut collection = MetadataCollection::default();
        let mut read_buf = Vec::new();

        // Root-level .gitattributes first. `gix_attributes` treats
        // missing files gracefully so we can always call add_patterns_file.
        add_attrs_if_exists(
            &mut search,
            &mut collection,
            &mut read_buf,
            &repo_root.join(".gitattributes"),
            Some(repo_root),
        );

        // Walk the tree for nested .gitattributes. Keep the walk simple
        // (std::fs::read_dir) — this runs once per session. Skip the
        // usual crab-internal directories so we never try to parse
        // binary data or chase into cloned submodules.
        collect_nested_attrs(
            repo_root,
            repo_root,
            &mut search,
            &mut collection,
            &mut read_buf,
            0,
        );

        debug!(
            root = %repo_root.display(),
            pattern_lists = search.num_pattern_lists(),
            "attrs_reader_built"
        );

        Ok(Self {
            search: Mutex::new(search),
            collection,
            // Case sensitivity mirrors the policy documented in
            // `worktree_stack_config`: default case-sensitive; callers
            // that know the FS is case-insensitive can construct with
            // `open_with_case` instead.
            case: Case::Sensitive,
            repo_root: repo_root.to_path_buf(),
        })
    }

    /// Open with an explicit case-folding mode. Used by callers that
    /// have resolved `core.ignoreCase` (or observed the platform default
    /// on macOS / Windows) and want matches to fold accordingly.
    pub fn open_with_case(repo_root: &Path, case: Case) -> Result<Self> {
        let mut reader = Self::open(repo_root)?;
        reader.case = case;
        Ok(reader)
    }

    /// Returns `true` when `rel_path` has an attribute of the form
    /// `filter=<filter_name>` in any applicable `.gitattributes` line
    /// (root or nested, with negation and precedence handled by
    /// `gix_attributes::Search`).
    ///
    /// `rel_path` must be repo-root-relative with `/` separators. The
    /// caller is responsible for stripping leading `./` if any.
    pub fn has_filter(&self, rel_path: &str, filter_name: &str) -> bool {
        // `pattern_matching_relative_path` takes `&self`, but the Outcome
        // buffer is our scratch space for this lookup. The Mutex guards
        // the Search so concurrent callers don't share the same Outcome
        // state; matching itself is idempotent.
        let Ok(search) = self.search.lock() else {
            return false;
        };

        let mut out = Outcome::default();
        out.initialize_with_selection(&self.collection, ["filter"]);

        let bytes: &BStr = rel_path.as_bytes().as_bstr();
        if !search.pattern_matching_relative_path(bytes, self.case, Some(false), &mut out) {
            return false;
        }

        // `iter_selected` yields the matched `filter` assignment.
        // `state.as_bstr()` returns `Some(&BStr)` for `filter=<value>`
        // and `None` for set/unset/unspecified. Byte-compare against
        // the requested filter name.
        let want = filter_name.as_bytes();
        for m in out.iter_selected() {
            if m.assignment.name.as_str() == "filter" {
                if let Some(v) = m.assignment.state.as_bstr() {
                    if v.as_bytes() == want {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The repo root this reader was built against. Handy when a caller
    /// needs to pass the same root to a sibling gix API (worktree stack,
    /// pathspec search) so construction policy stays aligned.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

fn add_attrs_if_exists(
    search: &mut Search,
    collection: &mut MetadataCollection,
    buf: &mut Vec<u8>,
    source: &Path,
    root: Option<&Path>,
) {
    if !source.is_file() {
        return;
    }
    match search.add_patterns_file(
        source.to_path_buf(),
        true, /* follow_symlinks */
        root,
        buf,
        collection,
        true, /* allow_macros */
    ) {
        Ok(_added) => {}
        Err(err) => {
            warn!(
                source = %source.display(),
                err = %err,
                "failed to read .gitattributes, skipping"
            );
        }
    }
}

fn collect_nested_attrs(
    repo_root: &Path,
    dir: &Path,
    search: &mut Search,
    collection: &mut MetadataCollection,
    buf: &mut Vec<u8>,
    depth: usize,
) {
    // Bound recursion — a pathological directory depth would be a
    // bigger problem elsewhere, but defending here is cheap.
    const MAX_DEPTH: usize = 32;
    if depth >= MAX_DEPTH {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip VCS / crab internals, symlinks we can't trust, and
        // hidden dirs aside from the root-level `.gitattributes`.
        if name_str == ".git" || name_str == ".crab" {
            continue;
        }

        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }

        let nested = path.join(".gitattributes");
        if nested.is_file() {
            add_attrs_if_exists(search, collection, buf, &nested, Some(repo_root));
        }

        collect_nested_attrs(repo_root, &path, search, collection, buf, depth + 1);
    }
}

/// Shared worktree-stack construction policy.
///
/// Codifies the single policy `gix_worktree::Stack` and the
/// classifier-side `gix_attributes::Search` should agree on when
/// building against the same repo: case folding, sparse-checkout file
/// location, and repo root. `gix_worktree::Stack` owns its own
/// attributes/ignore caches internally, so it does NOT consume the
/// `Search` instance built by [`AttrsReader`]. What these two paths
/// share is *policy* — which this struct makes explicit.
#[derive(Debug, Clone)]
pub struct WorktreeStackConfig {
    /// Repository root the stack should be rooted at.
    pub repo_root: PathBuf,
    /// Case-folding mode for path matching. Mirrors
    /// `AttrsReader::open_with_case`.
    pub case: Case,
    /// Whether the repo's filesystem is case-insensitive. Used by
    /// `gix_fs::Capabilities` consumers (including
    /// `gix_worktree_state::checkout`) to detect collisions.
    pub ignore_case_fs: bool,
    /// Filesystem-local path of `.git/info/sparse-checkout`, if any.
    /// `gix_worktree::Stack` reads this when it's present to limit
    /// checkout to the sparse set.
    pub sparse_checkout_file: Option<PathBuf>,
}

/// Resolve the shared worktree-stack construction policy for a repo
/// rooted at `repo_root`.
///
/// Today the config is derived conservatively from the filesystem:
/// case sensitivity follows the platform default (fold on macOS /
/// Windows, sensitive elsewhere), and sparse-checkout is discovered
/// via `.git/info/sparse-checkout`. Once Req 7's
/// `gix::Repository` facade lands (Task 8), this helper will honor
/// `core.ignoreCase` explicitly and resolve the ID-mapping source
/// from the repo's object database.
///
/// Callers that need the typed `gix_attributes::Source` for ID
/// mapping still thread through Task 7.1's checkout helper for the
/// worktree apply step; this function only exposes the *policy* —
/// the shared construction inputs both Task 6's `AttrsReader` and
/// Task 7.4's `gix_worktree::Stack` consume.
pub fn worktree_stack_config(repo_root: &Path) -> WorktreeStackConfig {
    // Platform-based default for case folding. macOS (APFS/HFS+) and
    // Windows (NTFS) default to case-insensitive; Linux filesystems
    // default to case-sensitive. Not exact — an ext4 user can
    // configure ext4 as case-insensitive — but matches git's default
    // behavior on all three major platforms.
    let ignore_case_fs = cfg!(target_os = "macos") || cfg!(target_os = "windows");
    let case = if ignore_case_fs {
        Case::Fold
    } else {
        Case::Sensitive
    };

    let git_info_dir = crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)
        .map_or_else(
            |_| repo_root.join(".git").join("info"),
            |ctx| ctx.per_worktree_git_dir.join("info"),
        );
    let sparse_checkout = git_info_dir.join("sparse-checkout");
    let sparse_checkout_file = if sparse_checkout.is_file() {
        Some(sparse_checkout)
    } else {
        None
    };

    WorktreeStackConfig {
        repo_root: repo_root.to_path_buf(),
        case,
        ignore_case_fs,
        sparse_checkout_file,
    }
}

/// Build a [`gix_worktree::Stack`] configured for checkout against
/// `index`, using the shared construction policy from
/// [`worktree_stack_config`].
///
/// The returned stack owns its own attributes cache via
/// [`gix_worktree::stack::state::Attributes`] seeded from the
/// worktree root — `gix_worktree::Stack` does not consume an
/// [`AttrsReader`]'s prebuilt `gix_attributes::Search` by design
/// (see the design doc for Req 6: `Stack` has its own
/// `StackDelegate`), but both paths agree on case folding,
/// sparse-checkout file location, and the repo root they walk.
///
/// The `id_mappings_from_index` step makes attributes loadable from
/// the index itself, which is what `gix_worktree_state::checkout`
/// expects when `.gitattributes` files are stored as tracked blobs
/// rather than on disk.
///
/// # Sparse checkout
///
/// This helper does not itself apply sparse-checkout patterns;
/// `gix_worktree_state::checkout` skips entries with the
/// `SKIP_WORKTREE` flag already set on them. The caller is
/// responsible for loading `.git/info/sparse-checkout`, compiling
/// the patterns, and setting the skip-worktree flag on each
/// index entry that should be excluded. Task 7.4a's
/// `--ignore-sparse` flag lets users opt out of sparse entirely —
/// in that mode the caller clears the skip-worktree flag before
/// calling checkout.
///
/// Added in scope of Task 7.4.
#[cfg(feature = "gix-worktree")]
pub fn build_checkout_stack(
    index: &gix_index::State,
    config: &WorktreeStackConfig,
) -> gix_worktree::Stack {
    use gix_worktree::stack::State;
    use gix_worktree::stack::state::Attributes;

    let path_backing = index.path_backing();
    let info_attributes =
        crate::git::worktree::WorktreeContext::resolve_from_path(&config.repo_root).map_or_else(
            |_| {
                config
                    .repo_root
                    .join(".git")
                    .join("info")
                    .join("attributes")
            },
            |ctx| ctx.per_worktree_git_dir.join("info").join("attributes"),
        );
    let info_attributes = if info_attributes.is_file() {
        Some(info_attributes)
    } else {
        None
    };

    // Source::IdMapping — load `.gitattributes` from tracked index
    // blobs first, falling back to worktree content. Mirrors what
    // `gix` itself uses for `gix_worktree_state::checkout`.
    let attributes = Attributes::new(
        gix_attributes::Search::default(), // no -c-globals; the stack loads repo-rooted
        info_attributes,
        gix_worktree::stack::state::attributes::Source::IdMapping,
        Default::default(),
    );
    let state = State::for_checkout(
        false, // unlink_on_collision — preserve existing files on collision
        gix_validate::path::component::Options::default(),
        attributes,
    );

    gix_worktree::Stack::from_state_and_ignore_case(
        config.repo_root.clone(),
        config.ignore_case_fs,
        state,
        index,
        path_backing,
    )
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn detects_root_filter_crab() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitattributes"), "*.bin filter=crab\n");
        let r = AttrsReader::open(tmp.path()).unwrap();
        assert!(r.has_filter("model.bin", "crab"));
        assert!(r.has_filter("dir/model.bin", "crab"));
        assert!(!r.has_filter("notes.txt", "crab"));
    }

    #[test]
    fn filter_lfs_is_distinct_from_filter_crab() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(
            &tmp.path().join(".gitattributes"),
            "*.bin filter=lfs\n*.safetensors filter=crab\n",
        );
        let r = AttrsReader::open(tmp.path()).unwrap();
        assert!(r.has_filter("a.bin", "lfs"));
        assert!(!r.has_filter("a.bin", "crab"));
        assert!(r.has_filter("a.safetensors", "crab"));
        assert!(!r.has_filter("a.safetensors", "lfs"));
    }

    #[test]
    fn nested_gitattributes_override_root() {
        let tmp = tempfile::tempdir().unwrap();
        // Root says everything under data/ is crab.
        write_file(
            &tmp.path().join(".gitattributes"),
            "data/**/*.bin filter=crab\n",
        );
        // Nested cancels it for archive.
        write_file(
            &tmp.path().join("data/archive/.gitattributes"),
            "*.bin -filter\n",
        );
        let r = AttrsReader::open(tmp.path()).unwrap();
        assert!(r.has_filter("data/current.bin", "crab"));
        assert!(!r.has_filter("data/archive/old.bin", "crab"));
    }

    #[test]
    fn missing_gitattributes_is_harmless() {
        let tmp = tempfile::tempdir().unwrap();
        let r = AttrsReader::open(tmp.path()).unwrap();
        assert!(!r.has_filter("anything.bin", "crab"));
    }

    #[test]
    fn worktree_stack_config_root_matches_input() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = worktree_stack_config(tmp.path());
        assert_eq!(cfg.repo_root, tmp.path());
    }

    #[test]
    fn worktree_stack_config_platform_case_default() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = worktree_stack_config(tmp.path());
        if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
            assert_eq!(cfg.case, Case::Fold, "expected case-fold on macOS/Windows");
            assert!(cfg.ignore_case_fs);
        } else {
            assert_eq!(
                cfg.case,
                Case::Sensitive,
                "expected case-sensitive on Linux"
            );
            assert!(!cfg.ignore_case_fs);
        }
    }

    #[test]
    fn worktree_stack_config_discovers_sparse_checkout() {
        let tmp = tempfile::tempdir().unwrap();

        // No file → None.
        let cfg = worktree_stack_config(tmp.path());
        assert!(cfg.sparse_checkout_file.is_none());

        // Present → Some(path).
        let info_dir = tmp.path().join(".git").join("info");
        std::fs::create_dir_all(&info_dir).unwrap();
        let sparse = info_dir.join("sparse-checkout");
        std::fs::write(&sparse, "src/*\n").unwrap();
        let cfg = worktree_stack_config(tmp.path());
        assert_eq!(cfg.sparse_checkout_file.as_deref(), Some(sparse.as_path()));
    }

    #[test]
    fn worktree_stack_config_uses_linked_worktree_private_git_info() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        let init = std::process::Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .output()
            .unwrap();
        if !init.status.success() {
            eprintln!("SKIP: git init failed");
            return;
        }
        std::fs::write(repo.join("a.txt"), b"a\n").unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "attrs@crab.dev"])
            .current_dir(&repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "crab-attrs"])
            .current_dir(&repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let commit = std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&repo)
            .output()
            .unwrap();
        if !commit.status.success() {
            eprintln!("SKIP: git commit failed");
            return;
        }
        let add = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .output()
            .unwrap();
        if !add.status.success() {
            eprintln!("SKIP: git worktree add failed");
            return;
        }

        let main_info = repo.join(".git").join("info");
        std::fs::create_dir_all(&main_info).unwrap();
        std::fs::write(main_info.join("sparse-checkout"), "main/**\n").unwrap();

        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(&linked).unwrap();
        let linked_info = ctx.per_worktree_git_dir.join("info");
        std::fs::create_dir_all(&linked_info).unwrap();
        let linked_sparse = linked_info.join("sparse-checkout");
        std::fs::write(&linked_sparse, "linked/**\n").unwrap();

        let cfg = worktree_stack_config(&linked);
        assert_eq!(
            cfg.sparse_checkout_file.as_deref(),
            Some(linked_sparse.as_path())
        );
    }
}

/// Classifier that caches an [`AttrsReader`] and applies it through
/// the simple `(rel_path) -> bool` shape used by every crab
/// classifier site.
///
/// Built once per command (or once per filter-process session) and
/// reused for every path decision — building the reader is cheaper
/// than the legacy per-site `.gitattributes` re-read, but still worth
/// amortizing across a walk.
pub struct TrackedClassifier {
    reader: AttrsReader,
    filter_name: String,
}

impl TrackedClassifier {
    /// Build a classifier that matches paths with
    /// `filter=<filter_name>` under the `.gitattributes` rooted at
    /// `repo_root`.
    pub fn open(repo_root: &Path, filter_name: &str) -> Result<Self> {
        Ok(Self {
            reader: AttrsReader::open(repo_root)?,
            filter_name: filter_name.to_owned(),
        })
    }

    /// Returns `true` when `rel_path` is covered by a
    /// `filter=<filter_name>` attribute in the repo's `.gitattributes`.
    ///
    /// `rel_path` must be repo-relative with `/` separators.
    pub fn is_tracked(&self, rel_path: &str) -> bool {
        self.reader.has_filter(rel_path, &self.filter_name)
    }

    /// Access the underlying [`AttrsReader`] for lookups beyond the
    /// single-filter classifier pattern (e.g. a caller that needs both
    /// `filter=crab` and `filter=lfs`).
    pub fn reader(&self) -> &AttrsReader {
        &self.reader
    }
}

/// Ignore-pattern reader backed by `gix_ignore::Search`.
///
/// Consolidates the `.gitignore` stack for classifier sites such as
/// `cmd/add.rs::walk_candidates` that previously had no ignore support
/// at all. Loads the root `.gitignore` (and, when available, the
/// repo's `.git/info/exclude`) at construction time. Nested
/// `.gitignore` files are not walked here — the walker is expected
/// to push additional lists via [`IgnoreReader::append_patterns_from_file`]
/// as it descends into subdirectories.
pub struct IgnoreReader {
    search: Mutex<gix_ignore::Search>,
    parse: gix_ignore::search::Ignore,
}

impl IgnoreReader {
    /// Open an ignore reader for the repo rooted at `repo_root`.
    ///
    /// Picks up `repo_root/.gitignore` (if present) and
    /// `repo_root/.git/info/exclude` (if the `.git` dir is a regular
    /// directory — submodules and worktrees fall back to just the
    /// root ignore). Missing files are not errors; the reader
    /// silently skips them.
    pub fn open(repo_root: &Path) -> Result<Self> {
        let parse = gix_ignore::search::Ignore::default();
        let mut search = gix_ignore::Search::default();
        let mut buf = Vec::new();

        let git_dir = repo_root.join(".git");
        if git_dir.is_dir() {
            match gix_ignore::Search::from_git_dir(&git_dir, None, &mut buf, parse) {
                Ok(from_git) => {
                    search.patterns.extend(from_git.patterns);
                }
                Err(err) => {
                    warn!(
                        git_dir = %git_dir.display(),
                        err = %err,
                        "failed to load .git/info/exclude; continuing without"
                    );
                }
            }
        }

        let root_ignore = repo_root.join(".gitignore");
        if root_ignore.is_file() {
            match std::fs::read(&root_ignore) {
                Ok(bytes) => {
                    search.add_patterns_buffer(&bytes, root_ignore.clone(), Some(repo_root), parse);
                }
                Err(err) => {
                    warn!(
                        source = %root_ignore.display(),
                        err = %err,
                        "failed to read .gitignore; continuing"
                    );
                }
            }
        }

        Ok(Self {
            search: Mutex::new(search),
            parse,
        })
    }

    /// Append a nested `.gitignore` file to the search tree.
    ///
    /// The walker calls this as it enters a subdirectory containing a
    /// `.gitignore` so precedence matches git's semantics (closer lists
    /// override outer ones). Silently no-ops when the file is missing.
    pub fn append_patterns_from_file(&self, source: &Path, root: Option<&Path>) {
        if !source.is_file() {
            return;
        }
        let bytes = match std::fs::read(source) {
            Ok(b) => b,
            Err(err) => {
                warn!(
                    source = %source.display(),
                    err = %err,
                    "failed to read nested .gitignore; continuing"
                );
                return;
            }
        };
        let Ok(mut search) = self.search.lock() else {
            return;
        };
        search.add_patterns_buffer(&bytes, source.to_path_buf(), root, self.parse);
    }

    /// Returns `true` when `rel_path` is matched by an ignore pattern
    /// whose effect is `Expendable` (standard `.gitignore` behavior).
    /// A `None` match or a `Precious` match returns `false` so callers
    /// don't accidentally drop precious entries.
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        let Ok(search) = self.search.lock() else {
            return false;
        };
        let bytes: &BStr = rel_path.as_bytes().as_bstr();
        let Some(m) = search.pattern_matching_relative_path(bytes, Some(is_dir), Case::Sensitive)
        else {
            return false;
        };
        // A negated pattern (`!foo`) is represented by the NEGATIVE flag.
        // Only unnegated, non-precious matches count as "ignored".
        if m.pattern.is_negative() {
            return false;
        }
        matches!(m.kind, gix_ignore::Kind::Expendable)
    }
}
