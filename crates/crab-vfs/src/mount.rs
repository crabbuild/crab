//! Mount/unmount lifecycle and PID file management.
//!
//! Handles occupied-target admission, creating FUSE sessions,
//! writing PID files for daemon mode, and graceful shutdown via
//! `CancellationToken`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuser::{Config as FuseConfig, MountOption, Session, SessionACL};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::core::error::{CrabError, Result};
use crate::engine::VfsEngine;
use crate::fuse::{CrabFs, FuseInvalidationIndex};
use crate::resolver::FuseResolver;

// ---------------------------------------------------------------------------
// Mount configuration
// ---------------------------------------------------------------------------

/// Configuration for a FUSE mount.
pub struct MountConfig {
    /// Path where the FUSE filesystem will be mounted.
    pub mountpoint: PathBuf,
    /// Absolute path to the real `.git` directory.
    pub git_dir: String,
    /// Whether to write a PID file (daemon mode).
    pub write_pid: bool,
    /// Path to the `.crab` directory for PID file storage.
    pub crab_dir: PathBuf,
    /// Whether to mount read-only.
    pub read_only: bool,
}

/// FUSE session plus shared state needed for later kernel invalidations.
pub struct MountedSession {
    pub session: Session<CrabFs>,
    pub invalidation_index: FuseInvalidationIndex,
}

// ---------------------------------------------------------------------------
// PID file management
// ---------------------------------------------------------------------------

/// Path to the PID file within the `.crab` directory.
fn pid_file_path(crab_dir: &Path) -> PathBuf {
    crab_dir.join("mount.pid")
}

/// Write the current process PID to `.crab/mount.pid`.
fn write_pid_file(crab_dir: &Path) -> Result<()> {
    let path = pid_file_path(crab_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    std::fs::write(&path, pid.to_string())?;
    debug!(pid, path = %path.display(), "wrote PID file");
    Ok(())
}

/// Remove the PID file if it exists.
fn remove_pid_file(crab_dir: &Path) {
    let path = pid_file_path(crab_dir);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(path = %path.display(), error = %e, "failed to remove PID file");
        } else {
            debug!(path = %path.display(), "removed PID file");
        }
    }
}

