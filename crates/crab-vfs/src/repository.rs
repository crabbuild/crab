//! Git clone, ref selection and reconciliation for mounted repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{debug, error, info, warn};

use crate::core::error::{CrabError, Result};

/// Strip credentials from a URL before logging.
///
/// Replaces the username and password with `***`, preserving the host,
/// path, scheme, and query. Returns the original string unchanged if
/// parsing fails.
pub fn redact_url(url_str: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url_str) {
        if !parsed.username().is_empty() || parsed.password().is_some() {
            let _ = parsed.set_username("***");
            let _ = parsed.set_password(None);
        }
        parsed.to_string()
    } else {
        url_str.to_owned()
    }
}

/// One fetched Git ref, retaining annotated-tag identity and its mounted commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedRef {
    pub object_oid: String,
    pub commit_oid: String,
}

/// Trait for fetching remote refs.
///
/// Implementations make the selected objects available to snapshot preparation.
pub trait RemoteRefFetcher: Send + Sync {
    /// Fetch the ref object and its peeled commit from the remote.
    ///
    /// Returns `None` if the ref does not exist on the remote.
    fn fetch_ref(&self, ref_name: &str) -> Result<Option<FetchedRef>>;
}

/// Remote ref fetcher used when remote polling is not applicable.
pub struct NoopRemoteRefFetcher;

impl RemoteRefFetcher for NoopRemoteRefFetcher {
    fn fetch_ref(&self, _ref_name: &str) -> Result<Option<FetchedRef>> {
        Ok(None)
    }
}

/// Git-backed remote ref fetcher for blobless mount clones.
///
/// Fetches the tracked ref into the local git directory before returning its
/// OID so the refresh service can build a new snapshot from locally available
/// objects.
pub struct GitRemoteRefFetcher {
    git_dir: PathBuf,
    remote: String,
}

impl GitRemoteRefFetcher {
    pub fn new(git_dir: PathBuf) -> Self {
        Self {
            git_dir,
            remote: "origin".to_owned(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_remote(git_dir: PathBuf, remote: impl Into<String>) -> Self {
        Self {
            git_dir,
            remote: remote.into(),
        }
    }
}

impl RemoteRefFetcher for GitRemoteRefFetcher {
    fn fetch_ref(&self, ref_name: &str) -> Result<Option<FetchedRef>> {
        let ref_name = normalized_fetch_ref(ref_name)?;
        if ref_name == "HEAD" {
            self.fetch_head()
        } else {
            self.fetch_named_ref(&ref_name)
        }
    }
}

impl GitRemoteRefFetcher {
    fn fetch_named_ref(&self, ref_name: &str) -> Result<Option<FetchedRef>> {
        let tracking_ref = remote_tracking_ref(&self.remote, ref_name);
        let refspec = format!("+{ref_name}:{tracking_ref}");
        let output = Command::new("git")
            .arg("fetch")
            .arg("--filter=blob:none")
            .arg(&self.remote)
            .arg(&refspec)
            .env("GIT_DIR", &self.git_dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .map_err(CrabError::Io)?;

        if !output.status.success() {
            return git_fetch_error(&output);
        }

        resolve_fetched_ref(&self.git_dir, &tracking_ref)
    }

    fn fetch_head(&self) -> Result<Option<FetchedRef>> {
        let output = Command::new("git")
            .arg("fetch")
            .arg("--filter=blob:none")
            .arg(&self.remote)
            .arg("HEAD")
            .env("GIT_DIR", &self.git_dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .map_err(CrabError::Io)?;

        if !output.status.success() {
            return git_fetch_error(&output);
        }

        resolve_fetched_ref(&self.git_dir, "FETCH_HEAD")
    }
}

fn remote_tracking_ref(remote: &str, ref_name: &str) -> String {
    if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
        return format!("refs/remotes/{remote}/{branch}");
    }
    let suffix = ref_name.strip_prefix("refs/").unwrap_or(ref_name);
    format!("refs/crab/remotes/{remote}/{suffix}")
}

pub(crate) fn fetch_mount_ref(git_dir: &Path, reference: &str) -> Result<(String, String)> {
    let ref_name = normalized_fetch_ref(reference)?;
    let fetched = GitRemoteRefFetcher::new(git_dir.to_owned())
        .fetch_ref(&ref_name)?
        .ok_or_else(|| CrabError::NotFound {
            path: format!("remote ref {ref_name}"),
        })?;
    let oid = reconcile_fetched_ref(git_dir, &ref_name, &fetched)?;
    Ok((oid, ref_name))
}

pub fn normalized_fetch_ref(ref_name: &str) -> Result<String> {
    let trimmed = ref_name.trim();
    if trimmed.is_empty() {
        return Err(CrabError::Configuration {
            key: "remote ref cannot be empty".into(),
            origin: "mount refresh".into(),
        });
    }
    if trimmed == "HEAD" || trimmed.starts_with("refs/") {
        return Ok(trimmed.to_owned());
    }
    Ok(format!("refs/heads/{trimmed}"))
}

fn git_fetch_error(output: &std::process::Output) -> Result<Option<FetchedRef>> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if remote_ref_missing(&stderr) {
        return Ok(None);
    }
    Err(classify_git_error(stderr.trim()))
}

fn remote_ref_missing(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("couldn't find remote ref") || lower.contains("could not find remote ref")
}

fn resolve_git_ref(git_dir: &Path, ref_name: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--end-of-options", ref_name])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(CrabError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Needed a single revision") || stderr.contains("unknown revision") {
            return Ok(None);
        }
        let redacted = redact_url(stderr.trim());
        return Err(CrabError::Internal(format!(
            "failed to resolve fetched ref {ref_name}: {redacted}"
        )));
    }

    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if oid.is_empty() {
        return Err(CrabError::Internal(format!(
            "fetched ref {ref_name} resolved to an empty OID"
        )));
    }
    Ok(Some(oid))
}

fn resolve_fetched_ref(git_dir: &Path, ref_name: &str) -> Result<Option<FetchedRef>> {
    let Some(object_oid) = resolve_git_ref(git_dir, ref_name)? else {
        return Ok(None);
    };
    // Another fetch can advance the tracking ref between these queries.
    // Peel the captured object so tag identity and selected commit stay paired.
    let commit_revision = format!("{object_oid}^{{commit}}");
    let commit_oid =
        resolve_git_ref(git_dir, &commit_revision)?.ok_or_else(|| CrabError::NotFound {
            path: commit_revision,
        })?;
    Ok(Some(FetchedRef {
        object_oid,
        commit_oid,
    }))
}

pub fn commit_time_from_oid(git_dir: &Path, oid: &str) -> Option<i64> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct", oid])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<i64>()
        .ok()
}

