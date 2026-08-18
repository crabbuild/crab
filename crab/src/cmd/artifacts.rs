//! Git-native immutable artifact versions and promotion labels.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use clap::{Args, Subcommand};
use fs4::fs_std::FileExt as LockFileExt;
use serde::Serialize;

use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crab_workflow::{
    ArtifactCatalog, ArtifactDecl, ArtifactManifest, ArtifactRegistry, Lockfile,
    artifact_stage_ref, artifact_version_ref, manifest_from_path, parse_yaml, snapshot_payload,
    verify_payload,
};

pub const ARTIFACTS_SCHEMA: &str = "artifacts";
const ARTIFACTS_VERSION_SCHEMA: &str = "artifacts.version";

/// Artifacts command family.
#[derive(Debug, Subcommand)]
pub enum ArtifactsCommand {
    /// List declarations, immutable versions, and stage labels.
    List(ArtifactListArgs),
    /// Show one declaration and its versions and labels.
    Show(ArtifactShowArgs),
    /// Copy one immutable version or stage-selected payload.
    Get(ArtifactGetArgs),
    /// Create an immutable version.
    Version {
        #[command(subcommand)]
        command: ArtifactVersionCommand,
    },
    /// Move a stage label to an immutable version.
    Promote(ArtifactPromoteArgs),
    /// Show promotion history.
    History(ArtifactHistoryArgs),
}

#[derive(Debug, Subcommand)]
pub enum ArtifactVersionCommand {
    /// Create a version from a declared working-tree path.
    Create(ArtifactVersionCreateArgs),
}

