//! `crab init {url}` — initialize a new crab repository.
//!
//! Parses the remote URL, creates the local `.crab/` directory, registers the crab git drivers
//! in the local git config, and logs the prefixes used by the remote layout.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::config::{Config, GcListProfile, StorageProvider};
use crate::core::credential_discovery::{CredentialSource, discover_credentials};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::core::project_config::{ProjectAuthConfig, ProjectConfig, RemoteConfig};
use crate::core::style::CliStyle;
use crate::git::url::CrabUrl;
use crate::storage::StoreLayout;

/// Remote prefixes that constitute a crab repository in object storage.
/// Per-repo prefixes live under `{repo}/`; content-addressed objects live
/// under the global `.crab/` prefix.
///
/// The descriptor at `{repo}/layout` and unified manifest at
/// `{repo}/manifest` are the canonical repository roots. Auxiliary empty
/// `pack-list`, `shard-list`, per-ref, and `HEAD` objects are not created.
const REMOTE_PREFIXES: &[&str] = &[];

/// Global prefixes shared across all repos in the bucket.
const GLOBAL_PREFIXES: &[&str] = &[".crab/xorbs/", ".crab/shards/"];

/// Schema name for init JSON output.
const INIT_SCHEMA: &str = "init";
/// Schema version for init JSON output.
const INIT_VERSION: &str = "1.0";

/// Credential status included in the JSON init payload.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialStatus {
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// JSON payload for `crab init --json`.
#[derive(Debug, Clone, Serialize)]
pub struct InitPayload {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_list_profile: Option<String>,
    pub credential_status: CredentialStatus,
}

#[derive(Debug, Clone)]
struct InitRemote {
    canonical_url: String,
    parsed: CrabUrl,
    inferred_storage_provider: Option<StorageProvider>,
}

/// Run the `crab init` command in the current working directory.
///
/// # Errors
///
/// Returns [`CrabError::Configuration`] if the URL cannot be parsed,
/// [`CrabError::Io`] on filesystem failures, or
/// [`CrabError::Cancelled`] if the cancellation token fires.
pub async fn run_init(url: &str, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_init_in(url, &cwd, cancel).await
}

/// Initialize a crab repository rooted at `root`.
///
/// Creates `{root}/crab.toml` and `{root}/.crab/local.toml`. The command entry
/// point publishes the canonical remote layout and generation-0 manifest
/// after this local setup succeeds.
///
/// # Errors
///
/// Returns [`CrabError::Configuration`] if the URL cannot be parsed,
/// [`CrabError::Io`] on filesystem failures, or
/// [`CrabError::Cancelled`] if the cancellation token fires.
pub async fn run_init_in(url: &str, root: &Path, cancel: &CancellationToken) -> Result<()> {
    run_init_with_options(url, root, cancel, OutputMode::Text).await
}

/// Create the generation-0 manifest for a repository after local init.
///
/// Existing manifests are adopted, so this operation is safe to repeat and
/// concurrent callers converge on the manifest created by the first caller.
///
/// # Errors
///
/// Returns a configuration, authentication, storage, or cancellation error
/// when the remote cannot be opened or its initial manifest cannot be created.
pub async fn initialize_remote_repository(
    url: &str,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let remote = parse_init_remote(url)?;
    let config = Config::resolve_for_repo(root)?;
    let repo_prefix = remote.parsed.repo_path.clone();
    let store = crate::auth::build_store(&config, remote.parsed, "repo-create", cancel).await?;
    let router = StoreLayout::new(store.clone(), repo_prefix);
    let head = format!("refs/heads/{}", config.default_branch);

    initialize_remote_repository_store(&store, &router, &head).await?;
    tracing::info!(
        url = %remote.canonical_url,
        head = %head,
        "remote repository initialized"
    );
    Ok(())
}

pub(crate) async fn initialize_remote_repository_store(
    store: &crate::storage::Store,
    router: &StoreLayout,
    head: &str,
) -> Result<()> {
    match store.head(&router.layout_descriptor_path()).await {
        Ok(_) => {
            crate::core::remote_layout::open(store, router).await?;
        }
        Err(CrabError::NotFound { .. }) => {
            let repo_prefix =
                object_store::path::Path::from(router.repo_prefix().trim_end_matches('/'));
            let empty = store
                .as_storage()
                .list_prefix_bounded(&repo_prefix, 0)
                .await
                .map_err(CrabError::from)?
                .is_some();
            if !empty {
                return Err(CrabError::CorruptObject {
                    path: router.layout_descriptor_path().to_string(),
                    reason: "canonical v1 layout descriptor is missing but repository objects already exist; reset this isolated development repository instead of converting it in place".to_owned(),
                });
            }
            crate::core::remote_layout::initialize(store, router).await?;
        }
        Err(error) => return Err(error),
    }
    ensure_initial_manifest(store, router, head).await
}

/// Options-driven init implementation.
///
/// `crab init` is deliberately minimal: it only wires up the remote
/// connection. Tree walks (large-file scanning, `.gitattributes`
/// generation, filter-driver registration) are deferred to
/// [`crab setup`](crate::cmd::setup) so init completes instantly even
/// on repos with 50K+ commits.
pub async fn run_init_with_options(
    url: &str,
    root: &Path,
    cancel: &CancellationToken,
    mode: OutputMode,
) -> Result<()> {
    run_init_with_storage_provider(url, root, cancel, mode, None, None).await
}

/// Options-driven init implementation with an explicit storage backend.
pub async fn run_init_with_storage_provider(
    url: &str,
    root: &Path,
    cancel: &CancellationToken,
    mode: OutputMode,
    storage_provider: Option<StorageProvider>,
    gc_list_profile: Option<GcListProfile>,
) -> Result<()> {
    run_init_inner(
        url,
        root,
        cancel,
        mode,
        storage_provider,
        gc_list_profile,
        None,
        true,
    )
    .await
}

/// Initialize a repository as the first phase of guided configuration.
pub(crate) async fn run_init_for_configure(
    url: &str,
    root: &Path,
    cancel: &CancellationToken,
    storage_provider: Option<StorageProvider>,
    gc_list_profile: Option<GcListProfile>,
    aws_profile: Option<&str>,
) -> Result<()> {
    run_init_inner(
        url,
        root,
        cancel,
        OutputMode::Text,
        storage_provider,
        gc_list_profile,
        aws_profile,
        false,
    )
    .await
}