/// Read the PID from an existing PID file, if any.
pub fn read_pid_file(crab_dir: &Path) -> Option<u32> {
    let path = pid_file_path(crab_dir);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Detach the native client and join every FUSE callback before returning.
/// Busy or uncertain native state retains serving ownership until detachment.
pub fn unmount_background_session(
    session: fuser::BackgroundSession,
    mountpoint: &Path,
) -> Result<()> {
    loop {
        let result = crate::native_mounts::contains(mountpoint);
        match result {
            Ok(false) => break,
            Ok(true) => {
                #[cfg(target_os = "linux")]
                let result = std::process::Command::new("fusermount3")
                    .arg("-u")
                    .arg(mountpoint)
                    .output();
                #[cfg(not(target_os = "linux"))]
                let result = std::process::Command::new("umount")
                    .arg(mountpoint)
                    .output();
                match result {
                    Ok(output) if output.status.success() => continue,
                    Ok(output) => {
                        warn!(mountpoint = %mountpoint.display(), status = %output.status, "FUSE unmount pending; retaining serving owner");
                    }
                    Err(error) => {
                        warn!(%error, mountpoint = %mountpoint.display(), "FUSE unmount pending; retaining serving owner");
                    }
                }
            }
            Err(error) => {
                warn!(%error, mountpoint = %mountpoint.display(), "FUSE mount state unavailable; retaining serving owner");
            }
        }
        // Keep callbacks and cache ownership while native clients are busy.
        // A deadline cannot authorize forced detach or abandoning their thread.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    // Join only after confirmed native detachment; fuser's auto-unmount socket
    // closure alone does not prove that the kernel client has detached.
    session.join().map_err(CrabError::Io)
}

/// Mount the FUSE filesystem and return its owned session.
/// Returns an error if the target is already mounted or setup fails.
pub fn mount(
    config: &MountConfig,
    resolver: Arc<FuseResolver>,
    engine: Arc<VfsEngine>,
    rt: Handle,
) -> Result<MountedSession> {
    let mountpoint = &config.mountpoint;

    // Ensure mountpoint directory exists.
    if !mountpoint.exists() {
        std::fs::create_dir_all(mountpoint)?;
    }

    crate::fuse_prereq::ensure_fuse_device_available()?;

    let mountpoint = &std::fs::canonicalize(mountpoint)?;
    if crate::native_mounts::contains(mountpoint)? {
        return Err(CrabError::Configuration {
            key: format!(
                "{} already contains a mounted filesystem; unmount it before mounting",
                mountpoint.display()
            ),
            origin: "FUSE mount".into(),
        });
    }

    let fs = CrabFs::new(resolver, engine, &config.git_dir, rt);
    let invalidation_index = fs.invalidation_index();

    let fuse_config = mount_config(config.read_only);

    let session = Session::new(fs, mountpoint, &fuse_config)?;

    // Write PID file if requested (daemon mode).
    if config.write_pid {
        write_pid_file(&config.crab_dir)?;
    }

    info!(mountpoint = %mountpoint.display(), "FUSE filesystem mounted");
    Ok(MountedSession {
        session,
        invalidation_index,
    })
}

fn mount_options(read_only: bool) -> Vec<MountOption> {
    let mut options = vec![
        MountOption::FSName("crab".to_owned()),
        MountOption::Subtype("crab".to_owned()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
    ];

    // fuser implements AutoUnmount by implicitly adding AllowOther when no
    // allow option is present. On macOS that can stall the macFUSE mount
    // handshake before Session::new returns, so rely on explicit unmount.
    #[cfg(not(target_os = "macos"))]
    options.push(MountOption::AutoUnmount);

    if read_only {
        options.push(MountOption::RO);
    } else {
        options.push(MountOption::RW);
    }
    options
}

fn mount_config(read_only: bool) -> FuseConfig {
    let mount_options = mount_options(read_only);
    let acl = if mount_options.contains(&MountOption::AutoUnmount) {
        SessionACL::All
    } else {
        SessionACL::Owner
    };

    let mut config = FuseConfig::default();
    config.mount_options = mount_options;
    config.acl = acl;
    config
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

/// Run the FUSE session in the foreground, blocking until the
/// cancellation token is triggered (SIGINT/SIGTERM).
///
/// On cancellation: unmounts the filesystem, removes the PID file,
/// and returns.
pub fn run_until_cancelled(
    session: Session<CrabFs>,
    mountpoint: &Path,
    cancel: CancellationToken,
    crab_dir: &Path,
) -> Result<()> {
    let crab_dir_owned = crab_dir.to_path_buf();
    let background = session.spawn()?;

    let result = loop {
        if cancel.is_cancelled() {
            info!("cancellation received, unmounting FUSE filesystem");
            break unmount_background_session(background, mountpoint);
        }

        if background.guard.is_finished() {
            break unmount_background_session(background, mountpoint);
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    remove_pid_file(&crab_dir_owned);
    result?;

    info!("FUSE session ended");
    Ok(())
}

/// Install signal handlers that cancel the token on SIGINT/SIGTERM.
pub fn install_signal_handler(cancel: CancellationToken, rt: &Handle) {
    rt.spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "failed to register SIGINT handler");
                    return;
                }
            };
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "failed to register SIGTERM handler, using SIGINT only");
                    // Fall back to ctrl_c only.
                    let _ = tokio::signal::ctrl_c().await;
                    info!("received SIGINT (ctrl-c fallback)");
                    cancel.cancel();
                    return;
                }
            };

            tokio::select! {
                _ = sigint.recv() => {
                    info!("received SIGINT");
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            info!("received ctrl-c");
        }

        cancel.cancel();
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn pid_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let crab_dir = dir.path().join(".crab");

        write_pid_file(&crab_dir).unwrap();

        let pid = read_pid_file(&crab_dir).unwrap();
        assert_eq!(pid, std::process::id());

        remove_pid_file(&crab_dir);
        assert!(read_pid_file(&crab_dir).is_none());
    }

    #[test]
    fn pid_file_path_is_correct() {
        let p = pid_file_path(Path::new("/tmp/.crab"));
        assert_eq!(p, PathBuf::from("/tmp/.crab/mount.pid"));
    }

    #[test]
    fn remove_pid_file_nonexistent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        // Should not panic or error.
        remove_pid_file(dir.path());
    }

    #[test]
    fn mount_options_set_access_mode() {
        assert!(mount_options(true).contains(&MountOption::RO));
        assert!(mount_options(false).contains(&MountOption::RW));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_config_uses_owner_acl_on_macos() {
        assert_eq!(mount_config(true).acl, SessionACL::Owner);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn mount_config_allows_auto_unmount_off_macos() {
        assert_eq!(mount_config(true).acl, SessionACL::All);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_options_skip_auto_unmount_on_macos() {
        assert!(!mount_options(true).contains(&MountOption::AutoUnmount));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn mount_options_use_auto_unmount_off_macos() {
        assert!(mount_options(true).contains(&MountOption::AutoUnmount));
    }
}