pub(crate) fn resolve_mount_ref(
    git_dir: &Path,
    reference: Option<&str>,
) -> Result<(String, String)> {
    let reference = match reference.filter(|reference| *reference != "HEAD") {
        Some(reference) => normalized_fetch_ref(reference)?,
        None => crab_git::ref_resolve::resolve_head_symref_at(git_dir)?
            .unwrap_or_else(|| "HEAD".to_owned()),
    };
    let revision = format!("{reference}^{{commit}}");
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--end-of-options", &revision])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(CrabError::NotFound {
            path: format!("mount ref {reference}"),
        });
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((oid, reference))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitRelationship {
    Equal,
    LocalAncestor,
    RemoteAncestor,
    Diverged,
}

pub(crate) fn reconcile_fetched_ref(
    git_dir: &Path,
    ref_name: &str,
    fetched: &FetchedRef,
) -> Result<String> {
    if !ref_name.starts_with("refs/") {
        return Ok(fetched.commit_oid.clone());
    }
    let local_oid = resolve_git_ref(git_dir, ref_name)?;
    let Some(local_oid) = local_oid else {
        let absent_oid = "0".repeat(fetched.object_oid.len());
        update_local_ref(git_dir, ref_name, &fetched.object_oid, &absent_oid)?;
        return Ok(fetched.commit_oid.clone());
    };
    // Only branches carry unpublished mount commits. Other selected refs retain
    // the fetched object identity; peeling an annotated tag here would rewrite it.
    if !ref_name.starts_with("refs/heads/") {
        if local_oid != fetched.object_oid {
            update_local_ref(git_dir, ref_name, &fetched.object_oid, &local_oid)?;
        }
        return Ok(fetched.commit_oid.clone());
    }
    let remote_oid = &fetched.commit_oid;
    match commit_relationship(git_dir, &local_oid, remote_oid)? {
        CommitRelationship::Equal => Ok(remote_oid.to_owned()),
        CommitRelationship::LocalAncestor => {
            update_local_ref(git_dir, ref_name, remote_oid, &local_oid)?;
            Ok(remote_oid.to_owned())
        }
        CommitRelationship::RemoteAncestor => {
            warn!(
                ref_name,
                local_oid, remote_oid, "preserving unpushed local daemon commit"
            );
            Ok(local_oid)
        }
        CommitRelationship::Diverged => {
            warn!(
                ref_name,
                local_oid,
                remote_oid,
                "local daemon commit diverged from remote; preserving local snapshot"
            );
            Ok(local_oid)
        }
    }
}

