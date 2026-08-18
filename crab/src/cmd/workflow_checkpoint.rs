//! Hidden stage-to-supervisor checkpoint control protocol.
//!
//! A stage inherits the control directory, run identity, stage identity, and
//! one-shot token from the experiment supervisor. The stage-facing command
//! never accepts those values as CLI arguments, so a user cannot forge a
//! checkpoint for another run. Requests and acknowledgements are exchanged
//! through a private directory using canonical JSON and a keyed Blake3 MAC.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use fs4::fs_std::FileExt as LockFileExt;
use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crab_workflow::{
    CheckpointLineage, CheckpointRecord, Lockfile, StageName, parse_yaml, snapshot_payload,
};

pub const WORKFLOW_CHECKPOINT_SCHEMA: &str = "workflow.checkpoint";
const PROTOCOL_VERSION: u16 = 1;
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_ACK_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize)]
struct CheckpointRequest<'a> {
    schema_version: u16,
    run_id: &'a str,
    stage: &'a str,
    nonce: String,
    payload: serde_json::Value,
    mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointAck {
    schema_version: u16,
    run_id: String,
    stage: String,
    nonce: String,
    accepted: bool,
    error: Option<String>,
    mac: String,
}

#[derive(Debug, Deserialize)]
struct CheckpointRequestOwned {
    schema_version: u16,
    run_id: String,
    stage: String,
    nonce: String,
    payload: serde_json::Value,
    mac: String,
}

#[derive(Debug, Serialize)]
struct CheckpointResult {
    run_id: String,
    stage: String,
    nonce: String,
    accepted: bool,
}

/// Arguments for the hidden workflow checkpoint command.
#[derive(Debug, Args)]
pub struct WorkflowCheckpointArgs {
    /// Maximum time to wait for the supervisor acknowledgement.
    #[arg(long, default_value_t = DEFAULT_WAIT_MS, hide = true)]
    pub wait_ms: u64,
    /// Emit structured JSON output.
    #[arg(long, conflicts_with = "jsonl", hide = true)]
    pub json: bool,
    /// Emit structured JSONL output.
    #[arg(long, conflicts_with = "json", hide = true)]
    pub jsonl: bool,
}

impl WorkflowCheckpointArgs {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, self.jsonl)
    }
}

/// Submit one authenticated checkpoint request and wait for durable ack.
pub fn run(args: &WorkflowCheckpointArgs) -> Result<()> {
    let control_dir = required_env("CRAB_WORKFLOW_CONTROL_DIR")?;
    let run_id = required_env("CRAB_WORKFLOW_RUN_ID")?;
    let stage = required_env("CRAB_WORKFLOW_STAGE")?;
    let token = required_env("CRAB_WORKFLOW_TOKEN")?;
    let control_dir = PathBuf::from(control_dir);
    validate_private_directory(&control_dir)?;

    let nonce = nonce(&run_id, &stage);
    let stage_hash = required_env("CRAB_WORKFLOW_STAGE_HASH")?;
    validate_checkpoint_digest(&stage_hash)?;
    let payload = serde_json::json!({"stage_hash": stage_hash});
    let mac = request_mac(&token, &run_id, &stage, &nonce, &payload);
    let request = CheckpointRequest {
        schema_version: PROTOCOL_VERSION,
        run_id: &run_id,
        stage: &stage,
        nonce: nonce.clone(),
        payload,
        mac,
    };
    let request_path = control_dir.join(format!("request-{nonce}.json"));
    atomic_write_json(&request_path, &request)?;
    let ack_path = control_dir.join(format!("ack-{nonce}.json"));
    let deadline = std::time::Instant::now() + Duration::from_millis(args.wait_ms);
    let ack = loop {
        if std::time::Instant::now() >= deadline {
            let _ = fs::remove_file(&request_path);
            return Err(CrabError::Configuration {
                key: "workflow_checkpoint_ack_timeout".to_owned(),
                origin: format!("stage '{stage}' did not receive supervisor acknowledgement"),
            });
        }
        if ack_path.is_file() {
            break read_ack(&ack_path, &token, &run_id, &stage, &nonce)?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = fs::remove_file(&request_path);
    let _ = fs::remove_file(&ack_path);
    if !ack.accepted {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_rejected".to_owned(),
            origin: ack
                .error
                .unwrap_or_else(|| "supervisor rejected checkpoint".to_owned()),
        });
    }
    let result = CheckpointResult {
        run_id,
        stage,
        nonce,
        accepted: true,
    };
    match args.output_mode() {
        OutputMode::Text => {}
        OutputMode::Json => emit_json(WORKFLOW_CHECKPOINT_SCHEMA, "1.0", result),
        OutputMode::Jsonl => {
            let mut stream =
                JsonlStream::new("workflow.checkpoint.event", "1.0", std::io::stdout());
            stream.emit_result(result);
        }
    }
    Ok(())
}