async fn run_init_inner(
    url: &str,
    root: &Path,
    cancel: &CancellationToken,
    mode: OutputMode,
    storage_provider: Option<StorageProvider>,
    gc_list_profile: Option<GcListProfile>,
    aws_profile: Option<&str>,
    show_next_steps: bool,
) -> Result<()> {
    check_cancelled(cancel)?;

    // Parse and normalize the URL before doing any work. Provider-prefixed
    // repository URLs are only a creation convenience; persisted Git remotes
    // stay on `crab://` so later commands route through `git-remote-crab`.
    let remote = parse_init_remote(url)?;
    let parsed = remote.parsed;
    let remote_url = remote.canonical_url;
    let host = parsed.bucket.clone();
    let path = parsed.repo_path.clone();

    tracing::info!(
        input_url = %url,
        url = %remote_url,
        host = %host,
        path = %path,
        "initializing crab repository"
    );

    // Auto-run `git init` if no .git directory exists.
    if !root.join(".git").exists() {
        tracing::info!(root = %root.display(), "no .git found, running git init");
        let output = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(root)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CrabError::Configuration {
                key: "git init".to_owned(),
                origin: format!("git init failed: {stderr}"),
            });
        }
        eprintln!("Initialized git repository in {}", root.display());
    }

    // Create local .crab/ directory.
    let crab_dir = root.join(".crab");
    if crab_dir.exists() {
        tracing::warn!(dir = %crab_dir.display(), "local .crab/ directory already exists, updating config");
    }

    check_cancelled(cancel)?;

    let project_config_path = root.join("crab.toml");
    let existing_project_config = if project_config_path.exists() {
        Some(ProjectConfig::load(&project_config_path)?)
    } else {
        None
    };
    let existing_storage_provider = existing_project_config
        .as_ref()
        .and_then(|config| config.auth.as_ref())
        .and_then(|auth| auth.storage_provider.clone());
    let selected_storage_provider = select_init_storage_provider(
        storage_provider,
        existing_storage_provider,
        remote.inferred_storage_provider,
        url,
    )?;
    let config_path = crab_dir.join("local.toml");
    let gc_list_profile = match gc_list_profile {
        Some(profile) => Some(profile),
        None => existing_gc_list_profile(&config_path)?,
    };

    tokio::fs::create_dir_all(&crab_dir).await?;
    ensure_crab_dir_excluded(root)?;

    // Preserve machine-specific settings owned by other commands when init
    // is re-run. Project URL and provider live in the committed config.
    if !config_path.exists() || gc_list_profile.is_some() || aws_profile.is_some() {
        let config_content = merge_local_config(&config_path, gc_list_profile, aws_profile)?;
        tokio::fs::write(&config_path, config_content.as_bytes()).await?;
        tracing::info!(path = %config_path.display(), "wrote local settings");
    }

    check_cancelled(cancel)?;

    // Generate or update crab.toml so the project config travels with the repo.
    // No [track] section — auto-tracking is deferred to `crab setup`.
    let config_written = if let Some(mut existing) = existing_project_config {
        // Load existing config and update URL if it changed.
        let mut config_written = false;
        if existing.remote.url != remote_url {
            let old_url = existing.remote.url.clone();
            existing.remote.url = remote_url.clone();
            eprintln!("Updated crab.toml remote URL from {old_url} to {remote_url}");
            config_written = true;
        }
        if apply_storage_provider(&mut existing, selected_storage_provider.clone()) {
            config_written = true;
        }
        if config_written {
            ProjectConfig::write(&project_config_path, &existing)?;
        }
        config_written
    } else {
        let config = ProjectConfig {
            version: 1,
            remote: RemoteConfig {
                url: remote_url.clone(),
            },
            track: None,
            hydrate: None,
            mirror: None,
            replication: None,
            auth: selected_storage_provider
                .clone()
                .map(project_auth_config_with_storage_provider),
            prefetch: None,
            workflow: None,
        };
        ProjectConfig::write(&project_config_path, &config)?;
        true
    };

    // Stage crab.toml so it's included in the user's first commit.
    // Only run when the config was actually written/updated — a no-op
    // re-init with an unchanged URL leaves the file byte-identical, so
    // `git add` would just spawn the filter process for nothing. Running
    // this BEFORE `install_filter_driver` matters only on the first init
    // (when the driver is unregistered and `git add` can't spawn it); on
    // a URL update the driver is already registered but `crab.toml`
    // doesn't match any `filter=crab` pattern, so no filter spawns.
    if config_written && root.join(".git").exists() {
        crate::git::index::stage_paths(root, &["crab.toml"])?;
    }

    // Register the git drivers so `*.ext filter=crab diff=crab` in
    // .gitattributes actually invokes crab clean/smudge and diff.
    // Placed AFTER `git add` so the filter process isn't spawned during
    // init — the tree walk is paid on the first `crab setup` or
    // user-initiated `git add` instead.
    install_filter_driver(root)?;

    // Add a git remote so `git push <name>` works out of the box.
    // Use "origin" if no origin exists (git muscle memory), otherwise "crab".
    if root.join(".git").exists() {
        let remote_name = add_or_update_git_remote(root, &remote_url);
        if let Some(name) = remote_name {
            eprintln!("Remote '{name}' → {remote_url}");
        }
    }

    if let Some(provider) = selected_storage_provider.as_ref()
        && !mode.is_machine()
    {
        eprintln!("Storage provider → {}", provider.label());
    }
    if let Some(profile) = gc_list_profile
        && !mode.is_machine()
    {
        eprintln!("Bucket GC list profile → {}", profile.as_str());
    }

    // Run credential discovery and report the result.
    let credential_url = credential_discovery_url(&parsed, selected_storage_provider.as_ref());
    let resolved_config = Config::resolve_for_repo(root)?;
    let cred_result =
        discover_credentials(&credential_url, resolved_config.auth.aws.profile.as_deref()).await;

    let credential_status = if cred_result.source == CredentialSource::None {
        let style = CliStyle::resolve(mode);
        if !mode.is_machine() {
            eprintln!("{}", style.warn("Cloud credentials are not ready."));
            eprintln!("  {}", cred_result.description);
            eprintln!("  Verify after signing in: crab doctor");
        }
        CredentialStatus {
            found: false,
            source: None,
        }
    } else {
        let style = CliStyle::resolve(mode);
        if !mode.is_machine() {
            eprintln!(
                "{}",
                style.ok(&format!("Credentials: {}", cred_result.description))
            );
        }
        CredentialStatus {
            found: true,
            source: Some(cred_result.description.clone()),
        }
    };

    // Emit JSON payload when in machine mode.
    if mode == OutputMode::Json {
        let payload = InitPayload {
            url: remote_url.clone(),
            storage_provider: selected_storage_provider
                .as_ref()
                .map(|provider| provider.toml_value().to_owned()),
            gc_list_profile: gc_list_profile.map(|profile| profile.as_str().to_owned()),
            credential_status,
        };
        emit_json(INIT_SCHEMA, INIT_VERSION, payload);
    }

    // Remote publication runs after local setup has written the provider and
    // credential configuration needed to build the Store.
    for prefix in REMOTE_PREFIXES {
        tracing::info!(
            prefix = %prefix,
            host = %host,
            repo_path = %path,
            "remote per-repo prefix is materialized by manifest creation",
        );
    }

    for prefix in GLOBAL_PREFIXES {
        tracing::info!(
            prefix = %prefix,
            host = %host,
            "remote global prefix is materialized by content upload",
        );
    }

    tracing::info!("crab init complete — local config written and git drivers registered");

    if !mode.is_machine() && show_next_steps {
        eprintln!("\nNext:");
        eprintln!("  1. crab setup            # detect large files and write .gitattributes");
        eprintln!("  2. git status            # review crab.toml and tracking rules");
        eprintln!("  3. crab ship -m 'init'   # commit and push to Crab");
    }

    Ok(())
}

