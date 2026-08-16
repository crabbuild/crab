//! `crab logs` — manage diagnostic log files.
//!
//! Crab writes structured trace logs to `~/.crab/logs/` (or the
//! directory specified by `CRAB_LOG_DIR`). This command lists, shows,
//! and clears those logs.

use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};

/// List all log files, newest last.
pub fn run_logs_list() -> Result<()> {
    let dir = log_dir();
    let logs = sorted_logs(&dir)?;

    if logs.is_empty() {
        eprintln!("no logs found in {}", dir.display());
        return Ok(());
    }

    for name in &logs {
        println!("{name}");
    }

    Ok(())
}

/// Show the contents of the most recent log file.
pub fn run_logs_last() -> Result<()> {
    let dir = log_dir();
    let logs = sorted_logs(&dir)?;

    if let Some(name) = logs.last() {
        return run_logs_show(name);
    }

    eprintln!("no logs to show");
    Ok(())
}

/// Show the contents of a specific log file.
pub fn run_logs_show(name: &str) -> Result<()> {
    let path = log_dir().join(name);
    let content = std::fs::read_to_string(&path).map_err(|e| CrabError::Configuration {
        key: format!("failed to read log {name}: {e}"),
        origin: path.display().to_string(),
    })?;

    print!("{content}");
    Ok(())
}

/// Delete all log files.
pub fn run_logs_clear() -> Result<()> {
    let dir = log_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
        eprintln!("cleared {}", dir.display());
    } else {
        eprintln!("no log directory to clear");
    }
    Ok(())
}

/// Resolve the log directory.
fn log_dir() -> PathBuf {
    std::env::var("CRAB_LOG_DIR").map_or_else(
        |_| {
            std::env::var_os("HOME").map_or_else(
                || PathBuf::from(".crab/logs"),
                |home| PathBuf::from(home).join(".crab/logs"),
            )
        },
        PathBuf::from,
    )
}

/// List log file names sorted alphabetically (oldest first).
fn sorted_logs(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut names: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_ok_and(|ft| ft.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();

    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_logs_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let logs = sorted_logs(dir.path()).unwrap();
        assert!(logs.is_empty());
    }

    #[test]
    fn sorted_logs_returns_sorted_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("2024-01-02.log"), "b").unwrap();
        std::fs::write(dir.path().join("2024-01-01.log"), "a").unwrap();
        std::fs::write(dir.path().join("2024-01-03.log"), "c").unwrap();

        let logs = sorted_logs(dir.path()).unwrap();
        assert_eq!(
            logs,
            vec!["2024-01-01.log", "2024-01-02.log", "2024-01-03.log"],
        );
    }

    #[test]
    fn sorted_logs_nonexistent_dir() {
        let logs = sorted_logs(Path::new("/nonexistent/path/crab/logs")).unwrap();
        assert!(logs.is_empty());
    }
}