/// Create a private control directory for one experiment run.
pub fn create_control_directory(root: &Path, run_id: &str) -> Result<PathBuf> {
    validate_run_id(run_id)?;
    let directory = root.join(".crab/workflow/checkpoints/control").join(run_id);
    ensure_parent_not_symlink(&directory)?;
    fs::create_dir_all(&directory).map_err(CrabError::Io)?;
    let metadata = fs::symlink_metadata(&directory).map_err(CrabError::Io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_control_dir_invalid".to_owned(),
            origin: directory.display().to_string(),
        });
    }
    set_private_permissions(&directory)?;
    Ok(directory)
}

/// Derive a per-run token without persisting it in workflow metadata.
#[must_use]
pub fn control_token(run_id: &str) -> String {
    let nonce = uuid::Uuid::now_v7();
    blake3::hash(format!("crab-workflow-checkpoint\0{run_id}\0{nonce}").as_bytes())
        .to_hex()
        .to_string()
}

/// Run the experiment-side checkpoint supervisor until the execution exits.
///
/// A request is acknowledged only after all declared checkpoint outputs have
/// been copied into immutable Crab-owned state and the lineage journal has
/// been atomically replaced. Invalid or stale requests receive a signed
/// rejection when their identity is safe to echo; malformed filenames are
/// ignored so an attacker cannot make the supervisor write arbitrary paths.
pub async fn supervise(
    control_dir: PathBuf,
    execution_root: PathBuf,
    persistence_root: PathBuf,
    run_id: String,
    token: String,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    validate_run_id(&run_id)?;
    validate_private_directory(&control_dir)?;
    loop {
        tokio::select! {
            _ = &mut stop => {
                cleanup_control_files(&control_dir);
                return Ok(());
            }
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        let mut requests = fs::read_dir(&control_dir)
            .map_err(CrabError::Io)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("request-")
                            && path
                                .extension()
                                .is_some_and(|extension| extension == "json")
                    })
            })
            .collect::<Vec<_>>();
        requests.sort();
        for request_path in requests {
            if let Err(error) = process_request(
                &request_path,
                &control_dir,
                &execution_root,
                &persistence_root,
                &run_id,
                &token,
            ) {
                tracing::warn!(
                    path = %request_path.display(),
                    error = %error,
                    "ignoring malformed workflow checkpoint request"
                );
                let _ = fs::remove_file(request_path);
            }
        }
    }
}

