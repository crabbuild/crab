//! `crab release` command namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use base64::Engine;
use clap::{Args, Subcommand};
use object_store::path::Path as ObjectPath;
use ring::signature::{ED25519, UnparsedPublicKey};
use schemars::JsonSchema;
use serde::Serialize;
use tracing::warn;

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::git::url::CrabUrl;
use crate::release::{
    RELEASE_MANIFEST_SCHEMA_VERSION, ReleaseCrabInventory, ReleaseLargeFile, ReleaseManifest,
    ReleaseRefTarget, ReleaseRevision, ReleaseSignature, ReleaseSignatureState,
    ReleaseWorkflowMetadata,
};
use crate::workflow::params;
use bytes::Bytes;
use crab_types::pointer::{MAX_POINTER_SIZE, Pointer};
use crab_workflow::{Workflow, yaml};
use tokio_util::sync::CancellationToken;

pub const RELEASE_CREATE_SCHEMA: &str = "release.create";
pub const RELEASE_VERIFY_SCHEMA: &str = "release.verify";
pub const RELEASE_EXPORT_SCHEMA: &str = "release.export";
pub const RELEASE_LIST_SCHEMA: &str = "release.list";
pub const RELEASE_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseCreatePayload {
    pub manifest: ReleaseManifest,
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    pub published: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseVerifyPayload {
    pub release_id: String,
    pub verified: bool,
    pub deep: bool,
    pub manifest_digest: String,
    pub content_digest: String,
    pub signature_state: ReleaseSignatureState,
    pub signature_verified: bool,
    pub issues: Vec<ReleaseVerifyIssue>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseVerifyIssue {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseExportPayload {
    pub release_id: String,
    pub manifest_digest: String,
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseListPayload {
    pub remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    pub releases: Vec<ReleaseListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReleaseListEntry {
    pub release_id: String,
    pub path: String,
    pub manifest_digest: String,
    pub content_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleasePublishResult {
    remote_url: String,
    path: String,
}

struct ReleaseRemote {
    remote_url: String,
    repo_prefix: String,
    store: crate::storage::Store,
    router: crate::storage::StoreLayout,
    config: Config,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ReleaseCmd {
    /// Create a dataset release manifest.
    Create(ReleaseCreateArgs),
    /// Verify a dataset release manifest.
    Verify(ReleaseVerifyArgs),
    /// Export a dataset release manifest bundle.
    Export(ReleaseExportArgs),
    /// List dataset release manifests.
    List(ReleaseListArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ReleaseCreateArgs {
    /// Release identifier to record in the manifest.
    #[arg(long, value_name = "ID")]
    pub name: Option<String>,
    /// Git revision to resolve for the release.
    #[arg(long, default_value = "HEAD")]
    pub rev: String,
    /// Write the manifest to a local JSON file.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Allow manifest creation from a dirty workspace.
    #[arg(long)]
    pub allow_dirty: bool,
    /// Publish the manifest to the Crab repository release namespace.
    #[arg(long)]
    pub publish: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ReleaseVerifyArgs {
    /// Local manifest JSON file to verify.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,
    /// Published release identifier to verify.
    #[arg(long, value_name = "ID")]
    pub name: Option<String>,
    /// Verify Git revision, refs, and Crab pointer inventory against the local repository.
    #[arg(long)]
    pub deep: bool,
    /// Detached Ed25519 signature file for signed manifests.
    #[arg(long, value_name = "PATH", requires = "public_key")]
    pub signature: Option<PathBuf>,
    /// Raw or base64-encoded Ed25519 public key for detached signature verification.
    #[arg(long = "public-key", value_name = "PATH", requires = "signature")]
    pub public_key: Option<PathBuf>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ReleaseExportArgs {
    /// Local manifest JSON file to export.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,
    /// Published release identifier to export.
    #[arg(long, value_name = "ID")]
    pub name: Option<String>,
    /// Destination JSON bundle path.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ReleaseListArgs {
    /// Include published repository releases when available.
    #[arg(long)]
    pub remote: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

impl ReleaseCmd {
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json(), false)
    }

    pub fn schema_name(&self) -> &'static str {
        match self {
            Self::Create(_) => RELEASE_CREATE_SCHEMA,
            Self::Verify(_) => RELEASE_VERIFY_SCHEMA,
            Self::Export(_) => RELEASE_EXPORT_SCHEMA,
            Self::List(_) => RELEASE_LIST_SCHEMA,
        }
    }

    fn json(&self) -> bool {
        match self {
            Self::Create(args) => args.json,
            Self::Verify(args) => args.json,
            Self::Export(args) => args.json,
            Self::List(args) => args.json,
        }
    }
}

pub async fn run(cmd: &ReleaseCmd) -> Result<()> {
    match cmd {
        ReleaseCmd::Create(args) => run_create(args, cmd.output_mode()).await,
        ReleaseCmd::Verify(args) => run_verify(args, cmd.output_mode()).await,
        ReleaseCmd::Export(args) => run_export(args, cmd.output_mode()).await,
        ReleaseCmd::List(args) => run_list(args, cmd.output_mode()).await,
    }
}

async fn run_verify(args: &ReleaseVerifyArgs, mode: OutputMode) -> Result<()> {
    validate_manifest_source(
        "release verify",
        args.manifest.as_ref(),
        args.name.as_deref(),
    )?;
    let repo_root = if args.name.is_some() || args.deep {
        Some(current_repo_root("release verify")?)
    } else {
        None
    };
    let manifest = match (&args.manifest, &args.name) {
        (Some(path), None) => read_manifest(path)?,
        (None, Some(release_id)) => {
            let root = repo_root
                .as_deref()
                .ok_or_else(|| CrabError::Configuration {
                    key: "release verify --name".to_owned(),
                    origin: "published release lookup requires a Git working tree".to_owned(),
                })?;
            let remote = open_release_remote(root, "release.verify").await?;
            read_published_manifest_from_store(&remote.store, &remote.repo_prefix, release_id)
                .await?
        }
        _ => {
            return Err(CrabError::Configuration {
                key: "release verify".to_owned(),
                origin: "release verify requires exactly one of --manifest PATH or --name ID"
                    .to_owned(),
            });
        }
    };

    let verifier =
        release_signature_verifier(args.signature.as_deref(), args.public_key.as_deref());
    let payload =
        verify_manifest_payload(&manifest, args.deep, repo_root.as_deref(), verifier).await?;
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(RELEASE_VERIFY_SCHEMA, RELEASE_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            let state = if payload.verified {
                "verified"
            } else {
                "failed"
            };
            println!(
                "release manifest {} {} {}",
                payload.release_id, state, payload.manifest_digest
            );
            println!(
                "signature: {}",
                signature_state_label(payload.signature_state)
            );
            for issue in &payload.issues {
                println!("{}: {}", issue.code, issue.message);
            }
        }
    }
    Ok(())
}

async fn run_export(args: &ReleaseExportArgs, mode: OutputMode) -> Result<()> {
    validate_manifest_source(
        "release export",
        args.manifest.as_ref(),
        args.name.as_deref(),
    )?;
    let Some(output_path) = &args.output else {
        return Err(CrabError::Configuration {
            key: "release export --output".to_owned(),
            origin: "release export requires --output PATH".to_owned(),
        });
    };

    let manifest = match (&args.manifest, &args.name) {
        (Some(path), None) => read_manifest(path)?,
        (None, Some(release_id)) => {
            let repo_root = current_repo_root("release export")?;
            let remote = open_release_remote(&repo_root, "release.export").await?;
            read_published_manifest_from_store(&remote.store, &remote.repo_prefix, release_id)
                .await?
        }
        _ => {
            return Err(CrabError::Configuration {
                key: "release export".to_owned(),
                origin: "release export requires exactly one of --manifest PATH or --name ID"
                    .to_owned(),
            });
        }
    };
    let payload = export_manifest_payload(&manifest, output_path)?;
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(RELEASE_EXPORT_SCHEMA, RELEASE_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            println!(
                "exported release manifest {} {} to {}",
                payload.release_id, payload.manifest_digest, payload.output_path
            );
        }
    }
    Ok(())
}

async fn run_list(args: &ReleaseListArgs, mode: OutputMode) -> Result<()> {
    let payload = if args.remote {
        let repo_root = current_repo_root("release list")?;
        let remote = open_release_remote(&repo_root, "release.list").await?;
        let releases =
            list_published_releases_from_store(&remote.store, &remote.repo_prefix).await?;
        ReleaseListPayload {
            remote: true,
            remote_url: Some(remote.remote_url),
            releases,
        }
    } else {
        ReleaseListPayload {
            remote: false,
            remote_url: None,
            releases: Vec::new(),
        }
    };

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(RELEASE_LIST_SCHEMA, RELEASE_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            if payload.releases.is_empty() {
                if payload.remote {
                    println!("no published release manifests");
                } else {
                    println!(
                        "no local release manifest index; pass --remote to list published releases"
                    );
                }
            } else {
                for release in &payload.releases {
                    println!(
                        "{} {} {}",
                        release.release_id, release.manifest_digest, release.path
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_manifest_source(
    command: &str,
    manifest: Option<&PathBuf>,
    name: Option<&str>,
) -> Result<()> {
    match (manifest, name) {
        (Some(_), None) | (None, Some(_)) => Ok(()),
        (Some(_), Some(_)) => Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: format!("{command} accepts either --manifest PATH or --name ID, not both"),
        }),
        (None, None) => Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: format!("{command} requires --manifest PATH or --name ID"),
        }),
    }
}

async fn open_release_remote(repo_root: &Path, operation: &str) -> Result<ReleaseRemote> {
    let remote_url = crate::cmd::workflow::read_crab_remote_url(repo_root)?;
    let parsed = CrabUrl::parse(&remote_url)?;
    let config = Config::resolve_for_repo(repo_root)?;
    let cancel = CancellationToken::new();
    let selection = crate::replication::StoreResolver::new(&config, &parsed, &cancel)
        .read_store(operation)
        .await?;
    Ok(ReleaseRemote {
        remote_url,
        repo_prefix: selection.router.repo_prefix().to_owned(),
        store: selection.store,
        router: selection.router,
        config,
    })
}

async fn read_published_manifest_from_store(
    store: &crate::storage::Store,
    repo_prefix: &str,
    release_id: &str,
) -> Result<ReleaseManifest> {
    let path = release_object_path(repo_prefix, release_id)?;
    let (bytes, _) = store.get_with_etag(&path).await?;
    read_manifest_bytes(bytes.as_ref(), path.as_ref())
}

async fn list_published_releases_from_store(
    store: &crate::storage::Store,
    repo_prefix: &str,
) -> Result<Vec<ReleaseListEntry>> {
    let prefix = release_namespace_path(repo_prefix);
    let mut entries = Vec::new();
    for meta in store.list_prefix(&prefix).await? {
        let Some(release_id) = release_id_from_object_path(repo_prefix, &meta.location) else {
            continue;
        };
        let manifest = read_published_manifest_from_store(store, repo_prefix, &release_id).await?;
        if manifest.release_id != release_id {
            return Err(CrabError::Configuration {
                key: meta.location.to_string(),
                origin: format!(
                    "release manifest id {} does not match object name {release_id}",
                    manifest.release_id
                ),
            });
        }
        entries.push(ReleaseListEntry {
            release_id,
            path: meta.location.to_string(),
            manifest_digest: manifest.identity_digest()?,
            content_digest: manifest.content_digest()?,
        });
    }
    entries.sort_by(|left, right| left.release_id.cmp(&right.release_id));
    Ok(entries)
}

async fn run_create(args: &ReleaseCreateArgs, mode: OutputMode) -> Result<()> {
    let repo_root = current_repo_root("release create")?;
    let mut payload = create_payload(args, &repo_root)?;

    if let Some(output) = &args.output {
        let bytes = payload.manifest.canonical_bytes()?;
        std::fs::write(output, bytes).map_err(CrabError::Io)?;
    }

    if args.publish {
        let publish = publish_manifest(&repo_root, &payload.manifest).await?;
        payload.published = true;
        payload.published_path = Some(publish.path);
        if let Err(err) = record_release_publish_audit(&repo_root, &publish.remote_url, &payload) {
            warn!(%err, "failed to append release publish audit event");
        }
    }

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(RELEASE_CREATE_SCHEMA, RELEASE_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            println!(
                "release manifest {} {}",
                payload.manifest.release_id, payload.digest
            );
            if let Some(path) = &payload.output_path {
                println!("wrote {path}");
            }
        }
    }
    Ok(())
}

fn create_payload(args: &ReleaseCreateArgs, repo_root: &Path) -> Result<ReleaseCreatePayload> {
    if !args.allow_dirty {
        ensure_clean_worktree(repo_root)?;
    }

    let commit = resolve_commit(repo_root, &args.rev)?;
    let release_id = args
        .name
        .clone()
        .unwrap_or_else(|| format!("release-{}", &commit[..12]));
    validate_release_id(&release_id)?;
    let selected_refs = selected_refs_for_commit(repo_root, &commit)?;
    let workflow = read_workflow_at_ref(repo_root, &commit)?;
    let params = read_release_params(repo_root, &commit, workflow.as_ref())?;
    let metrics = read_release_metrics(repo_root, &commit, workflow.as_ref())?;
    let large_files = pointer_inventory(repo_root, &commit)?;

    let manifest = ReleaseManifest {
        schema_version: RELEASE_MANIFEST_SCHEMA_VERSION,
        release_id,
        revision: ReleaseRevision {
            requested: args.rev.clone(),
            commit,
        },
        selected_refs,
        crab: ReleaseCrabInventory { large_files },
        workflow: ReleaseWorkflowMetadata {
            params,
            metrics,
            stages: Vec::new(),
        },
        signature: ReleaseSignature::default(),
    };
    let digest = manifest.content_digest()?;
    Ok(ReleaseCreatePayload {
        manifest,
        digest,
        output_path: args.output.as_ref().map(|path| path.display().to_string()),
        published: false,
        published_path: None,
    })
}

async fn publish_manifest(
    repo_root: &Path,
    manifest: &ReleaseManifest,
) -> Result<ReleasePublishResult> {
    let remote_url = crate::cmd::workflow::read_crab_remote_url(repo_root)?;
    let parsed = CrabUrl::parse(&remote_url)?;
    let config = Config::resolve_for_repo(repo_root)?;
    let cancel = CancellationToken::new();
    let store = crate::auth::build_store(&config, &parsed, "release.publish", &cancel).await?;
    let path = publish_manifest_to_store(&store, &parsed.repo_path, manifest).await?;
    Ok(ReleasePublishResult { remote_url, path })
}

async fn publish_manifest_to_store(
    store: &crate::storage::Store,
    repo_prefix: &str,
    manifest: &ReleaseManifest,
) -> Result<String> {
    let path = release_object_path(repo_prefix, &manifest.release_id)?;
    store
        .put(&path, Bytes::from(manifest.canonical_bytes()?))
        .await?;
    Ok(path.to_string())
}

fn release_object_path(repo_prefix: &str, release_id: &str) -> Result<ObjectPath> {
    validate_release_id(release_id)?;
    let repo_prefix = repo_prefix.trim_matches('/');
    if repo_prefix.is_empty() {
        return Ok(ObjectPath::from(format!(
            ".crab/releases/{release_id}.json"
        )));
    }
    Ok(ObjectPath::from(format!(
        "{repo_prefix}/.crab/releases/{release_id}.json"
    )))
}

fn release_namespace_path(repo_prefix: &str) -> ObjectPath {
    ObjectPath::from(release_namespace_prefix(repo_prefix))
}

fn release_namespace_prefix(repo_prefix: &str) -> String {
    let repo_prefix = repo_prefix.trim_matches('/');
    if repo_prefix.is_empty() {
        ".crab/releases/".to_owned()
    } else {
        format!("{repo_prefix}/.crab/releases/")
    }
}

fn release_id_from_object_path(repo_prefix: &str, path: &ObjectPath) -> Option<String> {
    let prefix = release_namespace_prefix(repo_prefix);
    let leaf = path.as_ref().strip_prefix(&prefix)?.strip_suffix(".json")?;
    if leaf.is_empty() || leaf.contains('/') {
        return None;
    }
    Some(leaf.to_owned())
}

fn validate_release_id(release_id: &str) -> Result<()> {
    if release_id.trim().is_empty()
        || release_id == "."
        || release_id == ".."
        || release_id
            .chars()
            .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
    {
        return Err(CrabError::Configuration {
            key: "release id".to_owned(),
            origin: "release id must be non-empty and must not contain path separators or controls"
                .to_owned(),
        });
    }
    Ok(())
}

fn record_release_publish_audit(
    repo_root: &Path,
    remote_url: &str,
    payload: &ReleaseCreatePayload,
) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "release.publish".to_owned(),
        outcome: AuditOutcome::Success,
        actor: None,
        repository: Some(remote_url.to_owned()),
        details: serde_json::json!({
            "release_id": payload.manifest.release_id.clone(),
            "manifest_digest": payload.digest.clone(),
            "published_path": payload.published_path.clone(),
        }),
    });
    append_event(&repo_root.join(default_log_path()), &event)
}

fn export_manifest_payload(
    manifest: &ReleaseManifest,
    output_path: &Path,
) -> Result<ReleaseExportPayload> {
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    std::fs::write(output_path, manifest.canonical_bytes()?).map_err(CrabError::Io)?;
    Ok(ReleaseExportPayload {
        release_id: manifest.release_id.clone(),
        manifest_digest: manifest.identity_digest()?,
        output_path: output_path.display().to_string(),
    })
}

fn read_manifest(path: &Path) -> Result<ReleaseManifest> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    read_manifest_bytes(&bytes, &path.display().to_string())
}

fn read_manifest_bytes(bytes: &[u8], source: &str) -> Result<ReleaseManifest> {
    serde_json::from_slice(bytes).map_err(|e| CrabError::Configuration {
        key: source.to_owned(),
        origin: format!("invalid release manifest JSON: {e}"),
    })
}

async fn verify_manifest_payload(
    manifest: &ReleaseManifest,
    deep: bool,
    repo_root: Option<&Path>,
    signature_verifier: Option<ReleaseSignatureVerifier<'_>>,
) -> Result<ReleaseVerifyPayload> {
    let mut payload =
        verify_manifest_metadata_payload(manifest, deep, repo_root, signature_verifier)?;
    if deep && let Some(root) = repo_root {
        verify_deep_manifest_content(root, manifest, &mut payload.issues).await?;
        payload.verified = payload.issues.is_empty();
    }
    Ok(payload)
}

fn verify_manifest_metadata_payload(
    manifest: &ReleaseManifest,
    deep: bool,
    repo_root: Option<&Path>,
    signature_verifier: Option<ReleaseSignatureVerifier<'_>>,
) -> Result<ReleaseVerifyPayload> {
    let manifest_digest = manifest.identity_digest()?;
    let content_digest = manifest.content_digest()?;
    let mut issues = Vec::new();
    verify_manifest_shape(manifest, &mut issues);
    let signature_verified = verify_signature_metadata(manifest, signature_verifier, &mut issues)?;
    if deep {
        match repo_root {
            Some(root) => verify_deep_manifest_metadata(root, manifest, &mut issues)?,
            None => issues.push(ReleaseVerifyIssue {
                code: "release.deep.no_repository".to_owned(),
                message: "deep verification requires a Git working tree".to_owned(),
            }),
        }
    }
    let verified = issues.is_empty();

    Ok(ReleaseVerifyPayload {
        release_id: manifest.release_id.clone(),
        verified,
        deep,
        manifest_digest,
        content_digest,
        signature_state: manifest.signature.state,
        signature_verified,
        issues,
    })
}

fn verify_manifest_shape(manifest: &ReleaseManifest, issues: &mut Vec<ReleaseVerifyIssue>) {
    if manifest.schema_version != RELEASE_MANIFEST_SCHEMA_VERSION {
        issues.push(ReleaseVerifyIssue {
            code: "release.schema.unsupported".to_owned(),
            message: format!(
                "manifest schema version {} is not supported by this Crab build",
                manifest.schema_version
            ),
        });
    }
    if let Err(err) = validate_release_id(&manifest.release_id) {
        issues.push(ReleaseVerifyIssue {
            code: "release.id.invalid".to_owned(),
            message: err.to_string(),
        });
    }
}

fn verify_deep_manifest_metadata(
    repo_root: &Path,
    manifest: &ReleaseManifest,
    issues: &mut Vec<ReleaseVerifyIssue>,
) -> Result<()> {
    match resolve_commit(repo_root, &manifest.revision.commit) {
        Ok(commit) if commit == manifest.revision.commit => {}
        Ok(commit) => issues.push(ReleaseVerifyIssue {
            code: "release.deep.commit_mismatch".to_owned(),
            message: format!(
                "manifest commit {} resolved to {commit}",
                manifest.revision.commit
            ),
        }),
        Err(err) => {
            issues.push(ReleaseVerifyIssue {
                code: "release.deep.missing_commit".to_owned(),
                message: format!(
                    "manifest commit {} is not present in the local repository: {err}",
                    manifest.revision.commit
                ),
            });
            return Ok(());
        }
    }

    let observed_large_files = pointer_inventory(repo_root, &manifest.revision.commit)?;
    let expected_large_files = manifest.normalized().crab.large_files;
    if observed_large_files != expected_large_files {
        issues.push(ReleaseVerifyIssue {
            code: "release.deep.pointer_inventory_mismatch".to_owned(),
            message: format!(
                "manifest lists {} Crab pointer file(s), local revision has {}",
                expected_large_files.len(),
                observed_large_files.len()
            ),
        });
    }

    let observed_refs = selected_refs_for_commit(repo_root, &manifest.revision.commit)?;
    for (name, expected) in &manifest.selected_refs {
        match observed_refs.get(name) {
            Some(observed) if observed == expected => {}
            Some(observed) => issues.push(ReleaseVerifyIssue {
                code: "release.deep.ref_mismatch".to_owned(),
                message: format!(
                    "ref {name} now points to {}, manifest recorded {}",
                    observed.oid, expected.oid
                ),
            }),
            None => issues.push(ReleaseVerifyIssue {
                code: "release.deep.ref_missing".to_owned(),
                message: format!("ref {name} from the manifest is not present locally"),
            }),
        }
    }
    Ok(())
}

async fn verify_deep_manifest_content(
    repo_root: &Path,
    manifest: &ReleaseManifest,
    issues: &mut Vec<ReleaseVerifyIssue>,
) -> Result<()> {
    if manifest.crab.large_files.is_empty() {
        return Ok(());
    }

    let hydrator = match release_deep_hydrator(repo_root).await {
        Ok(hydrator) => hydrator,
        Err(err) => {
            issues.push(ReleaseVerifyIssue {
                code: "release.deep.content_unavailable".to_owned(),
                message: format!("deep byte verification requires a readable Crab remote: {err}"),
            });
            return Ok(());
        }
    };

    verify_deep_manifest_content_with(repo_root, manifest, issues, |pointer_bytes| {
        let hydrator = Arc::clone(&hydrator);
        async move { hydrator.reconstruct_from_pointer(&pointer_bytes).await }
    })
    .await
}

async fn release_deep_hydrator(
    repo_root: &Path,
) -> Result<Arc<crate::cmd::hydrate::ShardHydrator>> {
    let remote = open_release_remote(repo_root, "release.verify.deep").await?;
    let caching_store = crab_cache_store::CachingStore::new(remote.store, &remote.config.cache)?;
    let hydrator = crate::cmd::hydrate::ShardHydrator::with_config_from_cli_layout(
        caching_store,
        remote.router,
        &remote.config,
    )?;
    Ok(Arc::new(hydrator))
}

async fn verify_deep_manifest_content_with<F, Fut>(
    repo_root: &Path,
    manifest: &ReleaseManifest,
    issues: &mut Vec<ReleaseVerifyIssue>,
    mut reconstruct: F,
) -> Result<()>
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let git_dir = params::find_git_dir(repo_root)?;
    let normalized = manifest.normalized();
    for file in &normalized.crab.large_files {
        let path = Path::new(&file.path);
        let Some(pointer_bytes) =
            params::read_blob_at_ref(&git_dir, &manifest.revision.commit, path)?
        else {
            issues.push(ReleaseVerifyIssue {
                code: "release.deep.pointer_missing".to_owned(),
                message: format!(
                    "manifest path {} is not present at {}",
                    file.path, manifest.revision.commit
                ),
            });
            continue;
        };

        if let Err(err) = Pointer::parse(&pointer_bytes) {
            issues.push(ReleaseVerifyIssue {
                code: "release.deep.pointer_invalid".to_owned(),
                message: format!("manifest path {} is not a Crab pointer: {err}", file.path),
            });
            continue;
        }

        match reconstruct(pointer_bytes).await {
            Ok(content) => verify_reconstructed_release_file(file, &content, issues),
            Err(err) => issues.push(ReleaseVerifyIssue {
                code: "release.deep.reconstruction_failed".to_owned(),
                message: format!(
                    "failed to reconstruct {} from Crab storage: {err}",
                    file.path
                ),
            }),
        }
    }
    Ok(())
}

fn verify_reconstructed_release_file(
    file: &ReleaseLargeFile,
    content: &[u8],
    issues: &mut Vec<ReleaseVerifyIssue>,
) {
    let actual_hash = blake3_digest(content);
    if actual_hash != file.file_hash {
        issues.push(ReleaseVerifyIssue {
            code: "release.deep.content_hash_mismatch".to_owned(),
            message: format!(
                "reconstructed {} hashes to {actual_hash}, manifest recorded {}",
                file.path, file.file_hash
            ),
        });
    }

    let actual_size = u64::try_from(content.len()).unwrap_or(u64::MAX);
    if actual_size != file.size {
        issues.push(ReleaseVerifyIssue {
            code: "release.deep.content_size_mismatch".to_owned(),
            message: format!(
                "reconstructed {} is {actual_size} byte(s), manifest recorded {}",
                file.path, file.size
            ),
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct ReleaseSignatureVerifier<'a> {
    signature_path: &'a Path,
    public_key_path: &'a Path,
}

fn release_signature_verifier<'a>(
    signature_path: Option<&'a Path>,
    public_key_path: Option<&'a Path>,
) -> Option<ReleaseSignatureVerifier<'a>> {
    match (signature_path, public_key_path) {
        (Some(signature_path), Some(public_key_path)) => Some(ReleaseSignatureVerifier {
            signature_path,
            public_key_path,
        }),
        _ => None,
    }
}

fn verify_signature_metadata(
    manifest: &ReleaseManifest,
    verifier: Option<ReleaseSignatureVerifier<'_>>,
    issues: &mut Vec<ReleaseVerifyIssue>,
) -> Result<bool> {
    match manifest.signature.state {
        ReleaseSignatureState::Unsigned => {
            if manifest.signature.key_id.is_some() || manifest.signature.signature_digest.is_some()
            {
                issues.push(ReleaseVerifyIssue {
                    code: "release.signature.unsigned_with_metadata".to_owned(),
                    message: "unsigned release manifests must not carry key or signature digests"
                        .to_owned(),
                });
            }
            if verifier.is_some() {
                issues.push(ReleaseVerifyIssue {
                    code: "release.signature.unexpected_verifier".to_owned(),
                    message: "signature verifier inputs were provided for an unsigned manifest"
                        .to_owned(),
                });
            }
            Ok(false)
        }
        ReleaseSignatureState::Signed => {
            let Some(key_id) = manifest.signature.key_id.as_deref() else {
                issues.push(ReleaseVerifyIssue {
                    code: "release.signature.missing_key".to_owned(),
                    message: "signed release manifest is missing key_id".to_owned(),
                });
                return Ok(false);
            };
            let Some(signature_digest) = manifest.signature.signature_digest.as_deref() else {
                issues.push(ReleaseVerifyIssue {
                    code: "release.signature.missing_digest".to_owned(),
                    message: "signed release manifest is missing signature_digest".to_owned(),
                });
                return Ok(false);
            };
            let Some(verifier) = verifier else {
                issues.push(ReleaseVerifyIssue {
                    code: "release.signature.verifier_required".to_owned(),
                    message: "signed release manifests require --signature and --public-key"
                        .to_owned(),
                });
                return Ok(false);
            };
            verify_detached_ed25519_signature(manifest, key_id, signature_digest, verifier, issues)
        }
        ReleaseSignatureState::Unsupported => {
            issues.push(ReleaseVerifyIssue {
                code: "release.signature.unsupported".to_owned(),
                message: "release manifest declares an unsupported signature state".to_owned(),
            });
            Ok(false)
        }
    }
}

fn verify_detached_ed25519_signature(
    manifest: &ReleaseManifest,
    key_id: &str,
    signature_digest: &str,
    verifier: ReleaseSignatureVerifier<'_>,
    issues: &mut Vec<ReleaseVerifyIssue>,
) -> Result<bool> {
    let public_key = read_base64_or_raw_file(verifier.public_key_path, 32, "public key")?;
    let signature = read_base64_or_raw_file(verifier.signature_path, 64, "signature")?;
    let observed_key_id = blake3_digest(&public_key);
    if key_id != observed_key_id {
        issues.push(ReleaseVerifyIssue {
            code: "release.signature.key_mismatch".to_owned(),
            message: format!("manifest key_id {key_id} does not match {observed_key_id}"),
        });
    }
    let observed_signature_digest = blake3_digest(&signature);
    if signature_digest != observed_signature_digest {
        issues.push(ReleaseVerifyIssue {
            code: "release.signature.digest_mismatch".to_owned(),
            message: format!(
                "manifest signature_digest {signature_digest} does not match {observed_signature_digest}"
            ),
        });
    }

    let identity_bytes = manifest.unsigned_identity().canonical_bytes()?;
    let public_key = UnparsedPublicKey::new(&ED25519, public_key);
    if public_key.verify(&identity_bytes, &signature).is_err() {
        issues.push(ReleaseVerifyIssue {
            code: "release.signature.invalid".to_owned(),
            message: "detached Ed25519 signature did not verify the unsigned manifest identity"
                .to_owned(),
        });
    }

    Ok(!issues
        .iter()
        .any(|issue| issue.code.starts_with("release.signature.")))
}

fn blake3_digest(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

fn read_base64_or_raw_file(path: &Path, raw_len: usize, label: &str) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    if bytes.len() == raw_len {
        return Ok(bytes);
    }
    let text =
        std::str::from_utf8(&bytes)
            .map(str::trim)
            .map_err(|err| CrabError::Configuration {
                key: path.display().to_string(),
                origin: format!("{label} file is neither raw bytes nor UTF-8 base64: {err}"),
            })?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|err| CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("invalid base64 {label} file: {err}"),
        })?;
    if decoded.len() != raw_len {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("{label} must decode to {raw_len} bytes"),
        });
    }
    Ok(decoded)
}