#[derive(Debug, Args)]
pub struct ArtifactListArgs {
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactShowArgs {
    pub name: String,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactGetArgs {
    pub name: String,
    #[arg(long, conflicts_with = "stage")]
    pub version: Option<String>,
    #[arg(long, conflicts_with = "version")]
    pub stage: Option<String>,
    #[arg(long, short)]
    pub output: Option<PathBuf>,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactVersionCreateArgs {
    pub name: String,
    #[arg(long)]
    pub path: Option<PathBuf>,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactPromoteArgs {
    pub name: String,
    pub version: String,
    pub stage: String,
    #[arg(long)]
    pub expected: Option<String>,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactHistoryArgs {
    pub name: String,
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

impl ArtifactsCommand {
    /// Return the structured-output schema for this subcommand.
    pub fn schema_name(&self) -> &'static str {
        match self {
            Self::Version { .. } => ARTIFACTS_VERSION_SCHEMA,
            _ => ARTIFACTS_SCHEMA,
        }
    }

    pub fn output_mode(&self) -> OutputMode {
        match self {
            Self::List(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Show(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Get(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::Version { command } => match command {
                ArtifactVersionCommand::Create(args) => {
                    OutputMode::from_flags(args.json, args.jsonl)
                }
            },
            Self::Promote(args) => OutputMode::from_flags(args.json, args.jsonl),
            Self::History(args) => OutputMode::from_flags(args.json, args.jsonl),
        }
    }

    pub fn run(self) -> Result<()> {
        let root = std::env::current_dir().map_err(CrabError::Io)?;
        match self {
            Self::List(args) => run_list(&root, &args),
            Self::Show(args) => run_show(&root, &args),
            Self::Get(args) => run_get(&root, &args),
            Self::Version { command } => match command {
                ArtifactVersionCommand::Create(args) => run_version_create(&root, &args),
            },
            Self::Promote(args) => run_promote(&root, &args),
            Self::History(args) => run_history(&root, &args),
        }
    }
}

#[derive(Debug, Serialize)]
struct ArtifactListPayload {
    catalog: ArtifactCatalog,
    registry: ArtifactRegistry,
}

#[derive(Debug, Serialize)]
struct ArtifactShowPayload {
    declaration: ArtifactDecl,
    versions: Vec<ArtifactManifest>,
    stages: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ArtifactVersionPayload {
    manifest: ArtifactManifest,
    version_ref: String,
}

#[derive(Debug, Serialize)]
struct ArtifactPromotionPayload {
    name: String,
    stage: String,
    version_id: String,
    stage_ref: String,
}

#[derive(Debug, Serialize)]
struct ArtifactGetPayload {
    name: String,
    version_id: String,
    output: String,
    content_hash: String,
    size: u64,
}

fn run_list(root: &Path, args: &ArtifactListArgs) -> Result<()> {
    let (catalog, registry, _) = load_state(root)?;
    let payload = ArtifactListPayload { catalog, registry };
    emit_payload(
        args.json,
        args.jsonl,
        ARTIFACTS_SCHEMA,
        payload,
        |payload| {
            println!("Artifacts:");
            for (name, declaration) in &payload.catalog.declarations {
                let count = payload.registry.versions.get(name).map_or(0, BTreeMap::len);
                println!("  {name} ({}, {count} versions)", declaration.path);
            }
        },
    );
    Ok(())
}

fn run_show(root: &Path, args: &ArtifactShowArgs) -> Result<()> {
    let (catalog, registry, _) = load_state(root)?;
    let declaration = catalog
        .declarations
        .get(&args.name)
        .cloned()
        .ok_or_else(|| CrabError::Configuration {
            key: "artifact_not_declared".into(),
            origin: args.name.clone(),
        })?;
    let versions = registry
        .versions
        .get(&args.name)
        .map(|values| values.values().cloned().collect())
        .unwrap_or_default();
    let stages = registry.stages.get(&args.name).cloned().unwrap_or_default();
    let payload = ArtifactShowPayload {
        declaration,
        versions,
        stages,
    };
    emit_payload(
        args.json,
        args.jsonl,
        ARTIFACTS_SCHEMA,
        payload,
        |payload| {
            println!("Artifact: {}", payload.declaration.name);
            println!("Path: {}", payload.declaration.path);
            println!("Versions: {}", payload.versions.len());
            for (stage, version) in &payload.stages {
                println!("  {stage}: {version}");
            }
        },
    );
    Ok(())
}

fn run_version_create(root: &Path, args: &ArtifactVersionCreateArgs) -> Result<()> {
    let registry_path = registry_path(root);
    let _registry_lock = lock_registry(&registry_path)?;
    let (catalog, mut registry, registry_path) = load_state(root)?;
    let declaration = catalog
        .declarations
        .get(&args.name)
        .cloned()
        .ok_or_else(|| CrabError::Configuration {
            key: "artifact_not_declared".into(),
            origin: args.name.clone(),
        })?;
    let declaration = args
        .path
        .as_ref()
        .map_or(declaration.clone(), |path| ArtifactDecl {
            path: path.to_string_lossy().replace('\\', "/"),
            ..declaration
        });
    ensure_safe_relative_path(Path::new(&declaration.path), "artifact_path_invalid")?;
    let _ = safe_output_path(root, Path::new(&declaration.path))?;
    ensure_clean_path(root, Path::new(&declaration.path))?;
    let (manifest, source_path) = manifest_from_path(root, &declaration, git_head(root))?;
    ensure_artifact_source(root, &declaration.path, &manifest.content_hash)?;
    let payload_path = version_payload_path(&registry_path, &manifest.version_id);
    let payload_existed = match fs::symlink_metadata(&payload_path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(CrabError::Io(error)),
    };
    if let Err(error) = snapshot_payload(&source_path, &payload_path)
        .and_then(|()| verify_payload(&payload_path, &manifest.content_hash, manifest.size))
    {
        if !payload_existed {
            let _ = remove_existing_path(&payload_path);
        }
        return Err(error.into());
    }
    if let Err(error) = registry
        .insert_version(manifest.clone())
        .and_then(|()| registry.save_atomic(&registry_path))
    {
        if !payload_existed {
            if let Err(cleanup) = remove_existing_path(&payload_path) {
                return Err(CrabError::Configuration {
                    key: "artifact_publication_cleanup_failed".into(),
                    origin: format!("publication failed: {error}; cleanup failed: {cleanup}"),
                });
            }
        }
        return Err(error.into());
    }
    let payload = ArtifactVersionPayload {
        version_ref: artifact_version_ref(&manifest.name, &manifest.version_id)?,
        manifest,
    };
    emit_payload(
        args.json,
        args.jsonl,
        ARTIFACTS_VERSION_SCHEMA,
        payload,
        |payload| {
            println!("Created {}", payload.manifest.version_id);
            println!("Ref: {}", payload.version_ref);
        },
    );
    Ok(())
}

fn run_promote(root: &Path, args: &ArtifactPromoteArgs) -> Result<()> {
    let registry_path = registry_path(root);
    let _registry_lock = lock_registry(&registry_path)?;
    let (_, mut registry, registry_path) = load_state(root)?;
    registry.promote(
        &args.name,
        &args.version,
        &args.stage,
        args.expected.as_deref(),
    )?;
    registry.save_atomic(&registry_path)?;
    let payload = ArtifactPromotionPayload {
        name: args.name.clone(),
        stage: args.stage.to_ascii_lowercase(),
        version_id: args.version.clone(),
        stage_ref: artifact_stage_ref(&args.name, &args.stage)?,
    };
    emit_payload(
        args.json,
        args.jsonl,
        ARTIFACTS_SCHEMA,
        payload,
        |payload| {
            println!(
                "Promoted {} to {} ({})",
                payload.name, payload.stage, payload.version_id
            );
        },
    );
    Ok(())
}

fn run_get(root: &Path, args: &ArtifactGetArgs) -> Result<()> {
    let (_, registry, registry_path) = load_state(root)?;
    let manifest = registry
        .resolve(&args.name, args.version.as_deref(), args.stage.as_deref())?
        .clone();
    let source = version_payload_path(&registry_path, &manifest.version_id);
    match fs::symlink_metadata(&source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CrabError::Configuration {
                key: "artifact_payload_missing".into(),
                origin: source.display().to_string(),
            });
        }
        Err(error) => return Err(CrabError::Io(error)),
    }
    verify_payload(&source, &manifest.content_hash, manifest.size).map_err(|error| {
        CrabError::Configuration {
            key: "artifact_payload_integrity".into(),
            origin: error.to_string(),
        }
    })?;
    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(
            Path::new(&manifest.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("artifact"),
        )
    });
    let output = safe_output_path(root, &output)?;
    materialize_payload(&source, &output, &manifest.content_hash, manifest.size)?;
    let output_display = output.strip_prefix(root).map_or_else(
        |_| output.display().to_string(),
        |path| path.display().to_string(),
    );
    let payload = ArtifactGetPayload {
        name: args.name.clone(),
        version_id: manifest.version_id,
        output: output_display,
        content_hash: manifest.content_hash,
        size: manifest.size,
    };
    emit_payload(
        args.json,
        args.jsonl,
        ARTIFACTS_SCHEMA,
        payload,
        |payload| {
            println!("Wrote {} ({})", payload.output, payload.version_id);
        },
    );
    Ok(())
}

fn run_history(root: &Path, args: &ArtifactHistoryArgs) -> Result<()> {
    let (_, registry, _) = load_state(root)?;
    let history = registry
        .history
        .iter()
        .filter(|event| event.name == args.name)
        .cloned()
        .collect::<Vec<_>>();
    emit_payload(
        args.json,
        args.jsonl,
        ARTIFACTS_SCHEMA,
        history,
        |history| {
            for event in history {
                println!(
                    "{} {} {}",
                    event.created_at_unix_ms, event.stage, event.version_id
                );
            }
        },
    );
    Ok(())
}

fn load_state(root: &Path) -> Result<(ArtifactCatalog, ArtifactRegistry, PathBuf)> {
    let workflow = parse_yaml(&fs::read_to_string(root.join("crab.yaml")).map_err(CrabError::Io)?)?;
    let catalog = ArtifactCatalog::from_metadata(&workflow.artifacts)?;
    let registry_path = registry_path(root);
    let registry = ArtifactRegistry::load(&registry_path)?;
    Ok((catalog, registry, registry_path))
}

fn registry_path(root: &Path) -> PathBuf {
    root.join(".crab/workflow/artifacts/registry.json")
}

fn lock_registry(path: &Path) -> Result<File> {
    let parent = path.parent().ok_or_else(|| CrabError::Configuration {
        key: "artifact_registry_path_invalid".into(),
        origin: path.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let lock_path = path.with_file_name(format!(
        ".{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("registry")
    ));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .map_err(CrabError::Io)?;
    LockFileExt::lock_exclusive(&file).map_err(CrabError::Io)?;
    Ok(file)
}

fn version_payload_path(registry_path: &Path, version_id: &str) -> PathBuf {
    registry_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("versions")
        .join(version_id.replace(':', "_"))
        .join("payload")
}

fn ensure_clean_path(root: &Path, path: &Path) -> Result<()> {
    let full = root.join(path);
    if !full.exists() {
        return Err(CrabError::Configuration {
            key: "artifact_path_missing".into(),
            origin: path.display().to_string(),
        });
    }
    for args in [["diff", "--quiet", "--"], ["diff", "--cached", "--"]] {
        let output = Command::new("git")
            .args(args)
            .arg(path)
            .current_dir(root)
            .output()
            .map_err(CrabError::Io)?;
        if !output.status.success() {
            return Err(CrabError::Configuration {
                key: "artifact_output_dirty".into(),
                origin: path.display().to_string(),
            });
        }
    }
    Ok(())
}

fn ensure_artifact_source(root: &Path, path: &str, content_hash: &str) -> Result<()> {
    let workflow = parse_yaml(&fs::read_to_string(root.join("crab.yaml")).map_err(CrabError::Io)?)?;
    let declared = workflow.stages.values().any(|stage| {
        stage
            .outs
            .iter()
            .any(|output| output.path.to_string_lossy().replace('\\', "/") == path)
    });
    let tracked = Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", path])
        .current_dir(root)
        .output()
        .is_ok_and(|output| output.status.success());
    if !declared && !tracked {
        return Err(CrabError::Configuration {
            key: "artifact_source_untracked".into(),
            origin: path.to_owned(),
        });
    }

    let lock =
        Lockfile::load(&root.join("crab.lock")).map_err(|error| CrabError::Configuration {
            key: "artifact_lock_unavailable".into(),
            origin: error.to_string(),
        })?;
    let expected = lock
        .stages
        .values()
        .flat_map(|stage| stage.outs.iter())
        .find(|output| output.path.to_string_lossy().replace('\\', "/") == path)
        .map(|output| format!("b3:{}", blake3::Hash::from(output.hash).to_hex()));
    if expected.as_deref() != Some(content_hash) {
        return Err(CrabError::Configuration {
            key: "artifact_lock_mismatch".into(),
            origin: path.to_owned(),
        });
    }
    Ok(())
}

fn ensure_safe_relative_path(path: &Path, error_key: &str) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CrabError::Configuration {
            key: error_key.to_owned(),
            origin: path.display().to_string(),
        });
    }
    Ok(())
}

fn safe_output_path(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure_safe_relative_path(path, "artifact_destination_invalid")?;
    let full = root.join(path);
    let mut current = root.to_path_buf();
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(CrabError::Configuration {
                key: "artifact_destination_symlink".into(),
                origin: path.display().to_string(),
            });
        }
    }
    if !full.starts_with(root) {
        return Err(CrabError::Configuration {
            key: "artifact_destination_invalid".into(),
            origin: path.display().to_string(),
        });
    }
    Ok(full)
}

fn git_head(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn materialize_payload(
    source: &Path,
    destination: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(CrabError::Configuration {
                key: "artifact_destination_exists".into(),
                origin: destination.display().to_string(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CrabError::Io(error)),
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(CrabError::Io)?;
    let temporary = parent.join(format!(
        ".{}.artifact-get-tmp-{}-{}",
        destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("artifact"),
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    if let Err(error) = snapshot_payload(source, &temporary)
        .and_then(|()| verify_payload(&temporary, expected_hash, expected_size))
        .and_then(|()| {
            match fs::symlink_metadata(destination) {
                Ok(_) => {
                    return Err(crab_workflow::WorkflowError::DvcMigrationInvalid {
                        key: "artifact_destination_exists".to_owned(),
                        origin: destination.display().to_string(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(crab_workflow::WorkflowError::Io(error)),
            }
            fs::rename(&temporary, destination).map_err(crab_workflow::WorkflowError::Io)
        })
    {
        let _ = remove_existing_path(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CrabError::Io(error)),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(CrabError::Io)
    } else {
        fs::remove_file(path).map_err(CrabError::Io)
    }
}

fn emit_payload<T, F>(json: bool, jsonl: bool, schema: &'static str, payload: T, text: F)
where
    T: Serialize,
    F: FnOnce(&T),
{
    match OutputMode::from_flags(json, jsonl) {
        OutputMode::Text => text(&payload),
        OutputMode::Json => emit_json(schema, "1.0", payload),
        OutputMode::Jsonl => {
            let mut stream = JsonlStream::new("artifacts.event", "1.0", std::io::stdout());
            stream.emit_result(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_payload_path_is_registry_owned() {
        let path = version_payload_path(
            Path::new(".crab/workflow/artifacts/registry.json"),
            "b3:abc",
        );
        assert_eq!(
            path,
            PathBuf::from(".crab/workflow/artifacts/versions/b3_abc/payload")
        );
    }
}