fn process_request(
    request_path: &Path,
    control_dir: &Path,
    execution_root: &Path,
    persistence_root: &Path,
    run_id: &str,
    token: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(request_path).map_err(CrabError::Io)?;
    if !metadata.is_file() || metadata.len() > MAX_ACK_BYTES {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_request_invalid".to_owned(),
            origin: request_path.display().to_string(),
        });
    }
    let mut bytes = Vec::new();
    File::open(request_path)
        .map_err(CrabError::Io)?
        .read_to_end(&mut bytes)
        .map_err(CrabError::Io)?;
    let request: CheckpointRequestOwned =
        serde_json::from_slice(&bytes).map_err(|error| CrabError::Configuration {
            key: "workflow_checkpoint_request_invalid".to_owned(),
            origin: error.to_string(),
        })?;
    let expected_nonce = request_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("request-"))
        .and_then(|name| name.strip_suffix(".json"))
        .unwrap_or_default();
    if !is_nonce(expected_nonce)
        || request.nonce != expected_nonce
        || request.schema_version != PROTOCOL_VERSION
        || request.run_id != run_id
        || request.mac
            != request_mac(
                token,
                run_id,
                &request.stage,
                &request.nonce,
                &request.payload,
            )
    {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_request_invalid".to_owned(),
            origin: "request identity or authentication failed".to_owned(),
        });
    }
    StageName::parse(&request.stage).map_err(|error| CrabError::Configuration {
        key: "workflow_checkpoint_stage_invalid".to_owned(),
        origin: error.to_string(),
    })?;
    let stage_hash = request_stage_hash(&request.payload)?;
    let ack_path = control_dir.join(format!("ack-{}.json", request.nonce));
    let (accepted, error) =
        if checkpoint_request_seen(persistence_root, run_id, &request.stage, &request.nonce)? {
            (true, None)
        } else {
            match snapshot_checkpoint(
                execution_root,
                persistence_root,
                run_id,
                &request.stage,
                false,
                Some(&request.nonce),
                Some(&stage_hash),
            ) {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error.to_string())),
            }
        };
    let stage = request.stage.clone();
    let nonce = request.nonce.clone();
    let payload = serde_json::json!({"accepted": accepted, "error": error.clone()});
    let mac = request_mac(token, run_id, &stage, &nonce, &payload);
    let ack = CheckpointAck {
        schema_version: PROTOCOL_VERSION,
        run_id: run_id.to_owned(),
        stage,
        nonce,
        accepted,
        error,
        mac,
    };
    atomic_write_json(&ack_path, &ack)?;
    let _ = fs::remove_file(request_path);
    Ok(())
}