fn signature_state_label(state: ReleaseSignatureState) -> &'static str {
    match state {
        ReleaseSignatureState::Unsigned => "unsigned",
        ReleaseSignatureState::Signed => "signed",
        ReleaseSignatureState::Unsupported => "unsupported",
    }
}

fn ensure_clean_worktree(repo_root: &Path) -> Result<()> {
    let output = git_output(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        "git status",
    )?;
    if output.is_empty() {
        return Ok(());
    }

    let first_path = first_dirty_path(&output).unwrap_or_else(|| "unknown path".to_owned());
    Err(CrabError::Configuration {
        key: "release create".to_owned(),
        origin: format!(
            "working tree has uncommitted changes at {first_path}; commit changes or pass --allow-dirty to record the committed revision anyway"
        ),
    })
}

fn first_dirty_path(output: &[u8]) -> Option<String> {
    let record = output
        .split(|byte| *byte == 0)
        .find(|record| !record.is_empty())?;
    let path = record.get(3..).unwrap_or(record);
    Some(String::from_utf8_lossy(path).into_owned())
}

fn current_repo_root(command: &str) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: command.to_owned(),
            origin: format!("not inside a Git working tree: {stderr}"),
        });
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return Err(CrabError::Internal(
            "git rev-parse --show-toplevel returned an empty path".to_owned(),
        ));
    }
    Ok(PathBuf::from(root))
}

