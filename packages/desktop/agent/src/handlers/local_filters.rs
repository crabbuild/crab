use std::path::{Path, PathBuf};

use crate::{AgentError, Result, shellout};

pub(super) async fn configure(root: &Path, options: crab_sdk::operation::Options) -> Result<()> {
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
    // Explicit working-tree writes need the paired clean filters, including in
    // plain Git clones. Otherwise verified hydration becomes raw binary Git
    // content when its stat cache is invalidated.
    let operation = client
        .configure_local(root)
        .with_options(options)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
    async fn materialization_configuration_cancellation_preserves_git_settings() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let output = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let path = root.join(".git/config");
        let before = std::fs::read(&path).unwrap();
        let cancellation = crab_sdk::operation::Cancellation::default();
        cancellation.cancel();
        let result = configure(
            root,
            crab_sdk::operation::Options::default().with_cancellation(cancellation),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}
