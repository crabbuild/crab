//! Bounded, verified Git-object discovery shared by LFS commands and publication.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{ChildStdin, Command};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::process::{self, GIT_ENV_REMOVALS, MAX_CAPTURE_BYTES};
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};

#[derive(Clone, Copy)]
pub(crate) enum GitObjectAccess {
    LocalOnly,
    PromisorAllowed,
}

#[derive(Clone, Copy)]
pub(crate) enum HistoryOperation<'a> {
    Fetch,
    PushAll,
    Push { remote: Option<&'a str> },
}

pub(crate) fn collect_pointers_for_fetch_in(
    repo_dir: &Path,
    refs: &[String],
    recent_commits: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    let mut entries = Vec::new();
    let mut seen: HashMap<[u8; 32], (u64, HashSet<String>)> = HashMap::new();
    let mut remaining = MAX_CAPTURE_BYTES;
    let mut retain = |path: String, pointer: LfsPointer| {
        if let Some((size, paths)) = seen.get(&pointer.oid) {
            if *size != pointer.size {
                return Err(CrabError::LfsObjectCorrupt {
                    oid: hex_encode(&pointer.oid),
                });
            }
            if paths.contains(&path) {
                return Ok(());
            }
        }
        // Preserve every path until include/exclude and checkout policy run.
        // Deduplicating by OID here would hide aliases and skip hydration.
        spend_scan_budget(
            &mut remaining,
            pointer_memory(&path, &pointer)
                + std::mem::size_of::<([u8; 32], (u64, HashSet<String>))>()
                + std::mem::size_of::<String>()
                + path.len(),
        )?;
        seen.entry(pointer.oid)
            .or_insert_with(|| (pointer.size, HashSet::new()))
            .1
            .insert(path.clone());
        entries.push((path, pointer));
        Ok(())
    };
    for revision in refs {
        visit_lfs_pointers_in_tree(
            repo_dir,
            revision,
            GitObjectAccess::PromisorAllowed,
            cancel,
            &mut retain,
        )?;
    }
    for (path, pointer) in previous_pointers_in_commits(repo_dir, recent_commits, cancel)? {
        check_cancelled(cancel)?;
        retain(path, pointer)?;
    }
    Ok(entries)
}

pub(crate) fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn collect_pointers_from_history_in(
    repo_dir: &Path,
    refs: &[String],
    operation: HistoryOperation<'_>,
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    let access = match operation {
        HistoryOperation::Fetch => GitObjectAccess::PromisorAllowed,
        HistoryOperation::PushAll | HistoryOperation::Push { .. } => GitObjectAccess::LocalOnly,
    };
    let head = ["HEAD".to_owned()];
    let refs = if refs.is_empty() && matches!(operation, HistoryOperation::Push { .. }) {
        &head
    } else {
        refs
    };
    let mut roots = Vec::new();
    let mut remaining = MAX_CAPTURE_BYTES;
    for revision in refs {
        // Resolve one operand before feeding stdin: revision options, ranges
        // and embedded newlines must not alter the selected graph.
        let mut resolve = git_command_in(repo_dir, access);
        resolve.args(["rev-parse", "--verify", "--end-of-options"]);
        resolve.arg(format!("{revision}^{{object}}"));
        let output = process::run(
            resolve,
            cancel,
            None::<fn(ChildStdin) -> Result<()>>,
            |stdout| Ok(process::capture_output(stdout, 65)?),
        )?;
        let bytes = successful_git("rev-parse", output)?;
        let oid = std::str::from_utf8(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let oid = oid
            .strip_suffix('\n')
            .filter(|oid| is_git_object_id(oid))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected one resolved object ID",
                )
            })?;
        spend_scan_budget(&mut remaining, std::mem::size_of::<String>() + oid.len())?;
        roots.push(oid.to_owned());
    }
    let mut command = git_command_in(repo_dir, access);
    command.args(["rev-list", "--objects"]);
    match operation {
        HistoryOperation::Fetch if refs.is_empty() => {
            command.arg("--all");
        }
        HistoryOperation::PushAll if refs.is_empty() => {
            command.args(["--branches", "--tags"]);
        }
        HistoryOperation::Push {
            remote: Some(remote),
        } => {
            // Git LFS excludes the selected remote's tracking refs. Command-line
            // --not does not invert the individually verified stdin roots.
            command.arg("--not").arg(format!("--remotes={remote}"));
        }
        _ => {}
    }
    command.args(["--stdin", "--"]);
    let output = process::run(
        command,
        cancel,
        Some(|mut stdin: ChildStdin| -> Result<()> {
            for root in roots {
                check_cancelled(cancel)?;
                writeln!(stdin, "{root}")?;
            }
            Ok(())
        }),
        |stdout| {
            let (pointers, ()) = read_pointer_inventory(
                repo_dir,
                stdout,
                b'\n',
                parse_reachable_object,
                access,
                cancel,
            )?;
            Ok(pointers)
        },
    )?;
    successful_git("rev-list", output)
}