fn resolve_commit(repo_root: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    let output = git_output(
        repo_root,
        &["rev-parse", "--verify", &spec],
        "git rev-parse",
    )?;
    let commit = String::from_utf8_lossy(&output).trim().to_owned();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CrabError::Internal(format!(
            "git rev-parse returned unexpected commit '{commit}'"
        )));
    }
    Ok(commit)
}

fn selected_refs_for_commit(
    repo_root: &Path,
    commit: &str,
) -> Result<BTreeMap<String, ReleaseRefTarget>> {
    let output = git_output(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)%09%(*objectname)",
            "refs/heads",
            "refs/tags",
        ],
        "git for-each-ref",
    )?;
    let text = String::from_utf8_lossy(&output);
    let mut refs = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(name) = parts.next().filter(|value| !value.is_empty()) else {
            continue;
        };
        let Some(oid) = parts.next().filter(|value| !value.is_empty()) else {
            continue;
        };
        let peeled = parts.next().filter(|value| !value.is_empty());
        if oid == commit || peeled == Some(commit) {
            refs.insert(
                name.to_owned(),
                ReleaseRefTarget {
                    oid: oid.to_owned(),
                    peeled_oid: peeled.map(ToOwned::to_owned),
                },
            );
        }
    }
    Ok(refs)
}

