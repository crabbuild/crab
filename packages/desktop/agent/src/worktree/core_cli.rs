//! Adapter for the core `crab worktree` JSON contract.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::process::Command;

use crate::{AgentError, Result};

#[cfg(test)]
#[derive(Clone)]
struct TestCrabBin {
    scope: PathBuf,
    bin: PathBuf,
}

#[cfg(test)]
static TEST_CRAB_BIN: std::sync::Mutex<Option<TestCrabBin>> = std::sync::Mutex::new(None);

#[derive(Debug, Deserialize)]
struct CrabEnvelope<T> {
    data: Option<T>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreeListPayload {
    pub worktrees: Vec<CrabWorktreeListEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreeListEntry {
    pub path: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub lock_reason: Option<String>,
    #[serde(default)]
    pub prunable: bool,
    #[serde(default)]
    pub crab: CrabWorktreeIdentity,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CrabWorktreeIdentity {
    #[serde(default)]
    pub identity: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreeAddPayload {
    pub created: bool,
    pub path: String,
    #[serde(default)]
    pub worktree: Option<CrabWorktreeListEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreePathPayload {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreeMovePayload {
    pub old_path: String,
    pub new_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreePrunePayload {
    pub dry_run: bool,
    pub pruned_lines: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CrabWorktreeRepairPayload {
    pub repaired: bool,
    pub paths: Vec<String>,
}

impl CrabWorktreeListEntry {
    pub(crate) fn is_main(&self) -> bool {
        self.crab.identity.as_deref() == Some("main")
    }
}

pub(crate) async fn list_worktrees(
    work_dir: &Path,
    with_crab_state: bool,
) -> Result<Vec<CrabWorktreeListEntry>> {
    let mut args = vec![
        "worktree".to_owned(),
        "list".to_owned(),
        "--json".to_owned(),
    ];
    if with_crab_state {
        args.push("--with-crab-state".to_owned());
    }
    let payload: CrabWorktreeListPayload = run_crab_json(work_dir, &args).await?;
    Ok(payload.worktrees)
}

pub(crate) async fn add_worktree(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreeAddPayload> {
    run_worktree_mutation_json(work_dir, "add", args).await
}

pub(crate) async fn remove_worktree(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreePathPayload> {
    run_worktree_mutation_json(work_dir, "remove", args).await
}

pub(crate) async fn move_worktree(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreeMovePayload> {
    run_worktree_mutation_json(work_dir, "move", args).await
}

pub(crate) async fn lock_worktree(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreePathPayload> {
    run_worktree_mutation_json(work_dir, "lock", args).await
}

pub(crate) async fn unlock_worktree(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreePathPayload> {
    run_worktree_mutation_json(work_dir, "unlock", args).await
}

pub(crate) async fn prune_worktrees(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreePrunePayload> {
    run_worktree_mutation_json(work_dir, "prune", args).await
}

pub(crate) async fn repair_worktrees(
    work_dir: &Path,
    args: &[String],
) -> Result<CrabWorktreeRepairPayload> {
    run_worktree_mutation_json(work_dir, "repair", args).await
}

async fn run_worktree_mutation_json<T: DeserializeOwned>(
    work_dir: &Path,
    verb: &str,
    verb_args: &[String],
) -> Result<T> {
    let mut args = vec!["worktree".to_owned(), "--json".to_owned(), verb.to_owned()];
    args.extend(verb_args.iter().cloned());
    run_crab_json(work_dir, &args).await
}

async fn run_crab_json<T: DeserializeOwned>(work_dir: &Path, args: &[String]) -> Result<T> {
    let bin = resolved_crab_bin(work_dir);
    let mut command = Command::new(&bin);
    command
        .args(args)
        .current_dir(work_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = crate::shellout::output(&mut command).await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let mut message = stderr.trim().to_owned();
        if message.is_empty() {
            message = stdout.trim().to_owned();
        }
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: message,
        });
    }

    let json_line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| {
            AgentError::Other(format!(
                "crab worktree JSON command returned no JSON on stdout: {}",
                stderr.trim()
            ))
        })?;
    let envelope: CrabEnvelope<T> = serde_json::from_str(json_line).map_err(AgentError::Decode)?;
    if let Some(data) = envelope.data {
        return Ok(data);
    }
    Err(AgentError::Other(format!(
        "crab worktree JSON command returned error envelope: {:?}",
        envelope.error
    )))
}

fn resolved_crab_bin(_work_dir: &Path) -> PathBuf {
    #[cfg(test)]
    {
        if let Ok(guard) = TEST_CRAB_BIN.lock()
            && let Some(test_bin) = guard.clone()
            && _work_dir.starts_with(&test_bin.scope)
        {
            return test_bin.bin;
        }
    }
    crate::shellout::resolve_crab_bin()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn install_fake_crab(response: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("argv.log");
        let bin = tmp.path().join("crab");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > '{}'\nprintf '%s\\n' '{}'\n",
            log.display(),
            response.replace('\'', "'\\''")
        );
        std::fs::write(&bin, script).expect("write fake crab");
        let mut perms = std::fs::metadata(&bin).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod fake crab");
        (tmp, log)
    }

    struct CrabBinGuard;

    impl Drop for CrabBinGuard {
        fn drop(&mut self) {
            *TEST_CRAB_BIN.lock().expect("test crab bin lock") = None;
        }
    }

    fn set_crab_bin(scope: &Path, path: &Path) -> CrabBinGuard {
        *TEST_CRAB_BIN.lock().expect("test crab bin lock") = Some(TestCrabBin {
            scope: scope.to_path_buf(),
            bin: path.to_path_buf(),
        });
        CrabBinGuard
    }

    #[tokio::test]
    async fn add_worktree_invokes_core_json_contract() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let response = r#"{"schema":"worktree.add","version":"1.0","timestamp":"t","data":{"created":true,"path":"/tmp/wt","worktree":{"path":"/tmp/wt","head":"abc","detached":true,"locked":false,"prunable":false,"crab":{"identity":"wt"}}}}"#;
        let (tmp, log) = install_fake_crab(response);
        let _env = set_crab_bin(tmp.path(), &tmp.path().join("crab"));

        let args = vec![
            "--hydrate=full".to_owned(),
            "/tmp/wt".to_owned(),
            "HEAD".to_owned(),
        ];
        let payload = add_worktree(tmp.path(), &args).await.expect("add payload");

        assert!(payload.created);
        assert_eq!(payload.path, "/tmp/wt");
        let argv = std::fs::read_to_string(log).expect("argv log");
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec![
                "worktree",
                "--json",
                "add",
                "--hydrate=full",
                "/tmp/wt",
                "HEAD"
            ]
        );
    }

    #[tokio::test]
    async fn repair_worktrees_parses_core_json_contract() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let response = r#"{"schema":"worktree.repair","version":"1.0","timestamp":"t","data":{"repaired":true,"paths":["/tmp/wt"]}}"#;
        let (tmp, log) = install_fake_crab(response);
        let _env = set_crab_bin(tmp.path(), &tmp.path().join("crab"));

        let payload = repair_worktrees(tmp.path(), &["/tmp/wt".to_owned()])
            .await
            .expect("repair payload");

        assert!(payload.repaired);
        assert_eq!(payload.paths, vec!["/tmp/wt"]);
        let argv = std::fs::read_to_string(log).expect("argv log");
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec!["worktree", "--json", "repair", "/tmp/wt"]
        );
    }
}