fn parse_reachable_object(record: &[u8], _state: &mut ()) -> Result<Option<(String, String)>> {
    let line = std::str::from_utf8(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (oid, name) = line.split_once(' ').unwrap_or((line, ""));
    if !is_git_object_id(oid) {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "invalid reachable object ID").into(),
        );
    }
    // Bulk transfer has no path policy. Git's name is only a display hint, not
    // a complete alias inventory; unnamed blobs (e.g. tagged blobs) count too.
    Ok(Some((oid.to_owned(), name.to_owned())))
}

pub(crate) fn collect_lfs_object_ids_from_range_in(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let mut object_ids: Vec<String> =
        collect_pointers_from_range_in(repo_dir, local_shas, remote_shas, cancel)?
            .into_iter()
            .map(|(_, pointer)| hex_encode(&pointer.oid))
            .collect();
    object_ids.sort();
    object_ids.dedup();
    Ok(object_ids)
}

pub(crate) fn collect_pointers_from_range_in(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    collect_pointers_from_range_in_with_base_refs(repo_dir, local_shas, remote_shas, &[], cancel)
}

pub(crate) fn collect_pointers_from_range_in_with_base_refs(
    repo_dir: &Path,
    local_shas: &[String],
    remote_shas: &[String],
    base_manifest_refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    // A size filter can hide a corrupt pointer behind a forged large header.
    // Verify the introduced range's named objects; large bodies stay streamed.
    let mut args = vec!["rev-list".to_owned(), "--objects".to_owned()];
    for sha in local_shas {
        args.push(sha.clone());
    }
    let mut exclusions = HashSet::with_capacity(remote_shas.len() + base_manifest_refs.len());
    exclusions.extend(remote_shas.iter().cloned());
    exclusions.extend(base_manifest_refs.iter().cloned());
    let mut exclusions = exclusions.into_iter().collect::<Vec<_>>();
    exclusions.sort_unstable();
    for sha in exclusions {
        args.push(format!("^{sha}"));
    }
    let mut entries = Vec::new();
    visit_lfs_blobs_in_git_command(
        repo_dir,
        &args,
        b'\n',
        parse_rev_list_record,
        GitObjectAccess::LocalOnly,
        cancel,
        |path, pointer| {
            entries.push((path, pointer));
            Ok(())
        },
    )?;
    Ok(entries)
}

pub(crate) fn visit_lfs_pointers_in_tree(
    repo_dir: &Path,
    ref_name: &str,
    access: GitObjectAccess,
    cancel: &CancellationToken,
    visitor: impl FnMut(String, LfsPointer) -> Result<()>,
) -> Result<()> {
    let args = vec![
        "ls-tree".to_owned(),
        "-r".to_owned(),
        "-z".to_owned(),
        "--full-tree".to_owned(),
        ref_name.to_owned(),
    ];
    visit_lfs_blobs_in_git_command(
        repo_dir,
        &args,
        b'\0',
        parse_ls_tree_record,
        access,
        cancel,
        visitor,
    )
}

type DiscoveryParser<State = Option<String>> =
    fn(&[u8], &mut State) -> Result<Option<(String, String)>>;

