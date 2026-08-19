use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crab_git::{
    PointerKind, classify,
    lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE},
};
use crab_types::pointer::Pointer;

use crate::error::{AuthServerError, Result};
use crate::view::ViewS3Credentials;

pub(super) struct ViewGitWorkspace {
    temp: tempfile::TempDir,
    source_git: PathBuf,
    filtered_git: PathBuf,
    export_stream: PathBuf,
    repacked_stream: PathBuf,
}

impl ViewGitWorkspace {
    pub(super) fn create(
        source_url: &str,
        include: &[String],
        deny: &[String],
        credentials: Option<&ViewS3Credentials>,
    ) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let source_git = temp.path().join("source.git");
        let filtered_git = temp.path().join("filtered.git");
        let export_stream = temp.path().join("view.fast-export");
        let repacked_stream = temp.path().join("view.repacked.fast-export");

        run_git_with_credentials(
            ["clone", "--bare", source_url, path_str(&source_git)?],
            None,
            credentials,
        )?;
        run_git(["init", "--bare", path_str(&filtered_git)?], None)?;
        export_filtered_history(&source_git, &export_stream, include, deny)?;

        Ok(Self {
            temp,
            source_git,
            filtered_git,
            export_stream,
            repacked_stream,
        })
    }

    pub(super) fn temp_path(&self) -> &Path {
        self.temp.path()
    }

    pub(super) fn filtered_git(&self) -> &Path {
        &self.filtered_git
    }

    pub(super) fn export_stream(&self) -> &Path {
        &self.export_stream
    }

    pub(super) fn repacked_stream(&self) -> &Path {
        &self.repacked_stream
    }

    pub(super) fn import_repacked_history(&self) -> Result<()> {
        import_filtered_history(&self.filtered_git, &self.repacked_stream)?;
        preserve_visible_head(&self.source_git, &self.filtered_git)
    }

    pub(super) fn validate_git_state(&self) -> Result<()> {
        run_git(
            [
                "--git-dir",
                path_str(&self.filtered_git)?,
                "fsck",
                "--strict",
                "--full",
                "--no-reflogs",
                "--no-dangling",
            ],
            None,
        )
    }
}

#[derive(Debug, Default)]
pub(super) struct ReachablePointerScan {
    pub(super) crab_pointers: Vec<Pointer>,
    pub(super) lfs_pointers: Vec<LfsPointer>,
}

pub(super) fn clone_bare(
    source_url: &str,
    target: &Path,
    credentials: Option<&ViewS3Credentials>,
) -> Result<()> {
    run_git_with_credentials(
        ["clone", "--bare", source_url, path_str(target)?],
        None,
        credentials,
    )
}

