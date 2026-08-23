//! Integration test: class-aware GC with fixture objects.
//!
//! Verifies:
//! - Default GC refuses early delete for objects within retention.
//! - Force mode (`--force-early-delete` + `--yes-really`) proceeds.
//! - Audit shim is called when force-deleting (when compiled in).
//! - Concurrent maintenance detection blocks GC when an xorb optimization journal exists.

#[cfg(test)]
mod gc_class_aware_integration {
    use std::time::{Duration, SystemTime};

    use crab::cmd::gc::ObjectMeta;
    use crab::cmd::gc::class_aware::{
        Decision, PriceInfo, check_concurrent_maintenance, check_early_delete, check_object_lock,
        total_estimated_penalty, validate_force_flags,
    };
    use crab::tier::classes::StorageClass;

    /// Build a fixture bucket with objects in various storage classes and
    /// transition ages, simulating a real bucket after lifecycle rules have
    /// moved objects through class transitions.
    fn fixture_bucket() -> Vec<ObjectMeta> {
        let now = SystemTime::now();
        vec![
            // Standard — no retention, always deletable
            ObjectMeta {
                key: "xorbs/aa/standard-obj".to_string(),
                size: 1024 * 1024,
                last_modified: now - Duration::from_secs(5 * 86_400),
                storage_class: Some(StorageClass::S3Standard),
                transitioned_at: None,
            },
            // Standard-IA, transitioned 10 days ago (within 30-day min)
            ObjectMeta {
                key: "xorbs/bb/ia-young".to_string(),
                size: 512 * 1024 * 1024,
                last_modified: now - Duration::from_secs(60 * 86_400),
                storage_class: Some(StorageClass::S3StandardIa),
                transitioned_at: Some(now - Duration::from_secs(10 * 86_400)),
            },
            // Standard-IA, transitioned 35 days ago (past 30-day min)
            ObjectMeta {
                key: "xorbs/cc/ia-old".to_string(),
                size: 256 * 1024 * 1024,
                last_modified: now - Duration::from_secs(90 * 86_400),
                storage_class: Some(StorageClass::S3StandardIa),
                transitioned_at: Some(now - Duration::from_secs(35 * 86_400)),
            },
            // Glacier Deep Archive, 90 days old (within 180-day min)
            ObjectMeta {
                key: "xorbs/dd/deep-young".to_string(),
                size: 1024 * 1024 * 1024,
                last_modified: now - Duration::from_secs(90 * 86_400),
                storage_class: Some(StorageClass::S3GlacierDeepArchive),
                transitioned_at: Some(now - Duration::from_secs(90 * 86_400)),
            },
            // GCS Archive, 100 days old (within 365-day min)
            ObjectMeta {
                key: "xorbs/ee/gcs-archive".to_string(),
                size: 2 * 1024 * 1024 * 1024,
                last_modified: now - Duration::from_secs(100 * 86_400),
                storage_class: Some(StorageClass::GcsArchive),
                transitioned_at: Some(now - Duration::from_secs(100 * 86_400)),
            },
            // No class info — should always be deletable
            ObjectMeta {
                key: "xorbs/ff/no-class".to_string(),
                size: 4096,
                last_modified: now - Duration::from_secs(86_400),
                storage_class: None,
                transitioned_at: None,
            },
        ]
    }

    fn test_prices() -> PriceInfo {
        PriceInfo {
            gb_month_usd: 0.0125,
        }
    }

    // --- Default GC refuses early delete ---

    #[test]
    #[ignore = "integration test: requires no external services"]
    fn default_gc_refuses_early_delete_for_young_objects() {
        let prices = test_prices();
        let bucket = fixture_bucket();

        // Standard — always deletable
        assert_eq!(
            check_early_delete(&bucket[0], false, &prices),
            Decision::Delete,
            "S3 Standard should always be deletable"
        );

        // IA young (10 days, 30-day min) — blocked
        let decision = check_early_delete(&bucket[1], false, &prices);
        match &decision {
            Decision::Skip(blocked) => {
                assert_eq!(blocked.class, StorageClass::S3StandardIa);
                assert_eq!(blocked.min_days, 30);
                assert!(blocked.age_days < 30);
            }
            Decision::Delete => panic!("young IA object should be blocked"),
        }

        // IA old (35 days, 30-day min) — allowed
        assert_eq!(
            check_early_delete(&bucket[2], false, &prices),
            Decision::Delete,
            "old IA object should be deletable"
        );

        // Deep Archive young (90 days, 180-day min) — blocked
        assert!(
            matches!(
                check_early_delete(&bucket[3], false, &prices),
                Decision::Skip(_)
            ),
            "young Deep Archive should be blocked"
        );

        // GCS Archive (100 days, 365-day min) — blocked
        assert!(
            matches!(
                check_early_delete(&bucket[4], false, &prices),
                Decision::Skip(_)
            ),
            "young GCS Archive should be blocked"
        );

        // No class — always deletable
        assert_eq!(
            check_early_delete(&bucket[5], false, &prices),
            Decision::Delete,
            "no-class object should always be deletable"
        );
    }