fn pointer_inventory(repo_root: &Path, rev: &str) -> Result<Vec<ReleaseLargeFile>> {
    let output = git_output(
        repo_root,
        &["ls-tree", "-r", "-l", "-z", rev],
        "git ls-tree",
    )?;
    let mut files = Vec::new();
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let metadata = String::from_utf8_lossy(&record[..tab]);
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|e| CrabError::Internal(format!("release manifest path is not UTF-8: {e}")))?;
        let mut parts = metadata.split_whitespace();
        let _mode = parts.next();
        let Some(kind) = parts.next() else {
            continue;
        };
        let Some(oid) = parts.next() else {
            continue;
        };
        let Some(size) = parts.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        if kind != "blob" || size > MAX_POINTER_SIZE {
            continue;
        }
        let blob = git_output(repo_root, &["cat-file", "-p", oid], "git cat-file")?;
        let Ok(pointer) = Pointer::parse(&blob) else {
            continue;
        };
        files.push(ReleaseLargeFile {
            path: path.to_owned(),
            file_hash: b3_hex(pointer.file_hash),
            size: pointer.size,
            shard_hint: pointer.shard_hint.map(b3_hex),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn read_workflow_at_ref(repo_root: &Path, commit: &str) -> Result<Option<Workflow>> {
    let git_dir = params::find_git_dir(repo_root)?;
    let Some(bytes) = params::read_blob_at_ref(&git_dir, commit, Path::new("crab.yaml"))? else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes).map_err(|e| CrabError::Configuration {
        key: "crab.yaml".to_owned(),
        origin: format!("workflow file is not UTF-8 at release revision: {e}"),
    })?;
    Ok(yaml::parse_at(&repo_root.join("crab.yaml"), &text).map(Some)?)
}

fn read_release_params(
    repo_root: &Path,
    commit: &str,
    workflow: Option<&Workflow>,
) -> Result<BTreeMap<String, String>> {
    let paths = workflow.map_or(&[][..], |workflow| workflow.params.as_slice());
    Ok(scalar_map_to_strings(params::read_at_ref(
        repo_root, commit, paths,
    )?))
}

fn read_release_metrics(
    repo_root: &Path,
    commit: &str,
    workflow: Option<&Workflow>,
) -> Result<BTreeMap<String, String>> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut paths = BTreeSet::new();
    if let Some(workflow) = workflow {
        paths.extend(workflow.metrics.iter().cloned());
        for stage in workflow.stages.values() {
            for metric in &stage.metrics {
                paths.insert(stage_scoped_path(stage.wdir.as_deref(), metric));
            }
        }
    }
    if paths.is_empty() {
        paths.insert(PathBuf::from("metrics.json"));
    }

    let mut metrics = BTreeMap::new();
    for path in paths {
        let Some(bytes) = params::read_blob_at_ref(&git_dir, commit, &path)? else {
            continue;
        };
        let parsed = params::parse(&bytes, &path)?;
        for (key, value) in parsed {
            metrics.insert(format!("{}:{key}", path.display()), value.display());
        }
    }
    Ok(metrics)
}

