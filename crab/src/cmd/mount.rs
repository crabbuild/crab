//! `crab mount`, `crab unmount`, and `crab mount status` subcommands.
//!
//! Provides the CLI surface for Crab virtual filesystem mounts:
//! - `crab mount <path>` — mount in foreground (blocks until SIGINT)
//! - `crab mount <path> --daemon` — fork to background, write PID file
//! - `crab mount <path> --ref=<branch>` — mount a specific branch
//! - `crab unmount <path>` — read PID file, send SIGTERM, clean up
//! - `crab mount status` — report mount state and hydration progress
//!
//! Mount backends are compiled independently behind `fuse` and `nfs` features.

use std::path::{Path, PathBuf};

#[cfg(any(feature = "fuse", feature = "nfs"))]
use serde::Serialize;
#[cfg(all(any(feature = "fuse", feature = "nfs"), target_os = "linux"))]
use std::os::fd::AsRawFd;
#[cfg(any(feature = "fuse", feature = "nfs"))]
use std::time::Duration;
#[cfg(any(feature = "fuse", feature = "nfs"))]
use tracing::debug;
use tracing::{info, warn};

use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;
#[cfg(any(feature = "fuse", feature = "nfs"))]
use crate::core::output::emit_json;

#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Mount backend selection for `crab mount`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum MountBackend {
    /// Use NFS when preflight passes, otherwise use FUSE when available.
    Auto,
    /// Native NFSv3 client mount backed by Crab's local NFS server.
    Nfs,
    /// Kernel FUSE mount backed by Crab's FUSE adapter.
    Fuse,
}

impl MountBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Nfs => "nfs",
            Self::Fuse => "fuse",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedMountBackend {
    Nfs,
    #[cfg_attr(
        all(feature = "nfs", not(feature = "fuse")),
        allow(
            dead_code,
            reason = "NFS-only builds keep the shared backend enum shape without constructing FUSE"
        )
    )]
    Fuse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum MountDoctorStatus {
    #[cfg_attr(
        not(any(feature = "fuse", feature = "nfs")),
        allow(
            dead_code,
            reason = "no-backend mount doctor can only emit failure checks"
        )
    )]
    Ok,
    #[cfg_attr(
        not(feature = "nfs"),
        allow(
            dead_code,
            reason = "NFS preflight warnings are the first mount doctor warning source"
        )
    )]
    Warn,
    Fail,
}

#[derive(Debug, serde::Serialize)]
struct MountDoctorCheck {
    name: &'static str,
    status: MountDoctorStatus,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
}