fn snapshot_checkpoint(
    execution_root: &Path,
    persistence_root: &Path,
    run_id: &str,
    stage_name: &str,
    terminal: bool,
    request_nonce: Option<&str>,
    stage_hash: Option<&str>,
) -> Result<()> {
    validate_run_id(run_id)?;
    let workflow_path = execution_root.join("crab.yaml");
    let workflow = parse_yaml(&fs::read_to_string(&workflow_path).map_err(CrabError::Io)?)?;
    let stage = workflow
        .stages
        .get(&StageName::parse(stage_name)?)
        .ok_or_else(|| CrabError::Configuration {
            key: "workflow_checkpoint_stage_unknown".to_owned(),
            origin: stage_name.to_owned(),
        })?;
    let checkpoints = stage
        .outs
        .iter()
        .filter(|output| output.checkpoint)
        .collect::<Vec<_>>();
    if checkpoints.is_empty() {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_stage_not_declared".to_owned(),
            origin: stage_name.to_owned(),
        });
    }
    StageName::parse(stage_name).map_err(|error| CrabError::Configuration {
        key: "workflow_checkpoint_stage_invalid".to_owned(),
        origin: error.to_string(),
    })?;
    let state_root = persistence_root
        .join(".crab/workflow/checkpoints")
        .join(run_id);
    let objects_root = state_root.join("objects");
    let lineage_path = state_root.join(format!("{stage_name}.json"));
    fs::create_dir_all(&objects_root).map_err(CrabError::Io)?;
    let mut outputs = BTreeMap::new();
    for output in checkpoints {
        let relative_path = stage
            .wdir
            .as_deref()
            .map_or_else(|| output.path.clone(), |wdir| wdir.join(&output.path));
        let path = execution_root
            .join(stage.wdir.as_deref().unwrap_or_else(|| Path::new("")))
            .join(&output.path);
        let (hash, _) = hash_checkpoint_path(&path)?;
        let hash_text = format!("b3:{}", hex(&hash));
        let object = objects_root.join(hex(&hash)).join("payload");
        snapshot_payload(&path, &object).map_err(|error| CrabError::Configuration {
            key: "workflow_checkpoint_snapshot_failed".to_owned(),
            origin: error.to_string(),
        })?;
        // Hash the immutable snapshot, not only the live working-tree path.
        // A stage can still be writing when it invokes the control command;
        // accepting the pre-copy hash would bind lineage to bytes that were
        // never actually stored.
        let (snapshot_hash, _) = hash_checkpoint_path(&object)?;
        let (live_hash, _) = hash_checkpoint_path(&path)?;
        if snapshot_hash != hash || live_hash != hash {
            return Err(CrabError::Configuration {
                key: "workflow_checkpoint_snapshot_changed".to_owned(),
                origin: format!(
                    "checkpoint output '{}' changed while it was being snapshotted",
                    relative_path.display()
                ),
            });
        }
        outputs.insert(
            relative_path.to_string_lossy().replace('\\', "/"),
            hash_text,
        );
    }
    let mut metrics = BTreeMap::new();
    for metric in &stage.metrics {
        let relative_path = stage
            .wdir
            .as_deref()
            .map_or_else(|| metric.clone(), |wdir| wdir.join(metric));
        let path = execution_root
            .join(stage.wdir.as_deref().unwrap_or_else(|| Path::new("")))
            .join(metric);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CrabError::Configuration {
                    key: "workflow_checkpoint_metric_symlink".to_owned(),
                    origin: path.display().to_string(),
                });
            }
            Ok(_) => {
                let (hash, _) = hash_checkpoint_path(&path)?;
                let object = objects_root.join(hex(&hash)).join("payload");
                snapshot_payload(&path, &object).map_err(|error| CrabError::Configuration {
                    key: "workflow_checkpoint_metric_snapshot_failed".to_owned(),
                    origin: error.to_string(),
                })?;
                let (snapshot_hash, _) = hash_checkpoint_path(&object)?;
                let (live_hash, _) = hash_checkpoint_path(&path)?;
                if snapshot_hash != hash || live_hash != hash {
                    return Err(CrabError::Configuration {
                        key: "workflow_checkpoint_snapshot_changed".to_owned(),
                        origin: format!(
                            "checkpoint metric '{}' changed while it was being snapshotted",
                            relative_path.display()
                        ),
                    });
                }
                metrics.insert(
                    relative_path.to_string_lossy().replace('\\', "/"),
                    format!("b3:{}", hex(&hash)),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CrabError::Io(error)),
        }
    }
    let _lineage_lock = CheckpointLineageLock::acquire(&lineage_path)?;
    let mut lineage =
        CheckpointLineage::load(&lineage_path).map_err(|error| CrabError::Configuration {
            key: "workflow_checkpoint_lineage_load".to_owned(),
            origin: error.to_string(),
        })?;
    if let Some(request_nonce) = request_nonce
        && lineage
            .records
            .iter()
            .any(|record| record.request_nonce.as_deref() == Some(request_nonce))
    {
        return Ok(());
    }
    let sequence = lineage.records.len() as u64;
    let parent = lineage.latest_resumable().map(|record| record.id.clone());
    let stage_hash = match stage_hash {
        Some(stage_hash) => {
            validate_checkpoint_digest(stage_hash)?;
            stage_hash.to_owned()
        }
        None => load_locked_stage_hash(execution_root, stage_name)?,
    };
    if terminal
        && request_nonce.is_none()
        && lineage.records.last().is_some_and(|record| {
            record.stage_hash == stage_hash
                && record.outputs == outputs
                && record.metrics == metrics
        })
    {
        // A successful stage that produced no new checkpoint does not need a
        // duplicate terminal record. This keeps the lineage immutable while
        // preserving the latest acknowledged event as the final state.
        return Ok(());
    }
    let mut record = CheckpointRecord {
        schema_version: crab_workflow::CHECKPOINT_SCHEMA_VERSION,
        id: String::new(),
        // A resumed run forks the immutable state into a new experiment
        // directory. Keep the source lineage identity so parent links remain
        // valid across the fork; the directory/run token still authenticates
        // the live stage and names the transport object namespace.
        experiment: lineage
            .records
            .first()
            .map_or_else(|| run_id.to_owned(), |record| record.experiment.clone()),
        stage: stage_name.to_owned(),
        sequence,
        parent,
        request_nonce: request_nonce.map(ToOwned::to_owned),
        stage_hash,
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                duration.as_millis().min(u128::from(u64::MAX)) as u64
            }),
        outputs,
        metrics,
        terminal,
        resumable: true,
    };
    let identity = serde_json::to_vec(&record).map_err(|error| CrabError::Configuration {
        key: "workflow_checkpoint_record_serialize".to_owned(),
        origin: error.to_string(),
    })?;
    record.id = format!("b3:{}", blake3::hash(&identity).to_hex());
    lineage
        .append(record)
        .map_err(|error| CrabError::Configuration {
            key: "workflow_checkpoint_lineage_append".to_owned(),
            origin: error.to_string(),
        })?;
    lineage
        .save_atomic(&lineage_path)
        .map_err(|error| CrabError::Configuration {
            key: "workflow_checkpoint_lineage_save".to_owned(),
            origin: error.to_string(),
        })
}

