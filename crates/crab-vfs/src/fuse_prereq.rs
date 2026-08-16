//! FUSE host prerequisite checks shared by mount entry points.

use std::path::Path;

use crate::core::error::{CrabError, Result};

#[cfg(target_os = "macos")]
const MACFUSE_FS_PATH: &str = "/Library/Filesystems/macfuse.fs";
#[cfg(target_os = "macos")]
const MACFUSE_LOADER: &str = "/Library/Filesystems/macfuse.fs/Contents/Resources/load_macfuse";
#[cfg(target_os = "macos")]
const DEV_DIR: &str = "/dev";
#[cfg(target_os = "linux")]
const FUSE_DEVICE: &str = "/dev/fuse";

/// Check that the platform FUSE dependency is installed and usable.
pub fn check_fuse_prerequisites() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if !Path::new(MACFUSE_FS_PATH).exists() {
            return Err(CrabError::Configuration {
                key: "macFUSE not found".into(),
                origin: "FUSE prerequisite check".into(),
            });
        }
        ensure_fuse_device_available()
    }

    #[cfg(target_os = "linux")]
    {
        if !Path::new(FUSE_DEVICE).exists() {
            return Err(CrabError::Configuration {
                key: "/dev/fuse not found".into(),
                origin: "FUSE prerequisite check".into(),
            });
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(CrabError::Configuration {
            key: "FUSE is not supported on this platform".into(),
            origin: "FUSE prerequisite check".into(),
        })
    }
}

/// Ensure the FUSE device is ready immediately before calling into fuser.
pub fn ensure_fuse_device_available() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if macfuse_device_exists(Path::new(DEV_DIR)) {
            return Ok(());
        }

        if Path::new(MACFUSE_LOADER).exists() {
            let output = std::process::Command::new(MACFUSE_LOADER).output()?;
            if !output.status.success() {
                return Err(CrabError::Configuration {
                    key: format!(
                        "macFUSE loader failed: {}",
                        macfuse_loader_failure_detail(&output)
                    ),
                    origin: "FUSE prerequisite check".into(),
                });
            }
        }

        if macfuse_device_exists(Path::new(DEV_DIR)) {
            return Ok(());
        }

        Err(CrabError::Configuration {
            key: "macFUSE kernel extension is not loaded; approve macFUSE in System Settings, reboot if prompted, then retry".into(),
            origin: "FUSE prerequisite check".into(),
        })
    }

    #[cfg(target_os = "linux")]
    {
        if Path::new(FUSE_DEVICE).exists() {
            return Ok(());
        }
        Err(CrabError::Configuration {
            key: "/dev/fuse not found".into(),
            origin: "FUSE prerequisite check".into(),
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(CrabError::Configuration {
            key: "FUSE is not supported on this platform".into(),
            origin: "FUSE prerequisite check".into(),
        })
    }
}

#[cfg(target_os = "macos")]
fn macfuse_device_exists(dev_dir: &Path) -> bool {
    match std::fs::read_dir(dev_dir) {
        Ok(entries) => entries.filter_map(std::result::Result::ok).any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(is_macos_fuse_device_name)
        }),
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn is_macos_fuse_device_name(name: &str) -> bool {
    name == "fuse" || name.starts_with("macfuse") || name.starts_with("osxfuse")
}

#[cfg(target_os = "macos")]
fn macfuse_loader_failure_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !stdout.is_empty() {
        return stdout;
    }

    match output.status.code() {
        Some(code) => format!(
            "exit status {code}; macFUSE is installed but its kernel extension is not loaded. Approve macFUSE in System Settings and reboot if macOS requests it"
        ),
        None => "terminated by signal; macFUSE is installed but its kernel extension is not loaded"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::os::unix::process::ExitStatusExt;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_fuse_device_names_match_macfuse_variants() {
        assert!(super::is_macos_fuse_device_name("fuse"));
        assert!(super::is_macos_fuse_device_name("macfuse0"));
        assert!(super::is_macos_fuse_device_name("osxfuse0"));
        assert!(!super::is_macos_fuse_device_name("disk0"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn silent_macfuse_loader_failure_reports_exit_status() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        let detail = super::macfuse_loader_failure_detail(&output);
        assert!(detail.contains("exit status 1"));
        assert!(detail.contains("System Settings"));
    }
}
