//! `crab lfs ls-files` — list LFS-tracked files.
//!
//! Lists files in the current HEAD (or all local refs) that are stored as
//! LFS pointers, showing abbreviated OID, local presence, and filename.
//! Mirrors the output format of `git lfs ls-files`.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};
use crate::lfs::batch::PatternFilter;
use crab_git::lfs_pointer::{LFS_VERSION_URL, LfsPointer, hex_encode};
use serde::Serialize;

/// A single LFS-tracked file entry discovered by scanning git trees.
struct LfsFileEntry {
    /// Relative path within the repository.
    filename: String,
    /// Parsed LFS pointer for this file.
    pointer: LfsPointer,
    /// Full hex-encoded SHA-256 OID.
    oid_hex: String,
}

/// Options for `crab lfs ls-files` output.
pub(crate) struct LfsLsFilesOptions {
    /// Explicit refs to scan instead of only `HEAD`.
    pub(crate) refs: Vec<String>,
    /// Scan every local ref instead of only `HEAD`.
    pub(crate) all: bool,
    /// Print full OIDs.
    pub(crate) long: bool,
    /// Print only file names.
    pub(crate) name_only: bool,
    /// Include human-readable object sizes in default output.
    pub(crate) size: bool,
    /// Print full pointer details.
    pub(crate) debug: bool,
    /// Include deleted files from a ref scan.
    pub(crate) deleted: bool,
    /// Include only matching paths.
    pub(crate) include: Option<String>,
    /// Exclude matching paths.
    pub(crate) exclude: Option<String>,
    /// Print stable JSON output.
    pub(crate) json: bool,
}

/// Run `crab lfs ls-files` with the given flags.
///
/// Scans git trees for LFS pointers and prints results to stdout.
/// Uses `git ls-tree` and `git cat-file` to enumerate and inspect blobs.
pub(crate) fn run_lfs_ls_files(options: LfsLsFilesOptions) -> Result<()> {
    validate_options(&options)?;
    let mut entries = collect_lfs_entries(&options)?;
    apply_path_filters(
        &mut entries,
        options.include.as_deref(),
        options.exclude.as_deref(),
    )?;

    if entries.is_empty() {
        if options.json {
            print_json(&entries, Path::new("."))?;
        }
        return Ok(());
    }

    let repo_root = discover_repo_workdir()?;

    if options.json && !options.debug {
        print_json(&entries, &repo_root)?;
        return Ok(());
    }

    for entry in &entries {
        if options.debug {
            print_debug(entry, &repo_root);
        } else if options.name_only {
            println!("{}", entry.filename);
        } else {
            print_default(entry, &repo_root, options.size, options.long);
        }
    }

    Ok(())
}

fn validate_options(options: &LfsLsFilesOptions) -> Result<()> {
    if options.all && !options.refs.is_empty() {
        return Err(CrabError::Configuration {
            key: "ls-files".to_owned(),
            origin: "cannot use --all with an explicit reference".to_owned(),
        });
    }
    if options.refs.len() > 2 {
        return Err(CrabError::Configuration {
            key: "ls-files".to_owned(),
            origin: "expected zero, one, or two explicit references".to_owned(),
        });
    }
    if options.deleted && options.refs.len() > 1 {
        return Err(CrabError::Configuration {
            key: "ls-files".to_owned(),
            origin: "cannot use --deleted with a reference range".to_owned(),
        });
    }
    if options
        .refs
        .first()
        .is_some_and(|ref_name| ref_name == "--all")
    {
        return Err(CrabError::Configuration {
            key: "ls-files".to_owned(),
            origin: "did you mean `crab lfs ls-files --all --`?".to_owned(),
        });
    }
    Ok(())
}