fn scalar_map_to_strings(map: params::ScalarMap) -> BTreeMap<String, String> {
    map.into_iter()
        .map(|(key, value)| (key, value.display()))
        .collect()
}

fn stage_scoped_path(wdir: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    wdir.map_or_else(|| path.to_path_buf(), |wdir| wdir.join(path))
}

fn b3_hex(bytes: [u8; 32]) -> String {
    format!("b3:{}", blake3::Hash::from_bytes(bytes).to_hex())
}

fn git_output(repo_root: &Path, args: &[&str], label: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn {label}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!("{label} failed: {stderr}")));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn git(repo: &Path, args: &[&str]) -> TestResult {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()?;
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
        let output = git_output(repo, args, "git")?;
        Ok(String::from_utf8_lossy(&output).trim().to_owned())
    }

    fn sample_repo() -> TestResult<(tempfile::TempDir, String, u64)> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();
        git(repo, &["init"])?;
        git(repo, &["config", "user.email", "crab@example.invalid"])?;
        git(repo, &["config", "user.name", "Crab Test"])?;

        let model = b"model-bytes";
        let pointer = Pointer {
            file_hash: *blake3::hash(model).as_bytes(),
            size: u64::try_from(model.len())?,
            shard_hint: None,
        };
        std::fs::write(repo.join("model.bin"), pointer.serialize())?;
        std::fs::write(repo.join("params.yaml"), "model:\n  lr: 0.01\n")?;
        std::fs::write(repo.join("metrics.json"), r#"{"accuracy": 0.95}"#)?;
        std::fs::write(
            repo.join("crab.yaml"),
            "params:\n  - params.yaml\nmetrics:\n  - metrics.json\nstages:\n  train:\n    cmd: python train.py\n    outs:\n      - model.bin\n",
        )?;
        git(repo, &["add", "."])?;
        git(repo, &["commit", "-m", "seed"])?;
        git(repo, &["tag", "v1"])?;
        Ok((dir, b3_hex(pointer.file_hash), pointer.size))
    }

    #[test]
    fn release_subcommands_resolve_output_mode_from_json_flag() {
        let verify = ReleaseCmd::Verify(ReleaseVerifyArgs {
            manifest: None,
            name: Some("model-v1".to_owned()),
            deep: false,
            signature: None,
            public_key: None,
            json: true,
        });
        assert_eq!(verify.output_mode(), OutputMode::Json);

        let list = ReleaseCmd::List(ReleaseListArgs {
            remote: true,
            json: false,
        });
        assert_eq!(list.schema_name(), RELEASE_LIST_SCHEMA);
    }

    #[test]
    fn create_payload_populates_manifest_from_git_and_workflow() -> TestResult {
        let (repo, expected_file_hash, expected_size) = sample_repo()?;
        let commit = git_stdout(repo.path(), &["rev-parse", "HEAD"])?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };

        let payload = create_payload(&args, repo.path())?;

        assert_eq!(payload.manifest.release_id, "model-v1");
        assert_eq!(payload.manifest.revision.commit, commit);
        assert!(payload.manifest.selected_refs.contains_key("refs/tags/v1"));
        assert_eq!(payload.manifest.crab.large_files.len(), 1);
        assert_eq!(payload.manifest.crab.large_files[0].path, "model.bin");
        assert_eq!(
            payload.manifest.crab.large_files[0].file_hash,
            expected_file_hash
        );
        assert_eq!(payload.manifest.crab.large_files[0].size, expected_size);
        assert_eq!(
            payload
                .manifest
                .workflow
                .params
                .get("model.lr")
                .map(String::as_str),
            Some("0.01")
        );
        assert_eq!(
            payload
                .manifest
                .workflow
                .metrics
                .get("metrics.json:accuracy")
                .map(String::as_str),
            Some("0.95")
        );
        assert!(payload.digest.starts_with("b3:"));
        Ok(())
    }

    #[test]
    fn create_payload_refuses_dirty_worktree_by_default() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        std::fs::write(repo.path().join("notes.txt"), "uncommitted")?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };

        let err = create_payload(&args, repo.path()).expect_err("dirty worktree should fail");

        assert!(matches!(
            err,
            CrabError::Configuration { key, origin }
                if key == "release create"
                    && origin.contains("notes.txt")
                    && origin.contains("--allow-dirty")
        ));
        Ok(())
    }

    #[test]
    fn create_payload_allows_dirty_worktree_when_requested() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        std::fs::write(repo.path().join("notes.txt"), "uncommitted")?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: true,
            publish: false,
            json: false,
        };

        let payload = create_payload(&args, repo.path())?;

        assert_eq!(payload.manifest.crab.large_files[0].path, "model.bin");
        Ok(())
    }

    #[test]
    fn release_object_path_uses_repo_release_namespace() -> TestResult {
        let path = release_object_path("org/repo", "model-v1")?;

        assert_eq!(path.as_ref(), "org/repo/.crab/releases/model-v1.json");
        Ok(())
    }

    #[test]
    fn release_id_rejects_path_segments() {
        let err = release_object_path("org/repo", "../model").expect_err("invalid release id");

        assert!(matches!(
            err,
            CrabError::Configuration { key, origin }
                if key == "release id" && origin.contains("path separators")
        ));
    }

    #[test]
    fn verify_payload_reports_unsigned_manifest_as_verified() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;

        let verify = verify_manifest_metadata_payload(&create.manifest, false, None, None)?;

        assert!(verify.verified);
        assert_eq!(verify.signature_state, ReleaseSignatureState::Unsigned);
        assert!(!verify.signature_verified);
        assert!(verify.issues.is_empty());
        assert_eq!(verify.manifest_digest, create.manifest.identity_digest()?);
        Ok(())
    }

    #[test]
    fn verify_payload_rejects_signed_manifest_without_verifier() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let mut create = create_payload(&args, repo.path())?;
        create.manifest.signature = ReleaseSignature {
            state: ReleaseSignatureState::Signed,
            key_id: Some("key-1".to_owned()),
            signature_digest: Some("b3:signature".to_owned()),
        };

        let verify = verify_manifest_metadata_payload(&create.manifest, false, None, None)?;

        assert!(!verify.verified);
        assert_eq!(verify.signature_state, ReleaseSignatureState::Signed);
        assert!(!verify.signature_verified);
        assert!(verify.issues.iter().any(|issue| {
            issue.code == "release.signature.verifier_required"
                && issue.message.contains("--signature")
        }));
        Ok(())
    }

    #[test]
    fn verify_payload_accepts_valid_detached_ed25519_signature() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let mut create = create_payload(&args, repo.path())?;
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| std::io::Error::other("failed to generate Ed25519 test key"))?;
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| std::io::Error::other("failed to parse Ed25519 test key"))?;
        let identity_bytes = create.manifest.unsigned_identity().canonical_bytes()?;
        let signature = key_pair.sign(&identity_bytes);
        let public_key = key_pair.public_key().as_ref();
        create.manifest.signature = ReleaseSignature {
            state: ReleaseSignatureState::Signed,
            key_id: Some(blake3_digest(public_key)),
            signature_digest: Some(blake3_digest(signature.as_ref())),
        };
        let signature_path = repo.path().join("release.sig");
        let public_key_path = repo.path().join("release.ed25519.pub");
        std::fs::write(&signature_path, signature.as_ref())?;
        std::fs::write(&public_key_path, public_key)?;

        let verify = verify_manifest_metadata_payload(
            &create.manifest,
            false,
            None,
            release_signature_verifier(Some(&signature_path), Some(&public_key_path)),
        )?;

        assert!(verify.verified);
        assert_eq!(verify.signature_state, ReleaseSignatureState::Signed);
        assert!(verify.signature_verified);
        assert!(verify.issues.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn deep_verify_requires_remote_byte_reconstruction() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;

        let verify =
            verify_manifest_payload(&create.manifest, true, Some(repo.path()), None).await?;

        assert!(!verify.verified);
        assert!(verify.deep);
        assert!(
            verify
                .issues
                .iter()
                .any(|issue| issue.code == "release.deep.content_unavailable")
        );
        Ok(())
    }

    #[tokio::test]
    async fn deep_verify_reconstructs_manifest_pointer_content() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;
        let mut issues = Vec::new();

        verify_deep_manifest_content_with(
            repo.path(),
            &create.manifest,
            &mut issues,
            |bytes| async move {
                Pointer::parse(&bytes)?;
                Ok(b"model-bytes".to_vec())
            },
        )
        .await?;

        assert!(issues.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn deep_verify_reports_reconstructed_content_mismatch() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;
        let mut issues = Vec::new();

        verify_deep_manifest_content_with(
            repo.path(),
            &create.manifest,
            &mut issues,
            |bytes| async move {
                Pointer::parse(&bytes)?;
                Ok(b"different-model-bytes".to_vec())
            },
        )
        .await?;

        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "release.deep.content_hash_mismatch")
        );
        Ok(())
    }

    #[tokio::test]
    async fn deep_verify_reports_pointer_inventory_mismatch() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let mut create = create_payload(&args, repo.path())?;
        create.manifest.crab.large_files[0].size += 1;

        let verify =
            verify_manifest_payload(&create.manifest, true, Some(repo.path()), None).await?;

        assert!(!verify.verified);
        assert!(
            verify
                .issues
                .iter()
                .any(|issue| { issue.code == "release.deep.pointer_inventory_mismatch" })
        );
        Ok(())
    }

    #[test]
    fn release_publish_audit_appends_local_event() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let mut payload = create_payload(&args, repo.path())?;
        payload.published = true;
        payload.published_path = Some("org/repo/.crab/releases/model-v1.json".to_owned());

        record_release_publish_audit(repo.path(), "crab://bucket/org/repo", &payload)?;

        let events = crate::audit::read_events(&repo.path().join(default_log_path()))?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].operation, "release.publish");
        assert_eq!(events[0].outcome, AuditOutcome::Success);
        assert_eq!(events[0].details["release_id"].as_str(), Some("model-v1"));
        assert!(events[0].digest_valid());
        Ok(())
    }

    #[test]
    fn export_manifest_payload_writes_canonical_manifest() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;
        let output = repo.path().join("dist/release.json");

        let export = export_manifest_payload(&create.manifest, &output)?;

        assert_eq!(export.release_id, "model-v1");
        assert_eq!(export.manifest_digest, create.manifest.identity_digest()?);
        assert_eq!(std::fs::read(&output)?, create.manifest.canonical_bytes()?);
        Ok(())
    }

    #[tokio::test]
    async fn publish_manifest_to_store_is_idempotent_for_same_manifest() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::Store::new(inner);

        let first = publish_manifest_to_store(&store, "org/repo", &create.manifest).await?;
        let second = publish_manifest_to_store(&store, "org/repo", &create.manifest).await?;

        assert_eq!(first, "org/repo/.crab/releases/model-v1.json");
        assert_eq!(second, first);

        let mut changed = create.manifest.clone();
        changed.crab.large_files[0].size += 1;
        let err = publish_manifest_to_store(&store, "org/repo", &changed)
            .await
            .expect_err("changed manifest must conflict");
        assert!(
            matches!(err, CrabError::CasConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_published_releases_reads_remote_manifest_namespace() -> TestResult {
        let (repo, _, _) = sample_repo()?;
        let args = ReleaseCreateArgs {
            name: Some("model-v1".to_owned()),
            rev: "HEAD".to_owned(),
            output: None,
            allow_dirty: false,
            publish: false,
            json: false,
        };
        let create = create_payload(&args, repo.path())?;
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = crate::storage::Store::new(inner);

        publish_manifest_to_store(&store, "org/repo", &create.manifest).await?;
        store
            .put(
                &ObjectPath::from("org/repo/.crab/releases/README.txt"),
                Bytes::from_static(b"not a release manifest"),
            )
            .await?;

        let releases = list_published_releases_from_store(&store, "org/repo").await?;
        let fetched = read_published_manifest_from_store(&store, "org/repo", "model-v1").await?;

        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].release_id, "model-v1");
        assert_eq!(releases[0].path, "org/repo/.crab/releases/model-v1.json");
        assert_eq!(
            releases[0].manifest_digest,
            create.manifest.identity_digest()?
        );
        assert_eq!(fetched, create.manifest);
        Ok(())
    }
}