impl MountDoctorCheck {
    #[cfg_attr(
        not(any(feature = "fuse", feature = "nfs")),
        allow(
            dead_code,
            reason = "no-backend mount doctor can only emit failure checks"
        )
    )]
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: MountDoctorStatus::Ok,
            detail: detail.into(),
            action: None,
        }
    }

    #[cfg_attr(
        not(feature = "nfs"),
        allow(
            dead_code,
            reason = "NFS preflight warnings are the first mount doctor warning source"
        )
    )]
    fn warn(name: &'static str, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name,
            status: MountDoctorStatus::Warn,
            detail: detail.into(),
            action: Some(action.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            name,
            status: MountDoctorStatus::Fail,
            detail: detail.into(),
            action: Some(action.into()),
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct MountDoctorSummary {
    ok: u32,
    warn: u32,
    fail: u32,
    ready: bool,
}

#[cfg(feature = "nfs")]
#[derive(Debug, serde::Serialize)]
struct MountDoctorNfsPreflight {
    ready: bool,
    backend_available: bool,
    native_client_available: bool,
    mountpoint_ready: bool,
    loopback_bind_ready: bool,
    control_endpoint_ready: bool,
    privilege_ready: bool,
    blocker_count: usize,
    warning_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<String>,
    blockers: Vec<crate::vfs::nfs_mount::NfsPreflightMessage>,
    warnings: Vec<crate::vfs::nfs_mount::NfsPreflightMessage>,
}

#[derive(Debug, serde::Serialize)]
struct MountDoctorAutoDecision {
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_backend: Option<String>,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nfs_ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nfs_blocker_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_error: Option<String>,
}

#[cfg(feature = "nfs")]
impl MountDoctorNfsPreflight {
    fn from_report(report: &crate::vfs::nfs_mount::NfsPreflightReport) -> Self {
        Self {
            ready: report.is_ready(),
            backend_available: report.backend_available,
            native_client_available: report.native_client_available,
            mountpoint_ready: report.mountpoint_ready,
            loopback_bind_ready: report.loopback_bind_ready,
            control_endpoint_ready: report.control_endpoint_ready,
            privilege_ready: report.privilege_ready,
            blocker_count: report.blockers.len(),
            warning_count: report.warnings.len(),
            next_action: report
                .blockers
                .iter()
                .filter_map(|blocker| blocker.action.clone())
                .next(),
            blockers: report.blockers.clone(),
            warnings: report.warnings.clone(),
        }
    }
}

#[derive(Debug)]
struct MountDoctorCollectedChecks {
    checks: Vec<MountDoctorCheck>,
    #[cfg(feature = "nfs")]
    nfs_preflight: Option<MountDoctorNfsPreflight>,
}

impl MountDoctorCollectedChecks {
    fn new(checks: Vec<MountDoctorCheck>) -> Self {
        Self {
            checks,
            #[cfg(feature = "nfs")]
            nfs_preflight: None,
        }
    }

    #[cfg(feature = "nfs")]
    fn with_nfs_preflight(
        checks: Vec<MountDoctorCheck>,
        report: &crate::vfs::nfs_mount::NfsPreflightReport,
    ) -> Self {
        Self {
            checks,
            nfs_preflight: Some(MountDoctorNfsPreflight::from_report(report)),
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct MountDoctorPayload {
    requested_backend: String,
    checked_backend: String,
    mountpoint: String,
    checks: Vec<MountDoctorCheck>,
    summary: MountDoctorSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_decision: Option<MountDoctorAutoDecision>,
    #[cfg(feature = "nfs")]
    #[serde(skip_serializing_if = "Option::is_none")]
    nfs_preflight: Option<MountDoctorNfsPreflight>,
}

#[cfg(feature = "nfs")]
const NFS_BACKGROUND_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(test)]
struct TestHomeGuard {
    original_home: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        match &self.original_home {
            Some(home) => {
                // SAFETY: tests using this helper hold TEST_ENV_LOCK.
                unsafe { std::env::set_var("HOME", home) };
            }
            None => {
                // SAFETY: tests using this helper hold TEST_ENV_LOCK.
                unsafe { std::env::remove_var("HOME") };
            }
        }
    }
}

#[cfg(test)]
fn set_test_home(home: &Path) -> TestHomeGuard {
    let guard = TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let original_home = std::env::var_os("HOME");
    // SAFETY: tests using this helper hold TEST_ENV_LOCK.
    unsafe { std::env::set_var("HOME", home) };
    TestHomeGuard {
        original_home,
        _guard: guard,
    }
}

#[cfg(test)]
fn tempdir_outside_repo_ancestors() -> tempfile::TempDir {
    let temp_root = std::env::temp_dir();
    let mut candidates = Vec::new();
    if let Some(parent) = temp_root.parent() {
        candidates.push(parent.to_path_buf());
    }
    candidates.push(temp_root);
    candidates.push(PathBuf::from("/tmp"));

    for candidate in candidates {
        if candidate.exists() && !is_inside_git_repo(&candidate) {
            if let Ok(dir) = tempfile::Builder::new()
                .prefix("crab-plain-")
                .tempdir_in(candidate)
            {
                return dir;
            }
        }
    }

    tempfile::Builder::new()
        .prefix("crab-plain-")
        .tempdir()
        .expect("temporary directory should be creatable")
}

// ---------------------------------------------------------------------------
// Mountpoint safety check
// ---------------------------------------------------------------------------

/// Check whether `path` is inside a git or crab working tree.
///
/// Walks up parent directories from `path` looking for `.git/` or `.crab/`.
/// Returns `true` if either marker directory is found, indicating the
/// mountpoint would be nested inside an existing repository.
pub fn is_inside_git_repo(path: &Path) -> bool {
    let mut current = path.to_path_buf();
    loop {
        if current.join(".git").exists() || current.join(".crab").exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

// ---------------------------------------------------------------------------
// FUSE prerequisite check
// ---------------------------------------------------------------------------

/// Check that the OS-level FUSE prerequisites are installed.
///
/// On macOS, checks for macFUSE at `/Library/Filesystems/macfuse.fs`.
/// On Linux, checks for `/dev/fuse`.
///
/// Returns `Ok(())` if prerequisites are met, or an error with
/// installation instructions if not.
pub fn check_fuse_prerequisites() -> Result<()> {
    Ok(crate::core::fuse_prereq::check_fuse_prerequisites()?)
}

/// Print a user-friendly error message when FUSE prerequisites are missing.
fn print_prerequisite_error(error: &CrabError) {
    #[cfg(target_os = "macos")]
    {
        eprint!("{}", macos_macfuse_unavailable_message(error));
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("error: FUSE kernel module is not available (/dev/fuse not found).");
        eprintln!("details: {error}");
        eprintln!();
        eprintln!("Install FUSE to use `crab mount --backend=fuse`:");
        eprintln!("  Ubuntu/Debian: sudo apt install fuse3 libfuse3-dev");
        eprintln!("  Fedora/RHEL:   sudo dnf install fuse3 fuse3-devel");
        eprintln!("  Arch:          sudo pacman -S fuse3");
        eprintln!();
        eprintln!("Then ensure the fuse module is loaded: sudo modprobe fuse");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        eprintln!("error: FUSE is not supported on this platform.");
        eprintln!("details: {error}");
    }
}

// ---------------------------------------------------------------------------
// Feature-gate error message
// ---------------------------------------------------------------------------

/// Print an error message when the `fuse` feature is not compiled in.
pub fn print_fuse_not_compiled() {
    #[cfg(target_os = "macos")]
    {
        print_fuse_mount_missing();
    }

    #[cfg(target_os = "linux")]
    {
        eprintln!("error: FUSE support was not compiled into this Crab build.");
        eprintln!();
        eprintln!("Install FUSE development headers, then rebuild with `--features fuse`:");
        eprintln!("  Ubuntu/Debian: sudo apt install fuse3 libfuse3-dev");
        eprintln!("  Fedora/RHEL:   sudo dnf install fuse3 fuse3-devel");
        eprintln!("  Arch:          sudo pacman -S fuse3");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        eprintln!("error: FUSE support is not available in this Crab build.");
    }
}

/// Delegate `crab mount` to the FUSE-enabled mount binary when the core CLI was
/// built without linking macFUSE. This keeps non-mount commands runnable even
/// when macFUSE is not installed on the user's machine.
pub fn run_fuse_mount_or_print() -> std::process::ExitCode {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if let Err(error) = check_fuse_prerequisites() {
            print_prerequisite_error(&error);
            return std::process::ExitCode::from(1);
        }

        let Some(fuse_mount) = find_fuse_mount() else {
            print_fuse_mount_missing();
            return std::process::ExitCode::from(1);
        };

        let status = std::process::Command::new(fuse_mount)
            .args(std::env::args_os().skip(1))
            .status();
        match status {
            Ok(status) if status.success() => std::process::ExitCode::SUCCESS,
            Ok(status) => std::process::ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(error) => {
                eprintln!("error: failed to start Crab FUSE mount binary: {error}");
                std::process::ExitCode::from(1)
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        print_fuse_not_compiled();
        std::process::ExitCode::from(1)
    }
}

#[cfg(target_os = "macos")]
fn macos_macfuse_unavailable_message(error: &CrabError) -> String {
    format!(
        "error: macFUSE is not ready.\n\n\
         Only `crab mount --backend=fuse` requires macFUSE; other Crab commands can keep running.\n\n\
         Details: {error}\n\n\
         Install macFUSE if needed, then approve it in System Settings and reboot if prompted:\n\
           brew install --cask macfuse\n\n\
         Or download from: https://macfuse.github.io/\n"
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn print_fuse_mount_missing() {
    eprintln!("error: `crab mount` requires the Crab FUSE mount binary.");
    eprintln!();
    eprintln!("Reinstall Crab to install `crab-fuse-mount` next to `crab`.");
    eprintln!(
        "Only `crab mount --backend=fuse` uses this binary; other Crab commands can keep running."
    );
    #[cfg(target_os = "macos")]
    {
        eprintln!("macFUSE must also be installed and approved before FUSE mounts can start:");
        eprintln!("  brew install --cask macfuse");
    }
    #[cfg(target_os = "linux")]
    {
        eprintln!("FUSE must also be installed before FUSE mounts can start:");
        eprintln!("  Ubuntu/Debian: sudo apt install fuse3 libfuse3-dev");
        eprintln!("  Fedora/RHEL:   sudo dnf install fuse3 fuse3-devel");
        eprintln!("  Arch:          sudo pacman -S fuse3");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn find_fuse_mount() -> Option<PathBuf> {
    const FUSE_MOUNT_NAME: &str = "crab-fuse-mount";

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        let fuse_mount = dir.join(FUSE_MOUNT_NAME);
        if fuse_mount.is_file() {
            return Some(fuse_mount);
        }
    }

    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let fuse_mount = dir.join(FUSE_MOUNT_NAME);
        if fuse_mount.is_file() {
            return Some(fuse_mount);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Mount command (fuse feature enabled)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// New CLI mount entry point (--repo / --mountpoint)
// ---------------------------------------------------------------------------

/// Mount options for the new CLI path.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub struct NewMountOpts {
    /// Source repository (remote URL or local path).
    pub repo: String,
    /// Local mount path, or Windows NFS drive target such as `Z:`.
    pub mountpoint: PathBuf,
    /// Mount backend to use.
    pub backend: MountBackend,
    /// Branch or ref to mount (default: HEAD).
    pub git_ref: Option<String>,
    /// Run in foreground (block until SIGINT).
    pub foreground: bool,
    /// Disable writes (no overlay).
    pub read_only: bool,
    /// Disable automatic remote polling.
    pub no_refresh: bool,
    /// Allow mounting inside a git/crab working tree.
    pub allow_nested: bool,
    /// Human-friendly name for this mount (default: derived from source).
    pub name: Option<String>,
    /// Cancellation token for graceful shutdown.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Run the mount command using the new CLI path (`--repo` / `--mountpoint`).
///
/// Parses the source, validates it, computes the cache directory,
/// builds the pipeline config, and executes the mount. Handles both
/// local and remote sources.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_with_new_cli(opts: NewMountOpts) -> Result<()> {
    use crate::vfs::pipeline::MountPipelineBuilder;
    use crate::vfs::source::MountSource;

    let initial_backend = resolve_mount_backend(opts.backend)?;

    // Resolve and validate the mountpoint.
    let mountpoint = prepare_mountpoint(&opts.mountpoint, initial_backend)?;
    let backend = resolve_mount_backend_after_prepare(opts.backend, initial_backend, &mountpoint)?;
    ensure_mount_backend_prerequisites(backend, &mountpoint)?;

    // Safety check: refuse to mount inside a git/crab working tree.
    if !opts.allow_nested
        && should_check_nested_mountpoint(backend, &mountpoint)
        && is_inside_git_repo(&mountpoint)
    {
        return Err(CrabError::Configuration {
            key: "mountpoint is inside a git repository. Mount outside the repo to avoid git seeing virtual files as untracked.".into(),
            origin: "crab mount".into(),
        });
    }

    // Background mode on Unix: delegate to the coordinator via IPC.
    // The coordinator handles pipeline execution, so we skip it here.
    #[cfg(all(unix, feature = "fuse"))]
    if !opts.foreground && backend == ResolvedMountBackend::Fuse {
        use crate::vfs::ipc_client::try_ipc_mount;

        let mountpoint_str = mountpoint.display().to_string();
        let ref_str = opts.git_ref.as_deref().unwrap_or("HEAD");

        return Ok(try_ipc_mount(
            &opts.repo,
            &mountpoint_str,
            ref_str,
            opts.read_only,
            opts.no_refresh,
        )
        .await?);
    }

    #[cfg(feature = "nfs")]
    if !opts.foreground && backend == ResolvedMountBackend::Nfs {
        return spawn_nfs_background(&opts, &mountpoint).await;
    }

    // Parse the source.
    let source = MountSource::parse(&opts.repo)?;

    // Resolve ref name to full form.
    let ref_name = opts.git_ref.as_deref().map(|r| {
        if r.starts_with("refs/") {
            r.to_owned()
        } else {
            format!("refs/heads/{r}")
        }
    });

    // Build pipeline config based on source type.
    let (pipeline_config, read_context, _cache_lock) = match source {
        MountSource::Local { path } => {
            build_local_pipeline_config(
                &path,
                &mountpoint,
                ref_name,
                opts.read_only,
                opts.cancel.clone(),
            )
            .await?
        }
        MountSource::Remote { url } => {
            build_remote_pipeline_config(
                &url,
                ref_name.as_deref(),
                opts.read_only,
                opts.cancel.clone(),
            )
            .await?
        }
    };

    info!(
        source = %pipeline_config.source,
        git_dir = %pipeline_config.git_dir.display(),
        cache_dir = %pipeline_config.cache_dir.display(),
        "executing mount pipeline"
    );

    // Build and execute the pipeline.
    let mut builder = MountPipelineBuilder::new(pipeline_config.clone());
    if let Some(context) = read_context {
        builder = builder.with_read_context(context);
    }
    if opts.no_refresh {
        builder = builder.with_no_refresh(true);
    }

    #[cfg(feature = "fuse")]
    let rt = tokio::runtime::Handle::current();
    let output = builder.execute()?;

    info!(
        generation = output.generation,
        head_oid = %output.head_oid,
        "mount pipeline ready"
    );

    match backend {
        ResolvedMountBackend::Nfs => {
            #[cfg(feature = "nfs")]
            {
                use crate::vfs::nfs_mount;
                use std::sync::Arc;

                if !opts.foreground {
                    warn!("NFS daemon mode helper unavailable, running in foreground");
                }
                let config = nfs_mount::NfsMountConfig {
                    mountpoint: mountpoint.clone(),
                    git_dir: pipeline_config.git_dir.display().to_string(),
                    exclusive_verifiers_path: pipeline_config
                        .cache_dir
                        .join("nfs-exclusive-verifiers.json"),
                    read_only: opts.read_only,
                    auto_refresh_interval: (!opts.read_only && !opts.no_refresh)
                        .then_some(Duration::from_secs(30)),
                    control_endpoint_override: None,
                };
                nfs_mount::install_signal_handler(opts.cancel.clone());
                let resolver = Arc::clone(&output.resolver);
                let engine = Arc::clone(&output.engine);
                let runtime = crate::vfs::nfs_control::NfsMountRuntime {
                    output,
                    config: pipeline_config.clone(),
                };
                let mounted = nfs_mount::mount(&config, resolver, engine, Some(runtime)).await?;

                println!("Mounted via NFS at {}", mountpoint.display());
                println!("Press Ctrl+C to unmount.");

                nfs_mount::run_until_cancelled(mounted, opts.cancel).await?;
                println!("Unmounted.");
            }
            #[cfg(not(feature = "nfs"))]
            {
                return Err(CrabError::Configuration {
                    key: "NFS support was not compiled into this Crab build".into(),
                    origin: "crab mount --backend=nfs".into(),
                });
            }
        }
        ResolvedMountBackend::Fuse => {
            #[cfg(feature = "fuse")]
            {
                use crate::vfs::mount;

                let config = mount::MountConfig {
                    mountpoint: mountpoint.clone(),
                    git_dir: pipeline_config.git_dir.display().to_string(),
                    write_pid: !opts.foreground,
                    crab_dir: pipeline_config.cache_dir.clone(),
                    read_only: opts.read_only,
                };
                if !opts.foreground {
                    warn!("daemon mode not supported on this platform, running in foreground");
                }
                mount::install_signal_handler(opts.cancel.clone(), &rt);
                let mounted = mount::mount(&config, output.resolver, output.engine, rt.clone())?;

                println!("Mounted via FUSE at {}", mountpoint.display());
                println!("Press Ctrl+C to unmount.");

                mount::run_until_cancelled(
                    mounted.session,
                    &mountpoint,
                    opts.cancel,
                    &pipeline_config.cache_dir,
                    rt,
                )?;
                println!("Unmounted.");
            }
            #[cfg(not(feature = "fuse"))]
            {
                return Err(CrabError::Configuration {
                    key: "FUSE support was not compiled into this Crab build".into(),
                    origin: "crab mount --backend=fuse".into(),
                });
            }
        }
    }

    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn prepare_mountpoint(path: &Path, _backend: ResolvedMountBackend) -> Result<PathBuf> {
    #[cfg(all(any(windows, test), feature = "nfs"))]
    if _backend == ResolvedMountBackend::Nfs
        && let Ok(target) = crate::vfs::nfs_mount::windows_mount_target(path)
    {
        return Ok(PathBuf::from(target));
    }

    if path.exists() {
        return Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    }

    std::fs::create_dir_all(path)?;
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn should_check_nested_mountpoint(_backend: ResolvedMountBackend, _mountpoint: &Path) -> bool {
    #[cfg(all(windows, feature = "nfs"))]
    if _backend == ResolvedMountBackend::Nfs
        && crate::vfs::nfs_mount::is_windows_mount_target(_mountpoint)
    {
        return false;
    }

    true
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn resolve_mount_backend(backend: MountBackend) -> Result<ResolvedMountBackend> {
    match backend {
        MountBackend::Auto => {
            #[cfg(feature = "nfs")]
            {
                Ok(ResolvedMountBackend::Nfs)
            }
            #[cfg(all(not(feature = "nfs"), feature = "fuse"))]
            {
                Ok(ResolvedMountBackend::Fuse)
            }
            #[cfg(not(any(feature = "nfs", feature = "fuse")))]
            {
                Err(CrabError::Configuration {
                    key: "neither NFS nor FUSE mount support was compiled into this Crab build"
                        .into(),
                    origin: "crab mount".into(),
                })
            }
        }
        MountBackend::Nfs => {
            #[cfg(feature = "nfs")]
            {
                Ok(ResolvedMountBackend::Nfs)
            }
            #[cfg(not(feature = "nfs"))]
            {
                Err(CrabError::Configuration {
                    key: "NFS support was not compiled into this Crab build".into(),
                    origin: "crab mount --backend=nfs".into(),
                })
            }
        }
        MountBackend::Fuse => {
            #[cfg(feature = "fuse")]
            {
                Ok(ResolvedMountBackend::Fuse)
            }
            #[cfg(not(feature = "fuse"))]
            {
                Err(CrabError::Configuration {
                    key: "FUSE support was not compiled into this Crab build".into(),
                    origin: "crab mount --backend=fuse".into(),
                })
            }
        }
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn resolve_mount_backend_after_prepare(
    requested: MountBackend,
    compiled_backend: ResolvedMountBackend,
    mountpoint: &Path,
) -> Result<ResolvedMountBackend> {
    match requested {
        MountBackend::Auto => resolve_auto_mount_backend(mountpoint),
        MountBackend::Nfs | MountBackend::Fuse => Ok(compiled_backend),
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn ensure_mount_backend_prerequisites(
    backend: ResolvedMountBackend,
    mountpoint: &Path,
) -> Result<()> {
    match backend {
        ResolvedMountBackend::Nfs => {
            #[cfg(feature = "nfs")]
            {
                ensure_nfs_preflight_ready(&crate::vfs::nfs_mount::preflight_for_mountpoint(
                    mountpoint,
                ))
            }
            #[cfg(not(feature = "nfs"))]
            {
                let _ = mountpoint;
                Err(CrabError::Configuration {
                    key: "NFS support was not compiled into this Crab build".into(),
                    origin: "crab mount --backend=nfs".into(),
                })
            }
        }
        ResolvedMountBackend::Fuse => {
            #[cfg(feature = "fuse")]
            {
                if let Err(error) = check_fuse_prerequisites() {
                    print_prerequisite_error(&error);
                    return Err(CrabError::Configuration {
                        key: "FUSE prerequisites not met".into(),
                        origin: "crab mount".into(),
                    });
                }
                Ok(())
            }
            #[cfg(not(feature = "fuse"))]
            {
                Err(CrabError::Configuration {
                    key: "FUSE support was not compiled into this Crab build".into(),
                    origin: "crab mount --backend=fuse".into(),
                })
            }
        }
    }
}

#[cfg(feature = "nfs")]
fn ensure_nfs_preflight_ready(report: &crate::vfs::nfs_mount::NfsPreflightReport) -> Result<()> {
    ensure_nfs_preflight_ready_for_backend(report, MountBackend::Nfs)
}

#[cfg(feature = "nfs")]
fn ensure_nfs_preflight_ready_for_backend(
    report: &crate::vfs::nfs_mount::NfsPreflightReport,
    backend: MountBackend,
) -> Result<()> {
    if report.is_ready() {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: report.blocker_summary(),
        origin: format!("crab mount --backend={}", backend.as_str()),
    })
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn resolve_auto_mount_backend(mountpoint: &Path) -> Result<ResolvedMountBackend> {
    #[cfg(feature = "nfs")]
    {
        let nfs_report = crate::vfs::nfs_mount::preflight_for_mountpoint(mountpoint);
        return resolve_auto_mount_backend_from_nfs_report(&nfs_report);
    }

    #[cfg(all(not(feature = "nfs"), feature = "fuse"))]
    {
        let _ = mountpoint;
        Ok(ResolvedMountBackend::Fuse)
    }

    #[cfg(not(any(feature = "nfs", feature = "fuse")))]
    {
        let _ = mountpoint;
        Err(CrabError::Configuration {
            key: "neither NFS nor FUSE mount support was compiled into this Crab build".into(),
            origin: "crab mount --backend=auto".into(),
        })
    }
}

#[cfg(feature = "nfs")]
fn resolve_auto_mount_backend_from_nfs_report(
    nfs_report: &crate::vfs::nfs_mount::NfsPreflightReport,
) -> Result<ResolvedMountBackend> {
    if nfs_report.is_ready() {
        return Ok(ResolvedMountBackend::Nfs);
    }
    if !nfs_blockers_allow_auto_fuse_fallback(nfs_report) {
        return ensure_nfs_preflight_ready_for_backend(nfs_report, MountBackend::Auto)
            .map(|()| ResolvedMountBackend::Nfs);
    }

    #[cfg(feature = "fuse")]
    {
        resolve_auto_mount_backend_from_probe(nfs_report, check_fuse_prerequisites())
    }

    #[cfg(not(feature = "fuse"))]
    {
        ensure_nfs_preflight_ready_for_backend(nfs_report, MountBackend::Auto)
            .map(|()| ResolvedMountBackend::Nfs)
    }
}

#[cfg(all(feature = "nfs", feature = "fuse"))]
fn resolve_auto_mount_backend_from_probe(
    nfs_report: &crate::vfs::nfs_mount::NfsPreflightReport,
    fuse_prerequisites: Result<()>,
) -> Result<ResolvedMountBackend> {
    match fuse_prerequisites {
        Ok(()) => {
            warn!(
                blocker_count = nfs_report.blockers.len(),
                "NFS preflight failed; falling back to FUSE for --backend=auto"
            );
            eprintln!("{}", auto_backend_fallback_message(nfs_report));
            Ok(ResolvedMountBackend::Fuse)
        }
        Err(fuse_error) => Err(CrabError::Configuration {
            key: auto_backend_failure_summary(nfs_report, &fuse_error),
            origin: "crab mount --backend=auto".into(),
        }),
    }
}

#[cfg(feature = "nfs")]
fn nfs_blockers_allow_auto_fuse_fallback(
    report: &crate::vfs::nfs_mount::NfsPreflightReport,
) -> bool {
    report
        .blockers
        .iter()
        .all(|blocker| nfs_blocker_allows_auto_fuse_fallback(&blocker.key))
}

#[cfg(feature = "nfs")]
fn nfs_blocker_allows_auto_fuse_fallback(key: &str) -> bool {
    matches!(
        key,
        "mount_nfs not found"
            | "mount.nfs not found"
            | "Windows Client for NFS mount.exe not found"
            | "Linux NFS mount permission unavailable"
            | "unsupported NFS platform"
    )
}

#[cfg(all(feature = "nfs", feature = "fuse"))]
fn auto_backend_fallback_message(nfs_report: &crate::vfs::nfs_mount::NfsPreflightReport) -> String {
    let mut message = format!(
        "NFS preflight failed; using FUSE for --backend=auto ({} blocker(s)).",
        nfs_report.blockers.len()
    );
    for blocker in &nfs_report.blockers {
        message.push_str("\n- ");
        message.push_str(&blocker.key);
        message.push_str(": ");
        message.push_str(&blocker.detail);
        if let Some(action) = &blocker.action {
            message.push_str("\n  next: ");
            message.push_str(action);
        }
    }
    message
}

#[cfg(all(feature = "nfs", feature = "fuse"))]
fn auto_backend_failure_summary(
    nfs_report: &crate::vfs::nfs_mount::NfsPreflightReport,
    fuse_error: &CrabError,
) -> String {
    format!(
        "no mount backend is usable.\n{}\nFUSE preflight failed: {fuse_error}",
        nfs_report.blocker_summary()
    )
}

/// Run `crab mount doctor`.
pub fn run_mount_doctor(
    requested_backend: MountBackend,
    mountpoint: Option<&Path>,
    json: bool,
) -> Result<()> {
    let temp_mountpoint;
    let mountpoint = if let Some(mountpoint) = mountpoint {
        mountpoint.to_path_buf()
    } else {
        temp_mountpoint = tempfile::tempdir().map_err(CrabError::Io)?;
        temp_mountpoint.path().to_path_buf()
    };
    let checked_backend = mount_doctor_checked_backend(requested_backend);
    let collected = collect_mount_doctor_checks(checked_backend, &mountpoint);
    let payload = mount_doctor_payload(requested_backend, checked_backend, &mountpoint, collected);

    if json {
        let output = serde_json::to_string_pretty(&payload).map_err(|error| {
            CrabError::Internal(format!("failed to serialize mount doctor: {error}"))
        })?;
        println!("{output}");
    } else {
        print_mount_doctor(&payload);
    }
    Ok(())
}

fn mount_doctor_checked_backend(requested_backend: MountBackend) -> MountBackend {
    if requested_backend != MountBackend::Auto {
        return requested_backend;
    }

    #[cfg(feature = "nfs")]
    {
        MountBackend::Nfs
    }
    #[cfg(all(not(feature = "nfs"), feature = "fuse"))]
    {
        MountBackend::Fuse
    }
    #[cfg(not(any(feature = "nfs", feature = "fuse")))]
    {
        MountBackend::Auto
    }
}

fn collect_mount_doctor_checks(
    checked_backend: MountBackend,
    mountpoint: &Path,
) -> MountDoctorCollectedChecks {
    match checked_backend {
        MountBackend::Nfs => collect_nfs_mount_doctor_checks(mountpoint),
        MountBackend::Fuse => MountDoctorCollectedChecks::new(collect_fuse_mount_doctor_checks()),
        MountBackend::Auto => MountDoctorCollectedChecks::new(vec![MountDoctorCheck::fail(
            "mount backend",
            "neither NFS nor FUSE mount support was compiled into this Crab build",
            "Install an NFS-capable Crab build or rebuild with the nfs or fuse feature.",
        )]),
    }
}

#[cfg(feature = "nfs")]
fn collect_nfs_mount_doctor_checks(mountpoint: &Path) -> MountDoctorCollectedChecks {
    let mut checks = Vec::new();

    checks.push(MountDoctorCheck::ok(
        "nfs feature",
        "NFS support is compiled into this Crab build",
    ));
    let helper = find_nfs_mount_helper();
    checks.push(nfs_helper_presence_doctor_check(helper.as_deref()));
    if let Some(helper) = helper.as_deref() {
        checks.push(nfs_helper_version_doctor_check(helper));
        checks.push(nfs_helper_layout_doctor_check(helper));
    }
    let report = crate::vfs::nfs_mount::preflight_for_mountpoint(mountpoint);
    checks.push(nfs_preflight_doctor_check(&report, mountpoint));
    checks.extend(
        report
            .warnings
            .iter()
            .map(nfs_preflight_warning_doctor_check),
    );
    MountDoctorCollectedChecks::with_nfs_preflight(checks, &report)
}

#[cfg(not(feature = "nfs"))]
fn collect_nfs_mount_doctor_checks(mountpoint: &Path) -> MountDoctorCollectedChecks {
    let _ = mountpoint;
    MountDoctorCollectedChecks::new(vec![MountDoctorCheck::fail(
        "nfs feature",
        "NFS support is not compiled into this Crab build",
        "Install an NFS-capable Crab build or rebuild with the nfs feature.",
    )])
}

fn collect_fuse_mount_doctor_checks() -> Vec<MountDoctorCheck> {
    let mut checks = Vec::new();

    #[cfg(feature = "fuse")]
    {
        checks.push(MountDoctorCheck::ok(
            "fuse feature",
            "FUSE support is compiled into this Crab build",
        ));
        match check_fuse_prerequisites() {
            Ok(()) => checks.push(MountDoctorCheck::ok(
                "fuse prerequisites",
                "native FUSE prerequisites are available",
            )),
            Err(error) => checks.push(MountDoctorCheck::fail(
                "fuse prerequisites",
                error.to_string(),
                "Install the platform FUSE prerequisites, then rerun crab mount doctor --backend=fuse.",
            )),
        }
    }

    #[cfg(not(feature = "fuse"))]
    {
        checks.push(MountDoctorCheck::fail(
            "fuse feature",
            "FUSE support is not compiled into this Crab build",
            "Install a FUSE-capable Crab build or rebuild with the fuse feature.",
        ));
    }

    checks
}

#[cfg(feature = "nfs")]
fn nfs_helper_presence_doctor_check(helper: Option<&Path>) -> MountDoctorCheck {
    if let Some(helper) = helper {
        return MountDoctorCheck::ok("nfs helper", format!("{} found", helper.display()));
    }

    MountDoctorCheck::fail(
        "nfs helper",
        "crab-nfs-mount was not found next to crab or on PATH",
        "Reinstall Crab or run `cd crab && make install` so crab-nfs-mount is installed with crab.",
    )
}

#[cfg(feature = "nfs")]
fn nfs_helper_version_doctor_check(helper: &Path) -> MountDoctorCheck {
    nfs_helper_version_doctor_check_from_result(helper, read_mount_helper_version(helper))
}

#[cfg(feature = "nfs")]
fn nfs_helper_version_doctor_check_from_result(
    helper: &Path,
    version: std::result::Result<String, String>,
) -> MountDoctorCheck {
    match version {
        Ok(version) if version == env!("CRAB_BUILD_VERSION") => MountDoctorCheck::ok(
            "nfs helper version",
            format!("{} reports Crab {}", helper.display(), version),
        ),
        Ok(version) => MountDoctorCheck::fail(
            "nfs helper version",
            format!(
                "{} reports Crab {}, but this crab binary is {}",
                helper.display(),
                version,
                env!("CRAB_BUILD_VERSION")
            ),
            "Reinstall Crab so crab and crab-nfs-mount come from the same release.",
        ),
        Err(error) => MountDoctorCheck::fail(
            "nfs helper version",
            format!("could not read {} version: {error}", helper.display()),
            "Reinstall Crab so crab-nfs-mount is executable and version-compatible.",
        ),
    }
}

#[cfg(feature = "nfs")]
fn read_mount_helper_version(helper: &Path) -> std::result::Result<String, String> {
    let output = std::process::Command::new(helper)
        .arg("--version")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            format!("helper exited with {}", output.status)
        } else {
            format!("helper exited with {}: {detail}", output.status)
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_crab_version_output(&stdout)
        .ok_or_else(|| format!("unexpected --version output: {}", stdout.trim()))
}

#[cfg(feature = "nfs")]
fn parse_crab_version_output(output: &str) -> Option<String> {
    let line = output.lines().find(|line| !line.trim().is_empty())?.trim();
    let mut parts = line.split_whitespace();
    if parts.next()? != "crab" {
        return None;
    }
    parts.next().map(str::to_owned)
}

#[cfg(feature = "nfs")]
fn nfs_helper_layout_doctor_check(helper: &Path) -> MountDoctorCheck {
    let current_exe = std::env::current_exe().ok();
    nfs_helper_layout_doctor_check_for_paths(current_exe.as_deref(), helper)
}

#[cfg(feature = "nfs")]
fn nfs_helper_layout_doctor_check_for_paths(
    current_exe: Option<&Path>,
    helper: &Path,
) -> MountDoctorCheck {
    let Some(current_exe) = current_exe else {
        return MountDoctorCheck::fail(
            "nfs helper layout",
            format!(
                "{} found, but current crab path could not be resolved",
                helper.display()
            ),
            "Run `crab mount doctor --backend=nfs` from the installed crab binary.",
        );
    };
    if helper_is_next_to_crab(current_exe, helper) {
        return MountDoctorCheck::ok(
            "nfs helper layout",
            format!("{} is installed next to crab", helper.display()),
        );
    }

    MountDoctorCheck::fail(
        "nfs helper layout",
        format!(
            "{} is not installed next to {}",
            helper.display(),
            current_exe.display()
        ),
        "Reinstall Crab so crab-nfs-mount is installed next to crab.",
    )
}

#[cfg(feature = "nfs")]
fn helper_is_next_to_crab(current_exe: &Path, helper: &Path) -> bool {
    let Some(current_dir) = current_exe.parent() else {
        return false;
    };
    let Some(helper_dir) = helper.parent() else {
        return false;
    };
    let current_dir =
        std::fs::canonicalize(current_dir).unwrap_or_else(|_| current_dir.to_path_buf());
    let helper_dir = std::fs::canonicalize(helper_dir).unwrap_or_else(|_| helper_dir.to_path_buf());
    current_dir == helper_dir
}

#[cfg(feature = "nfs")]
fn nfs_preflight_doctor_check(
    report: &crate::vfs::nfs_mount::NfsPreflightReport,
    mountpoint: &Path,
) -> MountDoctorCheck {
    if report.is_ready() {
        return MountDoctorCheck::ok(
            "nfs preflight",
            format!(
                "native client, mountpoint, loopback, control endpoint, and privilege checks passed for {}",
                mountpoint.display()
            ),
        );
    }

    MountDoctorCheck::fail(
        "nfs preflight",
        report.blocker_summary(),
        "Fix the listed blocker(s), then rerun crab mount doctor --backend=nfs.",
    )
}

#[cfg(feature = "nfs")]
fn nfs_preflight_warning_doctor_check(
    warning: &crate::vfs::nfs_mount::NfsPreflightMessage,
) -> MountDoctorCheck {
    MountDoctorCheck::warn(
        "nfs preflight warning",
        format!("{}: {}", warning.key, warning.detail),
        warning
            .action
            .clone()
            .unwrap_or_else(|| "Review the warning before mounting with NFS.".to_owned()),
    )
}

fn mount_doctor_payload(
    requested_backend: MountBackend,
    checked_backend: MountBackend,
    mountpoint: &Path,
    collected: MountDoctorCollectedChecks,
) -> MountDoctorPayload {
    let auto_decision = mount_doctor_auto_decision(requested_backend, &collected);
    let checks = collected.checks;
    let summary = mount_doctor_summary(&checks);
    MountDoctorPayload {
        requested_backend: requested_backend.as_str().to_owned(),
        checked_backend: checked_backend.as_str().to_owned(),
        mountpoint: mountpoint.display().to_string(),
        checks,
        summary,
        auto_decision,
        #[cfg(feature = "nfs")]
        nfs_preflight: collected.nfs_preflight,
    }
}

fn mount_doctor_auto_decision(
    requested_backend: MountBackend,
    collected: &MountDoctorCollectedChecks,
) -> Option<MountDoctorAutoDecision> {
    if requested_backend != MountBackend::Auto {
        return None;
    }

    #[cfg(feature = "nfs")]
    if let Some(preflight) = &collected.nfs_preflight {
        if let Some(failure) = nfs_auto_hard_failure(&collected.checks) {
            let check_name = failure.name.strip_prefix("nfs ").unwrap_or(failure.name);
            return Some(MountDoctorAutoDecision {
                selected_backend: None,
                reason: format!(
                    "NFS {check_name} check failed with a blocker that auto will not hide"
                ),
                next_action: failure.action.clone(),
                nfs_ready: Some(false),
                nfs_blocker_count: Some(preflight.blocker_count),
                fuse_ready: None,
                fuse_error: None,
            });
        }
        #[cfg(feature = "fuse")]
        let fuse_probe = Some(check_fuse_prerequisites().map_err(|error| error.to_string()));
        #[cfg(not(feature = "fuse"))]
        let fuse_probe = None;
        return Some(mount_doctor_auto_decision_from_nfs_preflight(
            preflight, fuse_probe,
        ));
    }

    #[cfg(all(not(feature = "nfs"), feature = "fuse"))]
    {
        return Some(match check_fuse_prerequisites() {
            Ok(()) => MountDoctorAutoDecision {
                selected_backend: Some("fuse".to_owned()),
                reason: "NFS support is not compiled in; FUSE prerequisites passed".to_owned(),
                next_action: None,
                nfs_ready: None,
                nfs_blocker_count: None,
                fuse_ready: Some(true),
                fuse_error: None,
            },
            Err(error) => MountDoctorAutoDecision {
                selected_backend: None,
                reason: "NFS support is not compiled in and FUSE prerequisites failed".to_owned(),
                next_action: None,
                nfs_ready: None,
                nfs_blocker_count: None,
                fuse_ready: Some(false),
                fuse_error: Some(error.to_string()),
            },
        });
    }

    #[cfg(not(any(feature = "nfs", feature = "fuse")))]
    {
        return Some(MountDoctorAutoDecision {
            selected_backend: None,
            reason: "neither NFS nor FUSE support is compiled into this Crab build".to_owned(),
            next_action: Some(
                "Install an NFS-capable Crab build or rebuild with the nfs or fuse feature."
                    .to_owned(),
            ),
            nfs_ready: None,
            nfs_blocker_count: None,
            fuse_ready: None,
            fuse_error: None,
        });
    }

    None
}

#[cfg(feature = "nfs")]
fn nfs_auto_hard_failure(checks: &[MountDoctorCheck]) -> Option<&MountDoctorCheck> {
    checks
        .iter()
        .filter(|check| check.name != "nfs preflight")
        .find(|check| check.status == MountDoctorStatus::Fail)
}

#[cfg(feature = "nfs")]
fn mount_doctor_auto_decision_from_nfs_preflight(
    preflight: &MountDoctorNfsPreflight,
    fuse_probe: Option<std::result::Result<(), String>>,
) -> MountDoctorAutoDecision {
    if preflight.ready {
        return MountDoctorAutoDecision {
            selected_backend: Some("nfs".to_owned()),
            reason: "NFS preflight passed; auto will use the preferred NFS backend".to_owned(),
            next_action: None,
            nfs_ready: Some(true),
            nfs_blocker_count: Some(0),
            fuse_ready: None,
            fuse_error: None,
        };
    }

    let nfs_can_fallback = preflight
        .blockers
        .iter()
        .all(|blocker| nfs_blocker_allows_auto_fuse_fallback(&blocker.key));
    let next_action = preflight.next_action.clone();
    if !nfs_can_fallback {
        return MountDoctorAutoDecision {
            selected_backend: None,
            reason: "NFS preflight failed with blockers that auto will not hide".to_owned(),
            next_action,
            nfs_ready: Some(false),
            nfs_blocker_count: Some(preflight.blocker_count),
            fuse_ready: None,
            fuse_error: None,
        };
    }

    match fuse_probe {
        Some(Ok(())) => MountDoctorAutoDecision {
            selected_backend: Some("fuse".to_owned()),
            reason: "NFS preflight failed; FUSE prerequisites passed".to_owned(),
            next_action,
            nfs_ready: Some(false),
            nfs_blocker_count: Some(preflight.blocker_count),
            fuse_ready: Some(true),
            fuse_error: None,
        },
        Some(Err(error)) => MountDoctorAutoDecision {
            selected_backend: None,
            reason: "NFS preflight failed and FUSE prerequisites failed".to_owned(),
            next_action,
            nfs_ready: Some(false),
            nfs_blocker_count: Some(preflight.blocker_count),
            fuse_ready: Some(false),
            fuse_error: Some(error),
        },
        None => MountDoctorAutoDecision {
            selected_backend: None,
            reason: "NFS preflight failed and FUSE support is not compiled in".to_owned(),
            next_action,
            nfs_ready: Some(false),
            nfs_blocker_count: Some(preflight.blocker_count),
            fuse_ready: None,
            fuse_error: None,
        },
    }
}

fn mount_doctor_summary(checks: &[MountDoctorCheck]) -> MountDoctorSummary {
    let mut ok = 0;
    let mut warn = 0;
    let mut fail = 0;
    for check in checks {
        match check.status {
            MountDoctorStatus::Ok => ok += 1,
            MountDoctorStatus::Warn => warn += 1,
            MountDoctorStatus::Fail => fail += 1,
        }
    }
    MountDoctorSummary {
        ok,
        warn,
        fail,
        ready: fail == 0,
    }
}

fn print_mount_doctor(payload: &MountDoctorPayload) {
    println!("crab mount doctor ({})", payload.checked_backend);
    if payload.requested_backend != payload.checked_backend {
        println!("Requested backend: {}", payload.requested_backend);
    }
    println!("Mountpoint: {}", payload.mountpoint);
    if let Some(decision) = &payload.auto_decision {
        match &decision.selected_backend {
            Some(selected) => println!("Auto decision: {selected}"),
            None => println!("Auto decision: no usable backend"),
        }
        println!("Auto reason: {}", decision.reason);
        if let Some(action) = &decision.next_action {
            println!("Auto next: {action}");
        }
    }
    println!();

    for check in &payload.checks {
        let icon = match check.status {
            MountDoctorStatus::Ok => "✓",
            MountDoctorStatus::Warn => "!",
            MountDoctorStatus::Fail => "✗",
        };
        println!(
            "  {icon} {:<22} {}",
            check.name,
            indent_multiline_detail(&check.detail)
        );
        if let Some(action) = &check.action {
            println!("    next: {action}");
        }
    }

    println!();
    if payload.summary.fail > 0 {
        println!(
            "{} mount problem(s) found, {} warning(s).",
            payload.summary.fail, payload.summary.warn
        );
    } else if payload.summary.warn > 0 {
        println!(
            "Mount checks passed with {} warning(s).",
            payload.summary.warn
        );
    } else {
        println!("Mount checks passed.");
    }
}

fn indent_multiline_detail(detail: &str) -> String {
    detail.replace('\n', "\n    ")
}

#[cfg(feature = "nfs")]
async fn spawn_nfs_background(opts: &NewMountOpts, mountpoint: &Path) -> Result<()> {
    let helper = resolve_nfs_background_helper(opts.backend)?;
    let log_path = nfs_background_log_path(mountpoint)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(CrabError::Io)?;
    let stderr_log = log.try_clone().map_err(CrabError::Io)?;
    let mut command = std::process::Command::new(helper);
    let control_endpoint = crate::vfs::nfs_control::fresh_endpoint_for_mountpoint(mountpoint)?;
    if let Some(endpoint) = control_endpoint.as_deref() {
        command.env(crate::vfs::nfs_control::CONTROL_ENDPOINT_ENV, endpoint);
    }
    command
        .arg("mount")
        .arg("--backend")
        .arg("nfs")
        .arg("--foreground")
        .arg("--repo")
        .arg(&opts.repo)
        .arg("--mountpoint")
        .arg(mountpoint);
    if let Some(git_ref) = &opts.git_ref {
        command.arg("--ref").arg(git_ref);
    }
    if opts.read_only {
        command.arg("--read-only");
    }
    if opts.no_refresh {
        command.arg("--no-refresh");
    }
    if opts.allow_nested {
        command.arg("--allow-nested");
    }
    if let Some(name) = &opts.name {
        command.arg("--name").arg(name);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(stderr_log));
    let mut child = command.spawn().map_err(CrabError::Io)?;
    let pid = child.id();
    let started_at = std::time::Instant::now();
    while started_at.elapsed() < NFS_BACKGROUND_STARTUP_TIMEOUT {
        if crate::vfs::nfs_mount::is_mounted(mountpoint)
            && nfs_background_control_ready(control_endpoint.as_deref()).await
        {
            if let Err(error) = register_nfs_background_mount(
                opts,
                mountpoint,
                pid,
                &log_path,
                control_endpoint.clone(),
            ) {
                warn!(error = %error, "failed to register NFS mount");
            }
            println!(
                "Mounted via NFS at {} (pid {}). Logs: {}",
                mountpoint.display(),
                pid,
                log_path.display()
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(CrabError::Io)? {
            let preflight_hint = nfs_background_startup_failure_hint(mountpoint);
            return Err(CrabError::Internal(format!(
                "NFS mount helper exited before mounting {}: {status}. See {}{}",
                mountpoint.display(),
                log_path.display(),
                preflight_hint
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if let Err(error) = child.kill() {
        warn!(pid, error = %error, "failed to terminate timed-out NFS mount helper");
    }
    let _ = child.wait();
    let preflight_hint = nfs_background_startup_failure_hint(mountpoint);
    Err(CrabError::Internal(format!(
        "timed out after {}s waiting for NFS mount helper to mount {}. See {}{}",
        NFS_BACKGROUND_STARTUP_TIMEOUT.as_secs(),
        mountpoint.display(),
        log_path.display(),
        preflight_hint
    )))
}

#[cfg(feature = "nfs")]
fn resolve_nfs_background_helper(requested_backend: MountBackend) -> Result<PathBuf> {
    let helper = find_nfs_mount_helper().ok_or_else(|| {
        nfs_helper_configuration_error(
            requested_backend,
            "crab-nfs-mount was not found next to crab or on PATH. Reinstall Crab or run `cd crab && make install` so background NFS mounts use the shipped helper.",
        )
    })?;
    let current_exe = std::env::current_exe().map_err(|error| {
        nfs_helper_configuration_error(
            requested_backend,
            format!(
                "could not resolve the current crab binary before starting crab-nfs-mount: {error}"
            ),
        )
    })?;
    check_nfs_background_helper(
        &current_exe,
        &helper,
        read_mount_helper_version(&helper),
        requested_backend,
    )?;
    Ok(helper)
}

#[cfg(feature = "nfs")]
fn check_nfs_background_helper(
    current_exe: &Path,
    helper: &Path,
    helper_version: std::result::Result<String, String>,
    requested_backend: MountBackend,
) -> Result<()> {
    if !helper_is_next_to_crab(current_exe, helper) {
        return Err(nfs_helper_configuration_error(
            requested_backend,
            format!(
                "{} is not installed next to {}. Reinstall Crab so background NFS mounts use the helper shipped with this crab binary.",
                helper.display(),
                current_exe.display()
            ),
        ));
    }
    match helper_version {
        Ok(version) if version == env!("CRAB_BUILD_VERSION") => Ok(()),
        Ok(version) => Err(nfs_helper_configuration_error(
            requested_backend,
            format!(
                "{} reports Crab {}, but this crab binary is {}. Reinstall Crab so crab and crab-nfs-mount come from the same release.",
                helper.display(),
                version,
                env!("CRAB_BUILD_VERSION")
            ),
        )),
        Err(error) => Err(nfs_helper_configuration_error(
            requested_backend,
            format!(
                "could not read {} version before starting background NFS mount: {error}. Reinstall Crab so crab-nfs-mount is executable and version-compatible.",
                helper.display()
            ),
        )),
    }
}

#[cfg(feature = "nfs")]
fn nfs_helper_configuration_error(
    requested_backend: MountBackend,
    key: impl Into<String>,
) -> CrabError {
    CrabError::Configuration {
        key: key.into(),
        origin: format!("crab mount --backend={}", requested_backend.as_str()),
    }
}

#[cfg(feature = "nfs")]
async fn nfs_background_control_ready(control_endpoint: Option<&str>) -> bool {
    let Some(endpoint) = control_endpoint else {
        return true;
    };
    crate::vfs::nfs_control::ping(endpoint).await.is_ok()
}

#[cfg(feature = "nfs")]
fn nfs_background_startup_failure_hint(mountpoint: &Path) -> String {
    let report = crate::vfs::nfs_mount::preflight_for_mountpoint(mountpoint);
    nfs_background_preflight_failure_hint(&report).unwrap_or_default()
}

#[cfg(feature = "nfs")]
fn nfs_background_preflight_failure_hint(
    report: &crate::vfs::nfs_mount::NfsPreflightReport,
) -> Option<String> {
    if report.blockers.is_empty() {
        return None;
    }
    Some(format!(
        "\nNFS preflight now reports:\n{}",
        report.blocker_summary()
    ))
}

#[cfg(feature = "nfs")]
fn register_nfs_background_mount(
    opts: &NewMountOpts,
    mountpoint: &Path,
    pid: u32,
    log_path: &Path,
    control_endpoint: Option<String>,
) -> Result<()> {
    use crate::vfs::mounts_registry::{self, MountEntry};

    let registry_path = mounts_registry::registry_path()?;
    let source = normalized_mount_source_for_registry(&opts.repo)?;
    let git_ref = opts.git_ref.as_deref().map_or_else(
        || "HEAD".to_owned(),
        |git_ref| {
            if git_ref.starts_with("refs/") {
                git_ref.to_owned()
            } else {
                format!("refs/heads/{git_ref}")
            }
        },
    );
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| mounts_registry::derive_name_from_source(&source));
    let entry = MountEntry {
        mountpoint: mountpoint.display().to_string(),
        source,
        git_ref,
        pid,
        start_time: mounts_registry::now_iso8601(),
        read_only: opts.read_only,
        name,
        backend: Some("nfs".to_owned()),
        log_path: Some(log_path.display().to_string()),
        control_endpoint,
    };
    Ok(mounts_registry::add_entry(&registry_path, entry)?)
}

#[cfg(feature = "nfs")]
fn normalized_mount_source_for_registry(source: &str) -> Result<String> {
    match crate::vfs::source::MountSource::parse(source)? {
        crate::vfs::source::MountSource::Remote { url } => Ok(url),
        crate::vfs::source::MountSource::Local { path } => {
            let canonical = std::fs::canonicalize(path).map_err(CrabError::Io)?;
            Ok(canonical.display().to_string())
        }
    }
}

#[cfg(feature = "nfs")]
fn nfs_background_log_path(mountpoint: &Path) -> Result<PathBuf> {
    let registry_path = crate::vfs::mounts_registry::registry_path()?;
    let base_dir = registry_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let log_dir = base_dir.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(CrabError::Io)?;
    Ok(log_dir.join(format!(
        "nfs-{}.log",
        sanitize_mountpoint_for_filename(mountpoint)
    )))
}

#[cfg(feature = "nfs")]
fn sanitize_mountpoint_for_filename(mountpoint: &Path) -> String {
    let raw = mountpoint.to_string_lossy();
    let mut name = String::with_capacity(raw.len().min(80));
    let mut last_was_dash = false;
    for byte in raw.bytes() {
        let ch = match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                last_was_dash = false;
                byte as char
            }
            _ if !last_was_dash => {
                last_was_dash = true;
                '-'
            }
            _ => continue,
        };
        if name.len() < 80 {
            name.push(ch);
        }
    }
    let trimmed = name.trim_matches('-');
    if trimmed.is_empty() {
        "mount".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(feature = "nfs")]
fn find_nfs_mount_helper() -> Option<PathBuf> {
    find_mount_helper("crab-nfs-mount")
}

#[cfg(feature = "nfs")]
fn find_mount_helper(name: &str) -> Option<PathBuf> {
    let candidates = mount_helper_filenames(name, cfg!(windows));
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        for candidate in &candidates {
            let helper = dir.join(candidate);
            if helper.is_file() {
                return Some(helper);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in &candidates {
            let helper = dir.join(candidate);
            if helper.is_file() {
                return Some(helper);
            }
        }
    }
    None
}

#[cfg(feature = "nfs")]
fn mount_helper_filenames(name: &str, is_windows: bool) -> Vec<String> {
    let mut candidates = vec![name.to_owned()];
    if is_windows && !name.to_ascii_lowercase().ends_with(".exe") {
        candidates.push(format!("{name}.exe"));
    }
    candidates
}

/// Build pipeline config for a local mount source.
///
/// Validates the local repo, computes a cache hash from the absolute path,
/// and attempts to read a StoreLayout from the repo's committed `crab.toml`
/// for pointer-file hydration.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn build_local_pipeline_config(
    repo_path: &Path,
    _mountpoint: &Path,
    ref_name: Option<String>,
    read_only: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(
    crate::vfs::pipeline::PipelineConfig,
    Option<crate::vfs::MountReadContext>,
    crate::vfs::clone_cache::MountCacheLock,
)> {
    use crate::vfs::clone_cache::{MountCacheLock, compute_cache_hash};
    use crate::vfs::pipeline::PipelineConfig;
    use crate::vfs::source::MountSource;

    // Validate the local repo — ensures .git exists.
    let git_dir = MountSource::validate_local(repo_path)?;

    info!(
        repo_path = %repo_path.display(),
        git_dir = %git_dir.display(),
        "validated local mount source"
    );

    // Compute cache hash from the absolute path of the repo.
    let abs_path = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let cache_hash = compute_cache_hash(&abs_path.to_string_lossy());

    // Store overlay and snapshot state in ~/.crab/mounts/repos/<hash>/
    let home = std::env::var("HOME").map_err(|_| CrabError::Configuration {
        key: "HOME environment variable not set".into(),
        origin: String::new(),
    })?;
    let cache_dir = PathBuf::from(home)
        .join(".crab")
        .join("mounts")
        .join("repos")
        .join(&cache_hash);

    let cache_lock = MountCacheLock::acquire(&cache_dir)?;
    std::fs::create_dir_all(&cache_dir)?;

    let config = PipelineConfig {
        source: abs_path.display().to_string(),
        git_dir,
        ref_name,
        read_only,
        cache_dir: cache_dir.clone(),
        cancel_token: cancel,
    };

    // Attempt to read StoreLayout from the repo's committed crab.toml
    // for pointer-file hydration from object storage.
    let crab_dir = repo_path.join(".crab");
    let read_context = resolve_mount_read_context_from_local_repo(&crab_dir).await;

    if read_context.is_none() {
        warn!(
            repo = %repo_path.display(),
            "no crab remote configured for local repo; \
             pointer files that require xorb hydration will return EIO"
        );
    }

    Ok((config, read_context, cache_lock))
}

/// Build pipeline config for a remote mount source.
///
/// Computes the cache directory from the URL hash, performs a blobless
/// clone (or updates an existing one), and constructs the StoreLayout
/// for xorb fetching.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn build_remote_pipeline_config(
    url: &str,
    ref_name: Option<&str>,
    read_only: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(
    crate::vfs::pipeline::PipelineConfig,
    Option<crate::vfs::MountReadContext>,
    crate::vfs::clone_cache::MountCacheLock,
)> {
    use crate::vfs::clone_cache::{MountCacheLock, cache_dir_for_url, ensure_blobless_clone};
    use crate::vfs::pipeline::PipelineConfig;
    use crate::vfs::source::MountSource;

    // Validate the remote URL format.
    let _components = MountSource::validate_remote(url)?;

    // Compute cache directory.
    let cache_dir = cache_dir_for_url(url)?;
    let cache_lock = MountCacheLock::acquire(&cache_dir)?;

    // Perform blobless clone (or update existing).
    let git_dir = ensure_blobless_clone(url, ref_name, &cache_dir)?;

    info!(
        url = %crate::vfs::refresh::redact_url(url),
        cache_dir = %cache_dir.display(),
        "remote clone ready"
    );

    let ref_name_owned = ref_name.map(|r| {
        if r.starts_with("refs/") {
            r.to_owned()
        } else {
            format!("refs/heads/{r}")
        }
    });

    let config = PipelineConfig {
        source: url.to_owned(),
        git_dir,
        ref_name: ref_name_owned,
        read_only,
        cache_dir: cache_dir.clone(),
        cancel_token: cancel,
    };

    // Attempt to build a StoreLayout for xorb fetching.
    let read_context = resolve_mount_read_context_from_remote_url(url)
        .await
        .ok_or_else(|| CrabError::Configuration {
            key: "object-store read layout unavailable for remote mount".into(),
            origin: "crab mount".into(),
        })?;

    Ok((config, Some(read_context), cache_lock))
}

/// Attempt to construct a `StoreLayout` from a local repo's `crab.toml`.
///
/// This enables pointer-file hydration for local mounts that have a crab
/// remote configured. If no remote is configured, returns `None` and
/// pointer files will return EIO when accessed.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn resolve_mount_read_context_from_local_repo(
    crab_dir: &Path,
) -> Option<crate::vfs::MountReadContext> {
    // Reuse the existing helper that reads from crab.toml.
    resolve_mount_read_context_from_config(crab_dir).await
}

/// Attempt to construct a `StoreLayout` from a remote URL.
///
/// Parses the URL, authenticates, and builds the object store.
/// Returns `None` if any step fails.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn resolve_mount_read_context_from_remote_url(
    url: &str,
) -> Option<crate::vfs::MountReadContext> {
    let config = crate::core::config::Config::resolve_local().unwrap_or_default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let cache_dir = crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
    let resolver = crab_auth_store::ManagedRepositoryResolver::new(cache_dir);
    let locator = resolver.classify(url).ok()?;
    let layout = match locator {
        crab_git::RepositoryLocator::Direct(repository) => {
            let parsed = crate::git::url::CrabUrl {
                bucket: repository.bucket,
                repo_path: repository.repo_prefix,
            };
            select_mount_read_layout(&config, &parsed, &cancel).await?
        }
        managed @ crab_git::RepositoryLocator::Managed(_) => {
            let resolved = crate::auth::build_repository_store(
                &config,
                managed,
                crab_auth::TransferOperation::Hydrate,
                &cancel,
            )
            .await
            .ok()?;
            crate::storage::StoreLayout::new(resolved.store, resolved.repository_prefix)
        }
    };
    build_mount_read_context(&config, layout)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn select_mount_read_layout(
    config: &crate::core::config::Config,
    parsed: &crate::git::url::CrabUrl,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<crate::storage::StoreLayout> {
    select_mount_read_layout_with_selector(
        config,
        parsed,
        cancel,
        |config, parsed, cancel| async move {
            crate::replication::select_read_store(&config, &parsed, "mount", &cancel).await
        },
    )
    .await
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn select_mount_read_layout_with_selector<F, Fut>(
    config: &crate::core::config::Config,
    parsed: &crate::git::url::CrabUrl,
    cancel: &tokio_util::sync::CancellationToken,
    select_read: F,
) -> Option<crate::storage::StoreLayout>
where
    F: FnOnce(
        crate::core::config::Config,
        crate::git::url::CrabUrl,
        tokio_util::sync::CancellationToken,
    ) -> Fut,
    Fut: std::future::Future<Output = Result<crate::replication::ReadStoreSelection>>,
{
    let selection = select_read(config.clone(), parsed.clone(), cancel.clone())
        .await
        .ok()?;
    if let crate::replication::ReadSource::Replica { name } = &selection.source {
        tracing::debug!(replica = %name, "selected read replica for mount");
    }
    Some(selection.router)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn build_mount_read_context(
    config: &crate::core::config::Config,
    layout: crate::storage::StoreLayout,
) -> Option<crate::vfs::MountReadContext> {
    let origin = layout.store().as_storage().clone();
    let store_layout = crab_storage::StoreLayout::with_global_prefix(
        origin.clone(),
        layout.repo_prefix().to_owned(),
        layout.global_prefix().to_owned(),
    );
    let caching_store = crab_cache_store::CachingStore::new(origin, &config.cache).ok()?;
    let read_layout = crab_read::ReadStoreLayout::with_global_prefix(
        caching_store.origin().clone(),
        layout.repo_prefix().to_owned(),
        layout.global_prefix().to_owned(),
    );
    let mut hydrator = crab_read::ShardHydrator::new(
        caching_store,
        read_layout,
        config.hydrate.download_concurrency,
    )
    .ok()?;

    match crate::cache::xet_chunk_cache_from_config(config) {
        Ok(handle) => hydrator = hydrator.with_xet_chunk_cache(handle.cache),
        Err(error) => {
            tracing::debug!(%error, "mount: continuing without shared xet chunk cache");
        }
    }

    Some(crate::vfs::MountReadContext {
        store_layout,
        hydrator: std::sync::Arc::new(hydrator),
    })
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Debug, Default)]
struct CrabMountReadResolver;

#[cfg(any(feature = "fuse", feature = "nfs"))]
#[async_trait::async_trait]
impl crate::vfs::MountReadResolver for CrabMountReadResolver {
    async fn resolve(
        &self,
        remote: &str,
    ) -> crate::vfs::Result<Option<crate::vfs::MountReadContext>> {
        Ok(resolve_mount_read_context_from_remote_url(remote).await)
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub(crate) fn mount_read_resolver() -> std::sync::Arc<dyn crate::vfs::MountReadResolver> {
    std::sync::Arc::new(CrabMountReadResolver)
}

// ---------------------------------------------------------------------------
// Legacy mount command (fuse feature enabled)
// ---------------------------------------------------------------------------

/// Run the mount command when the `fuse` feature is enabled.
///
/// Validates prerequisites, resolves the mountpoint, and either blocks
/// in foreground mode or forks to background in daemon mode.
///
/// This is the legacy path used when `--repo` is not provided (backward compat).
#[cfg(feature = "fuse")]
pub async fn run_mount(
    path: &Path,
    daemon: bool,
    git_ref: Option<&str>,
    allow_nested: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // Check OS prerequisites before attempting anything.
    if let Err(error) = check_fuse_prerequisites() {
        print_prerequisite_error(&error);
        return Err(CrabError::Configuration {
            key: "FUSE prerequisites not met".into(),
            origin: "crab mount".into(),
        });
    }

    let mountpoint = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Safety check: refuse to mount inside a git/crab working tree.
    if !allow_nested && is_inside_git_repo(&mountpoint) {
        return Err(CrabError::Configuration {
            key: "mountpoint is inside a git repository. Mount outside the repo to avoid git seeing virtual files as untracked.".into(),
            origin: "crab mount".into(),
        });
    }

    let crab_dir = find_crab_dir(&mountpoint);

    info!(
        mountpoint = %mountpoint.display(),
        daemon,
        git_ref = ?git_ref,
        "starting FUSE mount"
    );

    // Daemon mode: fork to background.
    if daemon {
        return run_daemon_mode(&mountpoint, &crab_dir, git_ref, cancel).await;
    }

    // Foreground mode: block until SIGINT.
    run_foreground_mode(&mountpoint, &crab_dir, git_ref, cancel).await
}

/// Run the mount in foreground mode, blocking until cancellation.
#[cfg(feature = "fuse")]
async fn run_foreground_mode(
    mountpoint: &Path,
    crab_dir: &Path,
    git_ref: Option<&str>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::vfs::clone_cache::MountCacheLock;
    use crate::vfs::mount;

    let rt = tokio::runtime::Handle::current();
    let _cache_lock = MountCacheLock::acquire(crab_dir)?;

    // Build the mount components.
    let (resolver, engine) = build_mount_components(crab_dir, git_ref).await?;

    let config = mount::MountConfig {
        mountpoint: mountpoint.to_path_buf(),
        git_dir: find_git_dir(crab_dir),
        write_pid: false,
        crab_dir: crab_dir.to_path_buf(),
        read_only: false,
    };

    // Install signal handler for graceful shutdown.
    mount::install_signal_handler(cancel.clone(), &rt);

    let mounted = mount::mount(&config, resolver, engine, rt.clone())?;

    println!("Mounted at {}", mountpoint.display());
    println!("Press Ctrl+C to unmount.");

    mount::run_until_cancelled(mounted.session, mountpoint, cancel, crab_dir, rt)?;

    println!("Unmounted.");
    Ok(())
}

/// Run the mount in daemon mode: delegate to the coordinator via IPC.
///
/// Uses the Connect_Or_Spawn pattern to reach the coordinator and sends
/// a mount request. On success, prints the mountpoint and coordinator PID.
#[cfg(feature = "fuse")]
async fn run_daemon_mode(
    mountpoint: &Path,
    crab_dir: &Path,
    git_ref: Option<&str>,
    _cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    #[cfg(unix)]
    {
        use crate::vfs::ipc_client::try_ipc_mount;

        // Determine the source URL from the crab remote config.
        let repo = read_remote_url_from_crab_dir(crab_dir)
            .unwrap_or_else(|_| mountpoint.display().to_string());

        let mountpoint_str = mountpoint.display().to_string();
        let ref_str = git_ref.unwrap_or("HEAD");

        Ok(try_ipc_mount(&repo, &mountpoint_str, ref_str, false, false).await?)
    }

    #[cfg(not(unix))]
    {
        warn!("daemon mode not supported on this platform, running in foreground");
        run_foreground_mode(mountpoint, crab_dir, git_ref, _cancel).await
    }
}

/// Build the resolver and engine for a mount.
///
/// Uses the `MountPipelineBuilder` to execute the full 11-step pipeline.
/// For standalone `crab mount`, the git dir is discovered from the `.crab`
/// directory's parent, and the `StoreLayout` is constructed from the
/// crab remote config if available.
#[cfg(feature = "fuse")]
async fn build_mount_components(
    crab_dir: &Path,
    git_ref: Option<&str>,
) -> Result<(
    std::sync::Arc<crate::vfs::resolver::FuseResolver>,
    std::sync::Arc<crate::vfs::engine::VfsEngine>,
)> {
    use tokio_util::sync::CancellationToken;

    use crate::vfs::pipeline::{MountPipelineBuilder, PipelineConfig};

    // Discover the git directory (sibling of .crab).
    let git_dir = crab_dir
        .parent()
        .map_or_else(|| PathBuf::from(".git"), |p| p.join(".git"));

    if !git_dir.exists() {
        return Err(CrabError::Configuration {
            key: format!(".git directory not found at {}", git_dir.display()),
            origin: "build_mount_components".into(),
        });
    }

    // Determine the source URL from the crab remote config.
    let source =
        read_remote_url_from_crab_dir(crab_dir).unwrap_or_else(|_| git_dir.display().to_string());

    // Resolve the ref to mount.
    let ref_name = git_ref.map(|r| {
        if r.starts_with("refs/") {
            r.to_owned()
        } else {
            format!("refs/heads/{r}")
        }
    });

    // Ensure cache directory exists.
    std::fs::create_dir_all(crab_dir)?;

    let cancel = CancellationToken::new();

    let config = PipelineConfig {
        source,
        git_dir: git_dir.clone(),
        ref_name,
        read_only: false,
        cache_dir: crab_dir.to_path_buf(),
        cancel_token: cancel,
    };

    // Attempt to construct a StoreLayout from the crab remote config.
    let read_context = resolve_mount_read_context_from_config(crab_dir).await;

    let mut builder = MountPipelineBuilder::new(config);
    if let Some(context) = read_context {
        builder = builder.with_read_context(context);
    }

    // Execute the pipeline synchronously on the current thread.
    let output = builder.execute()?;

    info!(
        generation = output.generation,
        head_oid = %output.head_oid,
        "mount pipeline ready"
    );

    Ok((output.resolver, output.engine))
}

/// Read the remote URL from the committed `crab.toml` file.
///
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn read_remote_url_from_crab_dir(crab_dir: &Path) -> Result<String> {
    let repo_root = crab_dir.parent().ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "cannot locate repository root from .crab directory".into(),
    })?;
    crate::core::project_config::ProjectConfig::remote_url(repo_root)
}

/// Attempt to construct a `StoreLayout` from the crab remote config.
///
/// Reads the remote URL from `crab.toml`, parses it as a Crab URL,
/// builds an authenticated object store, and returns the layout. Returns
/// `None` if any step fails (e.g. no remote configured, auth unavailable).
/// Pointer-file hydration will fall back to stub resolvers in that case.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn resolve_mount_read_context_from_config(
    crab_dir: &Path,
) -> Option<crate::vfs::MountReadContext> {
    let url_str = read_remote_url_from_crab_dir(crab_dir).ok()?;

    resolve_mount_read_context_from_remote_url(&url_str).await
}

// ---------------------------------------------------------------------------
// Unmount command
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is currently running.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is safe — it checks existence without signaling.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
pub fn is_pid_alive(pid: u32) -> bool {
    windows_tasklist_pid_alive(pid)
}

#[cfg(not(any(unix, windows)))]
pub fn is_pid_alive(_pid: u32) -> bool {
    false
}

/// Run the unmount command for a single mountpoint.
///
/// Strategy:
/// 1. Look up the mountpoint in `mounts.json` to find the PID.
/// 2. If found and PID is alive: send SIGTERM, wait up to 10s, then force-unmount.
/// 3. If found but PID is dead (stale): clean up registry and force-unmount.
/// 4. If not found in registry: fall back to direct force-unmount via OS tools.
/// 5. Remove the entry from `mounts.json` after successful unmount.
pub fn run_unmount(path: &Path) -> Result<()> {
    let mountpoint = normalize_unmount_path(path);
    let mountpoint_str = mountpoint.display().to_string();

    // Try to find the mount in the registry.
    let registry_path = crate::vfs::mounts_registry::registry_path().ok();
    let entry = registry_path.as_ref().and_then(|rp| {
        crate::vfs::mounts_registry::list_entries(rp)
            .ok()
            .and_then(|entries| entries.into_iter().find(|e| e.mountpoint == mountpoint_str))
    });

    if let Some(entry) = entry {
        let pid = entry.pid;

        if is_pid_alive(pid) {
            // Process is running — send SIGTERM and wait.
            info!(pid, mountpoint = %mountpoint.display(), "sending SIGTERM to mount process");
            terminate_and_force_unmount(pid, &mountpoint)?;
        } else {
            // Stale entry — PID is not running. Clean up.
            info!(pid, mountpoint = %mountpoint.display(), "stale mount entry detected (PID not running)");
            force_unmount_cli(&mountpoint).ok();
            println!("Cleaned up stale mount at {}.", mountpoint.display());
        }

        // Remove the entry from the registry.
        if let Some(ref rp) = registry_path
            && let Err(e) = crate::vfs::mounts_registry::remove_entry(rp, &mountpoint_str)
        {
            warn!(error = %e, "failed to remove entry from mounts registry");
        }

        // Also clean up the legacy PID file if present.
        let crab_dir = find_crab_dir(&mountpoint);
        clean_pid_file(&crab_dir);
    } else {
        // Not found in registry — fall back to legacy PID file or direct force-unmount.
        let crab_dir = find_crab_dir(&mountpoint);
        let pid = read_pid_file_for_unmount(&crab_dir);

        if let Some(pid) = pid {
            info!(pid, mountpoint = %mountpoint.display(), "found PID from legacy PID file");
            terminate_and_force_unmount(pid, &mountpoint)?;
            clean_pid_file(&crab_dir);
        } else {
            warn!(mountpoint = %mountpoint.display(), "no registry entry or PID file found, attempting force-unmount");
            force_unmount_cli(&mountpoint)?;
        }
    }

    println!("Unmounted {}.", mountpoint.display());
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn try_mount_control_unmount(path: &Path) -> Result<bool> {
    let Some(shutdown) = crate::vfs::mount_control::shutdown(path).await? else {
        return Ok(false);
    };

    match shutdown.backend {
        crate::vfs::mount_control::MountControlBackend::Nfs => {
            #[cfg(feature = "nfs")]
            {
                let Some(registry_path) = shutdown.registry_path else {
                    return Err(CrabError::Internal(
                        "NFS mount control shutdown missing registry path".into(),
                    ));
                };
                let Some(pid) = shutdown.pid else {
                    return Err(CrabError::Internal(
                        "NFS mount control shutdown missing helper PID".into(),
                    ));
                };
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                while tokio::time::Instant::now() < deadline {
                    if !is_pid_alive(pid)
                        || !crate::vfs::nfs_mount::is_mounted(&shutdown.mountpoint)
                    {
                        crate::vfs::mounts_registry::remove_entry(
                            &registry_path,
                            &shutdown.mountpoint_str,
                        )?;
                        clean_pid_file(&find_crab_dir(&shutdown.mountpoint));
                        println!("Unmounted {}.", shutdown.mountpoint.display());
                        return Ok(true);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }

                Err(CrabError::Internal(format!(
                    "NFS helper accepted shutdown for {} but did not unmount within 10s",
                    shutdown.mountpoint.display()
                )))
            }
            #[cfg(not(feature = "nfs"))]
            {
                Err(CrabError::Internal(
                    "NFS mount control returned without NFS support".into(),
                ))
            }
        }
        crate::vfs::mount_control::MountControlBackend::Fuse => {
            println!("Unmounted {}.", shutdown.mountpoint.display());
            Ok(true)
        }
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_control_refresh(path: &Path) -> Result<()> {
    if try_mount_control_refresh(path).await? {
        return Ok(());
    }
    Err(mount_control_target_missing("refresh", path))
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn try_mount_control_refresh(path: &Path) -> Result<bool> {
    let Some(update) = crate::vfs::mount_control::refresh(path).await? else {
        return Ok(false);
    };
    println!(
        "Refreshed {} mount at {}.",
        update.backend.as_str().to_uppercase(),
        update.mountpoint.display()
    );
    if let Some(head_oid) = update.head_oid {
        println!("  New HEAD: {head_oid}");
    }
    Ok(true)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_control_switch(path: &Path, git_ref: &str) -> Result<()> {
    if try_mount_control_switch(path, git_ref).await? {
        return Ok(());
    }
    Err(mount_control_target_missing("switch", path))
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn try_mount_control_switch(path: &Path, git_ref: &str) -> Result<bool> {
    let Some(update) = crate::vfs::mount_control::switch_ref(path, git_ref).await? else {
        return Ok(false);
    };
    let shown_ref = update.head_ref.as_deref().unwrap_or(git_ref);
    println!(
        "Switched {} mount at {} to ref '{}'.",
        update.backend.as_str().to_uppercase(),
        update.mountpoint.display(),
        shown_ref
    );
    if let Some(head_oid) = update.head_oid {
        println!("  New HEAD: {head_oid}");
    }
    Ok(true)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn mount_control_target_missing(action: &str, path: &Path) -> CrabError {
    CrabError::Configuration {
        key: format!(
            "no live mount found at {}; run `crab mount list` to choose a mounted path",
            path.display()
        ),
        origin: format!("crab mount {action}"),
    }
}

/// Unmount all active mounts listed in `mounts.json`.
///
/// Iterates all entries, unmounts each (handling stale entries), and
/// prints a summary.
pub fn run_unmount_all() -> Result<()> {
    let registry_path = crate::vfs::mounts_registry::registry_path()?;
    let entries = crate::vfs::mounts_registry::list_entries(&registry_path)?;

    if entries.is_empty() {
        println!("No active mounts.");
        return Ok(());
    }

    let _total = entries.len();
    let mut unmounted = 0u32;

    for entry in &entries {
        if unmount_registry_entry(&registry_path, entry)? {
            unmounted += 1;
        }
    }

    println!("Unmounted {unmounted} mount(s).");
    Ok(())
}

fn unmount_registry_entry(
    registry_path: &Path,
    entry: &crate::vfs::mounts_registry::MountEntry,
) -> Result<bool> {
    let mountpoint = Path::new(&entry.mountpoint);
    let pid = entry.pid;

    if is_pid_alive(pid) {
        info!(pid, mountpoint = %entry.mountpoint, "sending SIGTERM to mount process");
        terminate_and_force_unmount(pid, mountpoint)?;
    } else {
        info!(pid, mountpoint = %entry.mountpoint, "stale mount entry (PID not running)");
        force_unmount_cli(mountpoint).ok();
        println!("Cleaned up stale mount at {}.", entry.mountpoint);
    }

    clean_pid_file(&find_crab_dir(mountpoint));
    crate::vfs::mounts_registry::remove_entry(registry_path, &entry.mountpoint)?;
    Ok(true)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_unmount_all_live_or_persisted() -> Result<()> {
    let registry_path = crate::vfs::mounts_registry::registry_path()?;
    let entries = crate::vfs::mounts_registry::list_entries(&registry_path)?;

    if entries.is_empty() {
        println!("No active mounts.");
        return Ok(());
    }

    let mut unmounted = 0u32;
    for entry in &entries {
        let mountpoint = Path::new(&entry.mountpoint);
        match try_mount_control_unmount(mountpoint).await {
            Ok(true) => {
                unmounted += 1;
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                warn!(
                    mountpoint = %entry.mountpoint,
                    error = %error,
                    "live mount-control unmount failed, falling back"
                );
            }
        }

        if unmount_registry_entry(&registry_path, entry)? {
            unmounted += 1;
        }
    }

    println!("Unmounted {unmounted} mount(s).");
    Ok(())
}

fn normalize_unmount_path(path: &Path) -> PathBuf {
    #[cfg(all(any(windows, test), feature = "nfs"))]
    if let Ok(target) = crate::vfs::nfs_mount::windows_mount_target(path) {
        return PathBuf::from(target);
    }

    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Read a PID file for unmount purposes (works with or without the fuse feature).
fn read_pid_file_for_unmount(crab_dir: &Path) -> Option<u32> {
    let path = crab_dir.join("mount.pid");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Send SIGTERM to a process, wait up to 10s for it to exit, then
/// force-unmount via OS tools if it's still running.
#[cfg(unix)]
fn terminate_and_force_unmount(pid: u32, mountpoint: &Path) -> Result<()> {
    use std::time::{Duration, Instant};

    // SAFETY: kill() with a valid PID and signal is safe.
    let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH means process doesn't exist — already dead.
        if err.raw_os_error() == Some(libc::ESRCH) {
            info!(pid, "process already exited");
            return Ok(());
        }
        return Err(CrabError::Io(err));
    }

    // Wait for the process to exit (poll with timeout).
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // SAFETY: kill(pid, 0) checks if process exists without sending a signal.
        let alive = unsafe { libc::kill(pid as i32, 0) };
        if alive != 0 {
            // Process exited cleanly.
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Process did not exit within 10s — force-unmount via OS tools.
    warn!(
        pid,
        "process did not exit within 10s after SIGTERM, force-unmounting"
    );
    force_unmount_cli(mountpoint)
}

#[cfg(not(unix))]
fn terminate_and_force_unmount(pid: u32, mountpoint: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        force_unmount_cli(mountpoint)?;
        if wait_for_pid_exit(pid, std::time::Duration::from_secs(10)) {
            return Ok(());
        }
        warn!(
            pid,
            "mount process did not exit within 10s after unmount, terminating"
        );
        terminate_windows_process(pid)
    }

    #[cfg(not(windows))]
    {
        let _ = pid;
        force_unmount_cli(mountpoint)
    }
}

/// Attempt a force-unmount via OS tools.
fn force_unmount_cli(mountpoint: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("umount")
            .arg("-f")
            .arg(mountpoint)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CrabError::Internal(format!(
                "force-unmount failed: {stderr}"
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let mut failures = Vec::new();
        for command in linux_unmount_commands() {
            let output = std::process::Command::new(command.program)
                .args(command.args)
                .arg(mountpoint)
                .output();
            match output {
                Ok(output) if output.status.success() => return Ok(()),
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    failures.push(format!("{}: {stderr}", command.program));
                }
                Err(error) => {
                    failures.push(format!("{}: {error}", command.program));
                }
            }
        }
        Err(CrabError::Internal(format!(
            "force-unmount failed: {}",
            failures.join("; ")
        )))
    }

    #[cfg(all(windows, feature = "nfs"))]
    {
        let target = crate::vfs::nfs_mount::windows_mount_target(mountpoint)?;
        let umount_exe = crate::vfs::nfs_mount::windows_system_command("umount.exe")?;
        let output = std::process::Command::new(umount_exe)
            .arg(&target)
            .output()
            .map_err(CrabError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(CrabError::Internal(format!(
            "force-unmount failed for {target}: stdout={stdout} stderr={stderr}"
        )))
    }

    #[cfg(all(windows, not(feature = "nfs")))]
    {
        let _ = mountpoint;
        Err(CrabError::Internal(
            "Windows NFS force-unmount requires the nfs feature".into(),
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = mountpoint;
        Err(CrabError::Internal(
            "force-unmount not supported on this platform".into(),
        ))
    }
}

#[cfg(windows)]
fn wait_for_pid_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !is_pid_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !is_pid_alive(pid)
}

#[cfg(windows)]
fn terminate_windows_process(pid: u32) -> Result<()> {
    let output = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .map_err(CrabError::Io)?;
    if output.status.success() || !is_pid_alive(pid) {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(CrabError::Internal(format!(
        "taskkill failed for PID {pid}: stdout={stdout} stderr={stderr}"
    )))
}

#[cfg(windows)]
fn windows_tasklist_pid_alive(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| windows_tasklist_csv_line_has_pid(line, pid))
}

#[cfg(any(windows, test))]
fn windows_tasklist_csv_line_has_pid(line: &str, pid: u32) -> bool {
    let mut fields = line.split(',');
    let _image = fields.next();
    let Some(pid_field) = fields.next() else {
        return false;
    };
    pid_field.trim().trim_matches('"') == pid.to_string()
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LinuxUnmountCommand {
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(any(target_os = "linux", test))]
fn linux_unmount_commands() -> &'static [LinuxUnmountCommand] {
    &[
        LinuxUnmountCommand {
            program: "fusermount3",
            args: &["-u"],
        },
        LinuxUnmountCommand {
            program: "fusermount",
            args: &["-u"],
        },
        LinuxUnmountCommand {
            program: "umount",
            args: &[],
        },
    ]
}

/// Remove the PID file from the crab directory.
fn clean_pid_file(crab_dir: &Path) {
    let pid_path = crab_dir.join("mount.pid");
    if pid_path.exists()
        && let Err(e) = std::fs::remove_file(&pid_path)
    {
        warn!(path = %pid_path.display(), error = %e, "failed to remove PID file");
    }
}

// ---------------------------------------------------------------------------
// Mount list command
// ---------------------------------------------------------------------------

/// Redact a source URL/path for display.
///
/// Shows the first 20 characters followed by `…` if the source is longer.
pub fn redact_source(source: &str) -> String {
    if source.len() <= 20 {
        source.to_owned()
    } else {
        let mut s = source[..20].to_owned();
        s.push('\u{2026}');
        s
    }
}

/// Strip the `refs/heads/` prefix from a git ref for short display.
pub fn short_ref(git_ref: &str) -> &str {
    git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref)
}

/// Format a duration in seconds as a human-friendly uptime string.
///
/// Examples: "< 1m", "5m", "2h 15m", "3d 1h"
pub fn format_uptime(seconds: u64) -> String {
    if seconds < 60 {
        return "< 1m".to_owned();
    }

    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if days > 0 {
        let remaining_hours = hours % 24;
        if remaining_hours > 0 {
            format!("{days}d {remaining_hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        let remaining_minutes = minutes % 60;
        if remaining_minutes > 0 {
            format!("{hours}h {remaining_minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{minutes}m")
    }
}

/// Compute uptime from an ISO 8601 start_time string to now.
///
/// Returns the duration in seconds, or `None` if the timestamp cannot be parsed.
fn compute_uptime_secs(start_time: &str) -> Option<u64> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let start_secs = parse_iso8601_to_unix(start_time)?;

    if now_secs >= start_secs {
        Some(now_secs - start_secs)
    } else {
        Some(0)
    }
}

/// Parse a simple ISO 8601 UTC timestamp (YYYY-MM-DDTHH:MM:SSZ) to Unix seconds.
///
/// Returns `None` if the format doesn't match.
fn parse_iso8601_to_unix(ts: &str) -> Option<u64> {
    if ts.len() != 20 || !ts.ends_with('Z') {
        return None;
    }

    let year: i32 = ts.get(0..4)?.parse().ok()?;
    let month: u32 = ts.get(5..7)?.parse().ok()?;
    let day: u32 = ts.get(8..10)?.parse().ok()?;
    let hour: u64 = ts.get(11..13)?.parse().ok()?;
    let minute: u64 = ts.get(14..16)?.parse().ok()?;
    let second: u64 = ts.get(17..19)?.parse().ok()?;

    let days = days_from_civil(year, month, day)?;

    let total_secs = (days as u64) * 86400 + hour * 3600 + minute * 60 + second;
    Some(total_secs)
}

/// Convert (year, month, day) to days since 1970-01-01.
/// Inverse of the Howard Hinnant algorithm used in mounts_registry.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let y = if month <= 2 {
        i64::from(year) - 1
    } else {
        i64::from(year)
    };
    let m = if month <= 2 { month + 9 } else { month - 3 };

    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + i64::from(doe) - 719_468;

    Some(days)
}

/// JSON representation of a mount entry for `--json` output.
#[derive(serde::Serialize)]
struct MountListEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    mountpoint: String,
    source: String,
    #[serde(rename = "ref")]
    git_ref: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    uptime: String,
    read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    control_endpoint: Option<String>,
}

/// Run the `crab mount list` subcommand.
///
/// Reads `mounts.json`, checks each PID for liveness, computes uptime,
/// and displays a formatted table or JSON array.
pub fn run_mount_list(json: bool) -> Result<()> {
    let registry_path = crate::vfs::mounts_registry::registry_path()?;
    let entries = crate::vfs::mounts_registry::list_entries(&registry_path)?
        .into_iter()
        .map(MountListEntry::from)
        .collect();
    emit_mount_list(entries, json)
}

/// Run mount list, preferring live backend control when available.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_list_live_or_persisted(json: bool) -> Result<()> {
    let entries = crate::vfs::mount_control::list()
        .await?
        .into_iter()
        .map(MountListEntry::from)
        .collect();
    emit_mount_list(entries, json)
}

fn emit_mount_list(list_entries: Vec<MountListEntry>, json: bool) -> Result<()> {
    if list_entries.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No active mounts.");
        }
        return Ok(());
    }

    if json {
        let output = serde_json::to_string_pretty(&list_entries)
            .map_err(|e| CrabError::Internal(format!("failed to serialize mount list: {e}")))?;
        println!("{output}");
    } else {
        // Print table header.
        println!(
            "{:<12} {:<7} {:<16} {:<22} {:<12} {:<8} {:<7} UPTIME",
            "NAME", "BACKEND", "MOUNTPOINT", "SOURCE", "REF", "STATE", "PID"
        );

        for entry in &list_entries {
            println!(
                "{:<12} {:<7} {:<16} {:<22} {:<12} {:<8} {:<7} {}",
                truncate_col(&entry.name, 12),
                truncate_col(entry.backend.as_deref().unwrap_or("-"), 7),
                truncate_col(&entry.mountpoint, 16),
                redact_source(&entry.source),
                truncate_col(&entry.git_ref, 12),
                entry.state,
                entry
                    .pid
                    .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
                entry.uptime,
            );
        }
    }

    Ok(())
}

fn redact_control_endpoint(endpoint: Option<String>) -> Option<String> {
    endpoint.map(|endpoint| display_control_endpoint(&endpoint))
}

impl From<crate::vfs::mounts_registry::MountEntry> for MountListEntry {
    fn from(entry: crate::vfs::mounts_registry::MountEntry) -> Self {
        let state = if is_pid_alive(entry.pid) {
            "running".to_owned()
        } else {
            "stale".to_owned()
        };
        let uptime = if state == "running" {
            compute_uptime_secs(&entry.start_time).map_or_else(|| "-".to_owned(), format_uptime)
        } else {
            "-".to_owned()
        };

        Self {
            name: entry.name,
            backend: entry.backend,
            mountpoint: entry.mountpoint,
            source: entry.source,
            git_ref: short_ref(&entry.git_ref).to_owned(),
            state,
            pid: Some(entry.pid),
            uptime,
            read_only: entry.read_only,
            start_time: Some(entry.start_time),
            log_path: entry.log_path,
            control_endpoint: redact_control_endpoint(entry.control_endpoint),
        }
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
impl From<crate::vfs::mount_control::MountControlListEntry> for MountListEntry {
    fn from(entry: crate::vfs::mount_control::MountControlListEntry) -> Self {
        let state = if entry.live || entry.pid.is_some_and(is_pid_alive) {
            "running".to_owned()
        } else {
            "stale".to_owned()
        };
        let uptime = if state == "running" {
            entry
                .start_time
                .as_deref()
                .and_then(compute_uptime_secs)
                .map_or_else(|| "-".to_owned(), format_uptime)
        } else {
            "-".to_owned()
        };

        Self {
            name: entry.name,
            backend: entry.backend,
            mountpoint: entry.mountpoint,
            source: entry.source,
            git_ref: short_ref(&entry.head_ref).to_owned(),
            state,
            pid: entry.pid,
            uptime,
            read_only: entry.read_only,
            start_time: entry.start_time,
            log_path: entry.log_path,
            control_endpoint: redact_control_endpoint(entry.control_endpoint),
        }
    }
}

// ---------------------------------------------------------------------------
// Mount clean command
// ---------------------------------------------------------------------------

/// Run the `crab mount clean` subcommand.
///
/// Identifies inactive cache directories under `~/.crab/mounts/repos/` by
/// cross-referencing with `mounts.json`, deletes them, and reports freed
/// disk space.
///
/// With `--all`: requires no active mounts, then deletes everything under
/// `~/.crab/mounts/` (repos/, cache/, mounts.json).
pub fn run_mount_clean(all: bool) -> Result<()> {
    let home = std::env::var("HOME").map_err(|_| CrabError::Configuration {
        key: "HOME environment variable not set".into(),
        origin: "mount clean".into(),
    })?;
    let mounts_base = PathBuf::from(&home).join(".crab").join("mounts");
    let repos_dir = mounts_base.join("repos");
    let registry_path = mounts_base.join("mounts.json");

    if all {
        return run_mount_clean_all(&mounts_base, &registry_path);
    }

    // List directories under ~/.crab/mounts/repos/
    let repo_dirs = list_repo_dirs(&repos_dir);
    if repo_dirs.is_empty() {
        println!("No mount caches found.");
        return Ok(());
    }

    // Read active mounts from mounts.json.
    let entries = crate::vfs::mounts_registry::read_entries(&registry_path).unwrap_or_default();

    // Compute the set of active cache hashes from mount sources.
    let active_hashes: std::collections::HashSet<String> = entries
        .iter()
        .map(|e| compute_source_hash(&e.source))
        .collect();

    // Identify inactive directories (those not matching any active hash).
    let inactive_dirs: Vec<PathBuf> = repo_dirs
        .into_iter()
        .filter(|dir| {
            let dir_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            !active_hashes.contains(&dir_name)
        })
        .collect();

    if inactive_dirs.is_empty() {
        println!("No inactive caches to clean.");
        return Ok(());
    }

    // Delete inactive directories and sum freed bytes.
    let mut freed_bytes: u64 = 0;
    let mut cleaned_count: usize = 0;

    for dir in &inactive_dirs {
        let dir_size = dir_size_bytes(dir);
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {
                info!(path = %dir.display(), size = dir_size, "removed inactive cache");
                freed_bytes += dir_size;
                cleaned_count += 1;
            }
            Err(e) => {
                warn!(path = %dir.display(), error = %e, "failed to remove cache directory");
            }
        }
    }

    let freed_mb = freed_bytes as f64 / (1024.0 * 1024.0);
    println!("Cleaned {cleaned_count} cache(s), freed {freed_mb:.1} MB.");

    Ok(())
}

/// Run `crab mount clean --all`: delete everything under `~/.crab/mounts/`.
///
/// Requires no active mounts (all entries in mounts.json must be stale or
/// the file must be empty/absent).
fn run_mount_clean_all(mounts_base: &Path, registry_path: &Path) -> Result<()> {
    // Check for active mounts.
    let entries = crate::vfs::mounts_registry::read_entries(registry_path).unwrap_or_default();

    let active_count = entries.iter().filter(|e| is_pid_alive(e.pid)).count();
    if active_count > 0 {
        return Err(CrabError::Internal(format!(
            "cannot clean all: {active_count} mount(s) still active. \
             Unmount them first with `crab unmount --all`."
        )));
    }

    if !mounts_base.exists() {
        println!("No mount data found.");
        return Ok(());
    }

    // Compute total size before deletion.
    let total_size = dir_size_bytes(mounts_base);

    // Delete everything under ~/.crab/mounts/ (repos/, cache/, mounts.json, etc.)
    // but preserve the directory itself.
    let mut cleaned = 0u64;
    if let Ok(entries) = std::fs::read_dir(mounts_base) {
        for entry in entries.flatten() {
            let path = entry.path();
            let size = dir_size_bytes(&path);
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => {
                    info!(path = %path.display(), "removed");
                    cleaned += size;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to remove");
                }
            }
        }
    }

    let freed_mb = cleaned as f64 / (1024.0 * 1024.0);
    let _ = total_size; // total_size used for logging context
    println!("Cleaned all mount data, freed {freed_mb:.1} MB.");

    Ok(())
}

/// Compute the cache hash for a source URL/path.
///
/// Mirrors the logic in `clone_cache::compute_cache_hash` — first 12 hex
/// chars of SHA-256 of the normalized source string. Duplicated here to
/// avoid a dependency on the `fuse` feature gate.
fn compute_source_hash(source: &str) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};

    let trimmed = source.trim();
    let normalized = if let Some((scheme, rest)) = trimmed.split_once("://") {
        let normalized_rest = rest.trim_end_matches('/');
        format!("{}://{}", scheme.to_ascii_lowercase(), normalized_rest)
    } else {
        trimmed.trim_end_matches('/').to_owned()
    };

    let hash = Sha256::digest(normalized.as_bytes());
    let mut out = String::with_capacity(12);
    for byte in &hash[..6] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// List all directories under the given path.
fn list_repo_dirs(repos_dir: &Path) -> Vec<PathBuf> {
    if !repos_dir.exists() {
        return Vec::new();
    }

    match std::fs::read_dir(repos_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .collect(),
        Err(e) => {
            warn!(path = %repos_dir.display(), error = %e, "failed to read repos directory");
            Vec::new()
        }
    }
}

/// Compute the total size of a directory tree in bytes.
fn dir_size_bytes(path: &Path) -> u64 {
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }

    walkdir_size(path)
}

/// Recursively sum file sizes in a directory.
fn walkdir_size(dir: &Path) -> u64 {
    let mut total: u64 = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += walkdir_size(&path);
        } else {
            total += path.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// Truncate a string to fit a column width, appending `\u{2026}` if needed.
fn truncate_col(s: &str, max_width: usize) -> String {
    if s.len() <= max_width {
        s.to_owned()
    } else {
        let mut truncated = s[..max_width.saturating_sub(1)].to_owned();
        truncated.push('\u{2026}');
        truncated
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Debug, Clone, Serialize)]
pub struct MountOverlayPayload {
    pub mountpoint: String,
    pub cache_dir: String,
    pub diff: crate::vfs::publish::OverlayDiff,
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Debug, Clone, Serialize)]
pub struct MountOverlayExportPayload {
    pub mountpoint: String,
    pub destination: String,
    pub diff: crate::vfs::publish::OverlayDiff,
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Debug, Clone, Serialize)]
pub struct MountOverlayCommitPayload {
    pub mountpoint: String,
    pub cache_dir: String,
    pub result: crate::vfs::publish::OverlayCommitResult,
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_diff(path: &Path, json: bool) -> Result<()> {
    let context = resolve_mount_overlay_context(path).await?;
    let diff = stable_overlay_diff(&context).await?;
    if json {
        emit_json(
            "mount.diff",
            "1.0",
            MountOverlayPayload {
                mountpoint: context.mountpoint,
                cache_dir: context.overlay_paths.cache_dir.display().to_string(),
                diff,
            },
        );
        return Ok(());
    }
    print_overlay_diff(&diff);
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_export(path: &Path, destination: &Path, json: bool) -> Result<()> {
    let context = resolve_mount_overlay_context(path).await?;
    let _ = stable_overlay_diff(&context).await?;
    let diff = crate::vfs::publish::export_overlay_from_view(
        &context.overlay_paths,
        destination,
        &context.mountpoint_path,
    )?;
    if json {
        emit_json(
            "mount.export",
            "1.0",
            MountOverlayExportPayload {
                mountpoint: context.mountpoint,
                destination: destination.display().to_string(),
                diff,
            },
        );
        return Ok(());
    }
    println!(
        "Exported {} overlay change(s) to {}.",
        diff.changes.len(),
        destination.display()
    );
    println!(
        "Estimated upload size: {}",
        format_cache_size(diff.estimated_upload_bytes)
    );
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_reset(path: &Path, overlay: bool, yes: bool, json: bool) -> Result<()> {
    if !overlay || !yes {
        return Err(CrabError::Configuration {
            key: "reset requires --overlay --yes to discard local overlay changes".into(),
            origin: "crab mount reset".into(),
        });
    }

    let context = resolve_mount_overlay_context(path).await?;
    let _ = stable_overlay_diff(&context).await?;
    let diff = reset_overlay_for_context(&context).await?;
    if json {
        emit_json(
            "mount.reset",
            "1.0",
            MountOverlayPayload {
                mountpoint: context.mountpoint,
                cache_dir: context.overlay_paths.cache_dir.display().to_string(),
                diff,
            },
        );
        return Ok(());
    }
    println!("Discarded {} overlay change(s).", diff.changes.len());
    Ok(())
}

#[cfg(feature = "fuse")]
async fn drain_live_overlay_reset(context: &MountOverlayContext) -> Result<()> {
    const CLEAN_DRAIN_PASSES: usize = 2;
    const MAX_DRAIN_PASSES: usize = 6;

    let mut clean_passes = 0;
    for pass in 1..=MAX_DRAIN_PASSES {
        flush_mount_writes(&context.mountpoint_path)?;
        let diff = crate::vfs::ipc_client::try_ipc_reset_overlay(&context.mountpoint).await?;
        if diff.changes.is_empty() {
            clean_passes += 1;
            if clean_passes >= CLEAN_DRAIN_PASSES {
                return Ok(());
            }
            continue;
        }
        clean_passes = 0;
        debug!(
            mountpoint = %context.mountpoint,
            pass,
            changes = diff.changes.len(),
            "discarded delayed FUSE writes after overlay reset"
        );
    }

    Err(CrabError::Internal(
        "overlay reset did not quiesce delayed FUSE writes".into(),
    ))
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn reset_overlay_for_context(
    context: &MountOverlayContext,
) -> Result<crate::vfs::publish::OverlayDiff> {
    if let Some(diff) = crate::vfs::mount_control::reset_overlay(&context.mountpoint_path).await? {
        return Ok(diff);
    }

    #[cfg(feature = "fuse")]
    {
        match crate::vfs::ipc_client::try_ipc_reset_overlay(&context.mountpoint).await {
            Ok(diff) => {
                drain_live_overlay_reset(context).await?;
                return Ok(diff);
            }
            Err(e) if context.invalidate_via_ipc => return Err(e.into()),
            Err(_) => {}
        }
    }

    let diff = crate::vfs::publish::reset_overlay(&context.overlay_paths)?;
    invalidate_after_overlay_reset(context, &diff).await?;
    Ok(diff)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_commit(path: &Path, message: &str, push: bool, json: bool) -> Result<()> {
    let context = resolve_mount_overlay_context(path).await?;
    if context.read_only {
        return Err(CrabError::Configuration {
            key: "cannot commit overlay for a read-only mount".into(),
            origin: "crab mount commit".into(),
        });
    }
    let _ = stable_overlay_diff(&context).await?;
    let (result, committed_via_live_control) =
        if let Some(result) = crate::vfs::mount_control::commit(path, message, push).await? {
            (result, true)
        } else {
            let git_dir = context
                .git_dir
                .clone()
                .ok_or_else(|| CrabError::Configuration {
                    key: "cannot resolve git directory for mounted repository".into(),
                    origin: "crab mount commit".into(),
                })?;
            let ref_name = context
                .ref_name
                .clone()
                .ok_or_else(|| CrabError::Configuration {
                    key: "cannot resolve tracked ref for mounted repository".into(),
                    origin: "crab mount commit".into(),
                })?;

            (
                crate::vfs::publish::commit_overlay(&crate::vfs::publish::OverlayCommitOptions {
                    cache_dir: context.overlay_paths.cache_dir.clone(),
                    git_dir,
                    ref_name,
                    message: message.to_owned(),
                    push,
                })?,
                false,
            )
        };
    #[cfg(feature = "fuse")]
    if !committed_via_live_control && context.invalidate_via_ipc && result.overlay_cleaned {
        drain_live_overlay_reset(&context).await?;
    }
    #[cfg(not(feature = "fuse"))]
    let _ = committed_via_live_control;

    if json {
        emit_json(
            "mount.commit",
            "1.0",
            MountOverlayCommitPayload {
                mountpoint: context.mountpoint,
                cache_dir: context.overlay_paths.cache_dir.display().to_string(),
                result,
            },
        );
        return Ok(());
    }

    print_overlay_commit(&result);
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn stable_overlay_diff(
    context: &MountOverlayContext,
) -> Result<crate::vfs::publish::OverlayDiff> {
    const STABLE_PASSES: usize = 3;
    const MAX_PASSES: usize = 40;
    const QUIESCE_DELAY: Duration = Duration::from_millis(50);

    let mut last_key = None;
    let mut stable_passes = 0;

    for pass in 1..=MAX_PASSES {
        let diff = overlay_diff_once(context).await?;
        let key = overlay_diff_key(&diff);
        if last_key.as_ref() == Some(&key) {
            stable_passes += 1;
        } else {
            last_key = Some(key);
            stable_passes = 1;
        }

        if stable_passes >= STABLE_PASSES {
            return Ok(diff);
        }

        debug!(
            mountpoint = %context.mountpoint,
            pass,
            "waiting for mounted overlay writes to quiesce"
        );
        tokio::time::sleep(QUIESCE_DELAY).await;
    }

    Err(CrabError::Internal(
        "mounted overlay writes did not quiesce".into(),
    ))
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn overlay_diff_once(
    context: &MountOverlayContext,
) -> Result<crate::vfs::publish::OverlayDiff> {
    flush_mount_writes(&context.mountpoint_path)?;
    if context.invalidate_via_ipc {
        #[cfg(feature = "fuse")]
        return Ok(crate::vfs::ipc_client::try_ipc_overlay_diff(&context.mountpoint).await?);
        #[cfg(not(feature = "fuse"))]
        return Err(CrabError::Internal(
            "mount IPC requested but FUSE IPC support is not compiled".into(),
        ));
    }
    Ok(crate::vfs::publish::inspect_overlay(
        &context.overlay_paths,
    )?)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn overlay_diff_key(diff: &crate::vfs::publish::OverlayDiff) -> Vec<String> {
    diff.changes
        .iter()
        .map(|change| {
            format!(
                "{}\0{}\0{}\0{}\0{}",
                change.path, change.kind, change.size_bytes, change.mode, change.has_backing_file
            )
        })
        .collect()
}

#[cfg(all(any(feature = "fuse", feature = "nfs"), target_os = "linux"))]
fn flush_mount_writes(mountpoint: &Path) -> Result<()> {
    let file = std::fs::File::open(mountpoint)?;
    // SAFETY: syncfs only borrows a valid open directory/file descriptor and
    // does not retain it after the call returns.
    let rc = unsafe { libc::syncfs(file.as_raw_fd()) };
    if rc == 0 {
        return Ok(());
    }
    Err(CrabError::Io(std::io::Error::last_os_error()))
}

#[cfg(all(any(feature = "fuse", feature = "nfs"), target_os = "macos"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps shared flush call sites fallible on every platform"
)]
fn flush_mount_writes(_mountpoint: &Path) -> Result<()> {
    // SAFETY: libc::sync has no arguments and only asks the OS to schedule a
    // global filesystem flush. macOS does not expose Linux syncfs.
    unsafe { libc::sync() };
    Ok(())
}

#[cfg(all(
    any(feature = "fuse", feature = "nfs"),
    not(any(target_os = "linux", target_os = "macos"))
))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps shared flush call sites fallible on every platform"
)]
fn flush_mount_writes(_mountpoint: &Path) -> Result<()> {
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
struct MountOverlayContext {
    mountpoint: String,
    mountpoint_path: PathBuf,
    overlay_paths: crate::vfs::publish::OverlayPaths,
    git_dir: Option<PathBuf>,
    ref_name: Option<String>,
    read_only: bool,
    invalidate_via_ipc: bool,
}

#[cfg(feature = "fuse")]
async fn invalidate_after_overlay_reset(
    context: &MountOverlayContext,
    diff: &crate::vfs::publish::OverlayDiff,
) -> Result<()> {
    if !context.invalidate_via_ipc {
        return Ok(());
    }
    let paths = diff
        .changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(());
    }
    Ok(crate::vfs::ipc_client::try_ipc_invalidate(&context.mountpoint, paths).await?)
}

#[cfg(all(not(feature = "fuse"), feature = "nfs"))]
async fn invalidate_after_overlay_reset(
    _context: &MountOverlayContext,
    _diff: &crate::vfs::publish::OverlayDiff,
) -> Result<()> {
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn resolve_mount_overlay_context(path: &Path) -> Result<MountOverlayContext> {
    let mountpoint = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mountpoint_str = mountpoint.display().to_string();

    let registry_path = crate::vfs::mounts_registry::registry_path()?;
    let entries = crate::vfs::mounts_registry::read_entries(&registry_path)?;

    if let Some(entry) = entries.iter().find(|e| e.mountpoint == mountpoint_str) {
        return mount_context_from_entry(entry.clone(), false, mountpoint_str, &mountpoint);
    }

    #[cfg(feature = "fuse")]
    {
        if let Some(status) = ipc_status_for_mountpoint(&mountpoint_str).await {
            return mount_context_from_ipc_status(status);
        }
    }

    let is_dot_path = path.as_os_str() == ".";
    let entry = if is_dot_path {
        match entries.as_slice() {
            [entry] => Some(entry.clone()),
            [] => None,
            _ => {
                return Err(CrabError::Configuration {
                    key: "ambiguous mountpoint: multiple mounts active".into(),
                    origin: "crab mount overlay".into(),
                });
            }
        }
    } else {
        entries.into_iter().find(|e| e.mountpoint == mountpoint_str)
    };

    if let Some(entry) = entry {
        return mount_context_from_entry(entry, is_dot_path, mountpoint_str, &mountpoint);
    }

    let cache_dir = resolve_status_cache_dir(None, &mountpoint);
    Ok(MountOverlayContext {
        mountpoint: mountpoint_str,
        mountpoint_path: mountpoint,
        overlay_paths: crate::vfs::publish::OverlayPaths::from_cache_dir(&cache_dir),
        git_dir: None,
        ref_name: None,
        read_only: false,
        invalidate_via_ipc: false,
    })
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn mount_context_from_entry(
    entry: crate::vfs::mounts_registry::MountEntry,
    is_dot_path: bool,
    mountpoint_str: String,
    mountpoint: &Path,
) -> Result<MountOverlayContext> {
    let cache_dir = resolve_status_cache_dir(Some(&entry), mountpoint);
    let git_dir = git_dir_for_mount_source(&entry.source, &cache_dir)?;
    let display_mountpoint = if is_dot_path {
        entry.mountpoint.clone()
    } else {
        mountpoint_str
    };
    let mountpoint_path = if is_dot_path {
        PathBuf::from(&entry.mountpoint)
    } else {
        mountpoint.to_path_buf()
    };

    Ok(MountOverlayContext {
        mountpoint: display_mountpoint,
        mountpoint_path,
        overlay_paths: crate::vfs::publish::OverlayPaths::from_cache_dir(&cache_dir),
        git_dir: Some(git_dir),
        ref_name: Some(entry.git_ref),
        read_only: entry.read_only,
        invalidate_via_ipc: false,
    })
}

#[cfg(feature = "fuse")]
fn mount_context_from_ipc_status(
    status: crate::vfs::ipc_server::MountStatus,
) -> Result<MountOverlayContext> {
    let cache_dir = cache_dir_for_mount_source(&status.remote)?;
    let git_dir = git_dir_for_mount_source(&status.remote, &cache_dir)?;
    let mountpoint_path = PathBuf::from(&status.mountpoint);
    Ok(MountOverlayContext {
        mountpoint: status.mountpoint,
        mountpoint_path,
        overlay_paths: crate::vfs::publish::OverlayPaths::from_cache_dir(&cache_dir),
        git_dir: Some(git_dir),
        ref_name: Some(status.r#ref),
        read_only: status.read_only,
        invalidate_via_ipc: true,
    })
}

#[cfg(feature = "fuse")]
async fn ipc_status_for_mountpoint(
    mountpoint: &str,
) -> Option<crate::vfs::ipc_server::MountStatus> {
    let socket_path = crate::vfs::ipc_client::default_socket_path().ok()?;
    let mut client = crate::vfs::ipc_client::IpcClient::connect(&socket_path)
        .await
        .ok()?;
    let request = crate::vfs::ipc_server::IpcRequest::Status {
        mountpoint: mountpoint.to_owned(),
    };
    let response = client.send(&request).await.ok()?;
    response.ok.then_some(response.status).flatten()
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn cache_dir_for_mount_source(source: &str) -> Result<PathBuf> {
    let hash = crate::vfs::clone_cache::compute_cache_hash(source);
    let home = std::env::var("HOME").map_err(|_| CrabError::Configuration {
        key: "HOME environment variable not set".into(),
        origin: "crab mount overlay".into(),
    })?;
    Ok(PathBuf::from(home)
        .join(".crab")
        .join("mounts")
        .join("repos")
        .join(hash))
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn git_dir_for_mount_source(source: &str, cache_dir: &Path) -> Result<PathBuf> {
    match crate::vfs::source::MountSource::parse(source)? {
        crate::vfs::source::MountSource::Local { path } => {
            Ok(crate::vfs::source::MountSource::validate_local(&path)?)
        }
        crate::vfs::source::MountSource::Remote { .. } => {
            let nested = cache_dir.join(".git");
            if nested.exists() {
                Ok(nested)
            } else {
                Ok(cache_dir.to_path_buf())
            }
        }
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn print_overlay_diff(diff: &crate::vfs::publish::OverlayDiff) {
    if diff.changes.is_empty() {
        println!("No overlay changes.");
        return;
    }

    println!("{:<10} {:>12} PATH", "KIND", "SIZE");
    for change in &diff.changes {
        println!(
            "{:<10} {:>12} {}",
            change.kind,
            format_cache_size(change.size_bytes),
            change.path
        );
    }
    println!(
        "\n{} change(s), estimated upload size {}.",
        diff.changes.len(),
        format_cache_size(diff.estimated_upload_bytes)
    );
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn print_overlay_commit(result: &crate::vfs::publish::OverlayCommitResult) {
    if result.diff.changes.is_empty() {
        if result.pushed
            && let Some(commit_oid) = result.commit_oid.as_deref()
        {
            println!("Pushed existing commit {commit_oid}.");
            return;
        }
        println!("No overlay changes to commit.");
        return;
    }

    let commit = result.commit_oid.as_deref().unwrap_or("(none)");
    println!(
        "Committed {} overlay change(s) as {commit}.",
        result.diff.changes.len()
    );
    if result.pushed {
        println!("Pushed commit to origin.");
    }
    println!(
        "Estimated upload size: {}",
        format_cache_size(result.diff.estimated_upload_bytes)
    );
}

// ---------------------------------------------------------------------------
// Mount status command
// ---------------------------------------------------------------------------

/// Run the `crab mount status` subcommand.
///
/// Reports mount state, HEAD OID, tracked ref, mode, overlay dirty count,
/// cache size, last refresh, and uptime. Reads mount metadata from
/// `mounts.json` first, then reads persisted state from snapshot.sqlite
/// and overlay.db.
///
/// When `verbose` is true, lists individual dirty paths.
/// When `json` is true, outputs all fields as a JSON object.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub fn run_mount_status(path: &Path, verbose: bool, json: bool) -> Result<()> {
    let context = resolve_mount_status_context(path)?;

    let status = build_mount_status(
        context.entry.as_ref(),
        &context.cache_dir,
        &context.display_mountpoint,
        verbose,
    );

    emit_mount_status(&status, verbose, json)
}

/// Run mount status, preferring live helper identity when available.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_mount_status_live_or_persisted(
    path: &Path,
    verbose: bool,
    json: bool,
    live_only: bool,
) -> Result<()> {
    let context = resolve_mount_status_context(path)?;
    let live_path = PathBuf::from(&context.display_mountpoint);

    if let Ok(Some(live)) = crate::vfs::mount_control::status(&live_path).await {
        let status = build_mount_status_from_control(
            live,
            context.entry.as_ref(),
            &context.cache_dir,
            verbose,
        )?;
        return emit_mount_status(&status, verbose, json);
    }

    if live_only {
        return Err(CrabError::Configuration {
            key: format!(
                "live mount control is unavailable for {}; use `crab mount status` without --live-only for persisted fallback",
                context.display_mountpoint
            ),
            origin: "crab mount status --live-only".into(),
        });
    }

    let status = build_mount_status(
        context.entry.as_ref(),
        &context.cache_dir,
        &context.display_mountpoint,
        verbose,
    );
    emit_mount_status(&status, verbose, json)
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
struct MountStatusContext {
    entry: Option<crate::vfs::mounts_registry::MountEntry>,
    cache_dir: PathBuf,
    display_mountpoint: String,
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn resolve_mount_status_context(path: &Path) -> Result<MountStatusContext> {
    let mountpoint = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mountpoint_str = mountpoint.display().to_string();

    let registry_path = crate::vfs::mounts_registry::registry_path()?;
    let entries = crate::vfs::mounts_registry::read_entries(&registry_path)?;

    let is_dot_path = path.as_os_str() == ".";
    let entry = if is_dot_path {
        match entries.as_slice() {
            [entry] => Some(entry.clone()),
            [] => None,
            _ => {
                eprintln!("Multiple mounts are active. Specify which mount with --mountpoint:");
                for e in &entries {
                    eprintln!("  {}", e.mountpoint);
                }
                return Err(CrabError::Configuration {
                    key: "ambiguous mountpoint: multiple mounts active".into(),
                    origin: "crab mount status".into(),
                });
            }
        }
    } else {
        entries.into_iter().find(|e| e.mountpoint == mountpoint_str)
    };

    let cache_dir = resolve_status_cache_dir(entry.as_ref(), &mountpoint);
    let display_mountpoint = if is_dot_path {
        entry
            .as_ref()
            .map_or_else(|| mountpoint_str.clone(), |e| e.mountpoint.clone())
    } else {
        mountpoint_str.clone()
    };

    Ok(MountStatusContext {
        entry,
        cache_dir,
        display_mountpoint,
    })
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn emit_mount_status(status: &MountStatusPayload, verbose: bool, json: bool) -> Result<()> {
    if json {
        let output = serde_json::to_string_pretty(&status)
            .map_err(|e| CrabError::Internal(format!("failed to serialize mount status: {e}")))?;
        println!("{output}");
        return Ok(());
    }

    print_mount_status_display(status, verbose);

    Ok(())
}

/// JSON-serializable mount status payload.
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Debug, Clone, Serialize)]
struct MountStatusPayload {
    pub mountpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "ref")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub mode: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub overlay_dirty_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_dirty_paths: Option<Vec<String>>,
    pub cache_size_bytes: u64,
    pub cache_size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_relative: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    pub read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_endpoint: Option<String>,
    #[cfg(feature = "nfs")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nfs_runtime: Option<crate::vfs::nfs_control::NfsRuntimeStatus>,
}

/// Resolve the cache directory for a mount, either from the registry entry
/// or by searching common locations.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn resolve_status_cache_dir(
    entry: Option<&crate::vfs::mounts_registry::MountEntry>,
    mountpoint: &Path,
) -> PathBuf {
    if let Some(e) = entry {
        // Compute cache dir from the source (same logic as mount pipeline).
        let hash = crate::vfs::clone_cache::compute_cache_hash(&e.source);
        if let Ok(home) = std::env::var("HOME") {
            let dir = PathBuf::from(home)
                .join(".crab")
                .join("mounts")
                .join("repos")
                .join(&hash);
            if dir.exists() {
                return dir;
            }
        }
    }

    // Fallback: look for .crab directory relative to mountpoint.
    find_crab_dir(mountpoint)
}

/// Build the full mount status payload by reading persisted state.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn build_mount_status(
    entry: Option<&crate::vfs::mounts_registry::MountEntry>,
    cache_dir: &Path,
    mountpoint_str: &str,
    verbose: bool,
) -> MountStatusPayload {
    // Determine process state.
    let (state, pid) = determine_mount_state(entry, cache_dir);

    // Read snapshot state (HEAD OID, ref).
    let (head_oid, snapshot_ref) = read_snapshot_state(cache_dir);

    // Determine mode (read-only / read-write).
    let (mode, read_only) = determine_mode(entry, cache_dir);

    // Read overlay dirty count.
    let (overlay_dirty_count, overlay_dirty_paths) = read_overlay_state(cache_dir, verbose);

    // Compute cache size (walk chunks directory).
    let cache_size_bytes = compute_cache_size(cache_dir);
    let cache_size_human = format_cache_size(cache_size_bytes);

    // Last refresh: use mtime of snapshot.sqlite as proxy.
    let (last_refresh, last_refresh_relative) = read_last_refresh(cache_dir);

    // Uptime from start_time.
    let uptime = entry
        .and_then(|e| compute_uptime_secs(&e.start_time))
        .map(format_uptime);

    // Source and ref from registry entry, falling back to snapshot data.
    let source = entry.map(|e| e.source.clone());
    let backend = entry.and_then(|e| e.backend.clone());
    let git_ref = entry
        .map(|e| short_ref(&e.git_ref).to_owned())
        .or(snapshot_ref);
    let name = entry.map(|e| e.name.clone());
    let start_time = entry.map(|e| e.start_time.clone());
    let log_path = entry.and_then(|e| e.log_path.clone());
    let control_endpoint = entry
        .and_then(|e| e.control_endpoint.clone())
        .map(|endpoint| display_control_endpoint(&endpoint));

    MountStatusPayload {
        mountpoint: mountpoint_str.to_owned(),
        backend,
        source,
        git_ref,
        head_oid,
        mode,
        state,
        pid,
        overlay_dirty_count,
        overlay_dirty_paths: if verbose {
            Some(overlay_dirty_paths)
        } else {
            None
        },
        cache_size_bytes,
        cache_size_human,
        last_refresh,
        last_refresh_relative,
        uptime,
        start_time,
        read_only,
        name,
        log_path,
        control_endpoint,
        #[cfg(feature = "nfs")]
        nfs_runtime: None,
    }
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn build_mount_status_from_control(
    status: crate::vfs::mount_control::MountControlStatus,
    entry: Option<&crate::vfs::mounts_registry::MountEntry>,
    fallback_cache_dir: &Path,
    verbose: bool,
) -> Result<MountStatusPayload> {
    let cache_dir = status
        .source
        .as_deref()
        .map(cache_dir_for_mount_source)
        .transpose()?
        .unwrap_or_else(|| fallback_cache_dir.to_path_buf());
    let (head_oid, snapshot_ref) = read_snapshot_state(&cache_dir);
    let (overlay_dirty_count, overlay_dirty_paths) = read_overlay_state(&cache_dir, verbose);
    let cache_size_bytes = compute_cache_size(&cache_dir);
    let (last_refresh, last_refresh_relative) = read_last_refresh(&cache_dir);
    let mode = if status.read_only {
        "read-only"
    } else {
        "read-write"
    };
    let git_ref = status
        .head_ref
        .as_deref()
        .filter(|head_ref| !head_ref.is_empty())
        .map(|head_ref| short_ref(head_ref).to_owned())
        .or(snapshot_ref);
    let state = status.pid.map_or_else(
        || "running".to_owned(),
        |pid| format!("running (PID {pid})"),
    );

    Ok(MountStatusPayload {
        mountpoint: status.mountpoint.display().to_string(),
        backend: Some(status.backend.as_str().to_owned()),
        source: status.source.or_else(|| entry.map(|e| e.source.clone())),
        git_ref,
        head_oid: status.head_oid.or(head_oid),
        mode: mode.to_owned(),
        state,
        pid: status.pid,
        overlay_dirty_count,
        overlay_dirty_paths: if verbose {
            Some(overlay_dirty_paths)
        } else {
            None
        },
        cache_size_bytes,
        cache_size_human: format_cache_size(cache_size_bytes),
        last_refresh,
        last_refresh_relative,
        uptime: entry
            .and_then(|e| compute_uptime_secs(&e.start_time))
            .map(format_uptime),
        start_time: entry.map(|e| e.start_time.clone()),
        read_only: status.read_only,
        name: entry.map(|e| e.name.clone()),
        log_path: entry.and_then(|e| e.log_path.clone()),
        control_endpoint: entry
            .and_then(|e| e.control_endpoint.clone())
            .map(|endpoint| display_control_endpoint(&endpoint)),
        #[cfg(feature = "nfs")]
        nfs_runtime: status.nfs_runtime,
    })
}

/// Determine the mount process state from registry entry or PID file.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn determine_mount_state(
    entry: Option<&crate::vfs::mounts_registry::MountEntry>,
    _cache_dir: &Path,
) -> (String, Option<u32>) {
    // Try registry entry first.
    if let Some(e) = entry {
        if is_pid_alive(e.pid) {
            return (format!("running (PID {})", e.pid), Some(e.pid));
        }
        return (format!("stale (PID {} not running)", e.pid), Some(e.pid));
    }

    #[cfg(feature = "fuse")]
    {
        // Fall back to PID file in cache dir.
        if let Some(pid) = crate::vfs::mount::read_pid_file(_cache_dir) {
            if is_pid_alive(pid) {
                return (format!("running (PID {pid})"), Some(pid));
            }
            return (format!("stale (PID {pid} not running)"), Some(pid));
        }
    }

    ("not mounted".to_owned(), None)
}

/// Read HEAD OID and ref name from snapshot.sqlite.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn read_snapshot_state(cache_dir: &Path) -> (Option<String>, Option<String>) {
    let snapshot_db = cache_dir.join("snapshot.sqlite");
    if !snapshot_db.exists() {
        return (None, None);
    }

    match crate::vfs::snapshot::SnapshotStore::open_or_create(&snapshot_db) {
        Ok(store) => {
            let oid = store.head_oid().ok().flatten();
            let ref_name = store
                .ref_name()
                .ok()
                .flatten()
                .map(|r| short_ref(&r).to_owned());
            (oid, ref_name)
        }
        Err(_) => (None, None),
    }
}

/// Determine mount mode from registry entry or overlay.db presence.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn determine_mode(
    entry: Option<&crate::vfs::mounts_registry::MountEntry>,
    cache_dir: &Path,
) -> (String, bool) {
    if let Some(e) = entry {
        if e.read_only {
            return ("read-only".to_owned(), true);
        }
        return ("read-write".to_owned(), false);
    }

    // Fall back to checking overlay.db existence.
    let overlay_db = cache_dir.join("overlay.db");
    if overlay_db.exists() {
        ("read-write".to_owned(), false)
    } else {
        ("read-only".to_owned(), true)
    }
}

/// Read overlay dirty count and optionally dirty paths.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn read_overlay_state(cache_dir: &Path, include_paths: bool) -> (i64, Vec<String>) {
    let overlay_db = cache_dir.join("overlay.db");
    crate::vfs::overlay::OverlayStore::read_dirty_state(&overlay_db, include_paths)
        .unwrap_or_default()
}

/// Compute cache size by walking the chunks directory under the cache root.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn compute_cache_size(cache_dir: &Path) -> u64 {
    // Try the chunks subdirectory in the cache dir first.
    let chunks_dir = cache_dir.join("chunks");
    if chunks_dir.exists() {
        return dir_size_recursive(&chunks_dir);
    }

    // Fall back to the global cache directory.
    let global_cache = crate::cache::default_cache_root();
    let global_chunks = global_cache.join("chunks");
    if global_chunks.exists() {
        return dir_size_recursive(&global_chunks);
    }

    0
}

/// Compute total size of all files in a directory, recursively.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn dir_size_recursive(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }

    total
}

/// Format cache size as a human-readable string.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub fn format_cache_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Read last refresh time from snapshot.sqlite mtime.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn read_last_refresh(cache_dir: &Path) -> (Option<String>, Option<String>) {
    let snapshot_db = cache_dir.join("snapshot.sqlite");
    if !snapshot_db.exists() {
        return (None, None);
    }

    let Ok(meta) = std::fs::metadata(&snapshot_db) else {
        return (None, None);
    };

    let Ok(modified) = meta.modified() else {
        return (None, None);
    };

    let now = std::time::SystemTime::now();
    let elapsed_secs = now.duration_since(modified).unwrap_or_default().as_secs();

    let relative = format_relative_time(elapsed_secs);

    // Compute absolute timestamp from mtime.
    let mtime_secs = modified
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let absolute = crate::vfs::mounts_registry::format_unix_timestamp(mtime_secs);

    (Some(absolute), Some(relative))
}

/// Format seconds elapsed as a relative time string (e.g. "2 minutes ago").
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub fn format_relative_time(seconds: u64) -> String {
    if seconds < 60 {
        return "just now".to_owned();
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return if minutes == 1 {
            "1 minute ago".to_owned()
        } else {
            format!("{minutes} minutes ago")
        };
    }

    let hours = minutes / 60;
    if hours < 24 {
        return if hours == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{hours} hours ago")
        };
    }

    let days = hours / 24;
    if days == 1 {
        "1 day ago".to_owned()
    } else {
        format!("{days} days ago")
    }
}

/// Print the formatted mount status display.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn print_mount_status_display(status: &MountStatusPayload, verbose: bool) {
    println!("Mount: {}", status.mountpoint);

    if let Some(ref backend) = status.backend {
        println!("Backend: {backend}");
    }

    if let Some(ref source) = status.source {
        println!("Source: {source}");
    }

    if let Some(ref git_ref) = status.git_ref {
        println!("Ref: {git_ref}");
    }

    if let Some(ref oid) = status.head_oid {
        println!("HEAD: {oid}");
    } else {
        println!("HEAD: (none)");
    }

    println!("Mode: {}", status.mode);
    println!("State: {}", status.state);

    if status.overlay_dirty_count == 0 {
        println!("Overlay: clean");
    } else {
        println!("Overlay: {} dirty entries", status.overlay_dirty_count);
        if verbose && let Some(ref paths) = status.overlay_dirty_paths {
            for path in paths {
                println!("  {path}");
            }
        }
    }

    println!("Cache: {}", status.cache_size_human);

    if let Some(ref relative) = status.last_refresh_relative {
        println!("Last refresh: {relative}");
    } else {
        println!("Last refresh: (unknown)");
    }

    if let Some(ref uptime) = status.uptime {
        println!("Uptime: {uptime}");
    }

    if let Some(ref log_path) = status.log_path {
        println!("Logs: {log_path}");
    }

    if let Some(ref control_endpoint) = status.control_endpoint {
        println!("Control: {}", display_control_endpoint(control_endpoint));
    }

    #[cfg(feature = "nfs")]
    if let Some(ref runtime) = status.nfs_runtime {
        println!(
            "NFS lifecycle: {} ms startup ({} ms server bind, {} ms native mount)",
            runtime.lifecycle.startup_ms,
            runtime.lifecycle.server_bind_ms,
            runtime.lifecycle.native_mount_ms
        );
        println!(
            "NFS read leases: {} entries, {} hits, {} misses, {} stale retries",
            runtime.read_leases.entries,
            runtime.read_leases.hits,
            runtime.read_leases.misses,
            runtime.read_leases.stale_retries
        );
        println!(
            "NFS directory cache: {} entries, {} hits, {} misses, {} stale evictions",
            runtime.directory_pages.entries,
            runtime.directory_pages.hits,
            runtime.directory_pages.misses,
            runtime.directory_pages.stale_evictions
        );
        println!(
            "NFS writes: {} pending, {} sync errors",
            runtime.write_journal.pending_paths, runtime.write_journal.paths_with_sync_errors
        );
        if runtime.write_journal.sync_attempts > 0 {
            let last_sync_latency = runtime
                .write_journal
                .last_sync_latency_ms
                .map_or_else(|| "unknown".to_owned(), |latency| format!("{latency} ms"));
            let max_sync_latency = runtime
                .write_journal
                .max_sync_latency_ms
                .map_or_else(|| "unknown".to_owned(), |latency| format!("{latency} ms"));
            println!(
                "NFS write sync: {} attempts, {} ok, {} failed, {} last, {} max",
                runtime.write_journal.sync_attempts,
                runtime.write_journal.sync_successes,
                runtime.write_journal.sync_failures,
                last_sync_latency,
                max_sync_latency
            );
        }
        println!(
            "NFS reads: {} RPCs, {} requested bytes, {} returned bytes",
            runtime.protocol.read_rpcs,
            runtime.protocol.read_requested_bytes,
            runtime.protocol.read_returned_bytes
        );
        println!(
            "NFS directories: {} READDIRPLUS RPCs, {} entries, {} prefetch paths",
            runtime.protocol.readdirplus_rpcs,
            runtime.protocol.readdirplus_entries,
            runtime.protocol.readdirplus_prefetch_paths
        );
        println!(
            "NFS directory work: {} materialized, {} attr resolutions, {} skipped by cookie, {} large dirs",
            runtime.protocol.readdirplus_materialized_entries,
            runtime.protocol.readdirplus_attr_resolutions,
            runtime.protocol.readdirplus_skipped_entries,
            runtime.protocol.readdirplus_large_dirs
        );
        println!(
            "NFS directory resumes: {} resumed, {} cookie misses, {} prefetch errors",
            runtime.protocol.readdirplus_cookie_resumes,
            runtime.protocol.readdirplus_cookie_misses,
            runtime.protocol.readdirplus_prefetch_errors
        );
        println!(
            "VFS reads: {} opens, {} reads, {} returned bytes",
            runtime.vfs.open_read_calls, runtime.vfs.read_at_calls, runtime.vfs.returned_bytes
        );
        println!(
            "VFS source cache: {}/{} entries, {}/{} bytes, {} hits, {} resolver calls avoided, {} misses, {} stale evictions, {} invalidations",
            runtime.vfs.source_cache_entries,
            runtime.vfs.source_cache_max_entries,
            runtime.vfs.source_cache_estimated_bytes,
            runtime.vfs.source_cache_max_estimated_bytes,
            runtime.vfs.source_cache_hits,
            runtime.vfs.resolver_calls_avoided,
            runtime.vfs.source_cache_misses,
            runtime.vfs.source_cache_stale_evictions,
            runtime.vfs.source_cache_invalidations
        );
        println!(
            "VFS invalidations: {} path, {} subtree, {} rename, {} generation, {} reset, {} compacted",
            runtime.vfs.invalidation_path_events,
            runtime.vfs.invalidation_subtree_events,
            runtime.vfs.invalidation_rename_events,
            runtime.vfs.invalidation_generation_events,
            runtime.vfs.invalidation_overlay_reset_events,
            runtime.vfs.invalidation_compacted_full_resets
        );
        println!(
            "VFS sources: {} pointer reads, {} blob reads, {} overlay reads",
            runtime.vfs.base_pointer.reads,
            runtime.vfs.base_blob.reads,
            runtime.vfs.overlay_file.reads
        );
        println!(
            "VFS adaptive: {} sequential, {} strided, {} repeated, {} random",
            runtime.vfs.base_pointer.adaptive.sequential
                + runtime.vfs.base_blob.adaptive.sequential
                + runtime.vfs.base_empty.adaptive.sequential
                + runtime.vfs.overlay_file.adaptive.sequential,
            runtime.vfs.base_pointer.adaptive.strided
                + runtime.vfs.base_blob.adaptive.strided
                + runtime.vfs.base_empty.adaptive.strided
                + runtime.vfs.overlay_file.adaptive.strided,
            runtime.vfs.base_pointer.adaptive.repeated
                + runtime.vfs.base_blob.adaptive.repeated
                + runtime.vfs.base_empty.adaptive.repeated
                + runtime.vfs.overlay_file.adaptive.repeated,
            runtime.vfs.base_pointer.adaptive.random
                + runtime.vfs.base_blob.adaptive.random
                + runtime.vfs.base_empty.adaptive.random
                + runtime.vfs.overlay_file.adaptive.random
        );
        println!(
            "Hydration windows: {} hits, {} misses, {} inflight waits, {} remote bytes",
            runtime.hydration.read_window_cache_hits,
            runtime.hydration.read_window_cache_misses,
            runtime.hydration.read_window_inflight_waits,
            runtime.hydration.read_window_remote_bytes
        );
        println!(
            "Hydration prefetch: {} requested, {} scheduled, {} skipped, {} errors",
            runtime.hydration.read_window_prefetch_requests,
            runtime.hydration.read_window_prefetch_scheduled,
            runtime.hydration.read_window_prefetch_skipped,
            runtime.hydration.read_window_prefetch_errors
        );
    }
}

fn display_control_endpoint(endpoint: &str) -> String {
    let Some(value) = endpoint.strip_prefix("tcp:") else {
        return endpoint.to_owned();
    };
    let Some((addr, _token)) = value.split_once("?token=") else {
        return endpoint.to_owned();
    };
    format!("tcp:{addr}?token=<redacted>")
}

/// Payload emitted by `crab daemon list --json`.
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Serialize, schemars::JsonSchema)]
pub struct DaemonListPayload {
    pub repos: Vec<crate::vfs::daemon::RepoStatus>,
}

/// Payload emitted by `crab daemon status --json`.
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Serialize, schemars::JsonSchema)]
pub struct DaemonStatusPayload(pub crate::vfs::daemon::RepoStatus);

/// Payload emitted by `crab daemon commit --json`.
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Serialize, schemars::JsonSchema)]
pub struct DaemonCommitPayload {
    pub name: String,
    pub mountpoint: String,
    pub result: crate::vfs::publish::OverlayCommitResult,
}

/// Actions for the `crab daemon` subcommand.
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[derive(Debug)]
pub enum DaemonAction {
    AddRepo {
        name: String,
        remote: String,
        branch: String,
        mount_root: String,
        backend: crate::vfs::daemon::DaemonMountBackend,
    },
    RemoveRepo {
        name: String,
    },
    List {
        mode: OutputMode,
    },
    Status {
        name: String,
        mode: OutputMode,
    },
    SetRefresh {
        name: String,
        interval_secs: u64,
    },
    Remount {
        name: String,
        clean_overlay: bool,
    },
    Fetch {
        name: String,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Commit {
        name: String,
        message: String,
        push: bool,
        mode: OutputMode,
    },
}

/// Run the daemon command.
///
/// With no subcommand: start the daemon and block until SIGINT.
/// With a subcommand: execute the action against the registry and exit.
#[cfg(any(feature = "fuse", feature = "nfs"))]
pub async fn run_daemon(
    action: Option<DaemonAction>,
    root: Option<PathBuf>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let daemon_root = resolve_daemon_root(root);

    match action {
        None => run_daemon_foreground(&daemon_root, cancel).await,
        Some(DaemonAction::AddRepo {
            name,
            remote,
            branch,
            mount_root,
            backend,
        }) => run_daemon_add_repo(&daemon_root, &name, &remote, &branch, &mount_root, backend),
        Some(DaemonAction::RemoveRepo { name }) => {
            run_daemon_remove_repo(&daemon_root, &name).await
        }
        Some(DaemonAction::List { mode }) => run_daemon_list(&daemon_root, mode),
        Some(DaemonAction::Status { name, mode }) => {
            run_daemon_status(&daemon_root, &name, mode).await
        }
        Some(DaemonAction::SetRefresh {
            name,
            interval_secs,
        }) => run_daemon_set_refresh(&daemon_root, &name, interval_secs),
        Some(DaemonAction::Remount {
            name,
            clean_overlay,
        }) => {
            if clean_overlay {
                let repo_data_dir = daemon_root.join("repos").join(&name);
                let overlay_db = repo_data_dir.join("overlay.db");
                let upper_dir = repo_data_dir.join("overlay/upper");
                if let Err(e) = crate::vfs::overlay::OverlayStore::clean(&overlay_db, &upper_dir) {
                    warn!(repo = %name, error = %e, "failed to clean overlay before remount");
                } else {
                    info!(repo = %name, "overlay cleaned before remount");
                }
            }
            run_daemon_remount(&daemon_root, &name, cancel).await
        }
        Some(DaemonAction::Fetch { name }) => run_daemon_fetch(&daemon_root, &name, cancel).await,
        Some(DaemonAction::Enable { name }) => run_daemon_enable(&daemon_root, &name),
        Some(DaemonAction::Disable { name }) => run_daemon_disable(&daemon_root, &name),
        Some(DaemonAction::Commit {
            name,
            message,
            push,
            mode,
        }) => run_daemon_commit(&daemon_root, &name, &message, push, mode).await,
    }
}

/// Resolve the daemon root directory.
///
/// Uses the `--root` flag if provided, otherwise defaults to
/// `~/.crab/daemon`.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn resolve_daemon_root(root: Option<PathBuf>) -> PathBuf {
    if let Some(r) = root {
        return r;
    }
    match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join(".crab/daemon"),
        None => PathBuf::from(".crab/daemon"),
    }
}

/// Start the daemon in foreground mode, blocking until SIGINT.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn run_daemon_foreground(
    daemon_root: &Path,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::vfs::daemon::DaemonService;

    info!(root = %daemon_root.display(), "starting daemon");

    let daemon = DaemonService::new(daemon_root.to_path_buf(), cancel.clone())?
        .with_read_resolver(mount_read_resolver());
    let repos = daemon.list_repos().await?;
    println!("Daemon started with {} registered repo(s).", repos.len());
    for status in &repos {
        println!("  {status}");
    }
    println!("Press Ctrl+C to stop.");

    // Install signal handler for graceful shutdown.
    #[cfg(feature = "fuse")]
    crate::vfs::mount::install_signal_handler(cancel.clone(), &tokio::runtime::Handle::current());
    #[cfg(all(not(feature = "fuse"), feature = "nfs"))]
    crate::vfs::nfs_mount::install_signal_handler(cancel.clone());

    daemon.run().await?;
    println!("Daemon stopped.");
    Ok(())
}

/// Register a repo with the daemon.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn run_daemon_add_repo(
    daemon_root: &Path,
    name: &str,
    remote: &str,
    branch: &str,
    mount_root: &str,
    backend: crate::vfs::daemon::DaemonMountBackend,
) -> Result<()> {
    use crate::vfs::daemon::{Registry, RepoConfig};

    #[cfg(not(feature = "nfs"))]
    if backend == crate::vfs::daemon::DaemonMountBackend::Nfs {
        return Err(CrabError::Configuration {
            key: "NFS support was not compiled into this Crab build".into(),
            origin: "crab daemon add-repo --backend=nfs".into(),
        });
    }
    #[cfg(not(feature = "fuse"))]
    if backend == crate::vfs::daemon::DaemonMountBackend::Fuse {
        return Err(CrabError::Configuration {
            key: "FUSE support was not compiled into this Crab build".into(),
            origin: "crab daemon add-repo --backend=fuse".into(),
        });
    }

    let config = RepoConfig {
        name: name.to_owned(),
        remote: remote.to_owned(),
        remote_redacted: crate::vfs::refresh::redact_url(remote),
        branch: branch.to_owned(),
        mount_root: mount_root.to_owned(),
        refresh_interval_secs: 30,
        enabled: true,
        read_only: false,
        backend,
    };

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;
    let duplicate = registry.list_repos()?.into_iter().find(|existing| {
        existing.name != name
            && existing.remote == remote
            && existing.branch == branch
            && existing.backend == backend
    });
    registry.add_repo(&config)?;

    println!("Registered repo '{name}' (remote: {remote}, branch: {branch}, backend: {backend}).");
    println!("Mount root: {mount_root}/{name}/");
    if let Some(existing) = duplicate {
        eprintln!(
            "Warning: repo '{}' already tracks this remote, branch, and backend; each name keeps an independent clone, snapshot, overlay, and mount.",
            existing.name
        );
    }
    Ok(())
}

/// Remove a repo from the daemon.
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[expect(
    clippy::unused_async,
    reason = "async signature for consistent daemon command API"
)]
async fn run_daemon_remove_repo(daemon_root: &Path, name: &str) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;

    let removed = registry.remove_repo(name)?;
    if removed {
        println!("Removed repo '{name}'.");
    } else {
        eprintln!("Repo '{name}' not found in registry.");
        return Err(CrabError::NotFound {
            path: format!("repo '{name}'"),
        });
    }
    Ok(())
}

/// List all registered repos.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn run_daemon_list(daemon_root: &Path, mode: OutputMode) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;
    let repos = registry.list_repos()?;

    if mode == OutputMode::Json {
        let statuses: Vec<crate::vfs::daemon::RepoStatus> = repos
            .iter()
            .map(|config| crate::vfs::daemon::read_persisted_status(config, daemon_root))
            .collect();
        let payload = DaemonListPayload { repos: statuses };
        emit_json("daemon.list", "1.0", payload);
        return Ok(());
    }

    if repos.is_empty() {
        println!("No repos registered.");
        return Ok(());
    }

    println!(
        "{:<20} {:<40} {:<15} {:<10} {:<8} {:<7} {:<5}",
        "NAME", "REMOTE", "BRANCH", "REFRESH", "ENABLED", "BACKEND", "MODE",
    );
    println!("{}", "-".repeat(106));
    for repo in &repos {
        let enabled = if repo.enabled { "yes" } else { "no" };
        let mode = if repo.read_only { "ro" } else { "rw" };
        println!(
            "{:<20} {:<40} {:<15} {:<10} {:<8} {:<7} {:<5}",
            repo.name,
            if repo.remote_redacted.is_empty() {
                &repo.remote
            } else {
                &repo.remote_redacted
            },
            repo.branch,
            format!("{}s", repo.refresh_interval_secs),
            enabled,
            repo.backend,
            mode,
        );
    }
    Ok(())
}

/// Show detailed status for a single repo.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn run_daemon_status(daemon_root: &Path, name: &str, mode: OutputMode) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;

    let config = registry
        .get_repo(name)?
        .ok_or_else(|| CrabError::NotFound {
            path: format!("repo '{name}'"),
        })?;

    let status = crate::vfs::daemon::read_status(&config, daemon_root).await;
    if mode == OutputMode::Json {
        let payload = DaemonStatusPayload(status);
        emit_json("daemon.status", "1.0", payload);
        return Ok(());
    }

    let rw_mode = if config.read_only {
        "read-only"
    } else {
        "read-write"
    };
    let enabled = if config.enabled { "yes" } else { "no" };

    println!("Repo: {}", config.name);
    println!(
        "  Remote:          {}",
        if config.remote_redacted.is_empty() {
            &config.remote
        } else {
            &config.remote_redacted
        }
    );
    println!("  Branch:          {}", config.branch);
    println!("  Mount root:      {}", config.mount_root);
    println!("  Refresh:         {}s", config.refresh_interval_secs);
    println!("  Backend:         {}", config.backend);
    println!("  Mode:            {rw_mode}");
    println!("  Enabled:         {enabled}");
    println!("  State:           {}", status.state);
    println!(
        "  Live:            {}",
        if status.is_live { "yes" } else { "no" }
    );
    println!(
        "  HEAD OID:        {}",
        status.head_oid.as_deref().unwrap_or("(none)")
    );
    if status.dirty_count == 0 {
        println!("  Overlay:         clean");
    } else {
        println!("  Overlay:         dirty ({} entries)", status.dirty_count);
        for path in &status.dirty_paths {
            println!("    {path}");
        }
    }

    Ok(())
}

/// Update the refresh interval for a repo.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn run_daemon_set_refresh(daemon_root: &Path, name: &str, interval_secs: u64) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;

    let updated = registry.set_refresh_interval(name, interval_secs)?;
    if updated {
        println!("Set refresh interval for '{name}' to {interval_secs}s.");
    } else {
        eprintln!("Repo '{name}' not found in registry.");
        return Err(CrabError::NotFound {
            path: format!("repo '{name}'"),
        });
    }
    Ok(())
}

/// Remount a repo: stop and re-execute the full mount pipeline.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn run_daemon_remount(
    daemon_root: &Path,
    name: &str,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::vfs::daemon::DaemonService;

    let daemon = DaemonService::new(daemon_root.to_path_buf(), cancel)?
        .with_read_resolver(mount_read_resolver());
    daemon.remount_repo(name).await?;
    println!("Remounted repo '{name}'.");
    Ok(())
}

/// Trigger an immediate git fetch for a repo.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn run_daemon_fetch(
    daemon_root: &Path,
    name: &str,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::vfs::daemon::DaemonService;

    let daemon = DaemonService::new(daemon_root.to_path_buf(), cancel)?
        .with_read_resolver(mount_read_resolver());
    daemon.force_fetch(name).await?;
    println!("Fetch completed for repo '{name}'.");
    Ok(())
}

/// Enable a repo in the registry.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn run_daemon_enable(daemon_root: &Path, name: &str) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;

    let updated = registry.enable_repo(name)?;
    if updated {
        println!("Enabled repo '{name}'. It will be mounted on the next sync cycle.");
    } else {
        eprintln!("Repo '{name}' not found in registry.");
        return Err(CrabError::NotFound {
            path: format!("repo '{name}'"),
        });
    }
    Ok(())
}

/// Disable a repo in the registry.
#[cfg(any(feature = "fuse", feature = "nfs"))]
fn run_daemon_disable(daemon_root: &Path, name: &str) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;

    let updated = registry.disable_repo(name)?;
    if updated {
        println!("Disabled repo '{name}'. It will be unmounted on the next sync cycle.");
    } else {
        eprintln!("Repo '{name}' not found in registry.");
        return Err(CrabError::NotFound {
            path: format!("repo '{name}'"),
        });
    }
    Ok(())
}

/// Commit overlay mutations for a daemon-managed repo.
#[cfg(any(feature = "fuse", feature = "nfs"))]
async fn run_daemon_commit(
    daemon_root: &Path,
    name: &str,
    message: &str,
    push: bool,
    mode: OutputMode,
) -> Result<()> {
    use crate::vfs::daemon::Registry;

    let registry_path = daemon_root.join("config/repos.sqlite");
    let registry = Registry::open(&registry_path)?;
    let config = registry
        .get_repo(name)?
        .ok_or_else(|| CrabError::NotFound {
            path: format!("repo '{name}'"),
        })?;
    if config.read_only {
        return Err(CrabError::Configuration {
            key: "cannot commit overlay for a read-only daemon repo".into(),
            origin: "crab daemon commit".into(),
        });
    }

    let paths = config.computed_paths(daemon_root);
    #[cfg(feature = "nfs")]
    let (live_nfs_result, _offline_nfs_lock) =
        if config.backend == crate::vfs::daemon::DaemonMountBackend::Nfs {
            let endpoint = crate::vfs::daemon::read_nfs_control_endpoint(&paths)?;
            if let Some(endpoint) = endpoint
                && crate::vfs::nfs_control::ping(&endpoint).await.is_ok()
            {
                (
                    Some(crate::vfs::nfs_control::commit(&endpoint, message, push).await?),
                    None,
                )
            } else {
                (
                    None,
                    Some(crate::vfs::clone_cache::MountCacheLock::acquire(
                        &paths.repo_dir,
                    )?),
                )
            }
        } else {
            (None, None)
        };
    #[cfg(not(feature = "nfs"))]
    let live_nfs_result = None;

    let result = match live_nfs_result {
        Some(result) => result,
        None => crate::vfs::publish::commit_overlay(&crate::vfs::publish::OverlayCommitOptions {
            cache_dir: paths.repo_dir.clone(),
            git_dir: paths.git_dir,
            ref_name: daemon_branch_ref(&config.branch),
            message: message.to_owned(),
            push,
        })?,
    };

    if mode == OutputMode::Json {
        emit_json(
            "daemon.commit",
            "1.0",
            DaemonCommitPayload {
                name: config.name,
                mountpoint: paths.mount_path.display().to_string(),
                result,
            },
        );
        return Ok(());
    }

    println!("Repo: {}", config.name);
    print_overlay_commit(&result);
    Ok(())
}

#[cfg(any(feature = "fuse", feature = "nfs"))]
fn daemon_branch_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the `.crab` directory relative to a mountpoint.
///
/// Search order:
/// 1. Current working directory (the repo root when running `crab mount`)
/// 2. The mountpoint itself
/// 3. The mountpoint's parent
/// 4. Default: create in the CWD (repo root)
fn find_crab_dir(mountpoint: &Path) -> PathBuf {
    // Check CWD first — `crab mount` is typically run from the repo root.
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_candidate = cwd.join(".crab");
        if cwd_candidate.exists() || cwd.join(".git").exists() {
            return cwd_candidate;
        }
    }

    // Look for `.crab` in the mountpoint's parent or the mountpoint itself.
    let candidate = mountpoint.join(".crab");
    if candidate.exists() {
        return candidate;
    }
    if let Some(parent) = mountpoint.parent() {
        let parent_candidate = parent.join(".crab");
        if parent_candidate.exists() {
            return parent_candidate;
        }
    }
    // Default: create in CWD if available, otherwise alongside the mountpoint.
    if let Ok(cwd) = std::env::current_dir() {
        cwd.join(".crab")
    } else {
        mountpoint.join(".crab")
    }
}

/// Find the `.git` directory path as a string for the synthetic `.git` file.
#[cfg(feature = "fuse")]
fn find_git_dir(crab_dir: &Path) -> String {
    // The .git dir is typically a sibling of .crab.
    if let Some(parent) = crab_dir.parent() {
        let git_dir = parent.join(".git");
        if git_dir.exists() {
            return git_dir.to_string_lossy().into_owned();
        }
    }
    ".git".to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(feature = "nfs")]
#[allow(clippy::unwrap_used, reason = "test setup and assertions")]
mod nfs_tests {
    use super::*;
    use crate::vfs::nfs_mount::{NfsPreflightMessage, NfsPreflightReport};

    fn ready_nfs_report() -> NfsPreflightReport {
        NfsPreflightReport {
            backend_available: true,
            native_client_available: true,
            mountpoint_ready: true,
            loopback_bind_ready: true,
            control_endpoint_ready: true,
            privilege_ready: true,
            warnings: Vec::new(),
            blockers: Vec::new(),
        }
    }

    fn blocked_nfs_report(key: &str) -> NfsPreflightReport {
        NfsPreflightReport {
            blockers: vec![NfsPreflightMessage {
                key: key.to_owned(),
                detail: "blocked".to_owned(),
                action: Some("fix it".to_owned()),
            }],
            ..ready_nfs_report()
        }
    }

    #[test]
    fn nfs_background_log_path_uses_mount_logs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());

        let log_path = nfs_background_log_path(Path::new("/tmp/Crab Mount:Z")).unwrap();

        assert_eq!(
            log_path,
            tmp.path()
                .join(".crab")
                .join("mounts")
                .join("logs")
                .join("nfs-tmp-Crab-Mount-Z.log")
        );
    }

    #[test]
    fn nfs_background_preflight_failure_hint_preserves_actionable_blocker() {
        let hint =
            nfs_background_preflight_failure_hint(&blocked_nfs_report("mount.nfs not found"))
                .unwrap();

        assert!(hint.contains("NFS preflight now reports"));
        assert!(hint.contains("mount.nfs not found"));
        assert!(hint.contains("next: fix it"));
    }

    #[test]
    fn nfs_background_preflight_failure_hint_is_empty_when_ready() {
        assert!(nfs_background_preflight_failure_hint(&ready_nfs_report()).is_none());
    }

    #[test]
    fn sanitize_mountpoint_filename_has_stable_fallback() {
        assert_eq!(sanitize_mountpoint_for_filename(Path::new("///")), "mount");
    }

    #[test]
    fn nfs_helper_filename_candidates_match_platform_suffix() {
        assert_eq!(
            mount_helper_filenames("crab-nfs-mount", true),
            vec!["crab-nfs-mount".to_owned(), "crab-nfs-mount.exe".to_owned()]
        );
        assert_eq!(
            mount_helper_filenames("crab-nfs-mount.exe", true),
            vec!["crab-nfs-mount.exe".to_owned()]
        );
        assert_eq!(
            mount_helper_filenames("crab-nfs-mount", false),
            vec!["crab-nfs-mount".to_owned()]
        );
    }

    #[test]
    fn nfs_helper_doctor_check_reports_missing_helper_as_failure() {
        let check = nfs_helper_presence_doctor_check(None);

        assert_eq!(check.status, MountDoctorStatus::Fail);
        assert!(check.detail.contains("crab-nfs-mount"));
        assert!(check.action.unwrap().contains("make install"));
    }

    #[test]
    fn nfs_helper_version_parser_reads_clap_and_version_command_output() {
        assert_eq!(
            parse_crab_version_output("crab 1.2.3\n"),
            Some("1.2.3".to_owned())
        );
        assert_eq!(
            parse_crab_version_output("crab 1.2.3 (abcdef)\nbuilt at now\n"),
            Some("1.2.3".to_owned())
        );
        assert_eq!(parse_crab_version_output("other 1.2.3\n"), None);
    }

    #[test]
    fn nfs_helper_version_check_rejects_mismatched_helper() {
        let helper = Path::new("/tmp/crab-nfs-mount");

        let check = nfs_helper_version_doctor_check_from_result(helper, Ok("0.0.1".to_owned()));

        assert_eq!(check.status, MountDoctorStatus::Fail);
        assert!(check.detail.contains("0.0.1"));
        assert!(check.detail.contains(env!("CRAB_BUILD_VERSION")));
    }

    #[test]
    fn nfs_helper_layout_check_requires_colocated_helper() {
        let next_to_crab = nfs_helper_layout_doctor_check_for_paths(
            Some(Path::new("/opt/crab/bin/crab")),
            Path::new("/opt/crab/bin/crab-nfs-mount"),
        );
        let from_path = nfs_helper_layout_doctor_check_for_paths(
            Some(Path::new("/opt/crab/bin/crab")),
            Path::new("/usr/local/bin/crab-nfs-mount"),
        );

        assert_eq!(next_to_crab.status, MountDoctorStatus::Ok);
        assert_eq!(from_path.status, MountDoctorStatus::Fail);
    }

    #[test]
    fn nfs_background_helper_check_requires_colocated_matching_helper() {
        let current_exe = Path::new("/opt/crab/bin/crab");
        let colocated_helper = Path::new("/opt/crab/bin/crab-nfs-mount");
        let path_helper = Path::new("/usr/local/bin/crab-nfs-mount");

        check_nfs_background_helper(
            current_exe,
            colocated_helper,
            Ok(env!("CRAB_BUILD_VERSION").to_owned()),
            MountBackend::Nfs,
        )
        .unwrap();

        let layout_error = check_nfs_background_helper(
            current_exe,
            path_helper,
            Ok(env!("CRAB_BUILD_VERSION").to_owned()),
            MountBackend::Auto,
        )
        .unwrap_err();
        match layout_error {
            CrabError::Configuration { key, origin } => {
                assert_eq!(origin, "crab mount --backend=auto");
                assert!(key.contains("not installed next to"));
            }
            error => panic!("unexpected error: {error}"),
        }

        let version_error = check_nfs_background_helper(
            current_exe,
            colocated_helper,
            Ok("0.0.1".to_owned()),
            MountBackend::Nfs,
        )
        .unwrap_err();
        match version_error {
            CrabError::Configuration { key, origin } => {
                assert_eq!(origin, "crab mount --backend=nfs");
                assert!(key.contains("reports Crab 0.0.1"));
            }
            error => panic!("unexpected error: {error}"),
        }
    }

    #[test]
    fn nfs_preflight_doctor_check_preserves_blocker_summary() {
        let report = blocked_nfs_report("mount.nfs not found");

        let check = nfs_preflight_doctor_check(&report, Path::new("/mnt/crab"));

        assert_eq!(check.status, MountDoctorStatus::Fail);
        assert!(check.detail.contains("NFS preflight failed with 1 blocker"));
        assert!(check.detail.contains("mount.nfs not found"));
        assert!(check.detail.contains("next: fix it"));
    }

    #[test]
    fn mount_doctor_payload_summarizes_readiness() {
        let checks = vec![
            MountDoctorCheck::ok("ok", "ready"),
            MountDoctorCheck::warn("warn", "degraded", "review it"),
            MountDoctorCheck::fail("fail", "blocked", "fix it"),
        ];

        let payload = mount_doctor_payload(
            MountBackend::Auto,
            MountBackend::Nfs,
            Path::new("/mnt/crab"),
            MountDoctorCollectedChecks::new(checks),
        );

        assert_eq!(payload.requested_backend, "auto");
        assert_eq!(payload.checked_backend, "nfs");
        assert_eq!(payload.summary.ok, 1);
        assert_eq!(payload.summary.warn, 1);
        assert_eq!(payload.summary.fail, 1);
        assert!(!payload.summary.ready);
        assert_eq!(
            serde_json::to_value(&payload).unwrap()["checks"][0]["status"],
            "ok"
        );
    }

    #[test]
    fn mount_doctor_payload_includes_machine_readable_nfs_preflight() {
        let mut report = blocked_nfs_report("mount.nfs not found");
        report.native_client_available = false;
        report.warnings.push(NfsPreflightMessage {
            key: "Windows Client for NFS umount.exe not found".to_owned(),
            detail: "unmount may require manual cleanup".to_owned(),
            action: None,
        });
        let checks = vec![nfs_preflight_doctor_check(&report, Path::new("/mnt/crab"))];

        let payload = mount_doctor_payload(
            MountBackend::Nfs,
            MountBackend::Nfs,
            Path::new("/mnt/crab"),
            MountDoctorCollectedChecks::with_nfs_preflight(checks, &report),
        );

        let preflight = payload.nfs_preflight.as_ref().unwrap();
        assert!(!preflight.ready);
        assert!(!preflight.native_client_available);
        assert_eq!(preflight.blocker_count, 1);
        assert_eq!(preflight.warning_count, 1);
        assert_eq!(preflight.next_action.as_deref(), Some("fix it"));
        assert_eq!(preflight.blockers[0].key, "mount.nfs not found");
        assert_eq!(
            serde_json::to_value(&payload).unwrap()["nfs_preflight"]["blockers"][0]["action"],
            "fix it"
        );
    }

    #[test]
    fn mount_doctor_payload_includes_machine_readable_auto_decision() {
        let report = ready_nfs_report();
        let checks = vec![nfs_preflight_doctor_check(&report, Path::new("/mnt/crab"))];

        let payload = mount_doctor_payload(
            MountBackend::Auto,
            MountBackend::Nfs,
            Path::new("/mnt/crab"),
            MountDoctorCollectedChecks::with_nfs_preflight(checks, &report),
        );

        let decision = payload.auto_decision.as_ref().unwrap();
        assert_eq!(decision.selected_backend.as_deref(), Some("nfs"));
        assert!(decision.reason.contains("preferred NFS backend"));
        assert_eq!(
            serde_json::to_value(&payload).unwrap()["auto_decision"]["selected_backend"],
            "nfs"
        );
    }

    #[test]
    fn mount_doctor_auto_decision_keeps_helper_failures_visible() {
        let report = ready_nfs_report();
        let checks = vec![
            MountDoctorCheck::ok(
                "nfs feature",
                "NFS support is compiled into this Crab build",
            ),
            nfs_helper_presence_doctor_check(None),
            nfs_preflight_doctor_check(&report, Path::new("/mnt/crab")),
        ];

        let payload = mount_doctor_payload(
            MountBackend::Auto,
            MountBackend::Nfs,
            Path::new("/mnt/crab"),
            MountDoctorCollectedChecks::with_nfs_preflight(checks, &report),
        );

        let decision = payload.auto_decision.as_ref().unwrap();
        assert_eq!(decision.selected_backend, None);
        assert!(decision.reason.contains("NFS helper check failed"));
        assert!(decision.reason.contains("auto will not hide"));
        assert!(
            decision
                .next_action
                .as_deref()
                .is_some_and(|action| action.contains("make install"))
        );
        assert_eq!(decision.nfs_ready, Some(false));
        assert_eq!(decision.nfs_blocker_count, Some(0));
        assert_eq!(decision.fuse_ready, None);
    }

    #[test]
    fn auto_backend_prefers_nfs_when_nfs_preflight_is_ready() {
        let backend = resolve_auto_mount_backend_from_nfs_report(&ready_nfs_report()).unwrap();

        assert_eq!(backend, ResolvedMountBackend::Nfs);
    }

    #[test]
    fn explicit_nfs_backend_prerequisites_use_preflight_report() {
        ensure_nfs_preflight_ready(&ready_nfs_report()).unwrap();

        let error =
            ensure_nfs_preflight_ready(&blocked_nfs_report("mount.nfs not found")).unwrap_err();

        match error {
            CrabError::Configuration { key, origin } => {
                assert_eq!(origin, "crab mount --backend=nfs");
                assert!(key.contains("NFS preflight failed"));
                assert!(key.contains("mount.nfs not found"));
            }
            error => panic!("unexpected error: {error}"),
        }
    }

    #[test]
    fn auto_backend_nonfallback_preflight_error_uses_auto_origin() {
        let error = resolve_auto_mount_backend_from_nfs_report(&blocked_nfs_report(
            "NFS mountpoint is already mounted",
        ))
        .unwrap_err();

        match error {
            CrabError::Configuration { key, origin } => {
                assert_eq!(origin, "crab mount --backend=auto");
                assert!(key.contains("NFS mountpoint is already mounted"));
            }
            error => panic!("unexpected error: {error}"),
        }
    }

    #[test]
    fn auto_backend_fallback_allows_only_environmental_nfs_unavailability() {
        for blocker in [
            "mount_nfs not found",
            "mount.nfs not found",
            "Windows Client for NFS mount.exe not found",
            "Linux NFS mount permission unavailable",
            "unsupported NFS platform",
        ] {
            assert!(
                nfs_blockers_allow_auto_fuse_fallback(&blocked_nfs_report(blocker)),
                "{blocker} should permit FUSE fallback for --backend=auto"
            );
        }
    }

    #[test]
    fn auto_backend_fallback_rejects_stateful_nfs_failures() {
        for blocker in [
            "invalid Windows NFS mountpoint",
            "NFS mountpoint does not exist",
            "NFS mountpoint is not a directory",
            "NFS mountpoint is already mounted",
            "NFS server is not bound to loopback",
            "Windows NFS port contract failed",
            "NFS control endpoint unavailable",
        ] {
            assert!(
                !nfs_blockers_allow_auto_fuse_fallback(&blocked_nfs_report(blocker)),
                "{blocker} should remain visible instead of falling back to FUSE"
            );
        }
    }

    #[test]
    #[cfg(feature = "fuse")]
    fn auto_backend_falls_back_to_fuse_for_nfs_specific_blocker() {
        let backend = resolve_auto_mount_backend_from_probe(
            &blocked_nfs_report("mount.nfs not found"),
            Ok(()),
        )
        .unwrap();

        assert_eq!(backend, ResolvedMountBackend::Fuse);
    }

    #[test]
    #[cfg(feature = "fuse")]
    fn auto_backend_fallback_message_names_nfs_blocker_and_next_action() {
        let message = auto_backend_fallback_message(&blocked_nfs_report("mount.nfs not found"));

        assert!(message.contains("using FUSE for --backend=auto"));
        assert!(message.contains("mount.nfs not found: blocked"));
        assert!(message.contains("next: fix it"));
    }

    #[test]
    fn auto_decision_reports_fuse_fallback_with_nfs_next_action() {
        let preflight =
            MountDoctorNfsPreflight::from_report(&blocked_nfs_report("mount.nfs not found"));

        let decision = mount_doctor_auto_decision_from_nfs_preflight(&preflight, Some(Ok(())));

        assert_eq!(decision.selected_backend.as_deref(), Some("fuse"));
        assert_eq!(decision.next_action.as_deref(), Some("fix it"));
        assert_eq!(decision.fuse_ready, Some(true));
    }

    #[test]
    fn auto_decision_keeps_non_fallback_nfs_blockers_visible() {
        for blocker in [
            "NFS mountpoint is already mounted",
            "NFS control endpoint unavailable",
        ] {
            let preflight = MountDoctorNfsPreflight::from_report(&blocked_nfs_report(blocker));

            let decision = mount_doctor_auto_decision_from_nfs_preflight(&preflight, Some(Ok(())));

            assert_eq!(decision.selected_backend, None);
            assert!(decision.reason.contains("auto will not hide"));
            assert_eq!(decision.next_action.as_deref(), Some("fix it"));
            assert_eq!(decision.fuse_ready, None);
        }
    }

    #[test]
    #[cfg(feature = "fuse")]
    fn auto_backend_reports_nfs_and_fuse_failures_when_neither_is_ready() {
        let error = resolve_auto_mount_backend_from_probe(
            &blocked_nfs_report("mount.nfs not found"),
            Err(CrabError::Configuration {
                key: "FUSE prerequisites not met".to_owned(),
                origin: "crab mount --backend=fuse".to_owned(),
            }),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("no mount backend is usable"));
        assert!(error.contains("mount.nfs not found"));
        assert!(error.contains("FUSE preflight failed"));
    }

    #[test]
    fn windows_drive_mountpoint_is_canonicalized_for_registry_matching() {
        assert_eq!(
            prepare_mountpoint(Path::new("z:\\"), ResolvedMountBackend::Nfs).unwrap(),
            PathBuf::from("Z:")
        );
        assert_eq!(
            normalize_unmount_path(Path::new("z:\\")),
            PathBuf::from("Z:")
        );
    }

    #[test]
    fn windows_tasklist_pid_parser_matches_pid_column_only() {
        let line = "\"crab-nfs-mount.exe\",\"4242\",\"Console\",\"1\",\"11,800 K\"";

        assert!(windows_tasklist_csv_line_has_pid(line, 4242));
        assert!(!windows_tasklist_csv_line_has_pid(line, 11800));
        assert!(!windows_tasklist_csv_line_has_pid(
            "INFO: No tasks are running",
            4242
        ));
    }
}

#[cfg(test)]
#[cfg(feature = "fuse")]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// Verify that `read_remote_url_from_crab_dir` reads from crab.toml.
    #[test]
    fn read_remote_url_from_config_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let crab_dir = tmp.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        let config_content = "[remote]\nurl = \"crab://my-bucket/org/models\"\n";
        std::fs::write(tmp.path().join("crab.toml"), config_content).unwrap();

        let url = read_remote_url_from_crab_dir(&crab_dir).unwrap();
        assert_eq!(url, "crab://my-bucket/org/models");
    }

    #[test]
    fn read_remote_url_ignores_retired_remote_file() {
        let tmp = tempfile::tempdir().unwrap();
        let crab_dir = tmp.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        std::fs::write(crab_dir.join("remote"), "crab://bucket/repo\n").unwrap();

        assert!(read_remote_url_from_crab_dir(&crab_dir).is_err());
    }

    /// Verify that `read_remote_url_from_crab_dir` returns an error when
    /// no remote is configured.
    #[test]
    fn read_remote_url_no_config_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let crab_dir = tmp.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        let result = read_remote_url_from_crab_dir(&crab_dir);
        assert!(result.is_err());
    }

    #[cfg(feature = "fuse")]
    #[test]
    fn daemon_add_repo_only_updates_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon_root = tmp.path().join("daemon");
        let mount_root = tmp.path().join("mounts");
        let mount_root_str = mount_root.to_string_lossy().to_string();

        run_daemon_add_repo(
            &daemon_root,
            "repo-a",
            "file:///does/not/exist",
            "main",
            &mount_root_str,
            crate::vfs::daemon::DaemonMountBackend::Fuse,
        )
        .unwrap();

        let registry =
            crate::vfs::daemon::Registry::open(&daemon_root.join("config/repos.sqlite")).unwrap();
        let config = registry.get_repo("repo-a").unwrap().unwrap();
        assert_eq!(config.mount_root, mount_root_str);
        assert!(!daemon_root.join("repos/repo-a").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mount_read_layout_uses_selected_replica_store() {
        use bytes::Bytes;
        use object_store::memory::InMemory;
        use std::sync::Arc;

        let prefix = "org/repo";
        let marker = "packs/mount-marker";
        let primary = crate::storage::store::Store::new(Arc::new(InMemory::new()));
        let primary_layout = crate::storage::StoreLayout::new(primary.clone(), prefix.to_owned());
        primary
            .put(
                &primary_layout.repo_path(marker),
                Bytes::from_static(b"primary"),
            )
            .await
            .unwrap();

        let replica = crate::storage::store::Store::new(Arc::new(InMemory::new()));
        let replica_layout = crate::storage::StoreLayout::new(replica.clone(), prefix.to_owned());
        replica
            .put(
                &replica_layout.repo_path(marker),
                Bytes::from_static(b"replica"),
            )
            .await
            .unwrap();

        let config = crate::core::config::Config::default();
        let parsed = crate::git::url::CrabUrl::parse("crab://primary/org/repo").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let selected_layout =
            select_mount_read_layout_with_selector(&config, &parsed, &cancel, move |_, _, _| {
                let replica = replica.clone();
                async move {
                    Ok(crate::replication::ReadStoreSelection {
                        store: replica.clone(),
                        router: crate::storage::StoreLayout::new(replica, prefix.to_owned()),
                        source: crate::replication::ReadSource::Replica {
                            name: "west".to_owned(),
                        },
                    })
                }
            })
            .await
            .unwrap();

        let (body, _) = selected_layout
            .store()
            .get_with_etag(&selected_layout.repo_path(marker))
            .await
            .unwrap();
        assert_eq!(body, Bytes::from_static(b"replica"));

        let (primary_body, _) = primary
            .get_with_etag(&primary_layout.repo_path(marker))
            .await
            .unwrap();
        assert_eq!(primary_body, Bytes::from_static(b"primary"));
    }

    /// Verify that `build_mount_components` executes the pipeline
    /// successfully for a local git repo with a `.crab` directory.
    ///
    /// This test creates a minimal git repo, sets up `crab.toml`,
    /// and verifies the pipeline produces a resolver and engine.
    #[tokio::test(flavor = "multi_thread")]
    async fn build_mount_components_local_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().to_path_buf();

        // Initialize a git repo with a commit.
        let output = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init failed");

        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_root)
            .output()
            .unwrap();

        // Create a file and commit it.
        std::fs::write(repo_root.join("hello.txt"), "hello world\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "hello.txt"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&repo_root)
            .output()
            .unwrap();

        // Create .crab directory (no remote config — local mount).
        let crab_dir = repo_root.join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        let result = build_mount_components(&crab_dir, None).await;

        assert!(
            result.is_ok(),
            "build_mount_components failed: {:?}",
            result.err()
        );

        let (resolver, engine) = result.unwrap();
        // Verify the resolver and engine are valid Arc pointers.
        assert!(std::sync::Arc::strong_count(&resolver) >= 1);
        assert!(std::sync::Arc::strong_count(&engine) >= 1);
    }

    /// Verify that `build_mount_components` respects the `git_ref` parameter.
    #[tokio::test(flavor = "multi_thread")]
    async fn build_mount_components_with_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().to_path_buf();

        // Initialize a git repo with a commit on main.
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_root)
            .output()
            .unwrap();

        std::fs::write(repo_root.join("file.txt"), "content\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&repo_root)
            .output()
            .unwrap();

        let crab_dir = repo_root.join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        let result = build_mount_components(&crab_dir, Some("main")).await;

        assert!(
            result.is_ok(),
            "build_mount_components with ref failed: {:?}",
            result.err()
        );
    }

    /// Verify that `build_mount_components` fails gracefully when .git
    /// is missing.
    #[tokio::test(flavor = "multi_thread")]
    async fn build_mount_components_no_git_dir_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let crab_dir = tmp.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        let result = build_mount_components(&crab_dir, None).await;

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains(".git directory not found"),
            "expected .git not found error, got: {err_msg}"
        );
    }

    // --- build_local_pipeline_config tests ---

    /// Verify that `build_local_pipeline_config` validates the local repo
    /// and computes the correct cache directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_pipeline_config_valid_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let repo_path = tmp.path();
        std::fs::create_dir(repo_path.join(".git")).unwrap();

        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();

        let cancel = tokio_util::sync::CancellationToken::new();
        let (config, _layout, _cache_lock) = build_local_pipeline_config(
            repo_path,
            &mountpoint,
            Some("refs/heads/main".to_owned()),
            false,
            cancel,
        )
        .await
        .unwrap();

        // git_dir should point to .git
        assert_eq!(config.git_dir, repo_path.join(".git"));
        // ref_name should be preserved
        assert_eq!(config.ref_name, Some("refs/heads/main".to_owned()));
        // read_only should be false
        assert!(!config.read_only);
        // cache_dir should be under ~/.crab/mounts/repos/<hash>
        let cache_str = config.cache_dir.to_string_lossy();
        assert!(
            cache_str.contains(".crab/mounts/repos/"),
            "expected cache dir under .crab/mounts/repos/, got: {cache_str}"
        );
        // The hash should be 12 hex chars
        let hash = config.cache_dir.file_name().unwrap().to_string_lossy();
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Verify that `build_local_pipeline_config` fails when .git is missing.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_pipeline_config_no_git_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path();
        // No .git directory

        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();

        let cancel = tokio_util::sync::CancellationToken::new();
        let result = build_local_pipeline_config(repo_path, &mountpoint, None, false, cancel).await;

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("not a git repository"),
            "expected 'not a git repository' error, got: {err_msg}"
        );
    }

    /// Verify that `build_local_pipeline_config` returns None for store_layout
    /// when no crab.toml exists (no crab remote configured).
    #[tokio::test(flavor = "multi_thread")]
    async fn local_pipeline_config_no_crab_remote_returns_none_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let repo_path = tmp.path();
        std::fs::create_dir(repo_path.join(".git")).unwrap();

        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();

        let cancel = tokio_util::sync::CancellationToken::new();
        let (_config, layout, _cache_lock) =
            build_local_pipeline_config(repo_path, &mountpoint, None, false, cancel)
                .await
                .unwrap();

        // No crab remote configured → layout should be None.
        assert!(
            layout.is_none(),
            "expected None store_layout when no crab remote configured"
        );
    }

    /// Verify that the same local repo path always produces the same cache hash.
    #[tokio::test(flavor = "multi_thread")]
    async fn local_pipeline_config_deterministic_cache_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let repo_path = tmp.path();
        std::fs::create_dir(repo_path.join(".git")).unwrap();

        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();

        let cancel1 = tokio_util::sync::CancellationToken::new();
        let (config1, _, lock1) =
            build_local_pipeline_config(repo_path, &mountpoint, None, false, cancel1)
                .await
                .unwrap();
        drop(lock1);

        let cancel2 = tokio_util::sync::CancellationToken::new();
        let (config2, _, _lock2) =
            build_local_pipeline_config(repo_path, &mountpoint, None, false, cancel2)
                .await
                .unwrap();

        assert_eq!(config1.cache_dir, config2.cache_dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn local_pipeline_config_rejects_active_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let repo_path = tmp.path();
        std::fs::create_dir(repo_path.join(".git")).unwrap();

        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();

        let cancel1 = tokio_util::sync::CancellationToken::new();
        let (_config, _layout, lock1) =
            build_local_pipeline_config(repo_path, &mountpoint, None, false, cancel1)
                .await
                .unwrap();

        let cancel2 = tokio_util::sync::CancellationToken::new();
        let conflict =
            match build_local_pipeline_config(repo_path, &mountpoint, None, false, cancel2).await {
                Ok(_) => panic!("second active cache should be rejected"),
                Err(error) => error,
            };
        assert!(
            conflict
                .to_string()
                .contains("mount cache is already in use"),
            "got: {conflict}"
        );

        drop(lock1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod safety_check_tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_prereq_message_includes_loader_failure_detail() {
        let error = CrabError::Configuration {
            key: "macFUSE loader failed: exit status 1; approve macFUSE in System Settings".into(),
            origin: "FUSE prerequisite check".into(),
        };

        let message = macos_macfuse_unavailable_message(&error);

        assert!(message.contains("macFUSE is not ready"));
        assert!(message.contains("Details: configuration error"));
        assert!(message.contains("macFUSE loader failed"));
        assert!(message.contains("System Settings"));
        assert!(message.contains("brew install --cask macfuse"));
    }

    /// A path inside a directory containing `.git/` is detected as inside a repo.
    #[test]
    fn detects_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        let nested = tmp.path().join("subdir/deep");
        std::fs::create_dir_all(&nested).unwrap();

        assert!(is_inside_git_repo(&nested));
        assert!(is_inside_git_repo(tmp.path()));
    }

    /// A path inside a directory containing `.crab/` is detected as inside a repo.
    #[test]
    fn detects_crab_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let crab_dir = tmp.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();

        let nested = tmp.path().join("some/nested/path");
        std::fs::create_dir_all(&nested).unwrap();

        assert!(is_inside_git_repo(&nested));
    }

    /// A path with no `.git/` or `.crab/` in any ancestor is not inside a repo.
    #[test]
    fn returns_false_for_plain_dir() {
        let tmp = tempdir_outside_repo_ancestors();
        let nested = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        assert!(!is_inside_git_repo(&nested));
    }

    /// The root of a repo (where `.git/` lives) is itself considered inside.
    #[test]
    fn root_is_inside() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();

        assert!(is_inside_git_repo(tmp.path()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod unmount_tests {
    use super::*;
    use crate::vfs::mounts_registry::{self, MountEntry};

    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(home: &Path) -> Self {
            let original = std::env::var_os("HOME");
            // SAFETY: these tests update HOME before starting any worker
            // threads and restore it before returning to the harness.
            unsafe {
                std::env::set_var("HOME", home);
            }
            Self { original }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: restores the process environment for this test scope.
            unsafe {
                if let Some(original) = &self.original {
                    std::env::set_var("HOME", original);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }

    fn sample_entry(mountpoint: &str, pid: u32) -> MountEntry {
        MountEntry {
            mountpoint: mountpoint.to_owned(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid,
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "repo".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        }
    }

    #[cfg(feature = "nfs")]
    fn sample_nfs_entry(mountpoint: &str, endpoint: &str) -> MountEntry {
        MountEntry {
            backend: Some("nfs".to_owned()),
            control_endpoint: Some(endpoint.to_owned()),
            pid: std::process::id(),
            ..sample_entry(mountpoint, std::process::id())
        }
    }

    #[cfg(feature = "nfs")]
    fn free_tcp_control_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("tcp:{addr}?token=unmount-all-test")
    }

    #[cfg(feature = "nfs")]
    fn nfs_control_state(mountpoint: &Path) -> crate::vfs::nfs_control::NfsControlState {
        crate::vfs::nfs_control::NfsControlState {
            mountpoint: mountpoint.to_path_buf(),
            read_only: true,
            read_leases: crate::vfs::read_lease_pool::ReadLeasePool::new(4, 4096),
            directory_pages: crate::vfs::nfs::NfsDirectoryPageCache::new(4, 4096),
            write_journal: std::sync::Arc::new(crate::vfs::nfs::NfsWriteJournal::new()),
            protocol_stats: std::sync::Arc::new(crate::vfs::nfs::NfsProtocolStats::new()),
            engine: None,
            runtime: None,
            lifecycle: crate::vfs::nfs_control::NfsMountLifecycleStatus::default(),
        }
    }

    /// A PID of 0 is never alive (it's the kernel).
    #[test]
    fn is_pid_alive_returns_false_for_nonexistent_pid() {
        // PID 0 is the kernel scheduler — kill(0, 0) sends to the process group,
        // so use a very high PID that almost certainly doesn't exist.
        let fake_pid = 4_000_000_000;
        assert!(!is_pid_alive(fake_pid));
    }

    /// The current process PID should be alive.
    #[test]
    fn is_pid_alive_returns_true_for_current_process() {
        let my_pid = std::process::id();
        assert!(is_pid_alive(my_pid));
    }

    #[test]
    fn linux_force_unmount_falls_back_to_plain_umount() {
        let programs = linux_unmount_commands()
            .iter()
            .map(|command| command.program)
            .collect::<Vec<_>>();

        assert_eq!(programs, vec!["fusermount3", "fusermount", "umount"]);
    }

    /// run_unmount removes the registry entry for a stale mount.
    #[test]
    fn run_unmount_cleans_stale_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("mounts.json");

        // Use a PID that doesn't exist (stale).
        let fake_pid = 4_000_000_000;
        let mountpoint = tmp.path().join("mnt");
        std::fs::create_dir_all(&mountpoint).unwrap();
        let mountpoint_str = mountpoint.display().to_string();

        mounts_registry::add_entry(&registry_path, sample_entry(&mountpoint_str, fake_pid))
            .unwrap();

        // Verify entry exists.
        let entries = mounts_registry::list_entries(&registry_path).unwrap();
        assert_eq!(entries.len(), 1);

        // We can't call run_unmount directly because it uses registry_path()
        // which reads HOME. Instead, test the stale detection logic directly.
        assert!(!is_pid_alive(fake_pid));

        // Simulate what run_unmount does for stale entries.
        let entry = entries.into_iter().find(|e| e.mountpoint == mountpoint_str);
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert!(!is_pid_alive(entry.pid));

        // Remove the entry (simulating cleanup).
        let removed = mounts_registry::remove_entry(&registry_path, &mountpoint_str).unwrap();
        assert!(removed);

        // Verify registry is now empty.
        let entries = mounts_registry::list_entries(&registry_path).unwrap();
        assert!(entries.is_empty());
    }

    /// run_unmount_all handles an empty registry gracefully.
    #[test]
    fn unmount_all_empty_registry() {
        // When mounts.json doesn't exist, list_entries returns empty vec.
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("mounts.json");

        let entries = mounts_registry::list_entries(&registry_path).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn background_nfs_readiness_waits_for_control_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("view");
        let endpoint = free_tcp_control_endpoint();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            assert!(nfs_background_control_ready(None).await);
            assert!(!nfs_background_control_ready(Some(&endpoint)).await);

            let cancel = tokio_util::sync::CancellationToken::new();
            let handle = crate::vfs::nfs_control::spawn_server(
                Some(endpoint.clone()),
                nfs_control_state(&mountpoint),
                cancel.clone(),
            )
            .unwrap();

            let mut ready = false;
            for _ in 0..20 {
                if nfs_background_control_ready(Some(&endpoint)).await {
                    ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(ready);

            crate::vfs::nfs_control::shutdown(&endpoint).await.unwrap();
            handle.await.unwrap().unwrap();
        });
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn unmount_all_live_or_persisted_uses_nfs_control_shutdown() {
        let _env_lock = crate::test::git_repo::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let raw_mountpoint = tmp.path().join("view");
        std::fs::create_dir_all(&raw_mountpoint).unwrap();
        let mountpoint = normalize_unmount_path(&raw_mountpoint);
        let mountpoint_str = mountpoint.display().to_string();
        let endpoint = free_tcp_control_endpoint();
        let registry_path = mounts_registry::registry_path().unwrap();
        mounts_registry::add_entry(&registry_path, sample_nfs_entry(&mountpoint_str, &endpoint))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let cancel = tokio_util::sync::CancellationToken::new();
            let handle = crate::vfs::nfs_control::spawn_server(
                Some(endpoint.clone()),
                nfs_control_state(&mountpoint),
                cancel.clone(),
            )
            .unwrap();
            for _ in 0..20 {
                if crate::vfs::nfs_control::status(&endpoint).await.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            crate::vfs::nfs_control::status(&endpoint).await.unwrap();

            run_unmount_all_live_or_persisted().await.unwrap();

            assert!(cancel.is_cancelled());
            handle.await.unwrap().unwrap();
        });

        let entries = mounts_registry::list_entries(&registry_path).unwrap();
        assert!(entries.is_empty());
    }

    /// Stale detection correctly identifies dead PIDs in the registry.
    #[test]
    fn stale_detection_identifies_dead_pids() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("mounts.json");

        // Add entries with a mix of alive (current process) and dead PIDs.
        let my_pid = std::process::id();
        let dead_pid = 4_000_000_000;

        let mnt_alive = tmp.path().join("alive");
        let mnt_dead = tmp.path().join("dead");
        std::fs::create_dir_all(&mnt_alive).unwrap();
        std::fs::create_dir_all(&mnt_dead).unwrap();

        mounts_registry::add_entry(
            &registry_path,
            sample_entry(&mnt_alive.display().to_string(), my_pid),
        )
        .unwrap();
        mounts_registry::add_entry(
            &registry_path,
            sample_entry(&mnt_dead.display().to_string(), dead_pid),
        )
        .unwrap();

        let entries = mounts_registry::list_entries(&registry_path).unwrap();
        assert_eq!(entries.len(), 2);

        // Partition into alive and stale.
        let alive: Vec<_> = entries.iter().filter(|e| is_pid_alive(e.pid)).collect();
        let stale: Vec<_> = entries.iter().filter(|e| !is_pid_alive(e.pid)).collect();

        assert_eq!(alive.len(), 1);
        assert_eq!(alive[0].mountpoint, mnt_alive.display().to_string());

        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].mountpoint, mnt_dead.display().to_string());
    }

    /// clean_pid_file removes the PID file when it exists.
    #[test]
    fn clean_pid_file_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("mount.pid");
        std::fs::write(&pid_path, "12345").unwrap();
        assert!(pid_path.exists());

        clean_pid_file(tmp.path());
        assert!(!pid_path.exists());
    }

    /// clean_pid_file is a no-op when the file doesn't exist.
    #[test]
    fn clean_pid_file_noop_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // No PID file — should not panic.
        clean_pid_file(tmp.path());
    }

    /// read_pid_file_for_unmount reads a valid PID from the file.
    #[test]
    fn read_pid_file_for_unmount_reads_valid_pid() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mount.pid"), "42\n").unwrap();

        let pid = read_pid_file_for_unmount(tmp.path());
        assert_eq!(pid, Some(42));
    }

    /// read_pid_file_for_unmount returns None when file doesn't exist.
    #[test]
    fn read_pid_file_for_unmount_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = read_pid_file_for_unmount(tmp.path());
        assert_eq!(pid, None);
    }

    /// read_pid_file_for_unmount returns None for invalid content.
    #[test]
    fn read_pid_file_for_unmount_returns_none_for_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mount.pid"), "not-a-number").unwrap();

        let pid = read_pid_file_for_unmount(tmp.path());
        assert_eq!(pid, None);
    }

    #[cfg(feature = "fuse")]
    #[test]
    fn flush_mount_writes_accepts_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        flush_mount_writes(tmp.path()).unwrap();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod mount_list_tests {
    use super::*;

    // --- format_uptime tests ---

    #[test]
    fn uptime_less_than_one_minute() {
        assert_eq!(format_uptime(0), "< 1m");
        assert_eq!(format_uptime(30), "< 1m");
        assert_eq!(format_uptime(59), "< 1m");
    }

    #[test]
    fn uptime_exact_minutes() {
        assert_eq!(format_uptime(60), "1m");
        assert_eq!(format_uptime(300), "5m");
        assert_eq!(format_uptime(3540), "59m");
    }

    #[test]
    fn uptime_hours_and_minutes() {
        assert_eq!(format_uptime(3600), "1h");
        assert_eq!(format_uptime(3600 + 15 * 60), "1h 15m");
        assert_eq!(format_uptime(2 * 3600 + 30 * 60), "2h 30m");
        assert_eq!(format_uptime(23 * 3600 + 59 * 60), "23h 59m");
    }

    #[test]
    fn uptime_days_and_hours() {
        assert_eq!(format_uptime(86400), "1d");
        assert_eq!(format_uptime(86400 + 3600), "1d 1h");
        assert_eq!(format_uptime(3 * 86400 + 5 * 3600), "3d 5h");
        assert_eq!(format_uptime(30 * 86400), "30d");
    }

    // --- redact_source tests ---

    #[test]
    fn redact_short_source_unchanged() {
        assert_eq!(redact_source("crab://b/repo"), "crab://b/repo");
        assert_eq!(redact_source("s3://bucket/data"), "s3://bucket/data");
    }

    #[test]
    fn redact_exactly_20_chars_unchanged() {
        let source = "12345678901234567890"; // exactly 20
        assert_eq!(redact_source(source), source);
    }

    #[test]
    fn redact_long_source_truncated() {
        let source = "crab://my-bucket/org/ml-models/v2";
        let redacted = redact_source(source);
        assert_eq!(redacted, "crab://my-bucket/org\u{2026}");
        assert_eq!(redacted.chars().count(), 21); // 20 chars + ellipsis
    }

    #[test]
    fn redact_local_path_long() {
        let source = "/home/user/projects/my-very-long-repo-name";
        let redacted = redact_source(source);
        assert_eq!(redacted, "/home/user/projects/\u{2026}");
    }

    #[test]
    fn mount_list_payload_redacts_tcp_control_token() {
        let entry = crate::vfs::mounts_registry::MountEntry {
            mountpoint: "/mnt/models".to_owned(),
            source: "crab://bucket/ml-models".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 4_000_000_000,
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "ml-models".to_owned(),
            backend: Some("nfs".to_owned()),
            log_path: Some("/tmp/crab-nfs.log".to_owned()),
            control_endpoint: Some("tcp:127.0.0.1:50000?token=secret-token".to_owned()),
        };

        let payload = MountListEntry::from(entry);

        assert_eq!(
            payload.control_endpoint.as_deref(),
            Some("tcp:127.0.0.1:50000?token=<redacted>")
        );
    }

    // --- short_ref tests ---

    #[test]
    fn short_ref_strips_refs_heads_prefix() {
        assert_eq!(short_ref("refs/heads/main"), "main");
        assert_eq!(short_ref("refs/heads/feature-x"), "feature-x");
    }

    #[test]
    fn short_ref_preserves_other_refs() {
        assert_eq!(short_ref("refs/tags/v1.0"), "refs/tags/v1.0");
        assert_eq!(short_ref("main"), "main");
    }

    // --- parse_iso8601_to_unix tests ---

    #[test]
    fn parse_epoch() {
        assert_eq!(parse_iso8601_to_unix("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_known_timestamp() {
        // 2024-01-15T10:30:00Z = 1705314600
        assert_eq!(
            parse_iso8601_to_unix("2024-01-15T10:30:00Z"),
            Some(1_705_314_600)
        );
    }

    #[test]
    fn parse_invalid_format_returns_none() {
        assert_eq!(parse_iso8601_to_unix("not-a-timestamp"), None);
        assert_eq!(parse_iso8601_to_unix("2024-01-15 10:30:00"), None);
        assert_eq!(parse_iso8601_to_unix(""), None);
    }

    // --- days_from_civil tests ---

    #[test]
    fn days_from_civil_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
    }

    #[test]
    fn days_from_civil_known_date() {
        // 2024-01-15 is day 19737 since epoch
        assert_eq!(days_from_civil(2024, 1, 15), Some(19_737));
    }

    #[test]
    fn days_from_civil_invalid_month() {
        assert_eq!(days_from_civil(2024, 0, 1), None);
        assert_eq!(days_from_civil(2024, 13, 1), None);
    }

    // --- truncate_col tests ---

    #[test]
    fn truncate_col_short_string() {
        assert_eq!(truncate_col("hello", 10), "hello");
    }

    #[test]
    fn truncate_col_exact_fit() {
        assert_eq!(truncate_col("hello", 5), "hello");
    }

    #[test]
    fn truncate_col_too_long() {
        let result = truncate_col("hello world", 8);
        assert_eq!(result, "hello w\u{2026}");
    }

    // --- Integration-style test for run_mount_list ---

    #[test]
    fn run_mount_list_empty_registry() {
        // Set HOME to a temp dir so registry_path() resolves to an empty file.
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let result = run_mount_list(false);
        assert!(result.is_ok());
    }

    #[test]
    fn run_mount_list_json_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let result = run_mount_list(true);
        assert!(result.is_ok());
    }

    #[test]
    fn run_mount_list_with_entries() {
        use crate::vfs::mounts_registry;

        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());

        // Create the mounts directory and add entries.
        let mounts_dir = tmp.path().join(".crab").join("mounts");
        std::fs::create_dir_all(&mounts_dir).unwrap();
        let registry_path = mounts_dir.join("mounts.json");

        let entry = mounts_registry::MountEntry {
            mountpoint: "/mnt/models".to_owned(),
            source: "crab://bucket/ml-models".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 4_000_000_000, // non-existent PID -> stale
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "ml-models".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        };
        mounts_registry::add_entry(&registry_path, entry).unwrap();

        // Table output should work.
        let result = run_mount_list(false);
        assert!(result.is_ok());

        // JSON output should work.
        let result = run_mount_list(true);
        assert!(result.is_ok());
    }
}