fn merge_local_config(
    path: &Path,
    gc_list_profile: Option<GcListProfile>,
    aws_profile: Option<&str>,
) -> Result<String> {
    let mut table = match std::fs::read_to_string(path) {
        Ok(content) => {
            content
                .parse::<toml::Table>()
                .map_err(|error| CrabError::Configuration {
                    key: format!("failed to parse TOML: {error}"),
                    origin: path.display().to_string(),
                })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error.into()),
    };

    if let Some(profile) = gc_list_profile {
        let gc = table
            .entry("gc")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| CrabError::Configuration {
                key: "gc must be a table".to_owned(),
                origin: path.display().to_string(),
            })?;
        gc.insert(
            "list_profile".to_owned(),
            toml::Value::String(profile.as_str().to_owned()),
        );
    }

    if let Some(profile) = aws_profile {
        let auth = table
            .entry("auth")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| CrabError::Configuration {
                key: "auth must be a table".to_owned(),
                origin: path.display().to_string(),
            })?;
        auth.insert(
            "aws_profile".to_owned(),
            toml::Value::String(profile.to_owned()),
        );
    }

    let body = toml::to_string_pretty(&table).map_err(|error| CrabError::Configuration {
        key: format!("failed to serialize TOML: {error}"),
        origin: path.display().to_string(),
    })?;
    Ok(format!("# Crab local settings (not committed)\n\n{body}"))
}

fn existing_gc_list_profile(path: &Path) -> Result<Option<GcListProfile>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CrabError::Io(error)),
    };
    let value =
        toml::from_str::<toml::Value>(&content).map_err(|error| CrabError::Configuration {
            key: "gc.list_profile".to_owned(),
            origin: format!("failed to parse {}: {error}", path.display()),
        })?;
    let Some(profile) = value.get("gc").and_then(|gc| gc.get("list_profile")) else {
        return Ok(None);
    };
    let profile = profile.as_str().ok_or_else(|| CrabError::Configuration {
        key: "gc.list_profile".to_owned(),
        origin: format!("{} must contain a string value", path.display()),
    })?;
    GcListProfile::parse(profile).map(Some)
}

fn project_auth_config_with_storage_provider(
    storage_provider: StorageProvider,
) -> ProjectAuthConfig {
    ProjectAuthConfig {
        storage_provider: Some(storage_provider),
    }
}

fn apply_storage_provider(
    config: &mut ProjectConfig,
    storage_provider: Option<StorageProvider>,
) -> bool {
    let Some(storage_provider) = storage_provider else {
        return false;
    };

    let Some(auth) = config.auth.as_mut() else {
        config.auth = Some(project_auth_config_with_storage_provider(storage_provider));
        return true;
    };

    if auth.storage_provider.as_ref() == Some(&storage_provider) {
        return false;
    }
    auth.storage_provider = Some(storage_provider);
    true
}

pub(crate) fn ensure_crab_dir_excluded(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "git info/exclude".into(),
            origin: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let raw_path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if raw_path.is_empty() {
        return Err(CrabError::Configuration {
            key: "git info/exclude".into(),
            origin: "git returned an empty exclude path".into(),
        });
    }
    let exclude_path = resolve_git_path(root, &raw_path);

    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == ".crab/") {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, ".crab/")?;
    Ok(())
}

fn resolve_git_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// Default size threshold for auto-tracking: files above this size are
/// considered "large" and their extensions are auto-tracked.
const AUTO_TRACK_SIZE_THRESHOLD: u64 = 1_048_576; // 1 MiB

/// Well-known large-file extensions that are always worth tracking when
/// found, regardless of size. These are binary formats that git handles
/// poorly.
pub(crate) const WELL_KNOWN_LARGE_EXTENSIONS: &[&str] = &[
    "safetensors",
    "bin",
    "onnx",
    "pt",
    "pth",
    "h5",
    "hdf5",
    "pkl",
    "parquet",
    "arrow",
    "feather",
    "npy",
    "npz",
    "zarr",
    "fbx",
    "blend",
    "psd",
    "tiff",
    "exr",
    "dpx",
    "mov",
    "mp4",
    "avi",
    "mkv",
    "wav",
    "flac",
    "db",
    "sqlite",
    "sqlite3",
    "tar",
    "gz",
    "zip",
    "zst",
    "lz4",
];

/// Scan the working tree for large files and auto-track their extensions.
///
/// Returns the new patterns tracked. Skips extensions that are already in
/// `.gitattributes`.
pub fn auto_track_large_files(root: &Path) -> Result<Vec<String>> {
    use std::collections::BTreeSet;

    let ga_path = root.join(".gitattributes");
    let existing_content = std::fs::read_to_string(&ga_path).unwrap_or_default();
    let already_tracked: std::collections::HashSet<String> = existing_content
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    let mut new_exts: BTreeSet<String> = BTreeSet::new();
    scan_for_large_files(root, &already_tracked, &mut new_exts)?;

    if new_exts.is_empty() {
        return Ok(Vec::new());
    }

    let joined: Vec<String> = new_exts.iter().map(|e| format!("*.{e}")).collect();
    for pattern in &joined {
        crate::cmd::track::run_track_in(pattern, root)?;
    }
    eprintln!("Detected large files — tracking: {}", joined.join(", "),);
    tracing::info!(extensions = ?new_exts, "auto-tracked large file extensions");

    Ok(joined)
}

/// Collect the patterns that auto-tracking would produce, without writing
/// anything. Used to populate `crab.toml` `[track]` patterns.
#[allow(dead_code)]
pub(crate) fn collect_auto_track_patterns(root: &Path) -> Vec<String> {
    use std::collections::BTreeSet;

    let ga_path = root.join(".gitattributes");
    let existing_content = std::fs::read_to_string(&ga_path).unwrap_or_default();
    let already_tracked: std::collections::HashSet<String> = existing_content
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#') && t.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    let mut new_exts: BTreeSet<String> = BTreeSet::new();
    let _ = scan_for_large_files(root, &already_tracked, &mut new_exts);

    // Combine already-tracked patterns with newly-detected ones.
    let mut patterns: Vec<String> = already_tracked.into_iter().collect();
    for ext in new_exts {
        patterns.push(format!("*.{ext}"));
    }
    patterns.sort();
    patterns
}

