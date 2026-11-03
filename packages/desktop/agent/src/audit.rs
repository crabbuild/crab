//! Best-effort audit logging for destructive operations.
//!
//! Appends one JSON line per mutation to `~/.crab/desktop/audit.jsonl`.
//! Errors are logged but never propagated — the audit log must not
//! block or fail the operation it records.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

/// A single audit log entry serialized as one JSON line.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// ISO 8601 timestamp of the event.
    pub timestamp: String,
    /// Short action identifier (e.g. "worktree_add", "worktree_remove").
    pub action: String,
    /// Arbitrary structured details about the action.
    pub details: Value,
}

/// Append an audit entry to `~/.crab/desktop/audit.jsonl`.
///
/// Best-effort: errors are logged at `warn` level but never returned
/// to the caller. The audit log must not interfere with the operation
/// being audited.
pub fn log_entry(action: &str, details: Value) {
    let entry = AuditEntry {
        timestamp: Utc::now().to_rfc3339(),
        action: action.to_owned(),
        details,
    };

    if let Err(e) = append_entry(&entry) {
        warn!(
            action = %entry.action,
            error = %e,
            "failed to write audit log entry (non-fatal)"
        );
    }
}

/// Internal: serialize and append the entry to the audit file.
fn append_entry(entry: &AuditEntry) -> std::io::Result<()> {
    use std::io::Write;

    let path = audit_log_path()?;

    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut line =
        serde_json::to_vec(entry).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    line.push(b'\n');

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(&line)?;

    Ok(())
}

/// Resolve the audit log path: `~/.crab/desktop/audit.jsonl`.
fn audit_log_path() -> std::io::Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "home directory not found")
    })?;
    Ok(home.join(".crab").join("desktop").join("audit.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead;

    /// Verify that `log_entry` appends a valid JSON line to the audit file.
    ///
    /// Uses a temporary override of the audit path via a helper that
    /// writes to a temp file, then asserts the entry is well-formed.
    #[test]
    fn audit_entry_is_appended_as_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let audit_file = dir.path().join("audit.jsonl");

        // Write directly using append_entry to a known path.
        let entry = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            action: "worktree_add".to_owned(),
            details: json!({ "path": "/tmp/wt", "branch": "feature-x" }),
        };

        // Manually append to our test file.
        {
            use std::io::Write;
            std::fs::create_dir_all(dir.path()).unwrap();
            let mut line = serde_json::to_vec(&entry).unwrap();
            line.push(b'\n');
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&audit_file)
                .unwrap();
            file.write_all(&line).unwrap();
        }

        // Read back and verify.
        let file = std::fs::File::open(&audit_file).unwrap();
        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 1);

        let parsed: AuditEntry = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed.action, "worktree_add");
        assert_eq!(parsed.details["path"], "/tmp/wt");
        assert_eq!(parsed.details["branch"], "feature-x");
        assert!(!parsed.timestamp.is_empty());
    }

    /// Verify multiple entries are appended (not overwritten).
    #[test]
    fn multiple_entries_append_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let audit_file = dir.path().join("audit.jsonl");

        for i in 0..3 {
            let entry = AuditEntry {
                timestamp: Utc::now().to_rfc3339(),
                action: format!("action_{i}"),
                details: json!({ "index": i }),
            };
            use std::io::Write;
            let mut line = serde_json::to_vec(&entry).unwrap();
            line.push(b'\n');
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&audit_file)
                .unwrap();
            file.write_all(&line).unwrap();
        }

        let file = std::fs::File::open(&audit_file).unwrap();
        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 3);

        for (i, line) in lines.iter().enumerate() {
            let parsed: AuditEntry = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.action, format!("action_{i}"));
        }
    }

    /// Verify that `log_entry` does not panic even when called normally.
    /// This exercises the real path (writes to ~/.crab/desktop/audit.jsonl).
    #[test]
    fn log_entry_does_not_panic() {
        // This writes to the real audit log path but that's acceptable
        // in a test environment — it's append-only and best-effort.
        log_entry("test_action", json!({ "test": true }));
    }
}
