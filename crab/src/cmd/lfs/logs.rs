//! `crab lfs logs` — display Git LFS-style error logs.

use std::io::Write;
use std::path::Path;

use crate::core::error::{CrabError, Result};

const TRANSFER_LOG: &str = "transfers.log";

#[derive(Debug, Clone, Default)]
pub struct LfsLogsOptions {
    pub args: Vec<String>,
    pub transfer_history: bool,
    pub last: Option<usize>,
    pub clear: bool,
}

/// Run `crab lfs logs`.
pub fn run_lfs_logs(options: LfsLogsOptions) -> Result<()> {
    let repo_root = std::env::current_dir().map_err(CrabError::Io)?;
    let log_dir = crate::lfs::config::LfsConfig::resolve_storage_dir(&repo_root)?.join("logs");

    if options.transfer_history {
        return run_transfer_history(&log_dir, options.last, options.clear);
    }

    if options.clear || matches!(options.args.first().map(String::as_str), Some("clear")) {
        clear_error_logs(&log_dir)?;
        println!("Cleared {}", log_dir.display());
        return Ok(());
    }

    match options.args.as_slice() {
        [] => {
            for name in sorted_error_logs(&log_dir)? {
                println!("{name}");
            }
        }
        [arg] if arg == "last" => show_last_error_log(&log_dir)?,
        [arg] if arg == "boomtown" => {
            write_sample_error_log(&log_dir)?;
            return Err(CrabError::Internal("Sample panic message".to_owned()));
        }
        [cmd] if cmd == "show" => {
            eprintln!("Supply a log name.");
        }
        [cmd, name] if cmd == "show" => print_error_log(&log_dir, name)?,
        [arg] => print_error_log(&log_dir, arg)?,
        _ => {
            return Err(CrabError::Configuration {
                key: "crab lfs logs".to_owned(),
                origin: "usage: crab lfs logs [last|clear|show <file>|<file>]".to_owned(),
            });
        }
    }

    Ok(())
}

fn run_transfer_history(log_dir: &Path, last: Option<usize>, clear: bool) -> Result<()> {
    let log_path = log_dir.join(TRANSFER_LOG);

    if clear {
        if log_path.is_file() {
            std::fs::remove_file(&log_path).map_err(CrabError::Io)?;
            eprintln!("Cleared LFS transfer log");
        } else {
            eprintln!("No transfer log to clear");
        }
        return Ok(());
    }

    if !log_path.is_file() {
        eprintln!("No LFS transfer history found");
        eprintln!("Transfer events are logged during fetch, push, and pull operations.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&log_path).map_err(CrabError::Io)?;
    let lines: Vec<&str> = content.lines().collect();

    let display_lines = match last {
        Some(n) => {
            let start = lines.len().saturating_sub(n);
            &lines[start..]
        }
        None => &lines[..],
    };

    for line in display_lines {
        println!("{line}");
    }

    if display_lines.is_empty() {
        eprintln!("No transfer events recorded yet");
    }

    Ok(())
}

fn sorted_error_logs(log_dir: &Path) -> Result<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return Ok(Vec::new());
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        if !entry.file_type().map_err(CrabError::Io)?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == TRANSFER_LOG {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn show_last_error_log(log_dir: &Path) -> Result<()> {
    let logs = sorted_error_logs(log_dir)?;
    let Some(name) = logs.last() else {
        println!("No logs to show");
        return Ok(());
    };
    print_error_log(log_dir, name)
}

fn print_error_log(log_dir: &Path, name: &str) -> Result<()> {
    let content = read_error_log(log_dir, name)?;
    print!("{content}");
    Ok(())
}

fn read_error_log(log_dir: &Path, name: &str) -> Result<String> {
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(CrabError::Configuration {
            key: "crab lfs logs".to_owned(),
            origin: format!("invalid log name: {name}"),
        });
    }
    let path = log_dir.join(name);
    std::fs::read_to_string(&path).map_err(|source| CrabError::Configuration {
        key: format!("failed to read LFS log {name}"),
        origin: source.to_string(),
    })
}

fn clear_error_logs(log_dir: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return Ok(());
    };

    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == TRANSFER_LOG {
            continue;
        }
        let path = entry.path();
        if entry.file_type().map_err(CrabError::Io)?.is_dir() {
            std::fs::remove_dir_all(path).map_err(CrabError::Io)?;
        } else {
            std::fs::remove_file(path).map_err(CrabError::Io)?;
        }
    }

    Ok(())
}

fn write_sample_error_log(log_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(log_dir).map_err(CrabError::Io)?;
    let path = log_dir.join("boomtown.log");
    std::fs::write(
        path,
        "Sample panic message\nSample error message: Sample wrapped error message\n",
    )
    .map_err(CrabError::Io)
}

/// Append a transfer event to the log file.
pub fn log_transfer_event(operation: &str, count: u64, elapsed_secs: f64) {
    let Ok(repo_root) = std::env::current_dir() else {
        return;
    };
    let Ok(log_dir) = crate::lfs::config::LfsConfig::resolve_storage_dir(&repo_root)
        .map(|path| path.join("logs"))
    else {
        return;
    };
    let _ = std::fs::create_dir_all(&log_dir);

    let log_path = log_dir.join("transfers.log");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let entry = format!("{timestamp}\t{operation}\t{count} object(s)\t{elapsed_secs:.1}s\n");

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = f.write_all(entry.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_error_logs_excludes_transfer_history() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path();
        std::fs::write(log_dir.join("b.log"), "b").unwrap();
        std::fs::write(log_dir.join("a.log"), "a").unwrap();
        std::fs::write(log_dir.join(TRANSFER_LOG), "transfer").unwrap();

        assert_eq!(
            sorted_error_logs(log_dir).unwrap(),
            vec!["a.log".to_owned(), "b.log".to_owned()]
        );
    }

    #[test]
    fn read_error_log_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();

        assert!(read_error_log(dir.path(), "../config").is_err());
        assert!(read_error_log(dir.path(), "nested/log").is_err());
    }

    #[test]
    fn clear_error_logs_preserves_transfer_history() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path();
        std::fs::write(log_dir.join("error.log"), "error").unwrap();
        std::fs::write(log_dir.join(TRANSFER_LOG), "transfer").unwrap();

        clear_error_logs(log_dir).unwrap();

        assert!(!log_dir.join("error.log").exists());
        assert!(log_dir.join(TRANSFER_LOG).exists());
    }

    #[test]
    fn write_sample_error_log_creates_boomtown_log() {
        let dir = tempfile::tempdir().unwrap();

        write_sample_error_log(dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join("boomtown.log")).unwrap();
        assert!(content.contains("Sample panic message"));
    }
}
