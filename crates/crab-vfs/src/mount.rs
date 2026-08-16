//! Mount/unmount lifecycle and PID file management.
//!
//! Handles force-unmounting stale mounts, creating FUSE sessions,
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

const FUSE_UNMOUNT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
const FUSE_FORCE_UNMOUNT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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

// ---------------------------------------------------------------------------
// Force-unmount stale mounts
// ---------------------------------------------------------------------------

/// Attempt to force-unmount a stale mount at the given path.
///
/// On macOS: `umount -f <mountpoint>`
/// On Linux: `fusermount3 -u <mountpoint>` (fallback: `umount -l`)
///
/// This attempts a **clean** unmount first and only escalates to
/// `-f` / lazy unmount if the clean attempt fails. A forced unmount
/// terminates in-flight I/O and can lose dirty overlay writes that
/// haven't yet been flushed. Preferring the clean path first gives
/// the kernel a chance to flush pending writes before we pull the
/// plug.
pub fn force_unmount(mountpoint: &Path) -> Result<()> {
    info!(mountpoint = %mountpoint.display(), "attempting clean unmount of stale mount");

    // Attempt 1: clean unmount.
    #[cfg(target_os = "macos")]
    let clean = std::process::Command::new("umount")
        .arg(mountpoint)
        .output();

    #[cfg(target_os = "linux")]
    let clean = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .output();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let clean: std::io::Result<std::process::Output> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "unmount not supported on this platform",
    ));

    if let Ok(output) = &clean {
        if output.status.success() {
            info!(mountpoint = %mountpoint.display(), "clean unmount succeeded");
            return Ok(());
        }
        warn!(
            mountpoint = %mountpoint.display(),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "clean unmount failed, escalating to force"
        );
    }

    // Attempt 2: forced/lazy unmount.
    info!(mountpoint = %mountpoint.display(), "attempting force-unmount of stale mount");

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("umount")
        .arg("-f")
        .arg(mountpoint)
        .output();

    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("umount")
        .arg("-l")
        .arg(mountpoint)
        .output();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result: std::io::Result<std::process::Output> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "force-unmount not supported on this platform",
    ));

    match result {
        Ok(output) if output.status.success() => {
            info!(mountpoint = %mountpoint.display(), "force-unmount succeeded");
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                mountpoint = %mountpoint.display(),
                stderr = %stderr.trim(),
                "force-unmount failed"
            );
            Err(CrabError::Internal(format!(
                "force-unmount of {} failed: {stderr}",
                mountpoint.display()
            )))
        }
        Err(e) => {
            warn!(mountpoint = %mountpoint.display(), error = %e, "force-unmount command failed");
            Err(CrabError::Io(e))
        }
    }
}

pub fn unmount_background_session(
    session: fuser::BackgroundSession,
    mountpoint: &Path,
) -> Result<()> {
    let mountpoint = mountpoint.to_path_buf();
    let mountpoint_for_worker = mountpoint.clone();
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let result = session.umount_and_join().map_err(|e| {
            CrabError::Io(std::io::Error::new(
                e.kind(),
                format!("failed to unmount {}: {e}", mountpoint_for_worker.display()),
            ))
        });
        let _ = tx.send(result);
    });

    match rx.recv_timeout(FUSE_UNMOUNT_GRACE) {
        Ok(result) => return result,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(CrabError::Internal(format!(
                "FUSE unmount worker exited without reporting status for {}",
                mountpoint.display()
            )));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!(
                mountpoint = %mountpoint.display(),
                "FUSE session did not unmount within grace period, forcing OS unmount"
            );
        }
    }

    force_unmount(&mountpoint)?;

    match rx.recv_timeout(FUSE_FORCE_UNMOUNT_GRACE) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(CrabError::Internal(format!(
            "FUSE unmount worker exited without reporting status for {} after force-unmount",
            mountpoint.display()
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!(
                mountpoint = %mountpoint.display(),
                "FUSE session thread did not exit after force-unmount; continuing after OS unmount"
            );
            Ok(())
        }
    }
}

/// Check if a mountpoint appears to be in use (stale mount).
fn is_mountpoint_busy(mountpoint: &Path) -> bool {
    // A stale FUSE mount typically causes stat to hang or return ENOTCONN.
    // If metadata succeeds, the directory is accessible (not a stale mount).
    // Only ENOTCONN indicates a stale FUSE mount that needs force-unmounting.
    match std::fs::metadata(mountpoint) {
        Ok(_) => {
            // Directory is accessible — check if it's already a FUSE mount
            // by looking at /proc/mounts (Linux) or mount output.
            #[cfg(target_os = "linux")]
            {
                if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
                    let mp_str = mountpoint.to_string_lossy();
                    return mounts
                        .lines()
                        .any(|line| line.contains("fuse") && line.contains(mp_str.as_ref()));
                }
            }
            false
        }
        Err(e) => {
            // ENOTCONN (errno 107 on Linux, 57 on macOS) indicates stale FUSE.
            let raw = e.raw_os_error().unwrap_or(0);
            // macOS: ENOTCONN = 57, Linux: ENOTCONN = 107
            raw == 107 || raw == 57
        }
    }
}

// ---------------------------------------------------------------------------
// Mount
// ---------------------------------------------------------------------------

/// Mount the FUSE filesystem and return a session that can be run or
/// backgrounded.
///
/// If the mountpoint has a stale mount, attempts force-unmount first.
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

    // Force-unmount stale mounts.
    if is_mountpoint_busy(mountpoint) {
        if let Err(e) = force_unmount(mountpoint) {
            error!(
                mountpoint = %mountpoint.display(),
                error = %e,
                "cannot clear stale mount; is another process using this mountpoint?"
            );
            return Err(e);
        }
        // Brief pause to let the kernel clean up.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let fs = CrabFs::new(resolver, engine, &config.git_dir, rt);
    let invalidation_index = fs.invalidation_index();

    let fuse_config = mount_config(config.read_only);

    let session = Session::new(fs, mountpoint, &fuse_config).map_err(|e| {
        CrabError::Internal(format!(
            "FUSE mount at {} failed: {e}",
            mountpoint.display()
        ))
    })?;

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
    rt: Handle,
) -> Result<()> {
    let crab_dir_owned = crab_dir.to_path_buf();
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();

    rt.spawn(async move {
        cancel.cancelled().await;
        let _ = cancel_tx.send(());
    });

    let background = session
        .spawn()
        .map_err(|e| CrabError::Internal(format!("failed to spawn FUSE thread: {e}")))?;

    let result = loop {
        if cancel_rx.try_recv().is_ok() {
            info!("cancellation received, unmounting FUSE filesystem");
            break unmount_background_session(background, mountpoint)
                .map_err(|e| std::io::Error::other(e.to_string()));
        }

        if background.guard.is_finished() {
            break background.join();
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    remove_pid_file(&crab_dir_owned);
    result.map_err(|e| CrabError::Internal(format!("FUSE session error: {e}")))?;

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
