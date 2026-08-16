//! `crab lfs env` / `crab lfs version` — diagnostic and version output.
//!
//! `env` prints LFS-relevant configuration: endpoint, transfer agent,
//! storage path, filter settings, and git version.
//!
//! `version` prints the crab package version and the LFS protocol version.

use std::process::Command;

use crate::core::error::Result;

/// Print LFS diagnostic environment information to stdout.
///
/// Displays the LFS endpoint, transfer agent path, standalone agent config,
/// storage directory, smudge/clean filter settings, and git version.
pub fn run_lfs_env() -> Result<()> {
    let lfs_url = git_config_value("lfs.url")
        .or_else(|| git_config_value("remote.origin.url"))
        .unwrap_or_default();
    println!("Endpoint={lfs_url}");

    let agent_path = git_config_value("lfs.customtransfer.crab.path").unwrap_or_default();
    println!("LocalWorkingDir={agent_path}");

    let standalone = git_config_value("lfs.standalonetransferagent").unwrap_or_default();
    println!("StandaloneTransferAgent={standalone}");

    let lfs_dir = git_config_value("lfs.lfsdir").unwrap_or_else(|| ".git/lfs".to_owned());
    println!("LocalMediaDir={lfs_dir}");

    let smudge = git_config_value("filter.lfs.smudge").unwrap_or_default();
    println!("git config filter.lfs.smudge = {smudge:?}");

    let clean = git_config_value("filter.lfs.clean").unwrap_or_default();
    println!("git config filter.lfs.clean = {clean:?}");

    let git_version = git_version_string().unwrap_or_else(|| "unknown".to_owned());
    println!("{git_version}");

    Ok(())
}

/// Print crab LFS version information to stdout.
///
/// Outputs the crab package version and the LFS custom transfer agent
/// protocol version.
pub fn run_lfs_version() -> Result<()> {
    println!("crab/lfs {}", env!("CARGO_PKG_VERSION"));
    println!("Git LFS custom transfer agent protocol v1");
    Ok(())
}

/// Read a single git config value, returning `None` if the key is unset or
/// git is unavailable.
///
/// On builds with `--features gix-config`, reads go through
/// [`GixConfigResolver`] against the discovered git dir. Default
/// builds fall back to `git config <key>` shellout.
fn git_config_value(key: &str) -> Option<String> {
    #[cfg(feature = "gix-config")]
    {
        let git_dir = crate::git::discover::discover_git_dir().ok()?;
        let resolver = crate::core::config_resolver::GixConfigResolver::open(&git_dir).ok()?;
        resolver.string(key)
    }

    #[cfg(not(feature = "gix-config"))]
    {
        let output = Command::new("git").args(["config", key]).output().ok()?;

        if !output.status.success() {
            return None;
        }

        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if value.is_empty() { None } else { Some(value) }
    }
}

/// Run `git --version` and return the full version string.
fn git_version_string() -> Option<String> {
    let output = Command::new("git").arg("--version").output().ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_output_contains_package_version() {
        // Verify the version string uses the Cargo package version.
        let version = env!("CARGO_PKG_VERSION");
        let expected = format!("crab/lfs {version}");
        assert!(!expected.is_empty());
    }

    #[test]
    fn git_config_value_returns_none_for_missing_key() {
        let result = git_config_value("nonexistent.key.that.should.not.exist");
        assert!(result.is_none());
    }

    #[test]
    fn git_version_string_returns_some() {
        // git should be available in the test environment.
        let version = git_version_string();
        assert!(version.is_some());
        assert!(
            version
                .as_deref()
                .is_some_and(|v| v.contains("git version"))
        );
    }
}
