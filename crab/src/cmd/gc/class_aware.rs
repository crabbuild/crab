//! Class-aware GC guards: early-delete blocking, object-lock probing,
//! penalty estimation, and concurrent-maintenance detection.
//!
//! All functions in this module are pure (no network calls) except
//! `check_object_lock` which is a stub awaiting real provider integration.
//! The core decision function [`check_early_delete`] compares object age
//! against the storage class's minimum retention window and returns a
//! [`Decision`] that the GC sweep loop acts on.

use std::path::Path;
use std::time::SystemTime;

use serde::Serialize;
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};
use crate::tier::audit_shim::{self, AuditOp};
use crate::tier::classes::StorageClass;

use super::ObjectMeta;

// ---------------------------------------------------------------------------
// Decision type
// ---------------------------------------------------------------------------

/// Outcome of the early-delete check for a single object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Object may be deleted — either no class info, past retention, or
    /// force-delete was requested (with audit recorded).
    Delete,
    /// Object is within its minimum retention window and force-delete was
    /// not requested. The GC sweep should skip this object.
    Skip(EarlyDeleteBlocked),
}

/// Details about why an early delete was blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarlyDeleteBlocked {
    /// The storage class of the object.
    pub class: StorageClass,
    /// How many days old the object is (since transition or last-modified).
    pub age_days: u32,
    /// Minimum retention days for this class.
    pub min_days: u32,
    /// Estimated early-deletion penalty in USD, formatted to 2 decimal places.
    pub penalty_usd: String,
}

// ---------------------------------------------------------------------------
// Price info for penalty estimation
// ---------------------------------------------------------------------------

/// Minimal price information needed for early-delete penalty estimation.
///
/// Callers populate this from the active price table. When no price data
/// is available, use [`PriceInfo::default`] which yields `$0.00` penalties.
#[derive(Debug, Clone)]
pub struct PriceInfo {
    /// Storage cost per GB per month for the object's current class.
    pub gb_month_usd: f64,
}

impl Default for PriceInfo {
    fn default() -> Self {
        Self { gb_month_usd: 0.0 }
    }
}