struct CheckpointLineageLock {
    file: File,
}

impl CheckpointLineageLock {
    fn acquire(lineage_path: &Path) -> Result<Self> {
        let parent = lineage_path
            .parent()
            .ok_or_else(|| CrabError::Configuration {
                key: "workflow_checkpoint_lineage_lock".to_owned(),
                origin: lineage_path.display().to_string(),
            })?;
        ensure_parent_not_symlink(lineage_path)?;
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
        let lock_path = lineage_path.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(CrabError::Io)?;
        LockFileExt::lock_exclusive(&file).map_err(CrabError::Io)?;
        Ok(Self { file })
    }
}

impl Drop for CheckpointLineageLock {
    fn drop(&mut self) {
        let _ = LockFileExt::unlock(&self.file);
    }
}

fn checkpoint_request_seen(
    persistence_root: &Path,
    run_id: &str,
    stage_name: &str,
    request_nonce: &str,
) -> Result<bool> {
    let path = persistence_root
        .join(".crab/workflow/checkpoints")
        .join(run_id)
        .join(format!("{stage_name}.json"));
    let lineage = CheckpointLineage::load(&path).map_err(|error| CrabError::Configuration {
        key: "workflow_checkpoint_lineage_load".to_owned(),
        origin: error.to_string(),
    })?;
    Ok(lineage
        .records
        .iter()
        .any(|record| record.request_nonce.as_deref() == Some(request_nonce)))
}

/// Append terminal records for checkpoint stages after a successful DAG run.
pub fn finalize_checkpoints(
    execution_root: &Path,
    persistence_root: &Path,
    run_id: &str,
) -> Result<()> {
    validate_run_id(run_id)?;
    let workflow = parse_yaml(&fs::read_to_string(execution_root.join("crab.yaml"))?)?;
    let stages = workflow
        .stages
        .values()
        .filter(|stage| stage.outs.iter().any(|output| output.checkpoint))
        .map(|stage| stage.name.as_str().to_owned())
        .collect::<Vec<_>>();
    for stage in stages {
        snapshot_checkpoint(
            execution_root,
            persistence_root,
            run_id,
            &stage,
            true,
            None,
            None,
        )?;
    }
    Ok(())
}

fn hash_checkpoint_path(path: &Path) -> Result<([u8; 32], u64)> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_output_symlink".to_owned(),
            origin: path.display().to_string(),
        });
    }
    if metadata.is_file() {
        let mut file = File::open(path).map_err(CrabError::Io)?;
        let mut hasher = blake3::Hasher::new();
        let size = std::io::copy(&mut file, &mut hasher).map_err(CrabError::Io)?;
        return Ok((*hasher.finalize().as_bytes(), size));
    }
    if metadata.is_dir() {
        let tree = crab_workflow::hasher::hash_directory(path, false)?;
        return Ok((
            tree.hash,
            tree.manifest.iter().map(|entry| entry.size).sum(),
        ));
    }
    Err(CrabError::Configuration {
        key: "workflow_checkpoint_output_missing".to_owned(),
        origin: path.display().to_string(),
    })
}

fn cleanup_control_files(control_dir: &Path) {
    if let Ok(entries) = fs::read_dir(control_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn is_nonce(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_run_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_run_id_invalid".to_owned(),
            origin: "run identity must be a bounded path-safe token".to_owned(),
        });
    }
    Ok(())
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(CrabError::Io)?;
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").map_err(|_| CrabError::Configuration {
            key: "workflow_checkpoint_control_dir_acl".to_owned(),
            origin: "USERNAME is unavailable".to_owned(),
        })?;
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{user}:F"))
            .status()
            .map_err(CrabError::Io)?;
        if !status.success() {
            return Err(CrabError::Configuration {
                key: "workflow_checkpoint_control_dir_acl".to_owned(),
                origin: format!("icacls failed for {}", path.display()),
            });
        }
    }
    Ok(())
}

fn hex(bytes: &[u8; 32]) -> String {
    blake3::Hash::from(*bytes).to_hex().to_string()
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| CrabError::Configuration {
        key: "workflow_checkpoint_environment_missing".to_owned(),
        origin: name.to_owned(),
    })
}

