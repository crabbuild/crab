use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AgentError, Result, shellout};

#[derive(Deserialize)]
pub(super) struct Params {
    pub repo_id: String,
    paths: Vec<String>,
    auto_commit: Option<bool>,
    commit_message: Option<String>,
}

pub(super) async fn run(root: &Path, params: Params) -> Result<Value> {
    if params.paths.is_empty() {
        return Err(AgentError::Other(
            "Select at least one file to track".into(),
        ));
    }
    configure_filters(root).await?;
    let mut seen = HashSet::new();
    let mut patterns = Vec::new();
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for path in &params.paths {
        if !seen.insert(path) {
            continue;
        }
        match track(root, path, &mut patterns).await {
            Ok(()) => files.push(path.clone()),
            Err(error) => errors.push(json!({ "path": path, "stderr": error.to_string() })),
        }
    }
    let mut commit_paths = files.clone();
    if !patterns.is_empty() {
        git(root, &["add", "--", ".gitattributes"]).await?;
    }
    // A retry may find the rule already present after an earlier partial
    // failure. Include its pending attributes so pointers remain portable.
    if !git(
        root,
        &["status", "--porcelain", "-z", "--", ".gitattributes"],
    )
    .await?
    .is_empty()
    {
        commit_paths.push(".gitattributes".into());
    }
    let mut commit_sha = None;
    if params.auto_commit == Some(true) && !files.is_empty() {
        let params = crate::git_commit::GitCommitParams {
            repo_id: params.repo_id,
            message: params
                .commit_message
                .unwrap_or_else(|| format!("Track {} large files", files.len())),
            paths: Some(commit_paths.clone()),
            amend: None,
            sign_off: None,
            sign: None,
            author_date: None,
            co_authored_by: None,
        };
        match crate::git_commit::git_commit(root, &params).await {
            Ok(commit) => commit_sha = commit.get("sha").cloned(),
            Err(error) => errors.push(json!({ "path": "_commit", "stderr": error.to_string() })),
        }
    }
    Ok(json!({
        "ok": errors.is_empty(), "patterns_added": patterns,
        "files_tracked": files.iter().map(|path| json!({ "path": path, "status": "ok" })).collect::<Vec<_>>(),
        "commit_paths": commit_paths, "errors": errors, "commit_sha": commit_sha,
    }))
}

