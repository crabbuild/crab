//! First-run welcome message displayed once after installation.
//!
//! On the very first invocation of any `crab` command, a brief
//! getting-started message is printed to stderr. A marker file at
//! `~/.crab/.first-run-seen` suppresses the message on subsequent runs.

use std::path::PathBuf;

use crate::core::output::OutputMode;

/// Marker file name written after the welcome message is shown.
const MARKER_FILE: &str = ".first-run-seen";

const WELCOME: &str = "\
  Welcome to Crab — serverless Git for large files.

  Get to your first push:
    1. Create a cloud bucket/container and sign in to its provider
    2. crab configure <provider://bucket/repository>
    3. crab ship . -m 'Initial commit'

  Joining an existing repository?  crab clone <remote>
  Setup not working?              crab doctor

  Guide: https://crab.build/docs/cli/getting-started/first-repository";

/// Check and display the first-run welcome if needed.
///
/// Called at CLI entry before command dispatch. Suppressed when the
/// output mode is machine-readable (Json/Jsonl) to avoid corrupting
/// structured output streams.
pub fn maybe_show_welcome(mode: OutputMode) {
    if mode.is_machine() {
        return;
    }

    let Some(crab_dir) = home_crab_dir() else {
        return;
    };

    let marker = crab_dir.join(MARKER_FILE);
    if marker.exists() {
        return;
    }

    eprintln!("\n{WELCOME}\n");

    // Best-effort marker creation — failure is non-fatal.
    let _ = std::fs::create_dir_all(&crab_dir);
    let _ = std::fs::write(&marker, "");
}

/// Resolve `~/.crab` using the HOME environment variable.
///
/// Returns `None` when HOME is unset (e.g. some containerized
/// environments), in which case the welcome message is silently skipped.
fn home_crab_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".crab"))
}

#[cfg(test)]
mod tests {
    use super::WELCOME;

    #[test]
    fn welcome_leads_to_a_real_first_push() {
        assert!(WELCOME.contains("cloud bucket/container"));
        assert!(WELCOME.contains("crab configure"));
        assert!(WELCOME.contains("crab ship"));
        assert!(WELCOME.contains("crab doctor"));
    }
}