fn apply_path_filters(
    entries: &mut Vec<LfsFileEntry>,
    include: Option<&str>,
    exclude: Option<&str>,
) -> Result<()> {
    let include = include.map(PatternFilter::new).transpose()?;
    let exclude = exclude.map(PatternFilter::new).transpose()?;
    entries.retain(|entry| {
        if let Some(include) = &include
            && !include.matches(&entry.filename)
        {
            return false;
        }
        if let Some(exclude) = &exclude
            && exclude.matches(&entry.filename)
        {
            return false;
        }
        true
    });
    Ok(())
}

/// Collect all LFS file entries from the selected tree or history range.
fn collect_lfs_entries(options: &LfsLsFilesOptions) -> Result<Vec<LfsFileEntry>> {
    collect_lfs_entries_in(options, None)
}

fn collect_lfs_entries_in(
    options: &LfsLsFilesOptions,
    repo_root: Option<&Path>,
) -> Result<Vec<LfsFileEntry>> {
    let tree_lines = selected_git_objects(options, repo_root)?;
    let dedupe_filenames = !options.all && options.refs.len() < 2;

    if tree_lines.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-read all blobs in a single git cat-file --batch process.
    let oids_input: String = tree_lines
        .iter()
        .map(|(hash, _)| hash.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut command = git_command(repo_root);
    command
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git cat-file --batch: {e}")))?;

    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        let _ = stdin.write_all(oids_input.as_bytes());
    }
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|e| CrabError::Internal(format!("git cat-file --batch failed: {e}")))?;

    let stdout = &output.stdout;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0;
    let mut line_idx = 0;

    while pos < stdout.len() && line_idx < tree_lines.len() {
        // Parse header: "<hash> <type> <size>\n"
        let header_end = match stdout[pos..].iter().position(|&b| b == b'\n') {
            Some(p) => pos + p,
            None => break,
        };

        let header = String::from_utf8_lossy(&stdout[pos..header_end]);
        let parts: Vec<&str> = header.split_whitespace().collect();

        if parts.len() < 3 || parts[1] == "missing" {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        }

        let Ok(obj_size) = parts[2].parse::<usize>() else {
            pos = header_end + 1;
            line_idx += 1;
            continue;
        };

        let content_start = header_end + 1;
        let content_end = content_start + obj_size;

        if content_end > stdout.len() {
            break;
        }

        let content = &stdout[content_start..content_end];
        let (_, filename) = &tree_lines[line_idx];

        // Only check small blobs that could be LFS pointers.
        if parts[1] == "blob"
            && content.len() <= 1024
            && let Ok(pointer) = LfsPointer::parse(content)
            && pointer.size > 0
            && (!dedupe_filenames || !seen.contains(filename))
        {
            seen.insert(filename.clone());
            let oid_hex = hex_encode(&pointer.oid);
            entries.push(LfsFileEntry {
                filename: filename.clone(),
                pointer,
                oid_hex,
            });
        }

        // Skip past content + trailing newline.
        pos = content_end + 1;
        line_idx += 1;
    }

    Ok(entries)
}

fn selected_git_objects(
    options: &LfsLsFilesOptions,
    repo_root: Option<&Path>,
) -> Result<Vec<(String, String)>> {
    if options.all {
        return rev_list_all_objects(repo_root);
    }

    match options.refs.as_slice() {
        [exclude, include] => rev_list_objects(Some(include), Some(exclude), repo_root),
        [ref_name] if options.deleted => rev_list_objects(Some(ref_name), None, repo_root),
        [ref_name] => ls_tree_ref(ref_name, repo_root),
        [] if options.deleted => rev_list_objects(Some("HEAD"), None, repo_root),
        [] => ls_tree_head(repo_root),
        _ => Err(CrabError::Configuration {
            key: "ls-files".to_owned(),
            origin: "expected zero, one, or two explicit references".to_owned(),
        }),
    }
}

/// Run `git ls-tree -r HEAD` and return (blob_hash, filename) pairs.
fn ls_tree_head(repo_root: Option<&Path>) -> Result<Vec<(String, String)>> {
    ls_tree_ref("HEAD", repo_root)
}