fn previous_pointers_in_commits(
    repo_dir: &Path,
    commits: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(String, LfsPointer)>> {
    check_cancelled(cancel)?;
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    if commits.iter().any(|oid| !is_git_object_id(oid)) {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "expected frozen commit IDs").into(),
        );
    }
    let mut command = git_command_in(repo_dir, GitObjectAccess::PromisorAllowed);
    // Diff only the selected commits, including each merge parent. Old-side
    // blobs can predate the cutoff; scanning resulting trees misses them.
    // Pin formatting so display preferences cannot hide a parent or change framing.
    command.args([
        "-c",
        "log.showSignature=false",
        "-c",
        "log.diffMerges=separate",
        "log",
        "--stdin",
        "--no-walk=unsorted",
        "--format=",
        "--raw",
        "-z",
        "-m",
        "--root",
        "--no-renames",
        "--no-abbrev",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-decorate",
        "--no-notes",
        "--no-relative",
    ]);
    let write_commits = |mut stdin: ChildStdin| -> Result<()> {
        for commit in commits {
            check_cancelled(cancel)?;
            writeln!(stdin, "{commit}")?;
        }
        Ok(())
    };
    let output = process::run(command, cancel, Some(write_commits), |stdout| {
        let (pointers, state) = read_pointer_inventory(
            repo_dir,
            stdout,
            b'\0',
            parse_previous_object,
            GitObjectAccess::PromisorAllowed,
            cancel,
        )?;
        state.finish()?;
        Ok(pointers)
    })?;
    successful_git("log", output)
}

#[derive(Default)]
enum PreviousObject {
    #[default]
    Header,
    Path(Option<String>),
}

impl PreviousObject {
    fn finish(self) -> Result<()> {
        if matches!(self, Self::Path(_)) {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "missing previous-object path").into(),
            );
        }
        Ok(())
    }
}

fn parse_previous_object(
    record: &[u8],
    state: &mut PreviousObject,
) -> Result<Option<(String, String)>> {
    if let PreviousObject::Path(oid) = std::mem::take(state) {
        if record.is_empty() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "empty previous-object path").into(),
            );
        }
        return oid
            .map(|oid| {
                let path = std::str::from_utf8(record)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                Ok((oid, path.to_owned()))
            })
            .transpose();
    }
    let header = std::str::from_utf8(record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let parts: Vec<_> = header.split(' ').collect();
    let valid_mode =
        |mode: &str| matches!(mode, "000000" | "100644" | "100755" | "120000" | "160000");
    if parts.len() != 5
        || !parts[0].starts_with(':')
        || !valid_mode(&parts[0][1..])
        || !valid_mode(parts[1])
        || !is_git_object_id(parts[2])
        || !is_git_object_id(parts[3])
        || parts[2].len() != parts[3].len()
        || !matches!(parts[4], "A" | "D" | "M" | "T")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed previous-object header",
        )
        .into());
    }
    let old_mode = &parts[0][1..];
    let old_missing = parts[2].bytes().all(|byte| byte == b'0');
    let new_missing = parts[3].bytes().all(|byte| byte == b'0');
    if (old_mode == "000000") != old_missing
        || (parts[1] == "000000") != new_missing
        || (parts[4] == "A") != old_missing
        || (parts[4] == "D") != new_missing
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "inconsistent previous-object header",
        )
        .into());
    }
    let old_blob = matches!(old_mode, "100644" | "100755" | "120000");
    *state = PreviousObject::Path(old_blob.then(|| parts[2].to_owned()));
    Ok(None)
}

fn visit_lfs_blobs_in_git_command(
    repo_dir: &Path,
    args: &[String],
    record_terminator: u8,
    parse_record: DiscoveryParser,
    access: GitObjectAccess,
    cancel: &CancellationToken,
    mut visitor: impl FnMut(String, LfsPointer) -> Result<()>,
) -> Result<()> {
    let mut producer = git_command_in(repo_dir, access);
    producer.args(args);
    let discovery = process::run(
        producer,
        cancel,
        None::<fn(ChildStdin) -> Result<()>>,
        |stdout| {
            let (pointers, _) = read_pointer_inventory(
                repo_dir,
                stdout,
                record_terminator,
                parse_record,
                access,
                cancel,
            )?;
            Ok(pointers)
        },
    )?;
    let operation = args.first().map_or("discovery", String::as_str);
    // Each bounded batch is joined inside the discovery worker. Keep every
    // candidate private until discovery also exits successfully: a late
    // framing, checksum or producer failure invalidates the entire scan.
    for (path, pointer) in successful_git(operation, discovery)? {
        check_cancelled(cancel)?;
        visitor(path, pointer)?;
    }
    Ok(())
}

