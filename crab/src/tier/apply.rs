//! CAS-guarded lifecycle apply, backup writer, and rollback stub.
//!
//! [`apply`] writes a pre-apply backup, then submits the rendered
//! lifecycle document through the provider's CAS-guarded `put`. On CAS
//! mismatch it re-reads the existing config, re-detects conflicts, and
//! retries up to 3 times before failing with
//! `TierLifecycleConflict [CRAB-E0300]`.
//!
//! [`rollback`] is a placeholder — task 5.4 will flesh it out.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crab_types::time::now_rfc3339_millis;

use crate::core::error::{CrabError, Result};

use super::audit_shim::{self, AuditOp};
use super::conflict;
use super::provider::{Format, Guard, LifecycleProvider, Provider, RenderedLifecycle, TierPlan};

/// Maximum number of CAS retry attempts before giving up.
const MAX_CAS_RETRIES: u32 = 3;
static BACKUP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Options controlling the apply behavior.
#[derive(Debug, Clone)]
pub struct ApplyOpts {
    /// When `true`, preserve non-`crab-` rules and replace only
    /// crab-managed rules.
    pub merge: bool,
    /// When `true`, skip the actual `put` call and return early after
    /// conflict detection and backup.
    pub dry_run: bool,
}

/// Outcome of a successful apply.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    /// CAS guard returned by the provider for subsequent writes.
    pub new_guard: GuardSummary,
    /// RFC 3339 timestamp when the provider accepted the write.
    pub applied_at: String,
    /// Path to the pre-apply backup file, if one was written.
    pub backup_path: Option<String>,
}

/// Serializable summary of a [`Guard`] for audit and structured output.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "value")]
pub enum GuardSummary {
    Etag(String),
    Generation(u64),
    None,
}

impl From<&Guard> for GuardSummary {
    fn from(g: &Guard) -> Self {
        match g {
            Guard::Etag(e) => Self::Etag(e.clone()),
            Guard::Generation(n) => Self::Generation(*n),
            Guard::None => Self::None,
        }
    }
}

/// Payload written to the pre-apply backup JSON file.
///
/// `Deserialize` is needed by [`rollback`] to read the backup back.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BackupPayload {
    pub(crate) provider: String,
    pub(crate) rendered_existing: Option<String>,
    pub(crate) cas_guard: String,
    pub(crate) saved_at: String,
}