async fn configure_filters(root: &Path) -> Result<()> {
    let git_name = if cfg!(windows) { "git.exe" } else { "git" };
    let git = executable(root, Path::new(git_name))?;
    let selected_crab = shellout::resolve_crab_bin();
    let crab = executable(root, &selected_crab)?;
    let tools = crab_sdk::local::Tools::new(git, crab)?;
    let local = crab_sdk::local::Options::new(tools);
    let token_cache = crate::repository::token_cache_directory()?;
    let managed = crab_sdk::managed::Options::new(token_cache)?;
    let client = crab_sdk::Client::builder()
        .managed(managed)
        .local(local)
        .build()?;
    // A plain Git clone has no drivers. The existing SDK owner installs the
    // paired CLI, preserving attribute routing and avoiding raw blobs in Git.
    let operation = client
        .configure_local(root)
        .await
        .map(drop)
        .map_err(AgentError::from);
    let cleanup = client.close().await.map_err(AgentError::from);
    match (operation, cleanup) {
        (Err(operation), Err(cleanup)) => Err(AgentError::Cleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn executable(root: &Path, program: &Path) -> Result<PathBuf> {
    if program.is_absolute() || program.components().count() > 1 {
        return Ok(root.join(program));
    }
    let search = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&search)
        .map(|directory| root.join(directory).join(program))
        .find(|candidate| {
            let Ok(metadata) = candidate.metadata() else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            true
        })
        .ok_or_else(|| AgentError::Other(format!("{} is unavailable on PATH", program.display())))
}

async fn track(root: &Path, path: &str, patterns: &mut Vec<String>) -> Result<()> {
    crab_sdk::GitPath::new(path.as_bytes().to_vec())?;
    if !Path::new(path)
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        || !tokio::fs::symlink_metadata(root.join(path))
            .await?
            .is_file()
        || tokio::fs::canonicalize(root.join(path)).await?
            != tokio::fs::canonicalize(root).await?.join(path)
    {
        return Err(AgentError::Other(
            "Select a regular repository file to track".into(),
        ));
    }
    let output = git(root, &["check-attr", "-z", "filter", "--", path]).await?;
    let mut filter = output
        .split(|byte| *byte == 0)
        .nth(2)
        .ok_or_else(|| AgentError::Other("Git returned no filter attribute".into()))?;
    if matches!(filter, b"unspecified" | b"unset" | b"set") {
        let pattern = attribute_pattern(path)?;
        let output = shellout::command()?
            .args(["track", "--", &pattern])
            .current_dir(root)
            .output()
            .await?;
        checked(output)?;
        patterns.push(pattern);
        filter = b"crab";
    }
    if !matches!(filter, b"crab" | b"lfs") {
        return Err(AgentError::Other(
            "The selected file uses another Git filter".into(),
        ));
    }
    // The configured clean filter owns Crab chunking and LFS identity. Literal
    // Git paths avoid a second glob-based staging pass expanding this selection.
    git(root, &["add", "--", path]).await?;
    // Capture one immutable object before probing size; an index update between
    // the probe and read must not turn a pointer check into an unbounded read.
    let indexed = format!(":{path}");
    let object = String::from_utf8(git(root, &["rev-parse", "--verify", &indexed]).await?)?
        .trim()
        .to_owned();
    let size = git(root, &["cat-file", "-s", &object]).await?;
    let size = String::from_utf8(size)?
        .trim()
        .parse::<u64>()
        .map_err(std::io::Error::other)?;
    if size < crab_git::MAX_LFS_POINTER_SIZE as u64 {
        let bytes = git(root, &["cat-file", "blob", &object]).await?;
        if matches!(
            (filter, crab_git::classify(&bytes)),
            (b"crab", crab_git::PointerKind::Crab(_)) | (b"lfs", crab_git::PointerKind::Lfs(_))
        ) {
            return Ok(());
        }
    }
    Err(AgentError::Other(
        "Git did not stage the expected pointer. Configure the file's clean filter and retry."
            .into(),
    ))
}

fn attribute_pattern(path: &str) -> Result<String> {
    let mut pattern = String::from("\"/");
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']') {
            pattern.push_str("\\\\");
        }
        match character {
            '\\' => pattern.push_str("\\\\"),
            '"' => pattern.push_str("\\\""),
            character if character.is_ascii_control() => {
                write!(pattern, "\\{:03o}", character as u8).map_err(std::io::Error::other)?
            }
            character => pattern.push(character),
        }
    }
    pattern.push('"');
    Ok(pattern)
}

async fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = shellout::git_command()?;
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root);
    checked(command.output().await?)
}

fn checked(output: std::process::Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into(),
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attribute_patterns_match_only_the_selected_paths() {
        let directory = tempfile::tempdir().expect("repository");
        let root = directory.path();
        git(root, &["init"]).await.expect("init");
        let mut paths = vec![
            "data[1].bin",
            "nested/data[1].bin",
            "with spaces/海.bin",
            "weights",
        ];
        if cfg!(unix) {
            paths.extend(["literal*?.bin", "a\\b.bin", "line\n\t\u{7f}.bin"]);
        }
        for path in paths {
            let pattern = attribute_pattern(path).expect("pattern");
            std::fs::write(
                root.join(".gitattributes"),
                format!("{pattern} filter=crab -text\n"),
            )
            .expect("attributes");
            let output = git(root, &["check-attr", "-z", "filter", "--", path])
                .await
                .expect("attribute");
            assert_eq!(
                output.split(|byte| *byte == 0).nth(2),
                Some(b"crab".as_slice()),
                "{path:?}"
            );
            for other in [
                format!("copy/{path}"),
                format!("{path}.backup"),
                path.replace("[1]", "1"),
            ] {
                if other == path {
                    continue;
                }
                let output = git(root, &["check-attr", "-z", "filter", "--", &other])
                    .await
                    .expect("other attribute");
                assert_ne!(
                    output.split(|byte| *byte == 0).nth(2),
                    Some(b"crab".as_slice()),
                    "{path:?} matched {other:?}"
                );
            }
        }
    }
}