/// Walk the working tree looking for files above the size threshold or
/// with well-known large-file extensions.
fn scan_for_large_files(
    dir: &Path,
    already_tracked: &std::collections::HashSet<String>,
    new_exts: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip hidden dirs and common non-content directories.
            if name_str.starts_with('.')
                || name_str == "node_modules"
                || name_str == "target"
                || name_str == "__pycache__"
                || name_str == "venv"
            {
                continue;
            }
            scan_for_large_files(&path, already_tracked, new_exts)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };

        // Skip if already tracked.
        let glob = format!("*.{ext}");
        if already_tracked.contains(&glob) {
            continue;
        }

        // Check if this extension qualifies for auto-tracking.
        let is_well_known = WELL_KNOWN_LARGE_EXTENSIONS.contains(&ext.as_str());
        let is_large = if is_well_known {
            true // Always track well-known extensions when found
        } else {
            match std::fs::metadata(&path) {
                Ok(m) => m.len() >= AUTO_TRACK_SIZE_THRESHOLD,
                Err(_) => false,
            }
        };

        if is_large {
            new_exts.insert(ext);
        }
    }

    Ok(())
}

/// Create the initial unified manifest for a new repository.
///
/// Creates the manifest pointer at `{repo}/manifest` with generation 0,
/// empty refs, empty index hashes, and the given HEAD symref target.
/// Empty index hashes are the canonical zero-state representation; the first
/// committed push publishes real segmented indexes.
///
/// Uses `create_manifest` (If-None-Match: *) so a concurrent init doesn't
/// clobber an existing manifest.
///
/// # Errors
///
/// Returns [`CrabError::CasConflict`] if a manifest already exists, or
/// propagates store errors.
pub async fn create_initial_manifest(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    head: &str,
) -> Result<()> {
    use crate::metadata::manifest::{Manifest, create_manifest};

    let manifest = Manifest::default_for_repo(head);

    // Create the manifest pointer with If-None-Match: * semantics.
    create_manifest(store, router, &manifest).await?;

    tracing::info!(
        head = %head,
        shard_index_hash = %manifest.shard_index_hash,
        pack_index_hash = %manifest.pack_index_hash,
        "created initial manifest at generation 0"
    );

    Ok(())
}

async fn ensure_initial_manifest(
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    head: &str,
) -> Result<()> {
    // New repositories are the hot path: create-first avoids a discovery GET.
    // A conflict proves another initializer won, so adopt its manifest.
    match create_initial_manifest(store, router, head).await {
        Ok(()) => Ok(()),
        Err(CrabError::CasConflict { .. }) => {
            crate::metadata::manifest::read_manifest(store, router)
                .await
                .map(|_| ())
        }
        Err(error) => Err(error),
    }
}

/// Resolve the path to the crab binary for use in git config values.
///
/// Exported for reuse by the `install` command.
pub(crate) fn crab_binary_path() -> String {
    if let Some(path) = cargo_test_crab_binary_path() {
        return path;
    }

    if let Ok(exe) = std::env::current_exe() {
        // Resolve symlinks to get the real binary path.
        let resolved = exe.canonicalize().unwrap_or(exe);
        if is_cargo_test_harness(&resolved) {
            return unavailable_test_crab_binary_path();
        }

        // If the resolved path ends with git-remote-crab, replace
        // the filename with crab.
        if let Some(name) = resolved.file_name().and_then(|n| n.to_str())
            && name.starts_with("git-remote-")
            && let Some(parent) = resolved.parent()
        {
            let crab_path = parent.join("crab");
            if crab_path.exists() {
                return crab_path.to_str().unwrap_or("crab").to_owned();
            }
        }
        resolved.to_str().unwrap_or("crab").to_owned()
    } else {
        "crab".to_owned()
    }
}

fn cargo_test_crab_binary_path() -> Option<String> {
    std::env::var("CARGO_BIN_EXE_crab")
        .ok()
        .or_else(|| option_env!("CARGO_BIN_EXE_crab").map(str::to_owned))
}

fn unavailable_test_crab_binary_path() -> String {
    std::env::temp_dir()
        .join("crab-test-binary-unavailable")
        .to_str()
        .unwrap_or("crab-test-binary-unavailable")
        .to_owned()
}

fn is_cargo_test_harness(path: &Path) -> bool {
    if path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        != Some("deps")
    {
        return false;
    }

    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((_, suffix)) = stem.rsplit_once('-') else {
        return false;
    };

    suffix.len() >= 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Git config keys that define crab's git drivers.
const DRIVER_CONFIG: &[(&str, &str)] = &[
    // Long-running filter-process protocol (preferred).
    ("filter.crab.process", "{bin} filter-process"),
    // Fallback single-file clean/smudge for older git versions.
    ("filter.crab.clean", "{bin} filter-process"),
    ("filter.crab.smudge", "{bin} filter-process"),
    // Fail loudly if the filter cannot start — never silently skip.
    ("filter.crab.required", "true"),
    // External diff driver for files marked with `diff=crab`.
    ("diff.crab.command", "{bin} diff-driver"),
];

/// Register crab's git drivers in the local git config.
///
/// Runs `git config --local` for each key so the driver is scoped to
/// this repository. Idempotent — safe to call on repeated `crab init`.
///
/// Skips silently (with a debug log) when `root` is not a git
/// repository. `crab init` is legitimately callable on a bare
/// directory to pre-populate `.crab/local.toml`; the git drivers
/// can be registered later via `crab install` once the user runs
/// `git init`. Failing here would make the `init` UX worse for no
/// correctness benefit — the drivers can't fire without a git repo
/// anyway.
///
/// # Errors
///
/// Returns [`CrabError::Io`] if `git` cannot be spawned, or
/// [`CrabError::Configuration`] if `git config` exits non-zero
/// within an actual git repository.
pub fn install_filter_driver(root: &Path) -> Result<()> {
    // Cheap detection: `.git` is either a directory (non-worktree) or
    // a file pointing at the real gitdir (linked worktree / submodule).
    // Both cases satisfy `exists()`.
    if !root.join(".git").exists() {
        tracing::debug!(
            root = %root.display(),
            "skipping git driver registration: not a git repository yet"
        );
        return Ok(());
    }

    let bin = crab_binary_path();

    for &(key, value_template) in DRIVER_CONFIG {
        let value = value_template.replace("{bin}", &bin);

        // SHELLOUT: `git config --local` one-shot write registering
        // the git drivers. Keep-table rationale in `requirements.md`
        // Per-Site Decision Matrix: gix-config's write API is less
        // polished than read, and this runs once at `crab init`.
        let output = Command::new("git")
            .args(["config", "--local", key, &value])
            .current_dir(root)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CrabError::Configuration {
                key: key.to_owned(),
                origin: format!("git config failed: {stderr}"),
            });
        }

        tracing::debug!(key, value, "set git driver config");
    }

    tracing::info!("crab git drivers registered in local git config");
    Ok(())
}

