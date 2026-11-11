use std::path::{Path, PathBuf};
use std::process::Output;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use crate::{AgentError, Result, shellout};

#[derive(Clone, Copy)]
pub(crate) enum Operation {
    Stats,
    Clean { dry_run: bool },
}

impl Operation {
    fn schema(self) -> &'static str {
        match self {
            Self::Stats => "cache.stats",
            Self::Clean { .. } => "cache.clean",
        }
    }
}

fn command(cwd: Option<&Path>, operation: Operation) -> Result<Command> {
    let mut command = shellout::command()?;
    command
        .args([
            "cache",
            match operation {
                Operation::Stats => "stats",
                Operation::Clean { .. } => "clean",
            },
            "--json",
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if matches!(operation, Operation::Clean { dry_run: true }) {
        command.arg("--dry-run");
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    Ok(command)
}

pub(crate) async fn run(cwd: Option<&Path>, operation: Operation) -> Result<Value> {
    let output = command(cwd, operation)?.output().await?;
    let data = reply(output, operation)?;
    let Operation::Clean { .. } = operation else {
        return Ok(data);
    };
    let mut report: crab_cache::CacheCleanReport = serde_json::from_value(data)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let cleaned =
        crab_cache::clean_cache_namespaces(&content_root(), report.dry_run, &cancel).await?;
    report.files_removed = report.files_removed.saturating_add(cleaned.files_removed);
    report.bytes_reclaimed = report
        .bytes_reclaimed
        .saturating_add(cleaned.bytes_reclaimed);
    report.busy_entries = report.busy_entries.saturating_add(cleaned.busy_entries);
    report.unsafe_entries = report.unsafe_entries.saturating_add(cleaned.unsafe_entries);
    // CLI already counts desktop-content as one retained subtree. Its catalogs
    // and query workspaces remain, so do not count those descendants again.
    Ok(serde_json::to_value(report)?)
}

pub(crate) fn content_root() -> PathBuf {
    crab_cache::default_cache_root().join("desktop-content")
}

#[derive(Deserialize)]
struct Envelope {
    schema: String,
    version: String,
    data: Option<Value>,
    error: Option<Value>,
}

fn reply(output: Output, operation: Operation) -> Result<Value> {
    let failed = || AgentError::Cli {
        status: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    let envelope: Envelope = match serde_json::from_slice(&output.stdout) {
        Ok(envelope) => envelope,
        Err(_) if !output.status.success() => return Err(failed()),
        Err(error) => return Err(error.into()),
    };
    if envelope.schema != operation.schema() || envelope.version != "1.0" {
        return if output.status.success() {
            Err(AgentError::Other(
                "Crab returned an unexpected cache response".into(),
            ))
        } else {
            Err(failed())
        };
    }
    if let Some(error) = envelope.error {
        if let (Some(code), Some(message)) = (error["code"].as_str(), error["message"].as_str()) {
            return Err(AgentError::Command {
                status: output.status.code().unwrap_or(-1),
                code: code.into(),
                message: message.into(),
                details: error,
            });
        }
        return Err(AgentError::Other(
            "Crab returned an invalid cache error".into(),
        ));
    }
    let data = envelope
        .data
        .filter(Value::is_object)
        .ok_or_else(|| AgentError::Other("Crab returned no cache report".into()))?;
    // Stats deliberately exits 1 with a valid partial report. Preserve its
    // measured lower bounds and issues; cleanup never treats a failed exit as success.
    let partial = matches!(operation, Operation::Stats)
        && output.status.code() == Some(1)
        && (data["issues"]
            .as_array()
            .is_some_and(|issues| !issues.is_empty())
            || data["omitted_issues"]
                .as_u64()
                .is_some_and(|count| count > 0));
    if !output.status.success() && !partial {
        return Err(failed());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires CRAB_BIN pointing to the matching native runtime"]
    async fn native_cleanup_reports_usage_and_preserves_busy_and_retained_state() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let root = cwd.join("cache");
        let cache = crab_cache::LocalCache::new(root.clone());
        let key = crab_cache::CacheKey::RefTransaction(blake3::hash(b"data"));
        cache.put(&key, b"data").await.unwrap();
        let retained = root.join("retained-workspace/user-file");
        crab_cache::ensure_private_cache_directory(retained.parent().unwrap()).unwrap();
        std::fs::write(&retained, b"keep").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&retained, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::create_dir_all(cwd.join(".crab")).unwrap();
        std::fs::write(
            cwd.join(".crab/config.toml"),
            toml::to_string(&json!({
                "cache": { "chunk_cache_dir": root.join("chunks") }
            }))
            .unwrap(),
        )
        .unwrap();
        let run = |operation| {
            let root = &root;
            async move {
                let output = command(Some(cwd), operation)?
                    .env("CRAB_CACHE_DIR", root)
                    .output()
                    .await?;
                reply(output, operation)
            }
        };
        crab_cache::ensure_private_cache_directory(&root.join("mirrors")).unwrap();
        let owner = crab_cache::lifecycle::CacheUseGuard::acquire(
            &root.join("mirrors/repository.git"),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let error = run(Operation::Clean { dry_run: false }).await.unwrap_err();
        assert!(matches!(error, AgentError::Command { .. }));
        assert!(cache.contains(&key).await);
        drop(owner);
        let stats = run(Operation::Stats).await.unwrap();
        assert!(stats["observed"]["logical_bytes"].as_u64().unwrap() >= 8);
        let preview = run(Operation::Clean { dry_run: true }).await.unwrap();
        assert_eq!(preview["bytes_reclaimed"], 4);
        assert!(cache.contains(&key).await);
        let actual = run(Operation::Clean { dry_run: false }).await.unwrap();
        assert_eq!(actual["bytes_reclaimed"], 4);
        assert!(!cache.contains(&key).await);
        assert_eq!(std::fs::read(retained).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn partial_inspection_keeps_issues_but_failed_cleanup_is_never_success() {
        use std::os::unix::process::ExitStatusExt;
        let data = json!({ "issues": [{ "error": "busy" }], "omitted_issues": 0 });
        for operation in [Operation::Stats, Operation::Clean { dry_run: false }] {
            let output = Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: serde_json::to_vec(
                    &json!({ "schema": operation.schema(), "version": "1.0", "data": data }),
                )
                .unwrap(),
                stderr: b"inspection unavailable".to_vec(),
            };
            let result = reply(output, operation);
            match operation {
                Operation::Stats => assert_eq!(result.unwrap(), data),
                Operation::Clean { .. } => assert!(result.is_err()),
            }
        }
    }
}