fn read_pointer_inventory<State: Default>(
    repo_dir: &Path,
    stdout: impl Read,
    terminator: u8,
    parse_record: DiscoveryParser<State>,
    access: GitObjectAccess,
    cancel: &CancellationToken,
) -> Result<(Vec<(String, LfsPointer)>, State)> {
    let mut pointers = Vec::new();
    let mut remaining = MAX_CAPTURE_BYTES;
    let state = read_discovery(
        stdout,
        terminator,
        parse_record,
        cancel,
        MAX_CAPTURE_BYTES,
        |records| {
            pointers.extend(read_lfs_batch(
                repo_dir,
                records,
                access,
                cancel,
                &mut remaining,
            )?);
            Ok(())
        },
    )?;
    Ok((pointers, state))
}

fn read_lfs_batch(
    repo_dir: &Path,
    records: &[(String, String)],
    access: GitObjectAccess,
    cancel: &CancellationToken,
    remaining: &mut u64,
) -> Result<Vec<(String, LfsPointer)>> {
    let mut batch = git_command_in(repo_dir, access);
    batch.args(["cat-file", "--batch"]);
    let write_stdin = |mut stdin: ChildStdin| {
        for (oid, _) in records {
            check_cancelled(cancel)?;
            writeln!(stdin, "{oid}")?;
        }
        Ok(())
    };
    let read_stdout = |stdout| {
        let mut pointers = Vec::new();
        crab_git::batch::visit_small_blobs(
            stdout,
            records.iter().map(|(oid, _)| oid.as_str()),
            MAX_LFS_POINTER_SIZE,
            &|| cancel.is_cancelled(),
            |index, body| {
                if let PointerKind::Lfs(pointer) = classify(body)
                    && pointer.size > 0
                {
                    let path = &records[index].1;
                    spend_scan_budget(remaining, pointer_memory(path, &pointer))?;
                    pointers.push((path.clone(), pointer));
                }
                Ok(())
            },
        )?;
        Ok(pointers)
    };
    let output = process::run(batch, cancel, Some(write_stdin), read_stdout)?;
    successful_git("cat-file", output)
}

fn read_discovery<State: Default>(
    stdout: impl Read,
    terminator: u8,
    parse_record: DiscoveryParser<State>,
    cancel: &CancellationToken,
    batch_bytes: u64,
    mut visit_batch: impl FnMut(&[(String, String)]) -> Result<()>,
) -> Result<State> {
    const MAX_RECORD_BYTES: u64 = 1024 * 1024;
    let mut reader = BufReader::new(stdout);
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut state = State::default();
    let mut remaining = batch_bytes;
    loop {
        check_cancelled(cancel)?;
        record.clear();
        let read = (&mut reader)
            .take(MAX_RECORD_BYTES + 1)
            .read_until(terminator, &mut record)?;
        if read == 0 {
            break;
        }
        if read as u64 > MAX_RECORD_BYTES || record.last() != Some(&terminator) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Git LFS discovery record is oversized or truncated",
            )
            .into());
        }
        record.pop();
        if let Some((oid, path)) = parse_record(&record, &mut state)? {
            let bytes = std::mem::size_of::<(String, String)>() + oid.len() + path.len();
            if bytes as u64 > remaining && !records.is_empty() {
                visit_batch(&records)?;
                records.clear();
                remaining = batch_bytes;
            }
            spend_scan_budget(&mut remaining, bytes)?;
            records.push((oid, path));
        }
    }
    if !records.is_empty() {
        visit_batch(&records)?;
    }
    Ok(state)
}