#[cfg(test)]
#[cfg(all(feature = "fuse", unix))]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod mount_overlay_context_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;
    use crate::vfs::mounts_registry::{self, MountEntry};

    #[tokio::test(flavor = "multi_thread")]
    async fn registered_mount_context_skips_stale_fuse_ipc_status() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let mountpoint = tmp.path().join("nfs-view");
        std::fs::create_dir_all(&mountpoint).unwrap();
        let mountpoint = std::fs::canonicalize(&mountpoint).unwrap();
        let mountpoint_str = mountpoint.display().to_string();

        let registry_path = mounts_registry::registry_path().unwrap();
        let entry = MountEntry {
            mountpoint: mountpoint_str.clone(),
            source: "crab://bucket/nfs-repo".to_owned(),
            git_ref: "HEAD".to_owned(),
            pid: 12345,
            start_time: "2026-07-06T17:35:12Z".to_owned(),
            read_only: false,
            name: "nfs-repo".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        };
        mounts_registry::add_entry(&registry_path, entry).unwrap();

        let socket_path = crate::vfs::ipc_client::default_socket_path().unwrap();
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let contacted = Arc::new(AtomicBool::new(false));
        let contacted_for_task = Arc::clone(&contacted);
        let listener_task = tokio::spawn(async move {
            let Ok((stream, _addr)) = listener.accept().await else {
                return;
            };
            contacted_for_task.store(true, Ordering::SeqCst);
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines.next_line().await;
            let response = crate::vfs::ipc_server::IpcResponse::status_ok(
                crate::vfs::ipc_server::MountStatus {
                    mountpoint: mountpoint_str,
                    remote: "crab://bucket/stale-fuse-repo".to_owned(),
                    r#ref: "refs/heads/stale".to_owned(),
                    read_only: false,
                    head_oid: None,
                    pid: Some(99999),
                },
            );
            let mut line = serde_json::to_string(&response).unwrap();
            line.push('\n');
            let _ = writer.write_all(line.as_bytes()).await;
        });

        let context = resolve_mount_overlay_context(&mountpoint).await.unwrap();
        listener_task.abort();

        assert_eq!(context.mountpoint, mountpoint.display().to_string());
        assert_eq!(context.ref_name, Some("HEAD".to_owned()));
        assert!(!context.invalidate_via_ipc);
        assert!(!contacted.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
#[cfg(feature = "nfs")]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod nfs_mount_registration_tests {
    use super::*;
    use crate::test::git_repo::GIT_DIR_MUTEX;
    use crate::vfs::mounts_registry;

    struct CurrentDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        original: PathBuf,
    }

    impl CurrentDirGuard {
        fn set(path: &Path) -> Self {
            let lock = GIT_DIR_MUTEX
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self {
                _lock: lock,
                original,
            }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.original).unwrap();
        }
    }

    fn opts(repo: &str, mountpoint: PathBuf) -> NewMountOpts {
        NewMountOpts {
            repo: repo.to_owned(),
            mountpoint,
            backend: MountBackend::Nfs,
            git_ref: None,
            foreground: false,
            read_only: false,
            no_refresh: false,
            allow_nested: false,
            name: None,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[test]
    fn background_registration_stores_canonical_local_source() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let mountpoint = tmp.path().join("view");
        let log_path = tmp.path().join("nfs.log");
        let control_endpoint = Some("tcp:127.0.0.1:50000?token=parent-secret".to_owned());
        std::fs::create_dir_all(&mountpoint).unwrap();
        let _cwd = CurrentDirGuard::set(tmp.path());
        let _home = set_test_home(tmp.path());

        register_nfs_background_mount(
            &opts("./repo", mountpoint.clone()),
            &mountpoint,
            12345,
            &log_path,
            control_endpoint.clone(),
        )
        .unwrap();

        let entries =
            mounts_registry::read_entries(&mounts_registry::registry_path().unwrap()).unwrap();
        let canonical_repo = std::fs::canonicalize(repo).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, canonical_repo.display().to_string());
        assert_eq!(entries[0].name, "repo");
        assert_eq!(entries[0].backend.as_deref(), Some("nfs"));
        assert_eq!(
            entries[0].log_path.as_deref(),
            Some(log_path.to_string_lossy().as_ref())
        );
        assert_eq!(entries[0].control_endpoint, control_endpoint);
    }

    #[test]
    fn background_registration_preserves_remote_source() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let mountpoint = tmp.path().join("view");
        let log_path = tmp.path().join("nfs.log");
        let control_endpoint = Some("tcp:127.0.0.1:50001?token=parent-secret".to_owned());
        std::fs::create_dir_all(&mountpoint).unwrap();

        register_nfs_background_mount(
            &opts("crab://bucket/repo", mountpoint.clone()),
            &mountpoint,
            12345,
            &log_path,
            control_endpoint.clone(),
        )
        .unwrap();

        let entries =
            mounts_registry::read_entries(&mounts_registry::registry_path().unwrap()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "crab://bucket/repo");
        assert_eq!(entries[0].name, "repo");
        assert_eq!(entries[0].backend.as_deref(), Some("nfs"));
        assert_eq!(
            entries[0].log_path.as_deref(),
            Some(log_path.to_string_lossy().as_ref())
        );
        assert_eq!(entries[0].control_endpoint, control_endpoint);
    }
}

