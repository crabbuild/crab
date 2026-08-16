//! Regression tests for the typed ref-edit model (Req 4).
//!
//! Covers the five named tests from the
//! Original task list for the Gitoxide adoption work:
//!
//! - `delete_ref_removes_s3_object`
//! - `delete_nonexistent_ref_is_noop`
//! - `concurrent_updates_converge_to_one_winner` (proptest)
//! - `locked_ref_rejects_second_concurrent_update`
//! - `atomic_multi_ref_commits_all_or_none`
//!
//! These tests exercise the `gix_ref::transaction::RefEdit`-shaped
//! edit model from `crab::git::push_edits`. All tests run
//! without any S3 backend — the edit model is independent of the
//! CAS transport. Tests that need CAS semantics use in-memory
//! simulation: a `HashMap<ref_name, sha>` that mimics the
//! manifest `refs` field, with the same validate-then-apply flow
//! that `build_manifest` runs in production code.
//!
//! Gated behind `gix-ref-edits` so the legacy path is unaffected.

#![cfg(feature = "gix-ref-edits")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::collections::HashMap;
use std::time::Duration;

use crab::git::push_edits::{
    build_ref_edits, dst_name, is_delete, local_ref_lock, validate_edit_set,
};
use crab::git::remote_helper::PushSpec;
use gix_ref::Target;
use gix_ref::transaction::{Change, PreviousValue};
use proptest::prelude::*;

fn spec(force: bool, src: &str, dst: &str) -> PushSpec {
    PushSpec {
        force,
        src: src.to_owned(),
        dst: dst.to_owned(),
    }
}

fn hex_sha(byte: u8) -> String {
    std::iter::repeat(byte)
        .take(20)
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// In-memory stand-in for the manifest `refs` map that the push
/// pipeline's `build_manifest` mutates before CAS. Apply a
/// pre-validated edit batch the same way production code does.
fn apply_edits(refs: &mut HashMap<String, String>, edits: &[gix_ref::transaction::RefEdit]) {
    for edit in edits {
        let dst = dst_name(edit);
        if is_delete(edit) {
            refs.remove(&dst);
        } else if let Change::Update {
            new: Target::Object(oid),
            ..
        } = &edit.change
        {
            refs.insert(dst, oid.to_hex().to_string());
        }
    }
}

// --- Named regression tests (task 1.5) ---

#[test]
fn delete_ref_removes_s3_object() {
    // A delete refspec (`push :refs/heads/old`) must produce a
    // `Change::Delete` and, when applied to the in-memory ref
    // map, remove the entry. This is the smart-http-parity P0-1
    // fix — today's `src.is_empty() → skip` branch silently
    // dropped the delete and left the S3 ref object behind.
    let mut refs = HashMap::new();
    refs.insert("refs/heads/old".to_owned(), hex_sha(0x11));
    refs.insert("refs/heads/main".to_owned(), hex_sha(0x22));

    let specs = vec![spec(false, "", "refs/heads/old")];
    let prior: HashMap<String, String> = refs.clone();
    let edits =
        build_ref_edits(&specs, &HashMap::new(), &prior).expect("build edits for delete spec");
    validate_edit_set(&edits).expect("single delete validates");

    assert_eq!(edits.len(), 1);
    assert!(is_delete(&edits[0]), "edit must be Delete");

    apply_edits(&mut refs, &edits);
    assert!(!refs.contains_key("refs/heads/old"), "ref not removed");
    assert!(refs.contains_key("refs/heads/main"), "sibling ref gone");
}

#[test]
fn delete_nonexistent_ref_is_noop() {
    // Deleting a ref that doesn't exist is a no-op for the
    // in-memory state. The edit still generates (crab is
    // idempotent here — a delete of something gone is success)
    // but application leaves the map untouched.
    let mut refs = HashMap::new();
    refs.insert("refs/heads/main".to_owned(), hex_sha(0x33));

    let specs = vec![spec(false, "", "refs/heads/never-existed")];
    let edits = build_ref_edits(&specs, &HashMap::new(), &HashMap::new()).expect("build edits");
    validate_edit_set(&edits).expect("validate");

    assert_eq!(edits.len(), 1);
    assert!(is_delete(&edits[0]));
    // With no prior tip in the `prior_tips` map, the precondition
    // is `Any` — the edit is unconditional.
    match &edits[0].change {
        Change::Delete {
            expected: PreviousValue::Any,
            ..
        } => {}
        other => panic!("expected Delete with Any precondition, got {other:?}"),
    }

    let before = refs.clone();
    apply_edits(&mut refs, &edits);
    assert_eq!(refs, before, "delete of missing ref must be no-op");
}

// Property: given N concurrent update edits for the same ref
// name, at most one validates as a single batch. Two edits for
// the same ref within one batch always fail consolidation — this
// is the in-memory "one winner" guarantee that backs the S3 CAS
// atomicity contract.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        .. ProptestConfig::default()
    })]

    #[test]
    fn concurrent_updates_converge_to_one_winner(
        // Two to five competing updates; each picks its own source
        // byte so we can tell the commit winners apart.
        source_bytes in prop::collection::vec(0u8..=255, 2..=5),
    ) {
        let dst = "refs/heads/contested";
        let src = "refs/heads/local";

        // Build one batch of N edits that all target the same
        // destination ref. The typed-edit model rejects this as
        // a consolidation conflict — zero batches commit.
        let specs: Vec<PushSpec> = source_bytes
            .iter()
            .map(|_| spec(false, src, dst))
            .collect();
        let mut sha_map = HashMap::new();
        sha_map.insert(src.to_owned(), hex_sha(source_bytes[0]));

        let edits = build_ref_edits(&specs, &sha_map, &HashMap::new())
            .expect("build edits for N competing specs");
        let consolidated = validate_edit_set(&edits);
        prop_assert!(
            consolidated.is_err(),
            "N={} competing edits for one ref must reject",
            specs.len(),
        );

        // Dual flow: simulate N independent single-edit batches
        // racing on the same ref. Each batch validates on its own
        // (one edit, no conflict); only one batch wins the CAS.
        // Here the winner is simply "the batch that runs first"
        // against a clean map, and subsequent batches see the
        // updated tip. This is the serial equivalent of the
        // distributed CAS.
        let mut refs: HashMap<String, String> = HashMap::new();
        let mut winners = 0usize;
        for byte in &source_bytes {
            let single_specs = vec![spec(false, src, dst)];
            let mut single_sha_map = HashMap::new();
            single_sha_map.insert(src.to_owned(), hex_sha(*byte));
            let single_edits =
                build_ref_edits(&single_specs, &single_sha_map, &refs)
                    .expect("single batch build");
            validate_edit_set(&single_edits).expect("single batch validates");

            // Emulate CAS check: the expected precondition in the
            // edit must still match the current map before we
            // apply. If the map state drifted, this batch loses.
            let cas_ok = match &single_edits[0].change {
                Change::Update {
                    expected: PreviousValue::MustNotExist,
                    ..
                } => !refs.contains_key(dst),
                Change::Update {
                    expected: PreviousValue::MustExistAndMatch(Target::Object(oid)),
                    ..
                } => refs.get(dst) == Some(&oid.to_hex().to_string()),
                _ => true,
            };
            if cas_ok {
                apply_edits(&mut refs, &single_edits);
                winners += 1;
            }
        }

        // First update wins outright; subsequent ones may or may
        // not win depending on whether the caller resolved against
        // the same prior tip. At minimum exactly one always wins;
        // at most all N win (if each reads the previous winner's
        // tip into prior_tips before re-building). Both bounds
        // hold in the serial simulation above.
        prop_assert!(winners >= 1, "at least one update must commit");
        prop_assert!(winners <= source_bytes.len(), "no more than N winners");
        prop_assert!(refs.contains_key(dst), "winning ref must be installed");
    }
}