/// Run `git ls-tree -r <ref>` and return (blob_hash, filename) pairs.
fn ls_tree_ref(ref_name: &str, repo_root: Option<&Path>) -> Result<Vec<(String, String)>> {
    let output = git_command(repo_root)
        .args(["ls-tree", "-r", ref_name])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-tree: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Empty repo with no commits — not an error, just no files.
        if stderr.contains("Not a valid object name") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git ls-tree failed for {ref_name}: {stderr}"
        )));
    }

    Ok(parse_ls_tree_output(&output.stdout))
}

/// Run `git rev-list --objects --all` and return `(object_hash, filename)` pairs.
fn rev_list_all_objects(repo_root: Option<&Path>) -> Result<Vec<(String, String)>> {
    let output = git_command(repo_root)
        .args(["rev-list", "--objects", "--all"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(parse_rev_list_objects_output(&output.stdout))
}

/// Run `git rev-list --objects <include> ^<exclude>` and return object/path pairs.
fn rev_list_objects(
    include_ref: Option<&str>,
    exclude_ref: Option<&str>,
    repo_root: Option<&Path>,
) -> Result<Vec<(String, String)>> {
    let mut args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    if let Some(include_ref) = include_ref {
        args.push(include_ref.to_owned());
    }
    if let Some(exclude_ref) = exclude_ref {
        args.push(format!("^{exclude_ref}"));
    }

    let output = git_command(repo_root)
        .args(args)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git rev-list: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name")
            || stderr.contains("unknown revision")
            || stderr.contains("bad revision")
            || stderr.contains("ambiguous argument")
        {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git rev-list failed: {stderr}"
        )));
    }

    Ok(parse_rev_list_objects_output(&output.stdout))
}

fn git_command(repo_root: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(repo_root) = repo_root {
        command.current_dir(repo_root);
    }
    command
}

/// Parse the output of `git ls-tree -r`, extracting blob hash and filename.
///
/// Each line has the format: `<mode> <type> <hash>\t<filename>`
fn parse_ls_tree_output(output: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(output);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Split at tab to separate metadata from filename.
        let Some((meta, filename)) = line.split_once('\t') else {
            continue;
        };

        // meta = "<mode> <type> <hash>"
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let obj_type = parts[1];
        let blob_hash = parts[2];

        // Only process blobs (skip trees, commits, etc.).
        if obj_type != "blob" {
            continue;
        }

        results.push((blob_hash.to_owned(), filename.to_owned()));
    }

    results
}

/// Parse `git rev-list --objects` output into `(object_hash, filename)` pairs.
fn parse_rev_list_objects_output(output: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(output);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Some((object_hash, filename)) = line.split_once(' ') else {
            continue;
        };
        if filename.is_empty() {
            continue;
        }

        results.push((object_hash.to_owned(), filename.to_owned()));
    }

    results
}

/// Check whether the working tree has the smudged file content.
fn is_checkout(entry: &LfsFileEntry, repo_root: &Path) -> bool {
    let path = repo_root.join(&entry.filename);
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    metadata.is_file() && metadata.len() == entry.pointer.size
}

/// Check whether an LFS object is present in the local `.git/lfs/objects/` cache.
fn is_downloaded(oid_hex: &str, repo_root: &Path) -> bool {
    if oid_hex.len() < 4 {
        return false;
    }
    let Ok(lfs_dir) = crate::lfs::config::LfsConfig::resolve_storage_dir(repo_root) else {
        return false;
    };
    let path = lfs_dir
        .join("objects")
        .join(&oid_hex[..2])
        .join(&oid_hex[2..4])
        .join(oid_hex);
    path.exists()
}

/// Discover the repository working directory.
fn discover_repo_workdir() -> Result<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to discover repo root: {e}")))?;

    if !output.status.success() {
        // Fall back to current directory if git can't determine the root.
        return std::env::current_dir()
            .map_err(|e| CrabError::Internal(format!("failed to get cwd: {e}")));
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(std::path::PathBuf::from(root))
}

