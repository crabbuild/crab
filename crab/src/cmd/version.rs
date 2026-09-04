//! `crab version` — print version information.
//!
//! Displays the crab version, git commit SHA, and build timestamp.
//! With `--json`, emits a structured envelope including a schema registry
//! mapping every known payload schema to its current version.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};

/// Payload emitted by `crab version --json`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct VersionPayload {
    pub crab_version: String,
    pub git_sha: String,
    pub build_timestamp: String,
    pub schemas: BTreeMap<String, String>,
}

/// Build the static schema registry.
///
/// Maps every known payload schema name to its current version.
/// Covers migrated commands (v1.1), new --json commands, streaming
/// commands, event schemas, and the error schema (all v1.0).
fn schema_registry() -> BTreeMap<String, String> {
    let entries: &[(&str, &str)] = &[
        // Migrated commands (envelope-wrapped existing --json) — v1.1
        ("diff", "1.1"),
        ("du", "1.1"),
        ("lfs.locks", "1.1"),
        ("lfs.status", "1.1"),
        ("lock", "1.1"),
        ("locks", "1.1"),
        ("ls-files", "1.1"),
        ("unlock", "1.1"),
        // New --json read commands — v1.0
        ("config.get", "1.0"),
        ("cost", "1.0"),
        ("daemon.list", "1.0"),
        ("daemon.status", "1.0"),
        ("doctor", "1.0"),
        ("env", "1.0"),
        ("errors", "1.0"),
        ("mirror.apply", "1.0"),
        ("mirror.check", "1.0"),
        ("optimize.apply", "1.0"),
        ("optimize.plan", "1.0"),
        ("staging.stats", "1.0"),
        ("stat", "1.0"),
        ("stat.classes", "1.0"),
        ("stat.perf", "1.0"),
        ("stat.push-plan", "1.0"),
        ("skills.install", "1.0"),
        ("skills.list", "1.0"),
        ("status", "1.0"),
        ("tier.plan", "1.0"),
        ("track", "1.0"),
        ("version", "1.0"),
        // Enterprise replication schemas — v1.0
        ("replica.certification", "1.0"),
        ("replica.evidence.verify", "1.0"),
        ("replica.live-control-plane.evidence", "1.0"),
        ("replica.live-smoke.evidence", "1.0"),
        // Streaming commands (--json result + --jsonl events) — v1.0
        ("add", "1.0"),
        ("clone", "1.0"),
        ("dehydrate", "1.0"),
        ("export.file", "1.0"),
        ("export.plan", "1.0"),
        ("export.summary", "1.0"),
        ("fetch", "1.0"),
        ("fsck", "1.0"),
        ("gc", "1.0"),
        ("hydrate", "1.0"),
        ("optimize.xorbs.plan", "1.0"),
        ("prune", "1.0"),
        ("push", "1.0"),
        ("repack", "1.0"),
        // Event schemas — v1.0
        ("add.event", "1.0"),
        ("clone.event", "1.0"),
        ("dehydrate.event", "1.0"),
        ("fetch.event", "1.0"),
        ("fsck.event", "1.0"),
        ("gc.event", "1.0"),
        ("hydrate.event", "1.0"),
        ("mirror.apply.event", "1.0"),
        ("mirror.check.event", "1.0"),
        ("optimize.xorbs.event", "1.0"),
        ("prune.event", "1.0"),
        ("push.event", "1.0"),
        ("repack.event", "1.0"),
        ("tier.event", "1.0"),
        // Error schema — v1.0
        ("error", "1.0"),
    ];
    entries
        .iter()
        .map(|&(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Run `crab version`.
pub fn run_version(mode: OutputMode) -> Result<()> {
    if mode == OutputMode::Json {
        let payload = VersionPayload {
            crab_version: env!("CRAB_BUILD_VERSION").to_owned(),
            git_sha: env!("CRAB_BUILD_GIT_SHA").to_owned(),
            build_timestamp: env!("CRAB_BUILD_TIMESTAMP").to_owned(),
            schemas: schema_registry(),
        };
        emit_json("version", "1.0", payload);
        return Ok(());
    }

    println!(
        "crab {} ({})\nbuilt at {}",
        env!("CRAB_BUILD_VERSION"),
        env!("CRAB_BUILD_GIT_SHA"),
        env!("CRAB_BUILD_TIMESTAMP"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_payload_populates_required_fields() {
        let payload = VersionPayload {
            crab_version: env!("CRAB_BUILD_VERSION").to_owned(),
            git_sha: env!("CRAB_BUILD_GIT_SHA").to_owned(),
            build_timestamp: env!("CRAB_BUILD_TIMESTAMP").to_owned(),
            schemas: schema_registry(),
        };
        assert!(!payload.crab_version.is_empty());
        assert!(!payload.git_sha.is_empty());
        assert!(!payload.build_timestamp.is_empty());
        assert!(!payload.schemas.is_empty());
    }

    #[test]
    fn schema_registry_has_version_entry() {
        let schemas = schema_registry();
        assert_eq!(schemas.get("version").map(String::as_str), Some("1.0"));
    }

    #[test]
    fn schema_registry_keys_are_sorted() {
        let schemas = schema_registry();
        let keys: Vec<&String> = schemas.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "BTreeMap keys should be in sorted order");
    }

    #[test]
    fn schema_registry_is_comprehensive() {
        let schemas = schema_registry();

        // Migrated commands at v1.1
        for name in [
            "diff",
            "du",
            "lfs.locks",
            "lfs.status",
            "lock",
            "locks",
            "ls-files",
            "unlock",
        ] {
            assert_eq!(
                schemas.get(name).map(String::as_str),
                Some("1.1"),
                "missing or wrong version for {name}"
            );
        }

        // New --json commands at v1.0
        for name in [
            "config.get",
            "cost",
            "daemon.list",
            "daemon.status",
            "doctor",
            "env",
            "errors",
            "mirror.apply",
            "mirror.check",
            "optimize.apply",
            "optimize.plan",
            "staging.stats",
            "stat",
            "stat.classes",
            "stat.perf",
            "stat.push-plan",
            "skills.install",
            "skills.list",
            "status",
            "tier.plan",
            "track",
            "version",
            "replica.certification",
            "replica.evidence.verify",
            "replica.live-control-plane.evidence",
            "replica.live-smoke.evidence",
        ] {
            assert_eq!(
                schemas.get(name).map(String::as_str),
                Some("1.0"),
                "missing or wrong version for {name}"
            );
        }

        // Streaming commands at v1.0
        for name in [
            "add",
            "clone",
            "dehydrate",
            "export.summary",
            "fetch",
            "fsck",
            "gc",
            "hydrate",
            "optimize.xorbs.plan",
            "prune",
            "push",
            "repack",
        ] {
            assert_eq!(
                schemas.get(name).map(String::as_str),
                Some("1.0"),
                "missing or wrong version for {name}"
            );
        }

        // Event schemas at v1.0
        for name in [
            "add.event",
            "clone.event",
            "dehydrate.event",
            "export.file",
            "export.plan",
            "fetch.event",
            "fsck.event",
            "gc.event",
            "hydrate.event",
            "mirror.apply.event",
            "mirror.check.event",
            "optimize.xorbs.event",
            "prune.event",
            "push.event",
            "repack.event",
            "tier.event",
        ] {
            assert_eq!(
                schemas.get(name).map(String::as_str),
                Some("1.0"),
                "missing or wrong version for {name}"
            );
        }

        // Error schema at v1.0
        assert_eq!(
            schemas.get("error").map(String::as_str),
            Some("1.0"),
            "missing error schema"
        );

        // Total count: 8 migrated + 26 new json + 12 streaming + 16 events + 1 error = 63
        assert_eq!(schemas.len(), 63, "unexpected schema count");
    }

    #[test]
    fn version_text_mode_succeeds() {
        let result = run_version(OutputMode::Text);
        assert!(result.is_ok());
    }

    #[test]
    fn version_json_mode_succeeds() {
        let result = run_version(OutputMode::Json);
        assert!(result.is_ok());
    }
}