fn request_stage_hash(payload: &serde_json::Value) -> Result<String> {
    let stage_hash = payload
        .get("stage_hash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CrabError::Configuration {
            key: "workflow_checkpoint_stage_hash_missing".to_owned(),
            origin: "checkpoint request did not carry the executor-resolved stage hash".to_owned(),
        })?;
    validate_checkpoint_digest(stage_hash)?;
    Ok(stage_hash.to_owned())
}

fn validate_checkpoint_digest(value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix("b3:") else {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_stage_hash_invalid".to_owned(),
            origin: "stage hash must use the b3:<64-hex> form".to_owned(),
        });
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_stage_hash_invalid".to_owned(),
            origin: "stage hash must use the b3:<64-hex> form".to_owned(),
        });
    }
    Ok(())
}

fn load_locked_stage_hash(execution_root: &Path, stage_name: &str) -> Result<String> {
    let lockfile_path = execution_root.join("crab.lock");
    let lockfile = Lockfile::load(&lockfile_path).map_err(|error| CrabError::Configuration {
        key: "workflow_checkpoint_lockfile_load".to_owned(),
        origin: error.to_string(),
    })?;
    let stage = StageName::parse(stage_name)?;
    let locked = lockfile
        .get(&stage)
        .ok_or_else(|| CrabError::Configuration {
            key: "workflow_checkpoint_stage_hash_missing".to_owned(),
            origin: format!("crab.lock has no resolved entry for stage '{stage_name}'"),
        })?;
    Ok(format!("b3:{}", locked.stage_hash.as_hex()))
}

fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    if !metadata.is_dir() {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_control_dir_invalid".to_owned(),
            origin: path.display().to_string(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(CrabError::Configuration {
                key: "workflow_checkpoint_control_dir_not_private".to_owned(),
                origin: path.display().to_string(),
            });
        }
    }
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or_else(|| CrabError::Configuration {
        key: "workflow_checkpoint_control_dir_invalid".to_owned(),
        origin: path.display().to_string(),
    })?;
    ensure_parent_not_symlink(path)?;
    let bytes = serde_json::to_vec(value).map_err(|error| CrabError::Configuration {
        key: "workflow_checkpoint_request_serialize".to_owned(),
        origin: error.to_string(),
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("request"),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(CrabError::Io)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(CrabError::Io(error));
    }
    Ok(())
}

fn read_ack(
    path: &Path,
    token: &str,
    run_id: &str,
    stage: &str,
    nonce: &str,
) -> Result<CheckpointAck> {
    let metadata = fs::symlink_metadata(path).map_err(CrabError::Io)?;
    if !metadata.is_file() || metadata.len() > MAX_ACK_BYTES {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_ack_invalid".to_owned(),
            origin: path.display().to_string(),
        });
    }
    let mut file = File::open(path).map_err(CrabError::Io)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(CrabError::Io)?;
    let ack: CheckpointAck =
        serde_json::from_slice(&bytes).map_err(|error| CrabError::Configuration {
            key: "workflow_checkpoint_ack_invalid".to_owned(),
            origin: error.to_string(),
        })?;
    if ack.schema_version != PROTOCOL_VERSION
        || ack.run_id != run_id
        || ack.stage != stage
        || ack.nonce != nonce
    {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_ack_invalid".to_owned(),
            origin: "ack identity does not match request".to_owned(),
        });
    }
    let payload = serde_json::json!({
        "accepted": ack.accepted,
        "error": ack.error,
    });
    let expected = request_mac(token, run_id, stage, nonce, &payload);
    if ack.mac != expected {
        return Err(CrabError::Configuration {
            key: "workflow_checkpoint_ack_invalid".to_owned(),
            origin: "ack authentication failed".to_owned(),
        });
    }
    Ok(ack)
}

fn ensure_parent_not_symlink(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let allow_system_ancestor_link = parent.is_absolute();
    let mut current = PathBuf::new();
    let mut normal_components = 0_usize;
    for component in parent.components() {
        current.push(component.as_os_str());
        if !matches!(component, std::path::Component::Normal(_)) {
            continue;
        }
        normal_components += 1;
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if !(allow_system_ancestor_link && normal_components == 1) {
                    return Err(CrabError::Configuration {
                        key: "workflow_checkpoint_control_dir_symlink".to_owned(),
                        origin: current.display().to_string(),
                    });
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(CrabError::Io(error)),
        }
    }
    Ok(())
}