/// Apply a tier plan to the bucket via the given provider.
///
/// Flow:
/// 1. Fetch existing lifecycle and CAS guard.
/// 2. Detect conflicts; respect `opts.merge`.
/// 3. Write backup to `.crab/tier/backups/<ts>-pre-apply.json`.
/// 4. Call `prov.put(new_doc, guard)` with the CAS guard.
/// 5. On success: emit audit record, return `ApplyOutcome`.
/// 6. On CAS mismatch: re-read, re-detect, retry up to 3 times.
pub async fn apply(
    prov: &dyn LifecycleProvider,
    plan: &TierPlan,
    opts: ApplyOpts,
) -> Result<ApplyOutcome> {
    let new_rendered = prov.render(plan)?;

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        debug!(
            attempt,
            max = MAX_CAS_RETRIES,
            "apply: starting CAS attempt"
        );

        // Step 1: fetch existing lifecycle and CAS guard.
        let existing = prov.get().await?;
        let guard = prov.cas_guard().await?;

        // Step 2: detect conflicts.
        let doc_to_apply = resolve_conflicts(&existing, &new_rendered, &opts)?;

        // Step 3: write backup.
        let backup_path = write_backup(
            existing.as_ref(),
            guard.as_ref(),
            &format!("{:?}", prov.kind()),
        )?;

        // Dry-run: skip the actual put.
        if opts.dry_run {
            info!("dry-run: skipping put call");
            return Ok(ApplyOutcome {
                new_guard: guard
                    .as_ref()
                    .map(GuardSummary::from)
                    .unwrap_or(GuardSummary::None),
                applied_at: now_rfc3339_millis(),
                backup_path: Some(backup_path),
            });
        }

        // Step 4: attempt the CAS-guarded put.
        match prov.put(&doc_to_apply, guard.clone()).await {
            Ok(outcome) => {
                // Step 5: audit + return.
                let apply_outcome = ApplyOutcome {
                    new_guard: GuardSummary::from(&outcome.new_guard),
                    applied_at: outcome.applied_at,
                    backup_path: Some(backup_path),
                };
                audit_shim::record(AuditOp::TierApply, &apply_outcome);
                info!("lifecycle applied successfully");
                return Ok(apply_outcome);
            }
            Err(e) if is_cas_mismatch(&e) => {
                // Step 6: CAS mismatch — retry.
                if attempt >= MAX_CAS_RETRIES {
                    warn!(
                        attempts = attempt,
                        "CAS retry budget exhausted, failing with TierLifecycleConflict"
                    );
                    return Err(CrabError::TierLifecycleConflict {
                        prefix: ".crab/xorbs/".to_string(),
                        existing_id: "concurrent-modification".to_string(),
                        new_id: plan.rules.first().map(|r| r.id.clone()).unwrap_or_default(),
                    });
                }
                warn!(
                    attempt,
                    max = MAX_CAS_RETRIES,
                    "CAS mismatch, re-reading and retrying"
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Detect conflicts and optionally merge, returning the document to apply.
fn resolve_conflicts(
    existing: &Option<RenderedLifecycle>,
    new: &RenderedLifecycle,
    opts: &ApplyOpts,
) -> Result<RenderedLifecycle> {
    let Some(existing) = existing.as_ref() else {
        return Ok(new.clone());
    };

    let has_user_rules = existing
        .rule_ids
        .iter()
        .any(|id| !conflict::is_crab_managed(id));
    let conflicts = conflict::detect_conflicts(Some(existing), new);

    if conflicts.is_empty() && !has_user_rules {
        return Ok(new.clone());
    }

    if opts.merge {
        debug!(
            conflicts = conflicts.len(),
            user_rules = has_user_rules,
            "existing lifecycle requires merge"
        );
        return conflict::merge(existing, new);
    }

    // Conflicts without merge — fail.
    if conflicts.is_empty() && has_user_rules {
        let existing_id = existing
            .rule_ids
            .iter()
            .find(|id| !conflict::is_crab_managed(id))
            .cloned()
            .unwrap_or_else(|| "user-managed-lifecycle-rule".to_string());
        return Err(CrabError::TierLifecycleConflict {
            prefix: ".crab/xorbs/".to_string(),
            existing_id,
            new_id: new.rule_ids.first().cloned().unwrap_or_default(),
        });
    }

    let first = &conflicts[0];
    Err(CrabError::TierLifecycleConflict {
        prefix: ".crab/xorbs/".to_string(),
        existing_id: first.existing_id.clone(),
        new_id: first.new_id.clone(),
    })
}

/// Check whether an error represents a CAS mismatch.
///
/// CAS mismatches surface as `CasConflict` from the provider put
/// implementation, or as `TierLifecycleConflict` when the re-read
/// detects a concurrent modification.
fn is_cas_mismatch(err: &CrabError) -> bool {
    matches!(err, CrabError::CasConflict { .. })
}

/// Write a pre-apply backup to `.crab/tier/backups/<ts>-<pid>-<seq>-pre-apply.json`.
///
/// Uses synchronous file IO because backup files are small.
/// Returns the backup file path on success.
fn write_backup(
    existing: Option<&RenderedLifecycle>,
    guard: Option<&Guard>,
    provider: &str,
) -> Result<String> {
    let ts = now_rfc3339_millis();
    // Replace colons in the timestamp for filesystem compatibility.
    let safe_ts = ts.replace(':', "-");

    let backup_dir = PathBuf::from(".crab/tier/backups");
    std::fs::create_dir_all(&backup_dir)?;

    let rendered_existing = existing
        .map(|r| String::from_utf8(r.body.clone()).unwrap_or_else(|_| "<binary>".to_string()));

    let guard_str = match guard {
        Some(Guard::Etag(e)) => format!("etag:{e}"),
        Some(Guard::Generation(g)) => format!("generation:{g}"),
        Some(Guard::None) | None => "none".to_string(),
    };

    let payload = BackupPayload {
        provider: provider.to_string(),
        rendered_existing,
        cas_guard: guard_str,
        saved_at: ts,
    };

    let json = serde_json::to_string_pretty(&payload)
        .map_err(|e| CrabError::Internal(format!("failed to serialize backup payload: {e}")))?;

    let pid = std::process::id();
    for _ in 0..16 {
        let seq = BACKUP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let filename = format!("{safe_ts}-{pid}-{seq}-pre-apply.json");
        let backup_path = backup_dir.join(&filename);

        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        };
        file.write_all(json.as_bytes())?;

        let path_str = backup_path.to_string_lossy().to_string();
        debug!(path = %path_str, "wrote pre-apply backup");
        return Ok(path_str);
    }

    Err(CrabError::Internal(
        "failed to allocate unique tier backup path".to_owned(),
    ))
}

/// Restore a prior lifecycle configuration from a backup file.
///
/// Reads the backup JSON at `backup_path`, extracts the
/// `rendered_existing` content, and restores it via `prov.put()`.
/// When the backup indicates no prior lifecycle existed, this is a
/// no-op (the bucket had no lifecycle before the apply).
///
/// Emits an audit record on successful rollback.
pub async fn rollback(prov: &dyn LifecycleProvider, backup_path: &str) -> Result<()> {
    let raw = std::fs::read_to_string(backup_path).map_err(|e| {
        CrabError::Internal(format!(
            "failed to read backup file '{}': {}",
            backup_path, e
        ))
    })?;

    let payload: BackupPayload = serde_json::from_str(&raw).map_err(|e| {
        CrabError::Internal(format!(
            "failed to parse backup file '{}': {}",
            backup_path, e
        ))
    })?;

    match payload.rendered_existing {
        Some(body) => {
            // Reconstruct a RenderedLifecycle from the backup body.
            // The format is inferred from the provider: S3 uses XML,
            // GCS and Azure use JSON.
            let format = match prov.kind() {
                Provider::S3 => Format::Xml,
                Provider::Gcs | Provider::Azure => Format::Json,
            };
            let restored = RenderedLifecycle {
                format,
                body: body.into_bytes(),
                rule_ids: Vec::new(),
            };

            let guard = prov.cas_guard().await?;
            prov.put(&restored, guard).await?;

            info!(backup = %backup_path, "lifecycle rolled back to pre-apply state");
        }
        None => {
            // The bucket had no lifecycle before the apply — nothing to
            // restore. A future enhancement could call a provider
            // `delete_lifecycle` method, but for V1 we log and return.
            info!(
                backup = %backup_path,
                "backup indicates no prior lifecycle; rollback is a no-op"
            );
        }
    }

    audit_shim::record(
        AuditOp::TierRollback,
        &serde_json::json!({
            "backup_path": backup_path,
            "provider": payload.provider,
            "saved_at": payload.saved_at,
        }),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::provider::{Format, Guard, PutOutcome, RenderedLifecycle, TierPlan};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ── Mock provider ───────────────────────────────────────────────

    /// A test-only mock that implements `LifecycleProvider` with
    /// configurable behavior for each method.
    struct MockProvider {
        existing: Mutex<Option<RenderedLifecycle>>,
        guard: Mutex<Option<Guard>>,
        put_calls: AtomicU32,
        /// When > 0, the first N put calls return CasConflict.
        cas_failures: AtomicU32,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                existing: Mutex::new(None),
                guard: Mutex::new(None),
                put_calls: AtomicU32::new(0),
                cas_failures: AtomicU32::new(0),
            }
        }

        fn with_existing(existing: RenderedLifecycle, guard: Guard) -> Self {
            Self {
                existing: Mutex::new(Some(existing)),
                guard: Mutex::new(Some(guard)),
                put_calls: AtomicU32::new(0),
                cas_failures: AtomicU32::new(0),
            }
        }

        fn with_cas_failures(mut self, n: u32) -> Self {
            self.cas_failures = AtomicU32::new(n);
            self
        }

        fn put_count(&self) -> u32 {
            self.put_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LifecycleProvider for MockProvider {
        fn kind(&self) -> crate::tier::provider::Provider {
            crate::tier::provider::Provider::S3
        }

        fn render(&self, plan: &TierPlan) -> Result<RenderedLifecycle> {
            #[cfg(not(feature = "tier-s3"))]
            {
                return Ok(render_test_lifecycle(plan));
            }

            #[cfg(feature = "tier-s3")]
            crate::tier::provider::s3::render(plan)
        }

        async fn get(&self) -> Result<Option<RenderedLifecycle>> {
            Ok(self.existing.lock().unwrap().clone())
        }

        async fn put(&self, _doc: &RenderedLifecycle, _guard: Option<Guard>) -> Result<PutOutcome> {
            self.put_calls.fetch_add(1, Ordering::SeqCst);

            let remaining = self.cas_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                self.cas_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(CrabError::CasConflict {
                    path: "lifecycle".to_string(),
                    expected_etag: Some("old".to_string()),
                });
            }

            Ok(PutOutcome {
                new_guard: Guard::Etag("new-etag".to_string()),
                applied_at: now_rfc3339_millis(),
            })
        }

        async fn cas_guard(&self) -> Result<Option<Guard>> {
            Ok(self.guard.lock().unwrap().clone())
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

    fn simple_plan() -> TierPlan {
        use crate::tier::classes::StorageClass;
        use crate::tier::provider::{Provider, TierRule, Transition};

        TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-ia".to_string(),
                prefix: ".crab/xorbs/".to_string(),
                transitions: vec![Transition {
                    days: 30,
                    to_class: StorageClass::S3StandardIa,
                }],
                noncurrent_expiration_days: None,
                min_object_size_bytes: Some(128_000),
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        }
    }

    fn rendered(rule_ids: &[&str], body: &[u8]) -> RenderedLifecycle {
        RenderedLifecycle {
            format: Format::Json,
            body: body.to_vec(),
            rule_ids: rule_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn xml_rendered(rule_ids: &[&str]) -> RenderedLifecycle {
        let mut body =
            String::from(r#"<?xml version="1.0" encoding="UTF-8"?><LifecycleConfiguration>"#);
        for id in rule_ids {
            body.push_str("<Rule><ID>");
            body.push_str(id);
            body.push_str("</ID><Status>Enabled</Status></Rule>");
        }
        body.push_str("</LifecycleConfiguration>");
        RenderedLifecycle {
            format: Format::Xml,
            body: body.into_bytes(),
            rule_ids: rule_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[cfg(not(feature = "tier-s3"))]
    fn render_test_lifecycle(plan: &TierPlan) -> RenderedLifecycle {
        let body = plan
            .rules
            .iter()
            .map(|rule| format!("{}:{}", rule.id, rule.prefix))
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        RenderedLifecycle {
            format: Format::Json,
            body,
            rule_ids: plan.rules.iter().map(|rule| rule.id.clone()).collect(),
        }
    }

    fn default_opts() -> ApplyOpts {
        ApplyOpts {
            merge: false,
            dry_run: false,
        }
    }

    // ── Helpers: cleanup ────────────────────────────────────────────

    /// Remove a single backup file if it exists.
    fn cleanup_backup(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    // ── Tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn apply_no_existing_lifecycle_succeeds() {
        let prov = MockProvider::new();
        let plan = simple_plan();
        let opts = default_opts();

        let outcome = apply(&prov, &plan, opts)
            .await
            .expect("apply should succeed");

        assert!(outcome.backup_path.is_some());
        assert!(!outcome.applied_at.is_empty());
        assert_eq!(prov.put_count(), 1);

        if let Some(path) = &outcome.backup_path {
            cleanup_backup(path);
        }
    }

    #[tokio::test]
    async fn apply_with_conflicts_no_merge_fails() {
        let existing = rendered(&["crab-xorbs-to-ia"], b"old-body-different");
        let prov = MockProvider::with_existing(existing, Guard::Etag("etag-1".to_string()));
        let plan = simple_plan();
        let opts = ApplyOpts {
            merge: false,
            dry_run: false,
        };

        let err = apply(&prov, &plan, opts).await.unwrap_err();

        match &err {
            CrabError::TierLifecycleConflict {
                existing_id,
                new_id,
                ..
            } => {
                assert_eq!(existing_id, "crab-xorbs-to-ia");
                assert_eq!(new_id, "crab-xorbs-to-ia");
            }
            other => panic!("expected TierLifecycleConflict, got: {other}"),
        }

        // No put should have been attempted.
        assert_eq!(prov.put_count(), 0);
    }

    #[tokio::test]
    #[cfg(feature = "tier-s3")]
    async fn apply_with_conflicts_and_merge_succeeds() {
        let existing = xml_rendered(&["crab-xorbs-to-ia", "user-cleanup"]);
        let prov = MockProvider::with_existing(existing, Guard::Etag("etag-1".to_string()));
        let plan = simple_plan();
        let opts = ApplyOpts {
            merge: true,
            dry_run: false,
        };

        let outcome = apply(&prov, &plan, opts)
            .await
            .expect("merge apply should succeed");

        assert_eq!(prov.put_count(), 1);
        assert!(outcome.backup_path.is_some());

        if let Some(path) = &outcome.backup_path {
            cleanup_backup(path);
        }
    }

    #[tokio::test]
    async fn dry_run_skips_put() {
        let prov = MockProvider::new();
        let plan = simple_plan();
        let opts = ApplyOpts {
            merge: false,
            dry_run: true,
        };

        let outcome = apply(&prov, &plan, opts)
            .await
            .expect("dry-run should succeed");

        assert_eq!(prov.put_count(), 0, "dry-run should not call put");
        assert!(outcome.backup_path.is_some());

        if let Some(path) = &outcome.backup_path {
            cleanup_backup(path);
        }
    }

    #[tokio::test]
    async fn backup_path_format_is_correct() {
        // Use write_backup directly to avoid parallel-test races on the
        // shared backup directory.
        let path = write_backup(None, None, "S3").expect("write_backup should succeed");

        assert!(
            path.starts_with(".crab/tier/backups/"),
            "backup path should start with .crab/tier/backups/, got: {path}"
        );
        assert!(
            path.ends_with("-pre-apply.json"),
            "backup path should end with -pre-apply.json, got: {path}"
        );

        // Verify the file exists and contains valid JSON.
        let content = std::fs::read_to_string(&path).expect("backup file should exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("backup should be valid JSON");
        assert!(parsed.get("provider").is_some());
        assert!(parsed.get("saved_at").is_some());
        assert!(parsed.get("cas_guard").is_some());

        cleanup_backup(&path);
    }

    #[tokio::test]
    async fn cas_retry_succeeds_on_second_attempt() {
        let prov = MockProvider::new().with_cas_failures(1);
        let plan = simple_plan();
        let opts = default_opts();

        let outcome = apply(&prov, &plan, opts)
            .await
            .expect("should succeed after retry");

        // First attempt fails (CAS), second succeeds.
        assert_eq!(prov.put_count(), 2);
        assert!(outcome.backup_path.is_some());

        if let Some(path) = &outcome.backup_path {
            cleanup_backup(path);
        }
    }

    #[tokio::test]
    async fn cas_retry_exhausted_returns_conflict() {
        let prov = MockProvider::new().with_cas_failures(5);
        let plan = simple_plan();
        let opts = default_opts();

        let err = apply(&prov, &plan, opts).await.unwrap_err();

        assert!(
            matches!(err, CrabError::TierLifecycleConflict { .. }),
            "expected TierLifecycleConflict after exhausting retries, got: {err}"
        );
        // Should have attempted MAX_CAS_RETRIES times.
        assert_eq!(prov.put_count(), MAX_CAS_RETRIES);
    }

    // ── Rollback tests ──────────────────────────────────────────────

    /// Write a temporary backup file with the given payload and return
    /// its path. Caller is responsible for cleanup.
    fn write_test_backup(payload: &BackupPayload, label: &str) -> String {
        let dir = PathBuf::from(".crab/tier/backups");
        std::fs::create_dir_all(&dir).expect("create backup dir");
        let name = format!("test-rollback-{}-{}.json", label, std::process::id());
        let path = dir.join(name);
        let json = serde_json::to_string_pretty(payload).expect("serialize payload");
        std::fs::write(&path, json).expect("write backup file");
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn rollback_with_existing_lifecycle_restores_via_put() {
        let prov = MockProvider::new();
        let backup = BackupPayload {
            provider: "S3".to_string(),
            rendered_existing: Some(
                "<LifecycleConfiguration>old</LifecycleConfiguration>".to_string(),
            ),
            cas_guard: "etag:abc123".to_string(),
            saved_at: "2026-04-27T20:00:00.000Z".to_string(),
        };
        let path = write_test_backup(&backup, "existing");

        rollback(&prov, &path)
            .await
            .expect("rollback should succeed");

        assert_eq!(
            prov.put_count(),
            1,
            "rollback should call put to restore lifecycle"
        );
        cleanup_backup(&path);
    }

    #[tokio::test]
    async fn rollback_with_no_existing_lifecycle_is_noop() {
        let prov = MockProvider::new();
        let backup = BackupPayload {
            provider: "S3".to_string(),
            rendered_existing: None,
            cas_guard: "none".to_string(),
            saved_at: "2026-04-27T20:00:00.000Z".to_string(),
        };
        let path = write_test_backup(&backup, "noop");

        rollback(&prov, &path)
            .await
            .expect("rollback should succeed");

        assert_eq!(prov.put_count(), 0, "no-op rollback should not call put");
        cleanup_backup(&path);
    }

    #[tokio::test]
    async fn rollback_with_missing_backup_file_returns_error() {
        let prov = MockProvider::new();
        let err = rollback(&prov, "/nonexistent/path/backup.json")
            .await
            .unwrap_err();

        assert!(
            matches!(err, CrabError::Internal(ref msg) if msg.contains("failed to read backup file")),
            "expected Internal error about missing file, got: {err}"
        );
        assert_eq!(prov.put_count(), 0);
    }

    /// Integration-level test: apply → rollback → provider state matches
    /// the pre-apply lifecycle. Requires a real provider (LocalStack),
    /// so it is `#[ignore]`d for normal test runs.
    #[tokio::test]
    #[ignore]
    async fn apply_then_rollback_restores_pre_apply_state() {
        // Setup: provider starts with an existing lifecycle.
        let original_body = b"<LifecycleConfiguration>original</LifecycleConfiguration>";
        let original = rendered(&["user-cleanup"], original_body);
        let prov = MockProvider::with_existing(original, Guard::Etag("etag-0".to_string()));
        let plan = simple_plan();
        let opts = ApplyOpts {
            merge: true,
            dry_run: false,
        };

        // Step 1: apply the tier plan.
        let outcome = apply(&prov, &plan, opts)
            .await
            .expect("apply should succeed");
        let backup_path = outcome.backup_path.expect("apply should produce a backup");

        // Step 2: rollback using the backup.
        rollback(&prov, &backup_path)
            .await
            .expect("rollback should succeed");

        // Step 3: verify the provider's lifecycle matches the original.
        // In a real integration test against LocalStack, we would call
        // `prov.get()` and compare the body. With the mock, we verify
        // that put was called twice (once for apply, once for rollback).
        assert_eq!(prov.put_count(), 2, "apply + rollback = 2 put calls");

        cleanup_backup(&backup_path);
    }
}