/// Print a file entry in default format: `{oid_short} {presence} {filename}`.
fn print_default(entry: &LfsFileEntry, repo_root: &Path, show_size: bool, long: bool) {
    let oid_len = if long { 64 } else { 10 };
    let oid = &entry.oid_hex[..oid_len];
    let presence = if is_checkout(entry, repo_root) {
        "*"
    } else {
        "-"
    };

    if show_size {
        println!(
            "{oid} {presence} {filename} ({size})",
            filename = entry.filename,
            size = format_size(entry.pointer.size),
        );
    } else {
        println!("{oid} {presence} {}", entry.filename);
    }
}

/// Print a file entry in debug format with full pointer details.
fn print_debug(entry: &LfsFileEntry, repo_root: &Path) {
    let checkout = is_checkout(entry, repo_root);
    let downloaded = is_downloaded(&entry.oid_hex, repo_root);
    println!("filepath: {}", entry.filename);
    println!("    size: {}", entry.pointer.size);
    println!("checkout: {checkout}");
    println!("download: {downloaded}");
    println!("     oid: sha256 {}", entry.oid_hex);
    println!(" version: {LFS_VERSION_URL}");
    println!();
}

#[derive(Serialize)]
struct LsFilesPayload {
    files: Vec<LsFilesJsonEntry>,
}

#[derive(Serialize)]
struct LsFilesJsonEntry {
    name: String,
    size: u64,
    checkout: bool,
    downloaded: bool,
    oid_type: &'static str,
    oid: String,
    version: &'static str,
}

fn print_json(entries: &[LfsFileEntry], repo_root: &Path) -> Result<()> {
    let payload = LsFilesPayload {
        files: entries
            .iter()
            .map(|entry| LsFilesJsonEntry {
                name: entry.filename.clone(),
                size: entry.pointer.size,
                checkout: is_checkout(entry, repo_root),
                downloaded: is_downloaded(&entry.oid_hex, repo_root),
                oid_type: "sha256",
                oid: entry.oid_hex.clone(),
                version: LFS_VERSION_URL,
            })
            .collect(),
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, &payload)
        .map_err(|e| CrabError::Internal(format!("failed to serialize lfs ls-files JSON: {e}")))?;
    writeln!(handle).map_err(CrabError::Io)?;
    Ok(())
}