#[derive(Debug, Serialize)]
struct ForceEarlyDeletePayload {
    key: String,
    class: String,
    age_days: u32,
    min_days: u32,
    penalty_usd: String,
    size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Core: check_early_delete (Task 7.3)
// ---------------------------------------------------------------------------

/// Check whether deleting `obj` would incur an early-deletion penalty.
///
/// Pure function — no network calls. Uses wall-clock time internally to
/// compute object age.
///
/// # Returns
///
/// - `Decision::Delete` when:
///   - `obj.storage_class` is `None` (no class info, no guard), OR
///   - the object is past its minimum retention window, OR
///   - `force_early_delete` is `true` (audit record emitted).
/// - `Decision::Skip(EarlyDeleteBlocked)` when the object is within its
///   retention window and `force_early_delete` is `false`.
pub fn check_early_delete(
    obj: &ObjectMeta,
    force_early_delete: bool,
    prices: &PriceInfo,
) -> Decision {
    let Some(class) = obj.storage_class else {
        return Decision::Delete;
    };

    let min_days = class.min_retention_days();
    if min_days == 0 {
        return Decision::Delete;
    }

    let transition_time = obj.transitioned_at.unwrap_or(obj.last_modified);
    let age = SystemTime::now()
        .duration_since(transition_time)
        .unwrap_or_default();
    let age_days = (age.as_secs() / 86_400) as u32;

    if age_days >= min_days {
        return Decision::Delete;
    }

    let penalty_usd = estimate_penalty(obj.size, age_days, min_days, prices);

    if force_early_delete {
        let payload = ForceEarlyDeletePayload {
            key: obj.key.clone(),
            class: class.to_string(),
            age_days,
            min_days,
            penalty_usd: penalty_usd.clone(),
            size_bytes: obj.size,
        };
        audit_shim::record(AuditOp::ForceEarlyDelete, &payload);
        debug!(
            key = %obj.key,
            class = %class,
            age_days,
            min_days,
            penalty_usd = %penalty_usd,
            "force-early-delete: proceeding with audit record"
        );
        return Decision::Delete;
    }

    Decision::Skip(EarlyDeleteBlocked {
        class,
        age_days,
        min_days,
        penalty_usd,
    })
}

// ---------------------------------------------------------------------------
// Force-flag validation (Task 7.4)
// ---------------------------------------------------------------------------

/// Validate the `--force-early-delete` + `--yes-really` flag combination.
///
/// `--force-early-delete` requires `--yes-really` as a safety gate.
/// Returns an error when `force_early_delete` is set without `yes_really`.
pub fn validate_force_flags(force_early_delete: bool, yes_really: bool) -> Result<()> {
    if force_early_delete && !yes_really {
        return Err(CrabError::Configuration {
            key: "--force-early-delete requires --yes-really to confirm you accept early-deletion penalties".to_owned(),
            origin: "gc flags".to_owned(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Object-lock check (Task 7.5)
// ---------------------------------------------------------------------------

/// Check whether the object is under an object-lock retention policy.
///
/// When object lock is detected, returns [`CrabError::ObjectLockedRetention`]
/// regardless of any force flags — locked objects cannot be deleted.
///
/// # Current implementation — **fail-open stub**
///
/// Always returns `Ok(())` because the provider-native retention-probe
/// calls (`GetObjectRetention` / `GetObjectLegalHold` on S3, bucket
/// retention on GCS, legal-hold / immutability on Azure) are not
/// wired. That means this function **cannot detect** an active
/// retention policy today.
///
/// Data-safety implication: `crab gc` may attempt to delete an
/// object that is legally retained. The provider will reject the
/// delete, so no data is actually lost — but the GC run will see a
/// permission error instead of the structured
/// [`CrabError::ObjectLockedRetention`] it expects, and the CLI
/// surface will surface the raw provider error.
///
/// # Fix plan
///
/// Wiring this properly requires a new `RetentionProbe` trait with
/// per-provider implementations:
///
/// - **S3:** `aws_sdk_s3::Client::get_object_retention` +
///   `get_object_legal_hold`.
/// - **GCS:** `storage.buckets.get` → `retention_policy` (bucket-wide)
///   plus per-object `event_based_hold` / `temporary_hold` flags.
/// - **Azure:** `BlobClient::get_properties` → `lease_status` +
///   `legal_hold` + `immutability_policy`.
///
/// The trait belongs next to [`crate::cmd::gc::parallel_enum::ObjectLister`]
/// so both enumeration and retention checks share the same provider
/// routing. Tracked under `crab-storage-economy`; not done here
/// because flipping the fail-open behavior to fail-closed is a
/// breaking change that needs operator communication first.
pub fn check_object_lock(obj: &ObjectMeta) -> Result<()> {
    let _ = obj;
    Ok(())
}

// ---------------------------------------------------------------------------
// Penalty estimation (Task 7.6)
// ---------------------------------------------------------------------------

/// Estimate the early-deletion penalty for an object in USD.
///
/// Uses a simplified formula:
///   `penalty = (min_days - age_days) * gb_month_usd * size_gb / 30`
///
/// This approximates the provider's charge for the remaining retention
/// period. The real penalty model varies by provider but this gives a
/// useful order-of-magnitude estimate for `--dry-run` reporting.
///
/// Returns a formatted string with 2 decimal places (e.g. `"1.23"`).
pub fn estimate_penalty(
    size_bytes: u64,
    age_days: u32,
    min_days: u32,
    prices: &PriceInfo,
) -> String {
    if age_days >= min_days || prices.gb_month_usd <= 0.0 {
        return "0.00".to_owned();
    }

    let remaining_days = f64::from(min_days - age_days);
    let size_gb = size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let penalty = remaining_days * prices.gb_month_usd * size_gb / 30.0;

    format!("{penalty:.2}")
}

/// Sum estimated penalties for a batch of objects (used by `--dry-run`).
///
/// Returns the total estimated penalty as a formatted USD string.
pub fn total_estimated_penalty(objects: &[ObjectMeta], prices: &PriceInfo) -> String {
    let mut total = 0.0_f64;

    for obj in objects {
        let Some(class) = obj.storage_class else {
            continue;
        };
        let min_days = class.min_retention_days();
        if min_days == 0 {
            continue;
        }

        let transition_time = obj.transitioned_at.unwrap_or(obj.last_modified);
        let age = SystemTime::now()
            .duration_since(transition_time)
            .unwrap_or_default();
        let age_days = (age.as_secs() / 86_400) as u32;

        if age_days >= min_days {
            continue;
        }

        let remaining_days = f64::from(min_days - age_days);
        let size_gb = obj.size as f64 / (1024.0 * 1024.0 * 1024.0);
        total += remaining_days * prices.gb_month_usd * size_gb / 30.0;
    }

    format!("{total:.2}")
}

// ---------------------------------------------------------------------------
// Concurrent maintenance detection (Task 7.7)
// ---------------------------------------------------------------------------

/// Path to the xorb optimization journal relative to the repo root.
const RESTRIPE_JOURNAL_PATH: &str = ".crab/restripe/journal.db";

/// Check for concurrent maintenance operations that conflict with GC.
///
/// GC and xorb optimization cannot run simultaneously. This function
/// checks whether the journal exists at the well-known path, indicating
/// an optimization is in progress or was not cleaned up.
///
/// # Errors
///
/// Returns `ConcurrentMaintenance { other: "optimize xorbs" }` when the
/// journal file exists.
pub fn check_concurrent_maintenance(repo_root: &Path) -> Result<()> {
    let journal_path = repo_root.join(RESTRIPE_JOURNAL_PATH);
    if journal_path.exists() {
        warn!(
            journal = %journal_path.display(),
            "xorb optimization journal detected — refusing to start GC"
        );
        return Err(CrabError::ConcurrentMaintenance {
            other: "optimize xorbs",
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_obj_with_class(key: &str, size: u64, age: Duration, class: StorageClass) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size,
            last_modified: SystemTime::now() - age,
            storage_class: Some(class),
            transitioned_at: None,
        }
    }

    fn make_obj_no_class(key: &str, size: u64, age: Duration) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size,
            last_modified: SystemTime::now() - age,
            storage_class: None,
            transitioned_at: None,
        }
    }

    fn test_prices() -> PriceInfo {
        PriceInfo {
            gb_month_usd: 0.0125,
        }
    }

    // --- check_early_delete (Task 7.3) ---

    #[test]
    fn no_class_info_allows_delete() {
        let obj = make_obj_no_class("xorbs/ab/obj1", 1024, Duration::from_secs(86_400));
        let decision = check_early_delete(&obj, false, &test_prices());
        assert_eq!(decision, Decision::Delete);
    }

    #[test]
    fn standard_class_allows_delete() {
        let obj = make_obj_with_class(
            "xorbs/ab/obj2",
            1024,
            Duration::from_secs(86_400),
            StorageClass::S3Standard,
        );
        let decision = check_early_delete(&obj, false, &test_prices());
        assert_eq!(decision, Decision::Delete);
    }

    #[test]
    fn ia_within_retention_blocks_delete() {
        // S3 Standard-IA has 30-day minimum; object is 10 days old
        let obj = make_obj_with_class(
            "xorbs/ab/obj3",
            1024 * 1024 * 1024, // 1 GiB
            Duration::from_secs(10 * 86_400),
            StorageClass::S3StandardIa,
        );
        let decision = check_early_delete(&obj, false, &test_prices());
        match decision {
            Decision::Skip(blocked) => {
                assert_eq!(blocked.class, StorageClass::S3StandardIa);
                assert_eq!(blocked.age_days, 10);
                assert_eq!(blocked.min_days, 30);
                // penalty should be non-zero
                let penalty: f64 = blocked.penalty_usd.parse().expect("valid float");
                assert!(penalty > 0.0, "penalty should be positive");
            }
            Decision::Delete => panic!("expected Skip, got Delete"),
        }
    }

    #[test]
    fn ia_past_retention_allows_delete() {
        // S3 Standard-IA has 30-day minimum; object is 31 days old
        let obj = make_obj_with_class(
            "xorbs/ab/obj4",
            1024,
            Duration::from_secs(31 * 86_400),
            StorageClass::S3StandardIa,
        );
        let decision = check_early_delete(&obj, false, &test_prices());
        assert_eq!(decision, Decision::Delete);
    }

    #[test]
    fn glacier_deep_within_retention_blocks_delete() {
        // Deep Archive has 180-day minimum; object is 90 days old
        let obj = make_obj_with_class(
            "xorbs/ab/obj5",
            1024,
            Duration::from_secs(90 * 86_400),
            StorageClass::S3GlacierDeepArchive,
        );
        let decision = check_early_delete(&obj, false, &test_prices());
        assert!(matches!(decision, Decision::Skip(_)));
    }

    #[test]
    fn force_early_delete_proceeds_within_retention() {
        let obj = make_obj_with_class(
            "xorbs/ab/obj6",
            1024,
            Duration::from_secs(10 * 86_400),
            StorageClass::S3StandardIa,
        );
        let decision = check_early_delete(&obj, true, &test_prices());
        assert_eq!(decision, Decision::Delete);
    }

    #[test]
    fn transitioned_at_used_over_last_modified() {
        // Object was last-modified 60 days ago but transitioned 10 days ago.
        // For S3 Standard-IA (30-day min), the 10-day transition age should
        // trigger the block.
        let now = SystemTime::now();
        let obj = ObjectMeta {
            key: "xorbs/ab/obj7".to_string(),
            size: 1024,
            last_modified: now - Duration::from_secs(60 * 86_400),
            storage_class: Some(StorageClass::S3StandardIa),
            transitioned_at: Some(now - Duration::from_secs(10 * 86_400)),
        };
        let decision = check_early_delete(&obj, false, &test_prices());
        assert!(matches!(decision, Decision::Skip(_)));
    }

    // --- validate_force_flags (Task 7.4) ---

    #[test]
    fn force_without_yes_really_errors() {
        let result = validate_force_flags(true, false);
        assert!(result.is_err());
    }

    #[test]
    fn force_with_yes_really_succeeds() {
        let result = validate_force_flags(true, true);
        assert!(result.is_ok());
    }

    #[test]
    fn no_force_no_yes_really_succeeds() {
        let result = validate_force_flags(false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn no_force_with_yes_really_succeeds() {
        // Harmless: --yes-really without --force-early-delete is a no-op
        let result = validate_force_flags(false, true);
        assert!(result.is_ok());
    }

    // --- check_object_lock (Task 7.5) ---

    #[test]
    fn object_lock_stub_returns_ok() {
        let obj = make_obj_no_class("xorbs/ab/obj8", 1024, Duration::from_secs(86_400));
        assert!(check_object_lock(&obj).is_ok());
    }

    // --- estimate_penalty (Task 7.6) ---

    #[test]
    fn penalty_zero_when_past_retention() {
        let result = estimate_penalty(1024 * 1024 * 1024, 31, 30, &test_prices());
        assert_eq!(result, "0.00");
    }

    #[test]
    fn penalty_zero_when_no_price() {
        let result = estimate_penalty(1024 * 1024 * 1024, 10, 30, &PriceInfo::default());
        assert_eq!(result, "0.00");
    }

    #[test]
    fn penalty_positive_within_retention() {
        // 1 GiB object, 10 days old, 30-day min, $0.0125/GB/month
        // remaining = 20 days, size = 1 GB, penalty = 20 * 0.0125 * 1 / 30 ≈ 0.0083
        let result = estimate_penalty(1024 * 1024 * 1024, 10, 30, &test_prices());
        let penalty: f64 = result.parse().expect("valid float");
        assert!(penalty > 0.0, "penalty should be positive");
        assert!(penalty < 1.0, "penalty for 1 GiB should be under $1");
    }

    #[test]
    fn penalty_scales_with_size() {
        let prices = test_prices();
        // Use the raw estimate_penalty function to verify scaling.
        // 1 GiB vs 10 GiB — 10x size should yield ~10x penalty.
        let small = estimate_penalty(1024 * 1024 * 1024, 10, 30, &prices);
        let large = estimate_penalty(10 * 1024 * 1024 * 1024_u64, 10, 30, &prices);
        let small_val: f64 = small.parse().expect("valid float");
        let large_val: f64 = large.parse().expect("valid float");
        // Allow for rounding: 10x size should yield at least 5x penalty
        assert!(
            large_val > small_val * 5.0,
            "10x size should yield roughly 10x penalty: small={small_val}, large={large_val}"
        );
    }

    #[test]
    fn total_penalty_sums_across_objects() {
        let prices = test_prices();
        let objects = vec![
            make_obj_with_class(
                "xorbs/ab/a",
                1024 * 1024 * 1024,
                Duration::from_secs(10 * 86_400),
                StorageClass::S3StandardIa,
            ),
            make_obj_with_class(
                "xorbs/ab/b",
                1024 * 1024 * 1024,
                Duration::from_secs(10 * 86_400),
                StorageClass::S3StandardIa,
            ),
            // This one has no class — should not contribute
            make_obj_no_class(
                "xorbs/ab/c",
                1024 * 1024 * 1024,
                Duration::from_secs(86_400),
            ),
        ];
        let total = total_estimated_penalty(&objects, &prices);
        let total_val: f64 = total.parse().expect("valid float");
        assert!(total_val > 0.0);
    }

    // --- check_concurrent_maintenance (Task 7.7) ---

    #[test]
    fn no_journal_allows_gc() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        assert!(check_concurrent_maintenance(tmp.path()).is_ok());
    }

    #[test]
    fn journal_present_blocks_gc() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let journal_dir = tmp.path().join(".crab/restripe");
        std::fs::create_dir_all(&journal_dir).expect("create dirs");
        std::fs::write(journal_dir.join("journal.db"), b"fake journal").expect("write journal");

        let result = check_concurrent_maintenance(tmp.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            CrabError::ConcurrentMaintenance { other } => {
                assert_eq!(other, "optimize xorbs");
            }
            other => panic!("expected ConcurrentMaintenance, got {other:?}"),
        }
    }
}