#[cfg(test)]
#[cfg(any(feature = "fuse", feature = "nfs"))]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod mount_status_tests {
    use super::*;
    use crate::vfs::mounts_registry::{self, MountEntry};

    fn sample_entry(mountpoint: &str, pid: u32) -> MountEntry {
        MountEntry {
            mountpoint: mountpoint.to_owned(),
            source: "crab://bucket/ml-models".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid,
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "ml-models".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        }
    }

    // --- format_relative_time tests ---

    #[test]
    fn relative_time_just_now() {
        assert_eq!(format_relative_time(0), "just now");
        assert_eq!(format_relative_time(30), "just now");
        assert_eq!(format_relative_time(59), "just now");
    }

    #[test]
    fn relative_time_minutes() {
        assert_eq!(format_relative_time(60), "1 minute ago");
        assert_eq!(format_relative_time(120), "2 minutes ago");
        assert_eq!(format_relative_time(300), "5 minutes ago");
        assert_eq!(format_relative_time(3540), "59 minutes ago");
    }

    #[test]
    fn relative_time_hours() {
        assert_eq!(format_relative_time(3600), "1 hour ago");
        assert_eq!(format_relative_time(7200), "2 hours ago");
        assert_eq!(format_relative_time(82800), "23 hours ago");
    }

    #[test]
    fn relative_time_days() {
        assert_eq!(format_relative_time(86400), "1 day ago");
        assert_eq!(format_relative_time(172800), "2 days ago");
        assert_eq!(format_relative_time(604800), "7 days ago");
    }

    // --- format_cache_size tests ---

    #[test]
    fn cache_size_bytes() {
        assert_eq!(format_cache_size(0), "0 B");
        assert_eq!(format_cache_size(512), "512 B");
        assert_eq!(format_cache_size(1023), "1023 B");
    }

    #[test]
    fn cache_size_kilobytes() {
        assert_eq!(format_cache_size(1024), "1.0 KB");
        assert_eq!(format_cache_size(1536), "1.5 KB");
        assert_eq!(format_cache_size(10240), "10.0 KB");
    }

    #[test]
    fn cache_size_megabytes() {
        assert_eq!(format_cache_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_cache_size(128 * 1024 * 1024), "128.0 MB");
    }

    #[test]
    fn cache_size_gigabytes() {
        assert_eq!(format_cache_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_cache_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    // --- build_mount_status tests ---

    #[test]
    fn status_from_registry_entry_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();

        let entry = Some(sample_entry("/mnt/models", 4_000_000_000));
        let status = build_mount_status(entry.as_ref(), &cache_dir, "/mnt/models", false);

        assert_eq!(status.mountpoint, "/mnt/models");
        assert_eq!(status.source, Some("crab://bucket/ml-models".to_owned()));
        assert_eq!(status.git_ref, Some("main".to_owned()));
        assert!(status.state.contains("stale"));
        assert_eq!(status.pid, Some(4_000_000_000));
        assert_eq!(status.mode, "read-write");
        assert!(!status.read_only);
        assert_eq!(status.name, Some("ml-models".to_owned()));
    }

    #[test]
    fn status_from_registry_entry_running() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();

        // Use current process PID to simulate a running mount.
        let my_pid = std::process::id();
        let entry = Some(sample_entry("/mnt/models", my_pid));
        let status = build_mount_status(entry.as_ref(), &cache_dir, "/mnt/models", false);

        assert!(status.state.contains("running"));
        assert_eq!(status.pid, Some(my_pid));
    }

    #[test]
    fn status_no_registry_entry_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let cache_dir = tmp.path().to_path_buf();

        let status = build_mount_status(None, &cache_dir, "/mnt/models", false);

        assert_eq!(status.mountpoint, "/mnt/models");
        assert_eq!(status.source, None);
        assert_eq!(status.head_oid, None);
        assert_eq!(status.state, "not mounted");
        assert_eq!(status.pid, None);
        assert_eq!(status.overlay_dirty_count, 0);
        assert_eq!(status.cache_size_bytes, 0);
    }

    #[test]
    fn status_counts_delete_only_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();
        let store = crate::vfs::overlay::OverlayStore::open(
            &cache_dir.join("overlay.db"),
            &cache_dir.join("overlay/upper"),
        )
        .unwrap();
        use crate::vfs::engine::OverlayWriter;
        store.remove("deleted.txt").unwrap();
        drop(store);

        let status = build_mount_status(None, &cache_dir, "/mnt/models", true);

        assert_eq!(status.overlay_dirty_count, 1);
        assert_eq!(
            status.overlay_dirty_paths,
            Some(vec!["deleted.txt".to_owned()])
        );
    }

    #[test]
    fn status_from_live_control_reads_overlay_dirty_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let remote = "crab://bucket/ml-models";
        let cache_dir = tmp
            .path()
            .join(".crab/mounts/repos")
            .join(crate::vfs::clone_cache::compute_cache_hash(remote));
        let store = crate::vfs::overlay::OverlayStore::open(
            &cache_dir.join("overlay.db"),
            &cache_dir.join("overlay/upper"),
        )
        .unwrap();
        use crate::vfs::engine::OverlayWriter;
        store.remove("deleted.txt").unwrap();
        drop(store);

        let status = build_mount_status_from_control(
            crate::vfs::mount_control::MountControlStatus {
                backend: crate::vfs::mount_control::MountControlBackend::Fuse,
                mountpoint: PathBuf::from("/mnt/models"),
                source: Some(remote.to_owned()),
                head_ref: Some("refs/heads/main".to_owned()),
                read_only: false,
                head_oid: Some("abc123".to_owned()),
                pid: Some(12345),
                #[cfg(feature = "nfs")]
                nfs_runtime: None,
            },
            None,
            tmp.path(),
            true,
        )
        .unwrap();

        assert_eq!(status.source, Some(remote.to_owned()));
        assert_eq!(status.git_ref, Some("main".to_owned()));
        assert_eq!(status.head_oid, Some("abc123".to_owned()));
        assert_eq!(status.mode, "read-write");
        assert_eq!(status.state, "running (PID 12345)");
        assert_eq!(status.pid, Some(12345));
        assert_eq!(status.overlay_dirty_count, 1);
        assert_eq!(
            status.overlay_dirty_paths,
            Some(vec!["deleted.txt".to_owned()])
        );
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn status_live_or_persisted_falls_back_when_nfs_control_unavailable() {
        let _env_lock = crate::test::git_repo::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let raw_mountpoint = tmp.path().join("view");
        std::fs::create_dir_all(&raw_mountpoint).unwrap();
        let mountpoint = std::fs::canonicalize(&raw_mountpoint).unwrap();
        let registry_path = mounts_registry::registry_path().unwrap();
        let mut entry = sample_entry(&mountpoint.display().to_string(), 4_000_000_000);
        entry.backend = Some("nfs".to_owned());
        entry.control_endpoint = Some(closed_tcp_control_endpoint());
        mounts_registry::add_entry(&registry_path, entry).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime
            .block_on(run_mount_status_live_or_persisted(
                &mountpoint,
                false,
                true,
                false,
            ))
            .unwrap();
    }

    #[test]
    #[cfg(feature = "nfs")]
    fn status_live_only_reports_unavailable_nfs_control() {
        let _env_lock = crate::test::git_repo::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let raw_mountpoint = tmp.path().join("view");
        std::fs::create_dir_all(&raw_mountpoint).unwrap();
        let mountpoint = std::fs::canonicalize(&raw_mountpoint).unwrap();
        let registry_path = mounts_registry::registry_path().unwrap();
        let mut entry = sample_entry(&mountpoint.display().to_string(), 4_000_000_000);
        entry.backend = Some("nfs".to_owned());
        entry.control_endpoint = Some(closed_tcp_control_endpoint());
        mounts_registry::add_entry(&registry_path, entry).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let error = runtime
            .block_on(run_mount_status_live_or_persisted(
                &mountpoint,
                false,
                true,
                true,
            ))
            .unwrap_err();

        assert!(matches!(
            error,
            CrabError::Configuration { ref key, ref origin }
                if origin == "crab mount status --live-only"
                    && key.contains("live mount control is unavailable")
        ));
    }

    #[cfg(feature = "nfs")]
    fn closed_tcp_control_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("tcp:{addr}?token=mount-status-test")
    }

    #[test]
    fn display_control_endpoint_redacts_tcp_token() {
        assert_eq!(
            display_control_endpoint("tcp:127.0.0.1:50000?token=secret-token"),
            "tcp:127.0.0.1:50000?token=<redacted>"
        );
        assert_eq!(
            display_control_endpoint("unix:/tmp/crab-nfs.sock"),
            "unix:/tmp/crab-nfs.sock"
        );
    }

    #[test]
    fn status_payloads_redact_tcp_control_token() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();
        let mut entry = sample_entry("/mnt/models", 4_000_000_000);
        entry.backend = Some("nfs".to_owned());
        entry.control_endpoint = Some("tcp:127.0.0.1:50000?token=secret-token".to_owned());

        let persisted = build_mount_status(Some(&entry), &cache_dir, "/mnt/models", false);

        assert_eq!(
            persisted.control_endpoint.as_deref(),
            Some("tcp:127.0.0.1:50000?token=<redacted>")
        );

        let live = build_mount_status_from_control(
            crate::vfs::mount_control::MountControlStatus {
                backend: crate::vfs::mount_control::MountControlBackend::Nfs,
                mountpoint: PathBuf::from("/mnt/models"),
                source: None,
                head_ref: Some("refs/heads/main".to_owned()),
                read_only: false,
                head_oid: Some("abc123".to_owned()),
                pid: Some(12345),
                #[cfg(feature = "nfs")]
                nfs_runtime: None,
            },
            Some(&entry),
            &cache_dir,
            false,
        )
        .unwrap();

        assert_eq!(
            live.control_endpoint.as_deref(),
            Some("tcp:127.0.0.1:50000?token=<redacted>")
        );
    }

    #[test]
    fn status_read_only_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().to_path_buf();

        let mut entry = sample_entry("/mnt/models", 4_000_000_000);
        entry.read_only = true;
        entry.backend = Some("nfs".to_owned());
        entry.log_path = Some("/tmp/crab-nfs.log".to_owned());
        let status = build_mount_status(Some(&entry), &cache_dir, "/mnt/models", false);

        assert_eq!(status.mode, "read-only");
        assert!(status.read_only);
        assert_eq!(status.backend.as_deref(), Some("nfs"));
        assert_eq!(status.log_path.as_deref(), Some("/tmp/crab-nfs.log"));
    }

    // --- JSON serialization tests ---

    #[test]
    fn status_json_serialization() {
        let status = MountStatusPayload {
            mountpoint: "/mnt/models".to_owned(),
            backend: Some("nfs".to_owned()),
            source: Some("crab://bucket/ml-models".to_owned()),
            git_ref: Some("main".to_owned()),
            head_oid: Some("abc123def456".to_owned()),
            mode: "read-write".to_owned(),
            state: "running (PID 12345)".to_owned(),
            pid: Some(12345),
            overlay_dirty_count: 3,
            overlay_dirty_paths: None,
            cache_size_bytes: 128 * 1024 * 1024,
            cache_size_human: "128.0 MB".to_owned(),
            last_refresh: Some("2024-01-15T10:30:00Z".to_owned()),
            last_refresh_relative: Some("2 minutes ago".to_owned()),
            uptime: Some("2h 15m".to_owned()),
            start_time: Some("2024-01-15T08:15:00Z".to_owned()),
            read_only: false,
            name: Some("ml-models".to_owned()),
            log_path: Some("/tmp/crab-nfs.log".to_owned()),
            control_endpoint: Some("unix:/tmp/crab-nfs.sock".to_owned()),
            #[cfg(feature = "nfs")]
            nfs_runtime: None,
        };

        let json = serde_json::to_string_pretty(&status).unwrap();

        // Verify key fields are present in the JSON output.
        assert!(json.contains("\"mountpoint\""));
        assert!(json.contains("\"backend\""));
        assert!(json.contains("\"source\""));
        assert!(json.contains("\"ref\""));
        assert!(json.contains("\"head_oid\""));
        assert!(json.contains("\"mode\""));
        assert!(json.contains("\"state\""));
        assert!(json.contains("\"pid\""));
        assert!(json.contains("\"overlay_dirty_count\""));
        assert!(json.contains("\"cache_size_bytes\""));
        assert!(json.contains("\"cache_size_human\""));
        assert!(json.contains("\"last_refresh\""));
        assert!(json.contains("\"last_refresh_relative\""));
        assert!(json.contains("\"uptime\""));
        assert!(json.contains("\"read_only\""));
        assert!(json.contains("\"name\""));
        assert!(json.contains("\"log_path\""));
        assert!(json.contains("\"control_endpoint\""));
        #[cfg(feature = "nfs")]
        assert!(!json.contains("\"nfs_runtime\""));

        // Verify "ref" is used instead of "git_ref".
        assert!(!json.contains("\"git_ref\""));
    }

    #[test]
    fn status_json_skips_none_fields() {
        let status = MountStatusPayload {
            mountpoint: "/mnt/models".to_owned(),
            backend: None,
            source: None,
            git_ref: None,
            head_oid: None,
            mode: "read-only".to_owned(),
            state: "not mounted".to_owned(),
            pid: None,
            overlay_dirty_count: 0,
            overlay_dirty_paths: None,
            cache_size_bytes: 0,
            cache_size_human: "0 B".to_owned(),
            last_refresh: None,
            last_refresh_relative: None,
            uptime: None,
            start_time: None,
            read_only: true,
            name: None,
            log_path: None,
            control_endpoint: None,
            #[cfg(feature = "nfs")]
            nfs_runtime: None,
        };

        let json = serde_json::to_string_pretty(&status).unwrap();

        // None fields should be omitted from JSON.
        assert!(!json.contains("\"source\""));
        assert!(!json.contains("\"backend\""));
        assert!(!json.contains("\"ref\""));
        assert!(!json.contains("\"head_oid\""));
        assert!(!json.contains("\"pid\""));
        assert!(!json.contains("\"uptime\""));
        assert!(!json.contains("\"name\""));
        assert!(!json.contains("\"log_path\""));
        assert!(!json.contains("\"control_endpoint\""));
        #[cfg(feature = "nfs")]
        assert!(!json.contains("\"nfs_runtime\""));
        assert!(!json.contains("\"last_refresh\""));

        // Required fields should still be present.
        assert!(json.contains("\"mountpoint\""));
        assert!(json.contains("\"mode\""));
        assert!(json.contains("\"state\""));
        assert!(json.contains("\"overlay_dirty_count\""));
        assert!(json.contains("\"cache_size_bytes\""));
    }

    // --- Integration test for run_mount_status ---

    #[test]
    fn run_mount_status_no_mounts_dot_path() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());

        // Create the mounts directory (empty registry).
        let mounts_dir = tmp.path().join(".crab").join("mounts");
        std::fs::create_dir_all(&mounts_dir).unwrap();
        std::fs::write(mounts_dir.join("mounts.json"), "[]").unwrap();

        // With "." and no mounts, should report "not mounted".
        let result = run_mount_status(Path::new("."), false, false);
        assert!(result.is_ok());

        // JSON mode should also work.
        let result = run_mount_status(Path::new("."), false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn run_mount_status_single_mount_dot_path() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());

        // Create the mounts directory with one entry.
        let mounts_dir = tmp.path().join(".crab").join("mounts");
        std::fs::create_dir_all(&mounts_dir).unwrap();

        let entry = sample_entry("/mnt/models", 4_000_000_000);
        mounts_registry::add_entry(&mounts_dir.join("mounts.json"), entry).unwrap();

        // With "." and one mount, should use that mount.
        let result = run_mount_status(Path::new("."), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn run_mount_status_multiple_mounts_dot_path_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());

        // Create the mounts directory with two entries.
        let mounts_dir = tmp.path().join(".crab").join("mounts");
        std::fs::create_dir_all(&mounts_dir).unwrap();
        let registry_path = mounts_dir.join("mounts.json");

        mounts_registry::add_entry(&registry_path, sample_entry("/mnt/a", 4_000_000_000)).unwrap();
        mounts_registry::add_entry(&registry_path, sample_entry("/mnt/b", 4_000_000_001)).unwrap();

        // Verify entries were written.
        let entries = mounts_registry::read_entries(&registry_path).unwrap();
        assert_eq!(entries.len(), 2, "expected 2 entries in registry");

        // With "." and multiple mounts, should error asking user to specify.
        let result = run_mount_status(Path::new("."), false, false);
        assert!(
            result.is_err(),
            "expected error for ambiguous mountpoint with multiple mounts"
        );
    }
}

