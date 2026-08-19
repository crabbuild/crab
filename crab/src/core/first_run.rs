//! First-run welcome message displayed once after installation.
//!
//! On the very first invocation of any `crab` command, a brief
//! getting-started message is printed to stderr. A marker file at
//! `~/.crab/.first-run-seen` suppresses the message on subsequent runs.

use std::path::PathBuf;

use crate::core::output::OutputMode;

/// Marker file name written after the welcome message is shown.
const MARKER_FILE: &str = ".first-run-seen";

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

    eprintln!();
    eprintln!("  Welcome to Crab — serverless Git for large files.");
    eprintln!();
    eprintln!("  Get started:");
    eprintln!("    crab configure       Guided cloud and repository setup");
    eprintln!("    crab clone <remote>  Join an existing repository");
    eprintln!("    crab --help          Browse commands by task");
    eprintln!();
    eprintln!("  Docs: https://crab.build/docs/cli");
    eprintln!();

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