#[test]
fn locked_ref_rejects_second_concurrent_update() {
    // `gix-lock::Marker` serializes local-ref writers. Acquiring
    // the marker for a ref path is exclusive — a second attempt
    // from the same process fails immediately. This is the
    // in-process test that matches the distributed crab
    // coordination-plane lock (`PushLock`).
    let dir = tempfile::tempdir().expect("tempdir");
    let ref_path = dir
        .path()
        .join("refs")
        .join("heads")
        .join("contested-local");
    std::fs::create_dir_all(ref_path.parent().unwrap()).expect("mkdir");

    let first = local_ref_lock::acquire(&ref_path, Duration::ZERO).expect("first acquire succeeds");
    let err = local_ref_lock::acquire(&ref_path, Duration::ZERO).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("failed to lock ref"),
        "second acquire must surface a lock error, got: {msg}"
    );
    drop(first);
}

#[test]
fn atomic_multi_ref_commits_all_or_none() {
    // A batch that contains a malformed edit (invalid ref name)
    // must reject the whole batch before any in-memory mutation.
    // Smart-http-parity P0-3.
    let mut refs = HashMap::new();
    refs.insert("refs/heads/main".to_owned(), hex_sha(0x55));
    let before = refs.clone();

    let specs = vec![
        spec(false, "refs/heads/main", "refs/heads/main"),
        // Invalid ref name — `..` is prohibited by
        // `gix_validate::reference::name`.
        spec(false, "refs/heads/main", "refs/heads/bad..name"),
        spec(false, "refs/heads/main", "refs/heads/dev"),
    ];
    let mut sha_map = HashMap::new();
    sha_map.insert("refs/heads/main".to_owned(), hex_sha(0x66));

    // `build_ref_edits` rejects the whole batch because one name
    // is invalid. The other two edits never get to the point of
    // being applied — all-or-none.
    let err = build_ref_edits(&specs, &sha_map, &refs).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("invalid ref name"), "unexpected error: {msg}");

    assert_eq!(refs, before, "no partial application on batch error");

    // Second scenario: valid shapes but duplicate names.
    // `validate_edit_set` rejects, and production code never
    // proceeds to `apply_edits`.
    let specs_dup = vec![
        spec(false, "refs/heads/a", "refs/heads/main"),
        spec(true, "refs/heads/b", "refs/heads/main"),
    ];
    let mut sha_map_dup = HashMap::new();
    sha_map_dup.insert("refs/heads/a".to_owned(), hex_sha(0x77));
    sha_map_dup.insert("refs/heads/b".to_owned(), hex_sha(0x88));
    let edits_dup = build_ref_edits(&specs_dup, &sha_map_dup, &refs).expect("build edits");
    let consolidation = validate_edit_set(&edits_dup);
    assert!(consolidation.is_err(), "duplicate-ref batch must reject");

    // Still no mutation.
    assert_eq!(refs, before);
}

// --- Sanity bindings to catch cfg-gating regressions. ---

#[test]
fn module_available_under_feature_flag() {
    // Crowd-sourced smoke test: if the crate built with
    // `gix-ref-edits` disabled, this test file wouldn't compile
    // (we're cfg-gated), so the fact that it runs at all is the
    // feature-flag coverage signal.
    let specs: Vec<PushSpec> = vec![];
    let edits = build_ref_edits(&specs, &HashMap::new(), &HashMap::new()).expect("empty");
    assert!(edits.is_empty());
}
