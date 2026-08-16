//! Hermetic workflow command sandboxing.

use std::path::{Component, Path, PathBuf};

use tokio::process::Command;

use crate::stage::{Dep, OutKind, Stage};
use crate::{Result, WorkflowError as CrabError};

pub const HERMETIC_SANDBOX_POLICY_VERSION: u16 = 1;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathRuleKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathRule {
    path: PathBuf,
    kind: PathRuleKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HermeticSandboxPolicy {
    stage: String,
    repo_root: PathBuf,
    cwd: PathBuf,
    read_paths: Vec<PathRule>,
    write_paths: Vec<PathRule>,
    temp_paths: Vec<PathBuf>,
}

impl HermeticSandboxPolicy {
    pub fn for_stage(
        stage: &Stage,
        repo_root: Option<&Path>,
        cwd: Option<&Path>,
        workflow_root: &Path,
    ) -> Result<Self> {
        let repo_root = match repo_root {
            Some(path) => normalize_absolute(path)?,
            None => normalize_absolute(&std::env::current_dir().map_err(CrabError::Io)?)?,
        };
        let cwd = match cwd {
            Some(path) => normalize_absolute(path)?,
            None => repo_root.clone(),
        };

        let mut read_paths = Vec::new();
        let mut write_paths = Vec::new();
        for dep in &stage.deps {
            if let Some(path) = dep_path(dep) {
                read_paths.push(PathRule {
                    path: resolve_stage_path(&cwd, path)?,
                    kind: detected_rule_kind(&cwd, path, PathRuleKind::File),
                });
            }
        }
        for param in &stage.params {
            let path = param
                .file()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("params.yaml"));
            read_paths.push(PathRule {
                path: resolve_stage_path(&cwd, &path)?,
                kind: detected_rule_kind(&cwd, &path, PathRuleKind::File),
            });
        }
        for out in &stage.outs {
            if out.is_external_url() {
                continue;
            }
            let kind = match out.kind {
                OutKind::Directory => PathRuleKind::Directory,
                OutKind::File | OutKind::Stdout => PathRuleKind::File,
            };
            write_paths.push(PathRule {
                path: resolve_stage_path(&cwd, &out.path)?,
                kind,
            });
        }
        for path in stage.metrics.iter().chain(stage.plots.iter()) {
            write_paths.push(PathRule {
                path: resolve_stage_path(&cwd, path)?,
                kind: detected_rule_kind(&cwd, path, PathRuleKind::File),
            });
        }

        let temp_dir = normalize_absolute(
            &workflow_root
                .join("sandbox-tmp")
                .join(sandbox_stage_name(stage.name.as_str())),
        )?;
        std::fs::create_dir_all(&temp_dir).map_err(CrabError::Io)?;
        let mut temp_paths = vec![temp_dir];
        temp_paths.sort();
        temp_paths.dedup();

        read_paths.sort_by(|a, b| a.path.cmp(&b.path));
        read_paths.dedup();
        write_paths.sort_by(|a, b| a.path.cmp(&b.path));
        write_paths.dedup();

        Ok(Self {
            stage: stage.name.as_str().to_owned(),
            repo_root,
            cwd,
            read_paths,
            write_paths,
            temp_paths,
        })
    }

    pub fn stage(&self) -> &str {
        &self.stage
    }

    pub fn temp_dir(&self) -> &Path {
        self.temp_paths
            .first()
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("/tmp"))
    }

    pub fn wrap_command(&self, program: &str, args: &[String]) -> Result<Command> {
        ensure_supported(&self.stage)?;
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(self.profile()).arg("--").arg(program);
        command.args(args);
        Ok(command)
    }

    pub fn violation_path(&self, stderr: &str) -> Option<PathBuf> {
        let raw =
            parse_sandbox_report_path(stderr).or_else(|| parse_permission_denied_path(stderr))?;
        let path = Path::new(raw.trim_matches(['\'', '"']));
        if path.as_os_str().is_empty() {
            return None;
        }
        Some(if path.is_absolute() {
            normalize_lexical(path)
        } else {
            normalize_lexical(&self.cwd.join(path))
        })
    }

    fn profile(&self) -> String {
        let mut lines = vec![
            "(version 1)".to_owned(),
            "(deny default)".to_owned(),
            "(allow process*)".to_owned(),
            "(allow signal)".to_owned(),
            "(allow sysctl*)".to_owned(),
            "(allow mach-lookup)".to_owned(),
            "(allow file-read-metadata)".to_owned(),
        ];

        for path in system_read_paths() {
            lines.push(format!("(allow file-read* (subpath {}))", quoted(path)));
        }
        lines.push(format!(
            "(allow file-read-data (literal {}))",
            quoted(Path::new("/"))
        ));
        lines.push(format!(
            "(allow file-read-data (literal {}))",
            quoted(Path::new("/dev/null"))
        ));
        lines.push(format!(
            "(allow file-write* (literal {}))",
            quoted(Path::new("/dev/null"))
        ));

        for path in std::iter::once(&self.repo_root)
            .chain(std::iter::once(&self.cwd))
            .chain(self.temp_paths.iter())
        {
            lines.push(format!(
                "(allow file-read-metadata (subpath {}))",
                quoted(path)
            ));
        }

        for rule in self.read_paths.iter().chain(self.write_paths.iter()) {
            lines.push(read_rule(rule));
        }
        for rule in &self.write_paths {
            lines.push(write_rule(rule));
        }
        for path in &self.temp_paths {
            lines.push(format!("(allow file-read* (subpath {}))", quoted(path)));
            lines.push(format!("(allow file-write* (subpath {}))", quoted(path)));
        }

        lines.join("\n")
    }
}