pub(super) fn generate_view_pack(filtered_git: &Path) -> Result<Vec<u8>> {
    let object_list = run_git_capture_bytes(
        [
            "--git-dir",
            path_str(filtered_git)?,
            "rev-list",
            "--objects",
            "--all",
        ],
        None,
    )?;
    if object_list.is_empty() {
        return Ok(Vec::new());
    }

    let mut pack_objects = Command::new("git")
        .args([
            "--git-dir",
            path_str(filtered_git)?,
            "pack-objects",
            "--stdout",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(AuthServerError::Io)?;
    {
        let stdin = pack_objects
            .stdin
            .as_mut()
            .ok_or_else(|| AuthServerError::Internal("pack-objects stdin not available".into()))?;
        stdin.write_all(&object_list).map_err(AuthServerError::Io)?;
    }
    drop(pack_objects.stdin.take());

    let output = pack_objects
        .wait_with_output()
        .map_err(AuthServerError::Io)?;
    if !output.status.success() {
        return Err(AuthServerError::AuthFailed {
            path: format!(
                "git pack-objects failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let validation = tempfile::tempdir()?;
    let validation_pack = validation.path().join("view.pack");
    std::fs::write(&validation_pack, &output.stdout)?;
    run_git(
        [
            "--git-dir",
            path_str(filtered_git)?,
            "index-pack",
            "--strict",
            path_str(&validation_pack)?,
        ],
        None,
    )?;
    Ok(output.stdout)
}

pub(super) fn list_view_refs(filtered_git: &Path) -> Result<BTreeMap<String, String>> {
    let output = Command::new("git")
        .args(["--git-dir", path_str(filtered_git)?, "show-ref"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AuthServerError::Internal(format!("failed to run git show-ref: {e}")))?;
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stdout.is_empty() {
            return Ok(BTreeMap::new());
        }
        return Err(AuthServerError::AuthFailed {
            path: format!(
                "git show-ref failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let output = String::from_utf8(output.stdout).map_err(|_| {
        AuthServerError::Internal("git show-ref output was not valid UTF-8".to_owned())
    })?;

    let mut refs = BTreeMap::new();
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
            refs.insert(name.to_owned(), sha.to_owned());
        }
    }
    Ok(refs)
}

pub(super) fn resolve_view_head(
    filtered_git: &Path,
    refs: &BTreeMap<String, String>,
) -> Result<String> {
    let head = run_git_capture(
        ["--git-dir", path_str(filtered_git)?, "symbolic-ref", "HEAD"],
        None,
    )
    .unwrap_or_default()
    .trim()
    .to_owned();
    if refs.contains_key(&head) || refs.is_empty() {
        return Ok(if head.is_empty() {
            "refs/heads/main".to_owned()
        } else {
            head
        });
    }
    refs.keys()
        .find(|name| name.starts_with("refs/heads/"))
        .or_else(|| refs.keys().next())
        .cloned()
        .ok_or_else(|| {
            AuthServerError::Internal("view refs disappeared while resolving HEAD".to_owned())
        })
}

pub(super) fn scan_reachable_pointers(git_dir: &Path) -> Result<ReachablePointerScan> {
    let output = run_git_capture(
        [
            "--git-dir",
            path_str(git_dir)?,
            "rev-list",
            "--objects",
            "--all",
        ],
        None,
    )?;
    let mut seen_objects = HashSet::new();
    let mut seen_lfs_oids = HashSet::new();
    let mut scan = ReachablePointerScan::default();

    for line in output.lines() {
        let Some(oid) = line.split_whitespace().next() else {
            continue;
        };
        if !seen_objects.insert(oid.to_owned()) {
            continue;
        }

        let kind = run_git_capture(
            ["--git-dir", path_str(git_dir)?, "cat-file", "-t", oid],
            None,
        )?;
        if kind.trim() != "blob" {
            continue;
        }

        let size = run_git_capture(
            ["--git-dir", path_str(git_dir)?, "cat-file", "-s", oid],
            None,
        )?
        .trim()
        .parse::<usize>()
        .map_err(|e| AuthServerError::Internal(format!("git cat-file returned bad size: {e}")))?;
        if size > MAX_LFS_POINTER_SIZE {
            continue;
        }

        let bytes = run_git_capture_bytes(
            ["--git-dir", path_str(git_dir)?, "cat-file", "blob", oid],
            None,
        )?;
        match classify(&bytes) {
            PointerKind::Crab(pointer) => scan.crab_pointers.push(pointer),
            PointerKind::Lfs(pointer) if pointer.size > 0 => {
                if seen_lfs_oids.insert(pointer.oid) {
                    scan.lfs_pointers.push(pointer);
                }
            }
            PointerKind::Lfs(_) | PointerKind::NotAPointer => {}
        }
    }

    Ok(scan)
}

fn export_filtered_history(
    source_git: &Path,
    export_stream: &Path,
    include: &[String],
    deny: &[String],
) -> Result<()> {
    let file = File::create(export_stream)?;
    let mut args = vec![
        "--git-dir".to_owned(),
        path_str(source_git)?.to_owned(),
        "fast-export".to_owned(),
        "--all".to_owned(),
        "--".to_owned(),
    ];
    args.extend(include.iter().cloned());
    args.extend(deny.iter().map(|path| format!(":(exclude){path}")));
    run_git_owned(args, None, Some(Stdio::from(file)), None)
}

fn import_filtered_history(filtered_git: &Path, export_stream: &Path) -> Result<()> {
    let file = File::open(export_stream)?;
    run_git_owned(
        vec![
            "--git-dir".to_owned(),
            path_str(filtered_git)?.to_owned(),
            "fast-import".to_owned(),
        ],
        None,
        None,
        Some(Stdio::from(file)),
    )
}

fn preserve_visible_head(source_git: &Path, filtered_git: &Path) -> Result<()> {
    let head = run_git_capture(
        ["--git-dir", path_str(source_git)?, "symbolic-ref", "HEAD"],
        None,
    )?;
    let head = head.trim();
    if head.is_empty() {
        return Ok(());
    }
    let visible = Command::new("git")
        .args([
            "--git-dir",
            path_str(filtered_git)?,
            "show-ref",
            "--verify",
            "--quiet",
            head,
        ])
        .status()
        .map_err(|e| AuthServerError::Internal(format!("git show-ref failed: {e}")))?;
    if visible.success() {
        run_git(
            [
                "--git-dir",
                path_str(filtered_git)?,
                "symbolic-ref",
                "HEAD",
                head,
            ],
            None,
        )?;
    }
    Ok(())
}

pub(super) fn count_pack_objects(pack: &[u8]) -> u64 {
    if pack.len() >= 12 && &pack[0..4] == b"PACK" {
        u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as u64
    } else {
        0
    }
}

pub(super) fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| AuthServerError::Internal("path is not valid UTF-8".to_owned()))
}

pub(super) fn run_git<'a, I>(args: I, cwd: Option<&Path>) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    run_git_owned(
        args.into_iter().map(ToOwned::to_owned).collect(),
        cwd,
        None,
        None,
    )
}

fn run_git_with_credentials<'a, I>(
    args: I,
    cwd: Option<&Path>,
    credentials: Option<&ViewS3Credentials>,
) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut command = Command::new("git");
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(credentials) = credentials {
        credentials.apply(&mut command);
    }
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| AuthServerError::Internal(format!("failed to run git: {error}")))?;
    if !output.status.success() {
        return Err(AuthServerError::AuthFailed {
            path: format!(
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

pub(super) fn run_git_capture<'a, I>(args: I, cwd: Option<&Path>) -> Result<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AuthServerError::Internal(format!("failed to run git: {e}")))?;
    if !output.status.success() {
        return Err(AuthServerError::AuthFailed {
            path: format!(
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|_| AuthServerError::Internal("git output was not valid UTF-8".to_owned()))
}

pub(super) fn run_git_capture_bytes<'a, I>(args: I, cwd: Option<&Path>) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AuthServerError::Internal(format!("failed to run git: {e}")))?;
    if !output.status.success() {
        return Err(AuthServerError::AuthFailed {
            path: format!(
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(output.stdout)
}

pub(super) fn run_git_owned(
    args: Vec<String>,
    cwd: Option<&Path>,
    stdout: Option<Stdio>,
    stdin: Option<Stdio>,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(&args).stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(stdout) = stdout {
        cmd.stdout(stdout);
    }
    if let Some(stdin) = stdin {
        cmd.stdin(stdin);
    }
    let output = cmd
        .output()
        .map_err(|e| AuthServerError::Internal(format!("failed to run git: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(AuthServerError::AuthFailed {
        path: format!(
            "git command failed: git {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn scan_reachable_pointers_detects_crab_and_lfs_pointers() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let bare = temp.path().join("repo.git");
        fs::create_dir_all(&work).unwrap();
        run_git(["init", path_str(&work).unwrap()], None).unwrap();
        run_git(
            [
                "-C",
                path_str(&work).unwrap(),
                "config",
                "user.email",
                "view-test@example.com",
            ],
            None,
        )
        .unwrap();
        run_git(
            [
                "-C",
                path_str(&work).unwrap(),
                "config",
                "user.name",
                "View Test",
            ],
            None,
        )
        .unwrap();

        let crab_hash = "1".repeat(64);
        fs::write(
            work.join("crab.bin"),
            format!("version https://crab.dev/spec/v1\nfile-hash {crab_hash}\nsize 12\n"),
        )
        .unwrap();
        fs::write(
            work.join("lfs.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 5\n",
                "2".repeat(64)
            ),
        )
        .unwrap();
        run_git(["-C", path_str(&work).unwrap(), "add", "."], None).unwrap();
        run_git(
            [
                "-C",
                path_str(&work).unwrap(),
                "commit",
                "-m",
                "add pointers",
            ],
            None,
        )
        .unwrap();
        run_git(
            [
                "clone",
                "--bare",
                path_str(&work).unwrap(),
                path_str(&bare).unwrap(),
            ],
            None,
        )
        .unwrap();

        let scan = scan_reachable_pointers(&bare).unwrap();

        assert_eq!(scan.crab_pointers.len(), 1);
        assert_eq!(scan.lfs_pointers.len(), 1);
        assert_eq!(scan.lfs_pointers[0].size, 5);
    }
}