fn request_mac(
    token: &str,
    run_id: &str,
    stage: &str,
    nonce: &str,
    payload: &serde_json::Value,
) -> String {
    let key = blake3::hash(token.as_bytes());
    let canonical = serde_json::to_vec(&serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "run_id": run_id,
        "stage": stage,
        "nonce": nonce,
        "payload": payload,
    }))
    .unwrap_or_default();
    blake3::keyed_hash(key.as_bytes(), &canonical)
        .to_hex()
        .to_string()
}

fn nonce(run_id: &str, stage: &str) -> String {
    // A stage may acknowledge multiple checkpoints in one clock tick. Use a
    // UUID nonce rather than wall-clock time alone so replay protection does
    // not collapse two legitimate requests onto the same request filename.
    let nonce = uuid::Uuid::now_v7();
    blake3::hash(format!("{run_id}\0{stage}\0{}\0{nonce}", std::process::id()).as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_mac_is_deterministic_and_token_bound() {
        let payload = serde_json::json!({});
        let first = request_mac("secret", "run", "stage", "nonce", &payload);
        assert_eq!(
            first,
            request_mac("secret", "run", "stage", "nonce", &payload)
        );
        assert_ne!(
            first,
            request_mac("other", "run", "stage", "nonce", &payload)
        );
    }

    #[test]
    fn private_directory_rejects_missing_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(validate_private_directory(&temp.path().join("missing")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn control_directory_rejects_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        symlink(outside.path(), root.path().join(".crab")).expect("symlink");
        let error = create_control_directory(root.path(), "exp").expect_err("symlink parent");
        assert!(matches!(
            error,
            CrabError::Configuration { key, .. }
                if key == "workflow_checkpoint_control_dir_symlink"
        ));
    }

    #[test]
    fn snapshot_records_executor_resolved_stage_hash() {
        let execution = tempfile::tempdir().expect("execution tempdir");
        let persistence = tempfile::tempdir().expect("persistence tempdir");
        std::fs::write(
            execution.path().join("crab.yaml"),
            "stages:\n  train:\n    cmd: echo train\n    outs:\n      - path: model.bin\n        checkpoint: true\n",
        )
        .expect("workflow");
        std::fs::write(execution.path().join("model.bin"), b"model").expect("output");
        let expected = format!("b3:{}", "ab".repeat(32));

        snapshot_checkpoint(
            execution.path(),
            persistence.path(),
            "exp",
            "train",
            false,
            Some(&"01".repeat(32)),
            Some(&expected),
        )
        .expect("snapshot");

        let lineage = CheckpointLineage::load(
            &persistence
                .path()
                .join(".crab/workflow/checkpoints/exp/train.json"),
        )
        .expect("lineage");
        assert_eq!(lineage.records[0].stage_hash, expected);
    }

    #[test]
    fn finalization_does_not_duplicate_an_unchanged_checkpoint() {
        let execution = tempfile::tempdir().expect("execution tempdir");
        let persistence = tempfile::tempdir().expect("persistence tempdir");
        std::fs::write(
            execution.path().join("crab.yaml"),
            "stages:\n  train:\n    cmd: echo train\n    outs:\n      - path: model.bin\n        checkpoint: true\n",
        )
        .expect("workflow");
        std::fs::write(execution.path().join("model.bin"), b"model").expect("output");
        let stage_hash = format!("b3:{}", "ab".repeat(32));
        let nonce = "01".repeat(32);

        snapshot_checkpoint(
            execution.path(),
            persistence.path(),
            "exp",
            "train",
            false,
            Some(&nonce),
            Some(&stage_hash),
        )
        .expect("live snapshot");
        snapshot_checkpoint(
            execution.path(),
            persistence.path(),
            "exp",
            "train",
            true,
            None,
            Some(&stage_hash),
        )
        .expect("terminal snapshot");

        let lineage = CheckpointLineage::load(
            &persistence
                .path()
                .join(".crab/workflow/checkpoints/exp/train.json"),
        )
        .expect("lineage");
        assert_eq!(lineage.records.len(), 1);
        assert!(!lineage.records[0].terminal);
    }
}