pub fn ensure_supported(stage: &str) -> Result<()> {
    if cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).is_file() {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: format!("stage '{stage}' hermetic"),
        origin: "hermetic workflow execution requires macOS sandbox-exec in this build".to_owned(),
    })
}

fn dep_path(dep: &Dep) -> Option<&Path> {
    match dep {
        Dep::Path(path)
        | Dep::CrabRef { path, .. }
        | Dep::GitRef { path, .. }
        | Dep::StageOut { out: path, .. } => Some(path.as_path()),
        Dep::Url { .. } | Dep::OciImage { .. } => None,
    }
}

fn detected_rule_kind(base: &Path, path: &Path, fallback: PathRuleKind) -> PathRuleKind {
    let full_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    match std::fs::metadata(full_path) {
        Ok(metadata) if metadata.is_dir() => PathRuleKind::Directory,
        Ok(_) => PathRuleKind::File,
        Err(_) => fallback,
    }
}

fn resolve_stage_path(base: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_absolute(&path)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(CrabError::Io)?.join(path)
    };
    canonicalize_existing_prefix(&absolute)
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(normalize_lexical(&canonical));
    }

    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        if let Some(name) = existing.file_name() {
            missing.push(name.to_owned());
        }
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }

    let mut canonical = if existing.exists() {
        std::fs::canonicalize(existing).map_err(CrabError::Io)?
    } else {
        path.to_path_buf()
    };
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(normalize_lexical(&canonical))
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn system_read_paths() -> &'static [PathBuf] {
    use std::sync::OnceLock;

    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        [
            "/System",
            "/Library",
            "/usr",
            "/bin",
            "/sbin",
            "/private/var/db",
            "/private/etc",
            "/etc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    })
}

fn read_rule(rule: &PathRule) -> String {
    match rule.kind {
        PathRuleKind::File => format!("(allow file-read-data (literal {}))", quoted(&rule.path)),
        PathRuleKind::Directory => {
            format!("(allow file-read-data (subpath {}))", quoted(&rule.path))
        }
    }
}

fn write_rule(rule: &PathRule) -> String {
    match rule.kind {
        PathRuleKind::File => format!("(allow file-write* (literal {}))", quoted(&rule.path)),
        PathRuleKind::Directory => format!("(allow file-write* (subpath {}))", quoted(&rule.path)),
    }
}

fn quoted(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn sandbox_stage_name(stage: &str) -> String {
    stage
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_sandbox_report_path(stderr: &str) -> Option<&str> {
    stderr.lines().rev().find_map(|line| {
        if !line.contains("deny(") {
            return None;
        }
        line.split_whitespace()
            .rev()
            .find(|part| part.starts_with('/'))
    })
}

fn parse_permission_denied_path(stderr: &str) -> Option<&str> {
    stderr.lines().rev().find_map(|line| {
        let prefix = line
            .split(": Operation not permitted")
            .next()
            .or_else(|| line.split(": Permission denied").next())?;
        prefix.rsplit(": ").next()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::{Cmd, Dep, Out, OutKind, Stage, StageName};
    use tempfile::TempDir;

    fn test_stage() -> Stage {
        let mut stage = Stage::new(
            StageName::parse("prep").unwrap(),
            Cmd::Shell("cat input.txt > output.txt".to_owned()),
        );
        stage.hermetic = true;
        stage.deps.push(Dep::Path(PathBuf::from("input.txt")));
        stage
            .outs
            .push(Out::new(PathBuf::from("output.txt"), OutKind::File));
        stage
    }

    #[test]
    fn profile_allows_declared_dep_and_out_without_repo_read_access() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("input.txt"), b"ok").unwrap();
        let workflow_root = tmp.path().join(".crab/workflow");
        std::fs::create_dir_all(&workflow_root).unwrap();

        let policy = HermeticSandboxPolicy::for_stage(
            &test_stage(),
            Some(tmp.path()),
            Some(tmp.path()),
            &workflow_root,
        )
        .unwrap();
        let profile = policy.profile();

        assert!(profile.contains("input.txt"));
        assert!(profile.contains("output.txt"));
        assert!(!profile.contains(&format!(
            "(allow file-read-data (subpath {}))",
            quoted(tmp.path())
        )));
    }

    #[test]
    fn parses_permission_denied_path_relative_to_cwd() {
        let tmp = TempDir::new().unwrap();
        let workflow_root = tmp.path().join(".crab/workflow");
        std::fs::create_dir_all(&workflow_root).unwrap();
        let policy = HermeticSandboxPolicy::for_stage(
            &test_stage(),
            Some(tmp.path()),
            Some(tmp.path()),
            &workflow_root,
        )
        .unwrap();

        let path = policy
            .violation_path("cat: secret.txt: Operation not permitted")
            .unwrap();

        assert_eq!(
            path,
            std::fs::canonicalize(tmp.path())
                .unwrap()
                .join("secret.txt")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_exec_denies_undeclared_repo_reads() {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return;
        }

        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("input.txt"), b"ok").unwrap();
        std::fs::write(tmp.path().join("secret.txt"), b"secret").unwrap();
        let workflow_root = tmp.path().join(".crab/workflow");
        std::fs::create_dir_all(&workflow_root).unwrap();
        let policy = HermeticSandboxPolicy::for_stage(
            &test_stage(),
            Some(tmp.path()),
            Some(tmp.path()),
            &workflow_root,
        )
        .unwrap();

        let output = std::process::Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(policy.profile())
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("cat input.txt > output.txt")
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let output = std::process::Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(policy.profile())
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("cat secret.txt >/dev/null")
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}