pub(crate) fn pointer_memory(path: &str, pointer: &LfsPointer) -> usize {
    std::mem::size_of::<(String, LfsPointer)>()
        + path.len()
        + std::mem::size_of_val(pointer.extensions.as_slice())
        + pointer
            .extensions
            .iter()
            .map(|extension| extension.name.len() + extension.oid_type.len())
            .sum::<usize>()
}

pub(crate) fn spend_scan_budget(remaining: &mut u64, bytes: usize) -> io::Result<()> {
    *remaining = remaining.checked_sub(bytes as u64).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Git LFS scan exceeds inventory limit",
        )
    })?;
    Ok(())
}

pub(crate) fn successful_git<T>(operation: &str, output: process::Output<T>) -> Result<T> {
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "git {operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn parse_rev_list_record(
    record: &[u8],
    pending_oid: &mut Option<String>,
) -> Result<Option<(String, String)>> {
    if let Some(path) = record.strip_prefix(b"path=") {
        let oid = pending_oid.take().ok_or_else(|| {
            CrabError::Internal("git rev-list returned a path without an object ID".to_owned())
        })?;
        if path.is_empty() {
            return Err(CrabError::Internal(
                "git rev-list returned an empty object path".to_owned(),
            ));
        }
        return Ok(Some((oid, String::from_utf8_lossy(path).into_owned())));
    }

    let separator = record.iter().position(|byte| byte.is_ascii_whitespace());
    let (oid_bytes, path) = match separator {
        Some(index) => (&record[..index], &record[index + 1..]),
        None => (record, &record[0..0]),
    };
    let oid = std::str::from_utf8(oid_bytes).map_err(|_| {
        CrabError::Internal("git rev-list returned a malformed object record".to_owned())
    })?;
    if !is_git_object_id(oid) {
        return Err(CrabError::Internal(
            "git rev-list returned a malformed object record".to_owned(),
        ));
    }

    if !path.is_empty() {
        *pending_oid = None;
        return Ok(Some((
            oid.to_owned(),
            String::from_utf8_lossy(path).into_owned(),
        )));
    }

    // Commits and trees have no object name. Keep the most recent unnamed
    // object for Git versions that emit a separate `path=...` record; normal
    // line-delimited output replaces it on each row.
    *pending_oid = Some(oid.to_owned());
    Ok(None)
}

fn parse_ls_tree_record(
    record: &[u8],
    _pending_oid: &mut Option<String>,
) -> Result<Option<(String, String)>> {
    let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
        return Err(CrabError::Internal(
            "git ls-tree returned a malformed object record".to_owned(),
        ));
    };
    let (meta, path) = record.split_at(separator);
    let path = &path[1..];
    let parts: Vec<&str> = std::str::from_utf8(meta)
        .map_err(|_| CrabError::Internal("git ls-tree returned invalid UTF-8".to_owned()))?
        .split_whitespace()
        .collect();
    if parts.len() != 3 {
        return Err(CrabError::Internal(
            "git ls-tree returned a malformed object record".to_owned(),
        ));
    }
    if parts[1] != "blob" {
        return Ok(None);
    }
    if !is_git_object_id(parts[2]) || path.is_empty() {
        return Err(CrabError::Internal(
            "git ls-tree returned a malformed object record".to_owned(),
        ));
    }
    Ok(Some((
        parts[2].to_owned(),
        String::from_utf8_lossy(path).into_owned(),
    )))
}

fn git_command_in(repo_dir: &Path, access: GitObjectAccess) -> Command {
    let mut command = Command::new("git");
    for key in GIT_ENV_REMOVALS {
        command.env_remove(key);
    }
    command.arg("--no-replace-objects").current_dir(repo_dir);
    // Inspection/publication cannot hydrate a source implicitly. Fetch/pull
    // may resolve promised Git blobs, while preserving caller restrictions.
    if matches!(access, GitObjectAccess::LocalOnly) {
        // Git 2.30 ignores NO_LAZY_FETCH; an empty transport allowlist also
        // prevents implicit fetches on the oldest supported clients.
        command
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "");
    }
    command
}

#[cfg(test)]
mod tests;
