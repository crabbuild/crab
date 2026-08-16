use std::process::{Command, Stdio};

use crate::error::{AuthServerError, Result};

/// Returns the Git version used by auth helper orchestration.
pub fn git_version() -> Result<String> {
    let output = Command::new("git")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
    }
    let version =
        String::from_utf8(output.stdout).map_err(|_| invalid("git output was not valid UTF-8"))?;
    Ok(version.trim().to_owned())
}

fn invalid(message: impl Into<String>) -> AuthServerError {
    AuthServerError::Configuration {
        key: message.into(),
        origin: "crab-auth-server-doctor".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_version_reports_installed_git() -> Result<()> {
        let version = git_version()?;

        assert!(version.starts_with("git version "));
        Ok(())
    }
}