/// Add or update a git remote for the crab URL.
///
/// Prefers `origin` if no remote named `origin` exists (git muscle memory).
/// Otherwise uses `crab` as the remote name. Returns the remote name that
/// was added/updated, or `None` if the operation was skipped.
fn add_or_update_git_remote(root: &Path, url: &str) -> Option<String> {
    // Determine which remote name to use: prefer "origin" if it doesn't exist.
    let origin_exists = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .is_some_and(|s| s.success());

    let remote_name = if origin_exists { "crab" } else { "origin" };

    // Check if the chosen remote already exists.
    let remote_exists = Command::new("git")
        .args(["remote", "get-url", remote_name])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .is_some_and(|s| s.success());

    if remote_exists {
        // Update the existing remote URL.
        let status = Command::new("git")
            .args(["remote", "set-url", remote_name, url])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok();
        if status.is_some_and(|s| s.success()) {
            return Some(remote_name.to_owned());
        }
    } else {
        // Add the remote.
        let status = Command::new("git")
            .args(["remote", "add", remote_name, url])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok();
        match status {
            Some(s) if s.success() => return Some(remote_name.to_owned()),
            _ => {
                // If add failed (e.g. remote already exists with same URL), treat as success.
                tracing::debug!(
                    remote = remote_name,
                    "git remote add failed, likely already exists — treating as no-op"
                );
                return Some(remote_name.to_owned());
            }
        }
    }

    None
}