#[cfg(all(test, feature = "nfs", not(feature = "fuse")))]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod mount_control_command_tests {
    use super::*;

    #[test]
    fn refresh_reports_missing_live_mount_without_fuse_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let error = runtime
            .block_on(run_mount_control_refresh(Path::new(
                "/tmp/crab-missing-nfs-refresh",
            )))
            .unwrap_err();

        assert_mount_control_target_missing(error, "crab mount refresh");
    }

    #[test]
    fn switch_reports_missing_live_mount_without_fuse_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = set_test_home(tmp.path());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let error = runtime
            .block_on(run_mount_control_switch(
                Path::new("/tmp/crab-missing-nfs-switch"),
                "refs/heads/dev",
            ))
            .unwrap_err();

        assert_mount_control_target_missing(error, "crab mount switch");
    }

    fn assert_mount_control_target_missing(error: CrabError, origin: &str) {
        let CrabError::Configuration {
            key,
            origin: actual_origin,
        } = error
        else {
            panic!("expected configuration error");
        };
        assert_eq!(actual_origin, origin);
        assert!(key.contains("no live mount found"));
        assert!(key.contains("crab mount list"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod mount_clean_tests {
    use super::*;

    #[test]
    fn compute_source_hash_deterministic() {
        let h1 = compute_source_hash("crab://bucket/repo");
        let h2 = compute_source_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 12);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compute_source_hash_normalizes_scheme_case() {
        let h1 = compute_source_hash("CRAB://bucket/repo");
        let h2 = compute_source_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn compute_source_hash_trims_trailing_slashes() {
        let h1 = compute_source_hash("crab://bucket/repo///");
        let h2 = compute_source_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn compute_source_hash_different_urls_differ() {
        let h1 = compute_source_hash("crab://bucket/repo-a");
        let h2 = compute_source_hash("crab://bucket/repo-b");
        assert_ne!(h1, h2);
    }

    #[test]
    fn list_repo_dirs_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("nonexistent");
        let dirs = list_repo_dirs(&nonexistent);
        assert!(dirs.is_empty());
    }

    #[test]
    fn list_repo_dirs_finds_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let repos_dir = tmp.path().join("repos");
        std::fs::create_dir_all(repos_dir.join("abc123def456")).unwrap();
        std::fs::create_dir_all(repos_dir.join("fedcba987654")).unwrap();
        // Create a file (should be ignored).
        std::fs::write(repos_dir.join("not-a-dir.txt"), "hello").unwrap();

        let dirs = list_repo_dirs(&repos_dir);
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn dir_size_bytes_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let size = dir_size_bytes(tmp.path());
        assert_eq!(size, 0);
    }

    #[test]
    fn dir_size_bytes_with_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap(); // 5 bytes
        std::fs::write(tmp.path().join("b.txt"), "world!").unwrap(); // 6 bytes
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/c.txt"), "nested").unwrap(); // 6 bytes

        let size = dir_size_bytes(tmp.path());
        assert_eq!(size, 17);
    }

    #[test]
    fn clean_identifies_inactive_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts_base = tmp.path().join(".crab").join("mounts");
        let repos_dir = mounts_base.join("repos");
        let registry_path = mounts_base.join("mounts.json");

        // Create two cache directories.
        let active_hash = compute_source_hash("crab://bucket/active-repo");
        let inactive_hash = "deadbeef1234"; // Not matching any active source.
        std::fs::create_dir_all(repos_dir.join(&active_hash)).unwrap();
        std::fs::create_dir_all(repos_dir.join(inactive_hash)).unwrap();
        std::fs::write(repos_dir.join(inactive_hash).join("data.bin"), "some data").unwrap();

        // Register one active mount.
        std::fs::create_dir_all(&mounts_base).unwrap();
        let entry = crate::vfs::mounts_registry::MountEntry {
            mountpoint: "/mnt/active".to_owned(),
            source: "crab://bucket/active-repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: std::process::id(),
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "active-repo".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        };
        crate::vfs::mounts_registry::add_entry(&registry_path, entry).unwrap();

        // List repo dirs.
        let repo_dirs = list_repo_dirs(&repos_dir);
        assert_eq!(repo_dirs.len(), 2);

        // Read active mounts.
        let entries = crate::vfs::mounts_registry::read_entries(&registry_path).unwrap();
        let active_hashes: std::collections::HashSet<String> = entries
            .iter()
            .map(|e| compute_source_hash(&e.source))
            .collect();

        // Identify inactive.
        let inactive_dirs: Vec<PathBuf> = repo_dirs
            .into_iter()
            .filter(|dir| {
                let dir_name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                !active_hashes.contains(&dir_name)
            })
            .collect();

        assert_eq!(inactive_dirs.len(), 1);
        assert!(inactive_dirs[0].ends_with(inactive_hash));
    }

    #[test]
    fn clean_all_refuses_with_active_mounts() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts_base = tmp.path().join(".crab").join("mounts");
        let registry_path = mounts_base.join("mounts.json");

        std::fs::create_dir_all(&mounts_base).unwrap();

        // Register an active mount (using current PID so it appears alive).
        let entry = crate::vfs::mounts_registry::MountEntry {
            mountpoint: "/mnt/active".to_owned(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: std::process::id(),
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "repo".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        };
        crate::vfs::mounts_registry::add_entry(&registry_path, entry).unwrap();

        // clean --all should refuse.
        let result = run_mount_clean_all(&mounts_base, &registry_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cannot clean all"));
    }

    #[test]
    fn clean_all_succeeds_with_no_active_mounts() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts_base = tmp.path().join(".crab").join("mounts");
        let registry_path = mounts_base.join("mounts.json");

        // Create some data.
        std::fs::create_dir_all(mounts_base.join("repos").join("abc123")).unwrap();
        std::fs::write(mounts_base.join("repos").join("abc123").join("data"), "x").unwrap();
        std::fs::create_dir_all(mounts_base.join("cache")).unwrap();
        std::fs::write(mounts_base.join("cache").join("chunk"), "y").unwrap();

        // Register a stale mount (dead PID).
        std::fs::create_dir_all(&mounts_base).unwrap();
        let entry = crate::vfs::mounts_registry::MountEntry {
            mountpoint: "/mnt/stale".to_owned(),
            source: "crab://bucket/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid: 4_000_000_000, // Very high PID, almost certainly dead.
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "repo".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        };
        crate::vfs::mounts_registry::add_entry(&registry_path, entry).unwrap();

        // clean --all should succeed (stale mount is not "active").
        let result = run_mount_clean_all(&mounts_base, &registry_path);
        assert!(result.is_ok());

        // Everything under mounts_base should be gone.
        assert!(!mounts_base.join("repos").exists());
        assert!(!mounts_base.join("cache").exists());
        assert!(!registry_path.exists());
    }
}
