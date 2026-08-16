//! `crab audit` command group.

use std::path::PathBuf;

use clap::Subcommand;

use crate::audit::{
    AUDIT_EXPORT_SCHEMA, AUDIT_LOG_SCHEMA, AUDIT_SCHEMA_VERSION, AUDIT_VERIFY_SCHEMA,
    AuditExportPayload, AuditLogPayload, default_log_path, export_events, filter_events,
    read_events, verify_log,
};
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};

#[derive(Debug, Clone, Subcommand)]
pub enum AuditCmd {
    /// List audit events from the local audit log.
    Log {
        /// Audit log path. Defaults to `.crab/audit/events.jsonl`.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Filter by operation name.
        #[arg(long, value_name = "OP")]
        operation: Option<String>,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Verify audit event schema and digests.
    Verify {
        /// Audit log path. Defaults to `.crab/audit/events.jsonl`.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Export audit events as a portable JSON bundle.
    Export {
        /// Audit log path. Defaults to `.crab/audit/events.jsonl`.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Destination JSON file.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
        /// Filter by operation name.
        #[arg(long, value_name = "OP")]
        operation: Option<String>,
        /// Structured JSON output.
        #[arg(long)]
        json: bool,
    },
}

impl AuditCmd {
    pub fn output_mode(&self) -> OutputMode {
        match self {
            Self::Log { json, .. } | Self::Verify { json, .. } | Self::Export { json, .. } => {
                OutputMode::from_flags(*json, false)
            }
        }
    }

    pub fn schema_name(&self) -> &'static str {
        match self {
            Self::Log { .. } => AUDIT_LOG_SCHEMA,
            Self::Verify { .. } => AUDIT_VERIFY_SCHEMA,
            Self::Export { .. } => AUDIT_EXPORT_SCHEMA,
        }
    }
}

pub fn run(cmd: &AuditCmd) -> Result<()> {
    match cmd {
        AuditCmd::Log {
            path,
            operation,
            json,
        } => run_log(
            path.clone().unwrap_or_else(default_log_path),
            operation.as_deref(),
            OutputMode::from_flags(*json, false),
        ),
        AuditCmd::Verify { path, json } => run_verify(
            path.clone().unwrap_or_else(default_log_path),
            OutputMode::from_flags(*json, false),
        ),
        AuditCmd::Export {
            path,
            output,
            operation,
            json,
        } => run_export(
            path.clone().unwrap_or_else(default_log_path),
            output,
            operation.as_deref(),
            OutputMode::from_flags(*json, false),
        ),
    }
}

fn run_log(path: PathBuf, operation: Option<&str>, mode: OutputMode) -> Result<()> {
    let events = filter_events(read_events(&path)?, operation);
    let payload = AuditLogPayload {
        path: path.display().to_string(),
        events,
    };

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(AUDIT_LOG_SCHEMA, AUDIT_SCHEMA_VERSION, &payload)
        }
        OutputMode::Text => {
            if payload.events.is_empty() {
                println!("no audit events");
            } else {
                for event in &payload.events {
                    println!(
                        "{} {} {} {}",
                        event.timestamp_unix, event.operation, event.outcome, event.event_id
                    );
                }
            }
        }
    }
    Ok(())
}

fn run_verify(path: PathBuf, mode: OutputMode) -> Result<()> {
    let payload = verify_log(&path)?;
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(AUDIT_VERIFY_SCHEMA, AUDIT_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            if payload.invalid == 0 {
                println!("audit log OK: {} event(s)", payload.checked);
            } else {
                println!(
                    "audit log invalid: {} issue(s) across {} checked event(s)",
                    payload.invalid, payload.checked
                );
                for issue in &payload.issues {
                    println!("line {}: {}", issue.line, issue.reason);
                }
            }
        }
    }

    if payload.invalid > 0 {
        return Err(CrabError::CorruptObject {
            path: payload.path,
            reason: format!("{} invalid audit event(s)", payload.invalid),
        });
    }

    Ok(())
}

fn run_export(
    source_path: PathBuf,
    output_path: &PathBuf,
    operation: Option<&str>,
    mode: OutputMode,
) -> Result<()> {
    let exported = export_events(&source_path, output_path, operation)?;
    let payload = AuditExportPayload {
        source_path: source_path.display().to_string(),
        output_path: output_path.display().to_string(),
        exported,
    };

    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(AUDIT_EXPORT_SCHEMA, AUDIT_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            println!(
                "exported {} audit event(s) to {}",
                payload.exported, payload.output_path
            );
        }
    }

    Ok(())
}