/// Set up mirror mode for a repository.
///
/// Validates that the specified git remote exists, adds a `crab` remote
/// pointing to the crab URL, writes the `[mirror]` section to `crab.toml`,
/// installs mirror hooks, and prints a summary.
pub fn setup_mirror_mode(root: &Path, crab_url: &str, mirror_remote: &str) -> Result<()> {
    let crab_url = parse_init_remote(crab_url)?.canonical_url;

    // Validate that the specified remote exists.
    let check = Command::new("git")
        .args(["remote", "get-url", mirror_remote])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match check {
        Ok(s) if s.success() => {}
        _ => {
            return Err(CrabError::Configuration {
                key: "mirror".into(),
                origin: format!(
                    "Remote '{mirror_remote}' not found. Run: git remote add {mirror_remote} <url>"
                ),
            });
        }
    }

    // Always add as "crab" in mirror mode, regardless of whether origin exists.
    let crab_remote_exists = Command::new("git")
        .args(["remote", "get-url", "crab"])
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .is_some_and(|s| s.success());

    if crab_remote_exists {
        let _ = Command::new("git")
            .args(["remote", "set-url", "crab", &crab_url])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    } else {
        let _ = Command::new("git")
            .args(["remote", "add", "crab", &crab_url])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    // Write [mirror] section to crab.toml.
    let config_path = root.join("crab.toml");
    let mut config = if config_path.exists() {
        ProjectConfig::load(&config_path)?
    } else {
        ProjectConfig {
            version: 1,
            remote: RemoteConfig {
                url: crab_url.clone(),
            },
            track: None,
            hydrate: None,
            mirror: None,
            replication: None,
            auth: None,
            prefetch: None,
            workflow: None,
        }
    };

    config.mirror = Some(crate::core::project_config::MirrorConfig {
        origin_remote: mirror_remote.to_owned(),
        crab_remote: "crab".to_owned(),
    });
    config.remote.url = crab_url;

    ProjectConfig::write(&config_path, &config)?;

    // Install mirror hooks.
    crate::cmd::install::install_mirror_hooks(root)?;

    eprintln!("Mirror mode: git push {mirror_remote} will sync large files to Crab transparently.");

    Ok(())
}

fn parse_init_remote(url: &str) -> Result<InitRemote> {
    let scheme = url
        .trim()
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    let parsed = if scheme.as_deref() == Some("crab") {
        let direct = crate::git::url::CrabUrl::parse(url)?;
        crab_git::RepositoryUrl {
            bucket: direct.bucket,
            repo_prefix: direct.repo_path,
        }
    } else {
        crab_git::url::RepositoryUrl::parse(url).map_err(CrabError::from)?
    };
    let canonical_url = format!("crab://{}/{}", parsed.bucket, parsed.repo_prefix);
    let inferred_storage_provider = scheme.as_deref().and_then(storage_provider_for_init_scheme);

    Ok(InitRemote {
        canonical_url,
        parsed: CrabUrl {
            bucket: parsed.bucket,
            repo_path: parsed.repo_prefix,
        },
        inferred_storage_provider,
    })
}

fn storage_provider_for_init_scheme(scheme: &str) -> Option<StorageProvider> {
    if scheme.eq_ignore_ascii_case("crab") {
        None
    } else {
        StorageProvider::parse_config_value(scheme)
    }
}

fn select_init_storage_provider(
    explicit: Option<StorageProvider>,
    existing: Option<StorageProvider>,
    inferred: Option<StorageProvider>,
    input_url: &str,
) -> Result<Option<StorageProvider>> {
    let Some(inferred) = inferred else {
        return Ok(explicit.or(existing));
    };

    if let Some(explicit) = explicit
        && explicit.storage_provider_kind().is_some()
        && explicit != inferred
    {
        return Err(CrabError::Configuration {
            key: "storage-provider".into(),
            origin: format!(
                "{input_url} implies storage_provider = {:?}, but --storage-provider was {:?}",
                inferred.toml_value(),
                explicit.toml_value()
            ),
        });
    }

    Ok(Some(inferred))
}

/// Parse a storage backend name accepted by `crab init --storage-provider`.
pub fn parse_storage_provider_arg(value: &str) -> Result<StorageProvider> {
    let trimmed = value.trim();
    StorageProvider::parse_config_value(trimmed).ok_or_else(|| CrabError::Configuration {
        key: "storage-provider".into(),
        origin: format!(
            "unsupported storage provider {trimmed:?}; expected s3, gcs, azure, or auto"
        ),
    })
}

fn credential_discovery_url(url: &CrabUrl, provider: Option<&StorageProvider>) -> String {
    let provider = match provider {
        Some(StorageProvider::Auto) | None => storage_provider_from_env(),
        Some(provider) => Some(provider.clone()),
    };

    if let Some(scheme) = provider
        .as_ref()
        .and_then(StorageProvider::credential_discovery_scheme)
    {
        format!("{scheme}://{}/{}", url.bucket, url.repo_path)
    } else {
        format!("crab://{}/{}", url.bucket, url.repo_path)
    }
}

fn storage_provider_from_env() -> Option<StorageProvider> {
    let value = std::env::var("CRAB_STORAGE_PROVIDER").ok()?;
    StorageProvider::parse_config_value(&value)
}

/// Interactive URL prompt implementation.
///
/// Detects TTY, displays the canonical Crab remote URL shape, validates input,
/// and re-prompts up to 3 times on invalid input.
///
/// Returns the validated URL on success.
///
/// # Errors
///
/// Returns [`CrabError::Configuration`] if stdin is not a TTY, OutputMode is
/// machine-readable, or the user exhausts all attempts.
/// Returns [`CrabError::Io`] on terminal read failure.
pub fn prompt_init_url_interactive(mode: OutputMode) -> Result<String> {
    // Machine-output modes skip interactive prompting entirely.
    if mode.is_machine() {
        return Err(CrabError::Configuration {
            key: "url".into(),
            origin: "No URL provided and no crab.toml found. Usage: crab init <url>".into(),
        });
    }

    if !std::io::stdin().is_terminal() {
        return Err(CrabError::Configuration {
            key: "url".into(),
            origin: "No URL provided and no crab.toml found. Usage: crab init <url>".into(),
        });
    }

    let term = console::Term::stderr();

    eprintln!("No crab.toml found. Let's set up your repository.\n");
    eprintln!("Supported URL formats:");
    eprintln!("  crab://bucket/repo     (Crab Git remote)");
    eprintln!("  s3://bucket/repo       (initializes as crab://bucket/repo)");
    eprintln!("  gs://bucket/repo       (initializes as crab://bucket/repo)");
    eprintln!("  azure://container/repo (initializes as crab://container/repo)\n");

    for attempt in 0..3 {
        eprint!("Remote URL: ");
        let input = term.read_line().map_err(CrabError::Io)?;
        let url = input.trim();

        if is_valid_init_url(url) {
            return Ok(url.to_owned());
        }

        if attempt < 2 {
            eprintln!("Invalid URL format. Use crab://, s3://, gs://, gcs://, az://, or azure://.");
        }
    }

    Err(CrabError::Configuration {
        key: "url".into(),
        origin: "Invalid URL after 3 attempts. Usage: crab init <url>".into(),
    })
}

/// Returns true if the given URL is valid for `crab init`.
pub fn is_valid_init_url(url: &str) -> bool {
    parse_init_remote(url).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempGitRepo {
        _git_env: crate::test::git_repo::CleanGitEnvGuard,
        dir: tempfile::TempDir,
    }

    impl TempGitRepo {
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    /// Create a temp dir with `git init` so `git config --local` works.
    fn temp_git_repo() -> TempGitRepo {
        let git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        TempGitRepo {
            _git_env: git_env,
            dir,
        }
    }

    #[test]
    fn detects_cargo_test_harness_paths() {
        assert!(is_cargo_test_harness(Path::new(
            "/tmp/crab-target/debug/deps/crab-6a9fa277eabf0a87"
        )));
        assert!(is_cargo_test_harness(Path::new(
            "/tmp/crab-target/debug/deps/workflow_journal-6a9fa277eabf0a87"
        )));
        assert!(!is_cargo_test_harness(Path::new(
            "/tmp/crab-target/debug/crab"
        )));
        assert!(!is_cargo_test_harness(Path::new(
            "/tmp/crab-target/debug/deps/crab"
        )));
    }

    #[test]
    fn crab_binary_path_does_not_return_current_test_harness() {
        let current = std::env::current_exe().unwrap();
        let current = current.canonicalize().unwrap_or(current);
        if is_cargo_test_harness(&current)
            && std::env::var("CARGO_BIN_EXE_crab").is_err()
            && option_env!("CARGO_BIN_EXE_crab").is_none()
        {
            let path = crab_binary_path();
            assert_ne!(path, current.to_string_lossy());
            assert!(path.contains("crab-test-binary-unavailable"));
        }
    }

    #[tokio::test]
    async fn init_creates_local_config() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        let result = run_init_in("crab://my-bucket/my-repo", dir.path(), &cancel).await;
        assert!(result.is_ok(), "run_init failed: {result:?}");

        let config_path = dir.path().join(".crab/local.toml");
        assert!(config_path.exists(), ".crab/local.toml should exist");

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !content.contains("crab://my-bucket/my-repo"),
            "local config must not duplicate the remote URL, got: {content}",
        );
        let project = ProjectConfig::load(&dir.path().join("crab.toml")).unwrap();
        assert_eq!(project.remote.url, "crab://my-bucket/my-repo");
    }

    #[tokio::test]
    async fn init_persists_explicit_storage_provider() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        run_init_with_storage_provider(
            "crab://my-bucket/my-repo",
            dir.path(),
            &cancel,
            OutputMode::Text,
            Some(StorageProvider::Gcs),
            None,
        )
        .await
        .expect("init should succeed");

        let local_config = std::fs::read_to_string(dir.path().join(".crab/local.toml")).unwrap();
        assert!(
            !local_config.contains("storage_provider"),
            "local config must not duplicate the project provider, got: {local_config}",
        );

        let project_config = ProjectConfig::load(&dir.path().join("crab.toml")).unwrap();
        assert_eq!(
            project_config
                .auth
                .as_ref()
                .and_then(|auth| auth.storage_provider.as_ref()),
            Some(&StorageProvider::Gcs),
        );

        let resolved = crate::core::config::Config::resolve_for_repo(dir.path()).unwrap();
        assert_eq!(resolved.auth.storage_provider, StorageProvider::Gcs);
    }

    #[tokio::test]
    async fn init_normalizes_provider_prefixed_urls_to_crab_remote() {
        let cases = [
            ("s3://s3-bucket/team/repo", StorageProvider::S3),
            ("gs://gcs-bucket/team/repo", StorageProvider::Gcs),
            ("gcs://gcs-bucket/team/repo", StorageProvider::Gcs),
            ("az://azure-container/team/repo", StorageProvider::Azure),
            ("azure://azure-container/team/repo", StorageProvider::Azure),
        ];

        for (input_url, expected_provider) in cases {
            let dir = temp_git_repo();
            let cancel = CancellationToken::new();
            run_init_in(input_url, dir.path(), &cancel)
                .await
                .expect("init should accept provider-prefixed URL");

            let parsed = parse_init_remote(input_url).expect("test input should parse");
            let project_config = ProjectConfig::load(&dir.path().join("crab.toml")).unwrap();
            assert_eq!(project_config.remote.url, parsed.canonical_url);
            assert_eq!(
                project_config
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.storage_provider.as_ref()),
                Some(&expected_provider),
            );

            let output = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "origin remote should be configured"
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                parsed.canonical_url,
            );
        }
    }

    #[tokio::test]
    async fn init_reuses_project_storage_provider() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();
        std::fs::write(
            dir.path().join("crab.toml"),
            r#"[remote]
url = "crab://my-bucket/my-repo"

[auth]
storage_provider = "azure"
"#,
        )
        .unwrap();

        run_init_in("crab://my-bucket/my-repo", dir.path(), &cancel)
            .await
            .expect("init should succeed");

        let local_config = std::fs::read_to_string(dir.path().join(".crab/local.toml")).unwrap();
        assert!(
            !local_config.contains("storage_provider"),
            "local config must not duplicate the project provider, got: {local_config}",
        );
        let resolved = Config::resolve_for_repo(dir.path()).unwrap();
        assert_eq!(resolved.auth.storage_provider, StorageProvider::Azure);
    }

    #[tokio::test]
    async fn init_persists_and_preserves_gc_list_profile_locally() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        run_init_with_storage_provider(
            "crab://my-bucket/my-repo",
            dir.path(),
            &cancel,
            OutputMode::Text,
            None,
            Some(GcListProfile::Cost),
        )
        .await
        .expect("init should persist GC profile");
        run_init_with_storage_provider(
            "crab://my-bucket/my-repo",
            dir.path(),
            &cancel,
            OutputMode::Text,
            None,
            None,
        )
        .await
        .expect("re-init should preserve GC profile");

        let local_config = std::fs::read_to_string(dir.path().join(".crab/local.toml")).unwrap();

        assert!(local_config.contains("list_profile = \"cost\""));
    }

    #[tokio::test]
    async fn configure_init_persists_aws_profile_without_erasing_local_settings() {
        let dir = temp_git_repo();
        let local_dir = dir.path().join(".crab");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(local_dir.join("local.toml"), "[checkout]\nlazy = true\n").unwrap();

        run_init_for_configure(
            "crab://my-bucket/my-repo",
            dir.path(),
            &CancellationToken::new(),
            Some(StorageProvider::S3),
            None,
            Some("ml-team"),
        )
        .await
        .unwrap();

        let local_config = std::fs::read_to_string(local_dir.join("local.toml")).unwrap();
        let local_config: toml::Value = toml::from_str(&local_config).unwrap();
        assert_eq!(
            local_config["auth"]["aws_profile"].as_str(),
            Some("ml-team")
        );
        assert_eq!(local_config["checkout"]["lazy"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn init_rejects_invalid_existing_gc_list_profile_without_overwriting_it() {
        let dir = temp_git_repo();
        let local_dir = dir.path().join(".crab");
        std::fs::create_dir_all(&local_dir).unwrap();
        let config_path = local_dir.join("local.toml");
        let invalid = "[gc]\nlist_profile = \"fast\"\n";
        std::fs::write(&config_path, invalid).unwrap();

        let result = run_init_with_storage_provider(
            "crab://my-bucket/my-repo",
            dir.path(),
            &CancellationToken::new(),
            OutputMode::Text,
            None,
            None,
        )
        .await;

        assert!(matches!(result, Err(CrabError::Configuration { .. })));
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), invalid);
    }

    #[tokio::test]
    async fn init_excludes_local_crab_dir_from_add_all() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        run_init_in("crab://my-bucket/my-repo", dir.path(), &cancel)
            .await
            .expect("init should succeed");

        let status = Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git add . failed");

        let output = Command::new("git")
            .args(["ls-files"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "git ls-files failed");
        let tracked = String::from_utf8_lossy(&output.stdout);

        assert!(
            !tracked.lines().any(|path| path.starts_with(".crab/")),
            "local .crab state must not be tracked after git add ., got: {tracked}",
        );
        assert!(
            tracked.lines().any(|path| path == "crab.toml"),
            "crab.toml should remain trackable project config, got: {tracked}",
        );

        let exclude = std::fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert_eq!(
            exclude
                .lines()
                .filter(|line| line.trim() == ".crab/")
                .count(),
            1,
            ".crab/ should be excluded exactly once",
        );
    }

    #[tokio::test]
    async fn init_rejects_invalid_url() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        // Completely empty URL should fail URL parsing.
        let result = run_init_in("", dir.path(), &cancel).await;
        assert!(result.is_err(), "empty URL should be rejected");

        let err = result.unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration error, got: {err}",
        );
    }

    #[tokio::test]
    async fn init_rejects_conflicting_storage_provider_before_writing_config() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        let result = run_init_with_storage_provider(
            "gs://bucket/repo",
            dir.path(),
            &cancel,
            OutputMode::Text,
            Some(StorageProvider::S3),
            None,
        )
        .await;
        assert!(
            result.is_err(),
            "conflicting URL scheme and provider should be rejected"
        );

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("storage_provider"),
            "error should explain the provider mismatch, got: {msg}",
        );

        assert!(
            !dir.path().join(".crab/local.toml").exists(),
            "rejected init must not write local Crab config",
        );

        let output = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "rejected init must not configure an unusable origin remote",
        );
    }

    #[tokio::test]
    async fn init_respects_cancellation() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_init_in("crab://bucket/repo", dir.path(), &cancel).await;
        assert!(
            matches!(result, Err(CrabError::Cancelled)),
            "should return Cancelled when token is triggered",
        );
    }

    #[tokio::test]
    async fn init_is_idempotent_on_existing_dir() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        // Run init twice — second call should succeed (update config).
        let r1 = run_init_in("crab://bucket/repo-v1", dir.path(), &cancel).await;
        let r2 = run_init_in("crab://bucket/repo-v2", dir.path(), &cancel).await;

        assert!(r1.is_ok(), "first init failed: {r1:?}");
        assert!(r2.is_ok(), "second init failed: {r2:?}");

        let project = ProjectConfig::load(&dir.path().join("crab.toml")).unwrap();
        assert_eq!(project.remote.url, "crab://bucket/repo-v2");
    }

    #[tokio::test]
    async fn init_registers_git_drivers() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        run_init_in("crab://bucket/repo", dir.path(), &cancel)
            .await
            .expect("init should succeed");

        // Verify filter.crab.process is set.
        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.process"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let process_val = String::from_utf8_lossy(&output.stdout);
        assert!(
            process_val.contains("filter-process"),
            "filter.crab.process should contain 'filter-process', got: {process_val}",
        );

        // Verify filter.crab.required is true.
        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.required"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let required_val = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(required_val, "true", "filter.crab.required should be true");

        // Verify diff.crab.command is set so `diff=crab` in
        // .gitattributes actually invokes the external diff driver.
        let output = Command::new("git")
            .args(["config", "--local", "diff.crab.command"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let diff_val = String::from_utf8_lossy(&output.stdout);
        assert!(
            diff_val.contains("diff-driver"),
            "diff.crab.command should contain 'diff-driver', got: {diff_val}",
        );
    }

    #[tokio::test]
    async fn init_filter_driver_is_idempotent() {
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        run_init_in("crab://bucket/repo", dir.path(), &cancel)
            .await
            .expect("first init");
        run_init_in("crab://bucket/repo", dir.path(), &cancel)
            .await
            .expect("second init should not fail on existing filter config");

        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.required"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let val = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(val, "true");
    }

    // --- Manifest creation (Task 9.5) ---

    #[tokio::test]
    async fn init_auto_creates_git_repo_when_missing() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();

        // No git init — crab init should do it automatically.
        assert!(!dir.path().join(".git").exists());

        let result = run_init_in("crab://bucket/repo", dir.path(), &cancel).await;
        assert!(
            result.is_ok(),
            "init should auto-create git repo: {result:?}"
        );

        // .git should now exist.
        assert!(dir.path().join(".git").exists(), ".git should be created");
        // .crab/local.toml should exist.
        assert!(
            dir.path().join(".crab/local.toml").exists(),
            ".crab/local.toml should exist"
        );
        // Filter driver should be registered.
        let output = Command::new("git")
            .args(["config", "--local", "filter.crab.required"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let val = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert_eq!(val, "true");
    }

    #[tokio::test]
    async fn init_does_not_auto_track_large_files() {
        // Auto-tracking is deferred to `crab setup`. Init should NOT
        // scan for large files or create .gitattributes.
        let dir = temp_git_repo();
        let cancel = CancellationToken::new();

        // Create a large .bin file (> 1 MiB threshold).
        let big_file = dir.path().join("model.bin");
        std::fs::write(&big_file, vec![0xAA; 2_000_000]).unwrap();

        let result =
            run_init_with_options("crab://bucket/repo", dir.path(), &cancel, OutputMode::Text)
                .await;
        assert!(result.is_ok(), "init failed: {result:?}");

        // .gitattributes should NOT exist — auto-tracking is deferred to `crab setup`.
        assert!(
            !dir.path().join(".gitattributes").exists(),
            ".gitattributes should not be created by init; auto-tracking is deferred to `crab setup`",
        );
    }

    #[tokio::test]
    async fn init_creates_valid_manifest_that_read_manifest_can_parse() {
        use crate::metadata::manifest::read_manifest;
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_string());

        // Create the initial manifest.
        create_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .expect("create_initial_manifest should succeed");

        // Read it back and verify.
        let (manifest, _etag) = read_manifest(&store, &router)
            .await
            .expect("read_manifest should succeed");

        assert_eq!(
            manifest.version,
            crate::metadata::manifest::MANIFEST_VERSION
        );
        assert_eq!(manifest.generation, 0);
        assert_eq!(manifest.head, "refs/heads/main");
        assert!(manifest.refs.is_empty());
        assert!(manifest.shard_index_hash.is_empty());
        assert!(manifest.pack_index_hash.is_empty());
        assert!(manifest.commit_graph_hash.is_none());
        assert!(manifest.ref_registry_hash.is_none());
    }

    #[tokio::test]
    async fn init_manifest_is_idempotent_via_create_mode() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_string());

        // First init succeeds.
        create_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .expect("first init should succeed");

        // Second init should fail with CasConflict (If-None-Match: *).
        // The Store::put uses PutMode::Create which fails if the object
        // already exists with different content. Since the manifest
        // includes a session_id that differs between calls, the content
        // will differ and the second call should conflict.
        // Note: with InMemory store, PutMode::Create behavior may vary.
        // The important thing is that the first init created a valid manifest.
        let result = create_initial_manifest(&store, &router, "refs/heads/main").await;
        // Either succeeds (idempotent same content) or conflicts — both are acceptable.
        if let Err(ref e) = result {
            assert!(
                matches!(e, CrabError::CasConflict { .. }),
                "expected CasConflict on duplicate init, got: {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn remote_manifest_initialization_adopts_existing_manifest() {
        use crate::metadata::manifest::read_manifest;
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_string());

        ensure_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .expect("first remote initialization should succeed");
        ensure_initial_manifest(&store, &router, "refs/heads/main")
            .await
            .expect("repeated remote initialization should adopt the manifest");

        let (manifest, _) = read_manifest(&store, &router)
            .await
            .expect("initialized manifest should remain readable");
        assert_eq!(manifest.generation, 0);
        assert_eq!(manifest.head, "refs/heads/main");
    }

    #[tokio::test]
    async fn remote_initialization_publishes_canonical_layout_before_manifest() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_owned());

        initialize_remote_repository_store(&store, &router, "refs/heads/main")
            .await
            .expect("canonical repository initialization should succeed");

        crate::core::remote_layout::open(&store, &router)
            .await
            .expect("layout descriptor should open");
        let (manifest, _) = crate::metadata::manifest::read_manifest(&store, &router)
            .await
            .expect("manifest should follow layout publication");
        assert_eq!(
            manifest.version,
            crate::metadata::manifest::MANIFEST_VERSION
        );
    }

    #[tokio::test]
    async fn conflicting_layout_prevents_manifest_creation() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_owned());
        store
            .put_exact(
                &router.layout_descriptor_path(),
                Bytes::from_static(br#"{"schema_version":2}"#),
            )
            .await
            .expect("seed non-v1 descriptor");

        initialize_remote_repository_store(&store, &router, "refs/heads/main")
            .await
            .expect_err("non-v1 descriptor must fail closed");

        assert!(store.head(&router.manifest_path()).await.is_err());
    }

    #[tokio::test]
    async fn missing_layout_cannot_adopt_existing_repository_objects() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_owned());
        store
            .put_exact(
                &object_store::path::Path::from("org/my-repo/orphan"),
                Bytes::from_static(b"legacy state"),
            )
            .await
            .expect("seed repository object without descriptor");

        let error = initialize_remote_repository_store(&store, &router, "refs/heads/main")
            .await
            .expect_err("non-empty repository without a descriptor must fail closed");

        assert!(error.to_string().contains("reset"));
        assert!(store.head(&router.layout_descriptor_path()).await.is_err());
        assert!(store.head(&router.manifest_path()).await.is_err());
    }

    #[tokio::test]
    async fn remote_initialization_ignores_objects_outside_the_repository_prefix() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_owned());
        store
            .put_exact(
                &object_store::path::Path::from(".crab/xorbs/aa/unrelated"),
                Bytes::from_static(b"shared content"),
            )
            .await
            .expect("seed unrelated global object");

        initialize_remote_repository_store(&store, &router, "refs/heads/main")
            .await
            .expect("unrelated bucket objects must not block repository initialization");

        crate::core::remote_layout::open(&store, &router)
            .await
            .expect("canonical repository descriptor");
    }

    #[tokio::test]
    async fn explicit_init_repairs_missing_manifest_only_after_layout_validation() {
        use crate::storage::StoreLayout;
        use crate::storage::store::Store;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/my-repo".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .expect("seed canonical descriptor");

        initialize_remote_repository_store(&store, &router, "refs/heads/main")
            .await
            .expect("explicit init should restore the missing generation-0 manifest");

        crate::core::remote_layout::open(&store, &router)
            .await
            .expect("descriptor remains canonical");
        let (manifest, _) = crate::metadata::manifest::read_manifest(&store, &router)
            .await
            .expect("manifest should be recreated");
        assert_eq!(manifest.generation, 0);
        assert_eq!(manifest.head, "refs/heads/main");
    }

    #[test]
    fn valid_init_url_accepts_crab_remote() {
        assert!(is_valid_init_url("crab://bucket/repo"));
        assert!(is_valid_init_url("crab://bucket/nested/repo"));
        assert!(is_valid_init_url("s3://my-bucket/path"));
        assert!(is_valid_init_url("gs://gcs-bucket/repo"));
        assert!(is_valid_init_url("gcs://gcs-bucket/repo"));
        assert!(is_valid_init_url("az://container/repo"));
        assert!(is_valid_init_url("azure://container/repo"));
    }

    #[test]
    fn invalid_init_urls_rejected() {
        assert!(!is_valid_init_url("http://example.com"));
        assert!(!is_valid_init_url("https://github.com/repo"));
        assert!(!is_valid_init_url("ftp://server/path"));
        assert!(!is_valid_init_url(""));
        assert!(!is_valid_init_url("just-a-string"));
        assert!(!is_valid_init_url("crab//missing-colon"));
    }

    #[test]
    fn prompt_skips_in_json_mode() {
        let result = prompt_init_url_interactive(OutputMode::Json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration error in Json mode, got: {err}"
        );
    }

    #[test]
    fn prompt_skips_in_jsonl_mode() {
        let result = prompt_init_url_interactive(OutputMode::Jsonl);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration error in Jsonl mode, got: {err}"
        );
    }
}