fn commit_relationship(
    git_dir: &Path,
    local_oid: &str,
    remote_oid: &str,
) -> Result<CommitRelationship> {
    if local_oid == remote_oid {
        return Ok(CommitRelationship::Equal);
    }
    if git_is_ancestor(git_dir, local_oid, remote_oid)? {
        return Ok(CommitRelationship::LocalAncestor);
    }
    if git_is_ancestor(git_dir, remote_oid, local_oid)? {
        return Ok(CommitRelationship::RemoteAncestor);
    }
    Ok(CommitRelationship::Diverged)
}

fn git_is_ancestor(git_dir: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(ancestor)
        .arg(descendant)
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .status()
        .map_err(CrabError::Io)?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CrabError::Internal(format!(
            "git merge-base failed with {status}"
        ))),
    }
}

fn update_local_ref(git_dir: &Path, ref_name: &str, new_oid: &str, old_oid: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["update-ref", ref_name, new_oid, old_oid])
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(CrabError::Internal(format!(
        "git update-ref failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Ensure a blobless bare clone exists at `cache_dir` for the given URL.
///
/// If the cache directory already contains a bare git repo (detected by
/// the presence of a `HEAD` file), fetches the selected ref and preserves
/// unpublished branch commits. Cached clones can select refs absent from the
/// original single-branch clone.
/// Otherwise, performs a fresh blobless clone.
///
/// Returns the path to the git directory (same as `cache_dir` for bare repos).
///
/// # Auth
///
/// Repository-context variables are cleared for cloning; authentication
/// and configuration remain inherited. AWS credentials (`AWS_*`), GCP tokens (`GOOGLE_*`, `CLOUDSDK_*`),
/// Azure credentials (`AZURE_*`), and git credential helpers (`GIT_*`)
/// are automatically available to git for authentication.
pub fn ensure_blobless_clone(url: &str, branch: Option<&str>, cache_dir: &Path) -> Result<PathBuf> {
    let head_file = cache_dir.join("HEAD");

    if head_file.exists() {
        // Existing bare repo — fetch updates.
        info!(
            cache_dir = %cache_dir.display(),
            "updating existing blobless clone"
        );
        let ref_name = match branch.filter(|name| *name != "HEAD") {
            Some(name) => normalized_fetch_ref(name)?,
            None => resolve_mount_ref(cache_dir, None)?.1,
        };
        fetch_mount_ref(cache_dir, &ref_name)?;
    } else {
        // Fresh clone needed.
        info!(
            cache_dir = %cache_dir.display(),
            branch = branch.unwrap_or("HEAD"),
            "performing blobless clone"
        );
        git_clone_blobless(url, branch, cache_dir)?;
    }

    Ok(cache_dir.to_path_buf())
}

/// Perform a fresh blobless bare clone.
fn git_clone_blobless(url: &str, branch: Option<&str>, cache_dir: &Path) -> Result<()> {
    let parent = cache_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let replace_empty = match std::fs::symlink_metadata(cache_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() || std::fs::read_dir(cache_dir)?.next().transpose()?.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "clone destination is not an empty directory: {}",
                        cache_dir.display()
                    ),
                )
                .into());
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    // Git may leave partial files on failure. Only this private preparation
    // directory belongs to the clone; existing cache contents must survive.
    let prepared = tempfile::tempdir_in(parent)?;

    let mut args = vec!["clone", "--bare", "--filter=blob:none", "--single-branch"];

    // Published mounts also support missing remote HEAD through the advertised
    // main/master/sole-branch selection. Keep that policy shared by all clones.
    let branch_val = match branch.filter(|name| *name != "HEAD") {
        Some(name) => branch_arg_from_ref(name).to_owned(),
        None => resolve_default_clone_branch(url)?,
    };
    args.extend(["--branch", &branch_val]);

    args.push("--");
    args.push(url);

    debug!(url = %redact_url(url), "spawning git clone");

    let output = Command::new("git")
        .args(&args)
        .arg(prepared.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            error!(error = %e, "failed to spawn git clone");
            CrabError::Io(e)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err_msg = parse_git_error(&stderr);
        error!(error = %err_msg, "blobless clone failed");

        return Err(classify_git_error(&stderr));
    }

    if replace_empty {
        // rmdir fails if another owner populated the destination during cloning.
        std::fs::remove_dir(cache_dir)?;
    }
    std::fs::rename(prepared.path(), cache_dir)?;
    drop(prepared.keep());
    info!("blobless clone complete");
    Ok(())
}

fn branch_arg_from_ref(ref_name: &str) -> &str {
    ref_name
        .strip_prefix("refs/heads/")
        .or_else(|| ref_name.strip_prefix("refs/tags/"))
        .unwrap_or(ref_name)
}

fn resolve_default_clone_branch(source: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args([
            "ls-remote",
            "--symref",
            "--",
            source,
            "HEAD",
            "refs/heads/*",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(CrabError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let redacted_err = redact_url(stderr.trim());
        return Err(CrabError::Internal(format!(
            "failed to resolve remote default branch: {redacted_err}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (advertised_default, heads) = parse_ls_remote_heads(&stdout);
    select_default_clone_branch(advertised_default.as_deref(), &heads)
}

fn parse_ls_remote_heads(output: &str) -> (Option<String>, Vec<String>) {
    let mut advertised_default = None;
    let mut heads = Vec::new();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            let mut parts = rest.split_whitespace();
            if let (Some(target), Some(alias)) = (parts.next(), parts.next())
                && alias == "HEAD"
            {
                advertised_default = Some(branch_arg_from_ref(target).to_owned());
            }
            continue;
        }

        let mut parts = line.split_whitespace();
        let (Some(_oid), Some(ref_name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
            heads.push(branch.to_owned());
        }
    }

    heads.sort();
    heads.dedup();
    (advertised_default, heads)
}

fn select_default_clone_branch(default_branch: Option<&str>, heads: &[String]) -> Result<String> {
    if let Some(branch) = default_branch
        && !branch.is_empty()
    {
        return Ok(branch.to_owned());
    }

    if heads.iter().any(|branch| branch == "main") {
        return Ok("main".to_owned());
    }

    if heads.iter().any(|branch| branch == "master") {
        return Ok("master".to_owned());
    }

    if let [branch] = heads {
        return Ok(branch.clone());
    }

    Err(CrabError::Internal(
        "remote default branch is ambiguous; pass --ref to choose a branch".to_owned(),
    ))
}

/// Parse git stderr into a human-friendly error message.
fn parse_git_error(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();

    if is_auth_error(&lower) {
        "authentication failed: check your credentials".to_owned()
    } else if is_network_error(&lower) {
        "network error: could not reach remote".to_owned()
    } else if is_remote_not_found(&lower) {
        "remote not found: check the URL".to_owned()
    } else {
        // Fall back to the raw stderr (trimmed).
        stderr.trim().to_owned()
    }
}

/// Classify git stderr into a structured `CrabError`.
fn classify_git_error(stderr: &str) -> CrabError {
    let lower = stderr.to_ascii_lowercase();

    if is_auth_error(&lower) {
        CrabError::AuthFailed {
            path: "git remote".into(),
        }
    } else if is_network_error(&lower) {
        CrabError::Internal("network error: could not reach remote".into())
    } else if is_remote_not_found(&lower) {
        CrabError::NotFound {
            path: "git remote".into(),
        }
    } else {
        CrabError::Internal(format!("git operation failed: {}", stderr.trim()))
    }
}

/// Check if stderr indicates an authentication failure.
fn is_auth_error(stderr_lower: &str) -> bool {
    stderr_lower.contains("authentication failed")
        || stderr_lower.contains("could not read username")
        || stderr_lower.contains("invalid credentials")
        || stderr_lower.contains("401")
        || stderr_lower.contains("403")
        || stderr_lower.contains("permission denied")
        || stderr_lower.contains("access denied")
}

/// Check if stderr indicates a network error.
fn is_network_error(stderr_lower: &str) -> bool {
    stderr_lower.contains("could not resolve host")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("connection timed out")
        || stderr_lower.contains("network is unreachable")
        || stderr_lower.contains("no route to host")
        || stderr_lower.contains("ssl")
        || stderr_lower.contains("tls")
        || stderr_lower.contains("unable to access")
}

/// Check if stderr indicates the remote was not found.
fn is_remote_not_found(stderr_lower: &str) -> bool {
    stderr_lower.contains("repository not found")
        || stderr_lower.contains("does not appear to be a git repository")
        || stderr_lower.contains("not found")
        || stderr_lower.contains("404")
}

/// Populate the mount-owned index for a selected revision.
pub(crate) fn refresh_mount_index(
    git_dir: &Path,
    index_path: Option<&Path>,
    revision: &str,
) -> Result<()> {
    // Local repositories, including bare sources, own their staged state.
    // Only callers holding a mount-owned index may replace it.
    let Some(index_path) = index_path else {
        return Ok(());
    };
    let output = Command::new("git")
        .args(["read-tree", revision])
        .env("GIT_DIR", git_dir)
        .env("GIT_INDEX_FILE", index_path)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(CrabError::Internal(format!(
            "cannot populate mount index: {}",
            redact_url(String::from_utf8_lossy(&output.stderr).trim())
        )));
    }
    Ok(())
}

pub(crate) fn staging_root_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    let common_dir = crab_git::discover::resolve_common_dir(git_dir);
    let repo_root = common_dir.parent().ok_or_else(|| {
        CrabError::Internal(format!(
            "git common directory has no parent: {}",
            common_dir.display()
        ))
    })?;
    Ok(repo_root.join(".crab").join("staging"))
}

#[cfg(test)]
#[path = "repository_tests.rs"]
mod tests;