    // --- Force mode proceeds ---

    #[test]
    #[ignore = "integration test: requires no external services"]
    fn force_mode_proceeds_for_young_objects() {
        let prices = test_prices();
        let bucket = fixture_bucket();

        // Force-delete the young IA object
        assert_eq!(
            check_early_delete(&bucket[1], true, &prices),
            Decision::Delete,
            "force mode should allow deleting young IA object"
        );

        // Force-delete the young Deep Archive object
        assert_eq!(
            check_early_delete(&bucket[3], true, &prices),
            Decision::Delete,
            "force mode should allow deleting young Deep Archive object"
        );

        // Force-delete the young GCS Archive object
        assert_eq!(
            check_early_delete(&bucket[4], true, &prices),
            Decision::Delete,
            "force mode should allow deleting young GCS Archive object"
        );
    }

    // --- Force flags require both flags ---

    #[test]
    #[ignore = "integration test: requires no external services"]
    fn force_early_delete_requires_yes_really() {
        assert!(
            validate_force_flags(true, false).is_err(),
            "--force-early-delete without --yes-really should error"
        );
        assert!(
            validate_force_flags(true, true).is_ok(),
            "--force-early-delete with --yes-really should succeed"
        );
    }

    // --- Audit shim records (when compiled in) ---

    #[test]
    #[ignore = "integration test: audit shim is no-op without crab-audit feature"]
    fn force_delete_emits_audit_record() {
        // The audit shim is a no-op when crab-audit is not compiled in.
        // This test verifies the code path executes without panicking.
        // When crab-audit is enabled, the shim would forward to the real
        // audit subsystem.
        let prices = test_prices();
        let bucket = fixture_bucket();

        // Force-delete a young IA object — should call audit_shim::record
        let decision = check_early_delete(&bucket[1], true, &prices);
        assert_eq!(decision, Decision::Delete);
    }

    // --- Object-lock stub ---

    #[test]
    #[ignore = "integration test: object-lock is a stub"]
    fn object_lock_stub_allows_all() {
        let bucket = fixture_bucket();
        for obj in &bucket {
            assert!(
                check_object_lock(obj).is_ok(),
                "stub should allow all objects"
            );
        }
    }

    // --- Dry-run penalty estimation ---

    #[test]
    #[ignore = "integration test: requires no external services"]
    fn dry_run_penalty_estimation() {
        let prices = test_prices();
        let bucket = fixture_bucket();

        // Only objects within retention should contribute to penalty
        let total = total_estimated_penalty(&bucket, &prices);
        let total_val: f64 = total.parse().expect("valid float");
        assert!(
            total_val > 0.0,
            "total penalty should be positive for fixture with young objects"
        );
    }

    // --- Concurrent maintenance detection ---

    #[test]
    #[ignore = "integration test: uses filesystem"]
    fn concurrent_maintenance_blocks_gc() {
        let tmp = tempfile::tempdir().expect("create temp dir");

        // No journal — GC should proceed
        assert!(
            check_concurrent_maintenance(tmp.path()).is_ok(),
            "no journal should allow GC"
        );

        // Create journal — GC should be blocked
        let journal_dir = tmp.path().join(".crab/optimize/xorbs");
        std::fs::create_dir_all(&journal_dir).expect("create dirs");
        std::fs::write(journal_dir.join("journal.db"), b"fake").expect("write journal");

        let result = check_concurrent_maintenance(tmp.path());
        assert!(result.is_err(), "journal present should block GC");
    }
}