/// Format a byte count as a human-readable size string.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct TempGitRepo {
        _git_env: crate::test::git_repo::CleanGitEnvGuard,
        dir: tempfile::TempDir,
    }

    impl TempGitRepo {
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn default_options() -> LfsLsFilesOptions {
        LfsLsFilesOptions {
            refs: Vec::new(),
            all: false,
            long: false,
            name_only: false,
            size: false,
            debug: false,
            deleted: false,
            include: None,
            exclude: None,
            json: false,
        }
    }

    fn entry(filename: &str) -> LfsFileEntry {
        LfsFileEntry {
            filename: filename.to_owned(),
            pointer: LfsPointer {
                oid: [9; 32],
                size: 12,
                extensions: Vec::new(),
            },
            oid_hex: hex_encode(&[9; 32]),
        }
    }

    fn temp_git_repo() -> TempGitRepo {
        let git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch=main"]);
        git(
            dir.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        git(dir.path(), &["config", "user.name", "Crab Test"]);
        TempGitRepo {
            _git_env: git_env,
            dir,
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn commit_pointer(root: &Path, path: &str, oid_byte: u8, size: u64, message: &str) {
        let pointer = LfsPointer {
            oid: [oid_byte; 32],
            size,
            extensions: Vec::new(),
        };
        std::fs::write(root.join(path), pointer.serialize()).unwrap();
        git(root, &["add", path]);
        git(root, &["commit", "-m", message]);
    }

    #[test]
    fn validate_options_rejects_all_with_ref() {
        let mut options = default_options();
        options.all = true;
        options.refs.push("HEAD".to_owned());

        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn validate_options_accepts_reference_range() {
        let mut options = default_options();
        options.refs.push("HEAD~1".to_owned());
        options.refs.push("HEAD".to_owned());

        assert!(validate_options(&options).is_ok());
    }

    #[test]
    fn validate_options_rejects_more_than_two_refs() {
        let mut options = default_options();
        options.refs.push("HEAD~2".to_owned());
        options.refs.push("HEAD~1".to_owned());
        options.refs.push("HEAD".to_owned());

        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn validate_options_rejects_deleted_with_reference_range() {
        let mut options = default_options();
        options.deleted = true;
        options.refs.push("HEAD~1".to_owned());
        options.refs.push("HEAD".to_owned());

        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn apply_path_filters_uses_include_and_exclude() {
        let mut entries = vec![
            entry("assets/a.bin"),
            entry("assets/b.dat"),
            entry("notes/c.bin"),
        ];

        apply_path_filters(&mut entries, Some("assets/*"), Some("*.dat")).unwrap();

        let filenames: Vec<&str> = entries
            .iter()
            .map(|entry| entry.filename.as_str())
            .collect();
        assert_eq!(filenames, vec!["assets/a.bin"]);
    }

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(1_048_576), "1.0 MB");
        assert_eq!(format_size(10_485_760), "10.0 MB");
    }

    #[test]
    fn format_size_gigabytes() {
        assert_eq!(format_size(1_073_741_824), "1.0 GB");
    }

    #[test]
    fn parse_ls_tree_output_extracts_blobs() {
        let output = b"100644 blob abc123def456\tpath/to/file.bin\n\
                        040000 tree deadbeef1234\tsome/dir\n\
                        100644 blob 789abc012345\tanother.txt\n";
        let result = parse_ls_tree_output(output);
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0],
            ("abc123def456".to_owned(), "path/to/file.bin".to_owned())
        );
        assert_eq!(
            result[1],
            ("789abc012345".to_owned(), "another.txt".to_owned())
        );
    }

    #[test]
    fn parse_ls_tree_output_handles_empty() {
        let result = parse_ls_tree_output(b"");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_rev_list_objects_output_keeps_paths_with_spaces() {
        let output = b"abc123 path/with spaces.bin\n\
                       def456\n\
                       789abc plain.dat\n";
        let entries = parse_rev_list_objects_output(output);
        assert_eq!(
            entries,
            vec![
                ("abc123".to_owned(), "path/with spaces.bin".to_owned()),
                ("789abc".to_owned(), "plain.dat".to_owned()),
            ]
        );
    }

    #[test]
    fn collect_lfs_entries_deleted_scans_history() {
        let repo = temp_git_repo();
        commit_pointer(repo.path(), "a.dat", 1, 11, "add a");
        git(repo.path(), &["rm", "a.dat"]);
        git(repo.path(), &["commit", "-m", "remove a"]);

        let mut options = default_options();
        options.deleted = true;
        let entries = collect_lfs_entries_in(&options, Some(repo.path())).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "a.dat");
    }

    #[test]
    fn collect_lfs_entries_reference_range_keeps_modified_versions() {
        let repo = temp_git_repo();
        commit_pointer(repo.path(), "b.dat", 1, 11, "add b");
        git(repo.path(), &["tag", "base"]);
        commit_pointer(repo.path(), "c.dat", 2, 22, "add c");
        commit_pointer(repo.path(), "c.dat", 3, 33, "modify c");
        git(repo.path(), &["tag", "tip"]);

        let mut options = default_options();
        options.refs = vec!["base".to_owned(), "tip".to_owned()];
        let entries = collect_lfs_entries_in(&options, Some(repo.path())).unwrap();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.filename == "c.dat"));
    }
}
