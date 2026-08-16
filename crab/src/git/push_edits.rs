//! Typed ref-edit construction for the push pipeline.
//!
//! This module collapses the ad-hoc `PushSpec::is_empty() → skip`
//! short-circuit in [`crate::git::push_native`] and [`crate::git::push`]
//! onto `gix-ref`'s `RefEdit` type. Every push refspec becomes a
//! typed `Create`, `Update`, or `Delete` change with a precondition
//! describing what the remote value must look like for the edit to
//! apply. That gives the S3 CAS layer a single, uniform in-memory
//! edit model instead of per-call-site filter/branch logic.
//!
//! Three build rules:
//!
//! - `src.is_empty()` (the `push :refs/heads/old` form in
//!   git-remote-helper protocol) → `Change::Delete`.
//!   Closes smart-http-parity P0-1.
//! - `src != ""` with a known prior SHA for `dst` →
//!   `Change::Update { expected: MustExistAndMatch(prev), new }`.
//! - `src != ""` with no prior SHA for `dst` →
//!   `Change::Update { expected: MustNotExist, new }`.
//!   (A fresh remote ref creation; `RefEdit` doesn't have a
//!   distinct `Create` variant — `Update` with `MustNotExist` is
//!   how gitoxide models it.)
//!
//! Force-push (`spec.force == true`) demotes the precondition to
//! `PreviousValue::Any`, matching git's `+refspec` semantics.
//!
//! After building the batch, the caller runs
//! [`validate_edit_set`] which consolidates the edits via
//! `gix_ref::transaction::RefEditsExt::assure_one_name_has_one_edit`.
//! Two edits for the same ref name — the only consolidation case
//! crab actually hits, since we don't resolve symbolic refs at
//! this layer — surface as [`CrabError::Internal`] before the
//! first S3 call. This is the `Change::consolidate` shape called
//! for by the spec; the real gix-ref API names the operation
//! `assure_one_name_has_one_edit`, so this module adopts that
//! naming.
//!
//! LOC accounting: the legacy `.filter(|s| !s.src.is_empty())` and
//! `if spec.src.is_empty() { continue; }` call sites stay in the
//! legacy build. Under `--features gix-ref-edits`, they are replaced
//! by a single call to [`build_ref_edits`] + dispatch on the
//! resulting `Change` variant.

#![cfg(feature = "gix-ref-edits")]

use std::collections::HashMap;

use gix_hash::ObjectId;
use gix_ref::{
    FullName, Target,
    transaction::{Change, LogChange, PreviousValue, RefEdit, RefEditsExt, RefLog},
};

use crate::core::error::{CrabError, Result};
use crate::core::tracing::gix_boundary;
use crate::git::remote_helper::PushSpec;

/// Build a batch of `gix-ref` ref edits from the push specs plus the
/// resolved source-SHA map.
///
/// The `sha_map` is produced by [`crab_git::ref_resolve::resolve_refs_batch`]
/// upstream: it maps each non-empty `spec.src` to its resolved hex
/// SHA. For delete specs (`spec.src.is_empty()`), `sha_map` is not
/// consulted.
///
/// `prior_tips` carries the remote tip SHAs (keyed by `dst`) known
/// before the push — used to pick the correct [`PreviousValue`]
/// precondition. A missing entry means "we don't know the prior
/// state" which, for a force push, translates to
/// [`PreviousValue::Any`]; for a normal push to an unknown ref, it
/// translates to [`PreviousValue::MustNotExist`] (the ref is
/// being created). For a normal push with a known prior, we use
/// [`PreviousValue::MustExistAndMatch`] so the CAS fails if the
/// remote tip moved since we read it.
///
/// # Errors
///
/// - [`CrabError::Internal`] when `dst` isn't a valid ref name
///   (e.g. contains ASCII control characters, has `//`, ends in
///   `.lock`, or otherwise fails `gix_validate::reference::name`).
/// - [`CrabError::Internal`] when a non-delete spec has an empty
///   or missing entry in `sha_map` (caller didn't pre-resolve).
/// - [`CrabError::Internal`] when a SHA isn't 40 hex chars.
pub fn build_ref_edits(
    specs: &[PushSpec],
    sha_map: &HashMap<String, String>,
    prior_tips: &HashMap<String, String>,
) -> Result<Vec<RefEdit>> {
    let _span = gix_boundary!("refs", "build_ref_edits").entered();

    let mut edits = Vec::with_capacity(specs.len());
    for spec in specs {
        let name = FullName::try_from(spec.dst.as_str()).map_err(|e| {
            CrabError::Internal(format!("invalid ref name '{}' in push spec: {e}", spec.dst))
        })?;

        let change = if spec.src.is_empty() {
            // Delete ref. The expected value mirrors git's
            // receive-pack semantics: if we know the prior tip, we
            // match on it (so a concurrent update rejects the
            // delete); if we don't, `Any` keeps the delete
            // unconditional. `MustNotExist` is explicitly invalid
            // for `Delete` per gix-ref docs.
            let expected = match prior_tips.get(&spec.dst) {
                Some(prev) if !spec.force && !prev.is_empty() => {
                    // Malformed prior SHAs (non-hex, wrong length)
                    // come from legacy manifests predating strict
                    // validation. Downgrade to `Any` rather than
                    // failing the push — the S3 CAS will still
                    // catch real conflicts via its own ETag path.
                    match parse_sha_opt(prev) {
                        Some(oid) => PreviousValue::MustExistAndMatch(Target::Object(oid)),
                        None => PreviousValue::Any,
                    }
                }
                _ => PreviousValue::Any,
            };
            Change::Delete {
                expected,
                log: RefLog::AndReference,
            }
        } else {
            let new_sha = sha_map
                .get(&spec.src)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CrabError::Internal(format!(
                        "build_ref_edits: missing resolved SHA for src ref '{}'",
                        spec.src
                    ))
                })?;
            let new_oid = parse_sha(new_sha, &spec.src)?;

            let expected = if spec.force {
                // Force push: bypass the precondition; preserves
                // the typed-edit model while matching git's
                // `+refspec` semantics.
                PreviousValue::Any
            } else {
                match prior_tips.get(&spec.dst) {
                    Some(prev) if !prev.is_empty() => match parse_sha_opt(prev) {
                        Some(prev_oid) => {
                            PreviousValue::MustExistAndMatch(Target::Object(prev_oid))
                        }
                        // Same legacy-SHA tolerance as for deletes.
                        None => PreviousValue::Any,
                    },
                    _ => PreviousValue::MustNotExist,
                }
            };

            Change::Update {
                log: LogChange::default(),
                expected,
                new: Target::Object(new_oid),
            }
        };

        edits.push(RefEdit {
            change,
            name,
            // `deref = false`: crab operates on full ref names
            // end-to-end. Symbolic-ref splitting is a property of
            // local `.git/refs/HEAD` handling, not the S3 ref
            // plane.
            deref: false,
        });
    }

    Ok(edits)
}

/// Validate the edit set before any CAS call.
///
/// Rejects two edits naming the same ref (which would race against
/// each other during CAS and produce a non-deterministic winner),
/// and any edit whose `dst` failed name validation. Runs once over
/// the whole batch — not per-ref — so a partially-published batch
/// is impossible. This is the validation pass called for by
/// smart-http-parity P0-3 (atomic multi-ref).
///
/// The spec calls this step `Change::consolidate`. In the actual
/// gix-ref API, it's `RefEditsExt::assure_one_name_has_one_edit`.
///
/// # Errors
///
/// [`CrabError::Internal`] when two edits share a ref name.
pub fn validate_edit_set(edits: &[RefEdit]) -> Result<()> {
    let _span = gix_boundary!("refs", "validate_edit_set").entered();

    // `RefEditsExt` is implemented on `Vec<E: Borrow<RefEdit>>`.
    // Cheapest way to invoke it without a clone is to wrap the
    // slice references. The trait doesn't mutate the contents for
    // this call.
    let refs: Vec<&RefEdit> = edits.iter().collect();
    // SAFETY-style note: `Vec<&RefEdit>` satisfies
    // `E: Borrow<RefEdit> + BorrowMut<RefEdit>` because
    // `&T: Borrow<T>`; however `BorrowMut` is not impl'd for `&T`,
    // so we need to materialise owned clones for the ext call.
    // Clone is cheap — RefEdit is a small enum of string + oid —
    // and this runs once per push, not per ref.
    let owned: Vec<RefEdit> = refs.iter().map(|r| (*r).clone()).collect();
    owned.assure_one_name_has_one_edit().map_err(|name| {
        CrabError::Internal(format!(
            "conflicting ref edits: ref '{}' has multiple edits in one batch",
            name
        ))
    })
}

/// Classify an edit as a delete.
///
/// Used by push-pipeline call sites that used to key off
/// `spec.src.is_empty()`. With the typed model, delete-vs-update
/// dispatch becomes a pattern match on [`Change`].
#[must_use]
pub fn is_delete(edit: &RefEdit) -> bool {
    matches!(edit.change, Change::Delete { .. })
}

/// Extract the destination ref name of an edit.
///
/// Convenience for loops that need to project back to the ref-name
/// string used by downstream crab code (manifest keys, outcome
/// maps).
pub fn dst_name(edit: &RefEdit) -> String {
    edit.name.as_bstr().to_string()
}

/// Parse a 40-char hex SHA-1 into an [`ObjectId`].
fn parse_sha(sha: &str, context: &str) -> Result<ObjectId> {
    ObjectId::from_hex(sha.as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid SHA '{sha}' for '{context}': {e}")))
}

/// Best-effort variant of [`parse_sha`] used for prior-tip values
/// read from the manifest. Returns `None` on parse failure instead
/// of producing an error — the caller handles the miss by falling
/// back to `PreviousValue::Any`, which keeps a malformed legacy
/// manifest entry from blocking a push.
fn parse_sha_opt(sha: &str) -> Option<ObjectId> {
    ObjectId::from_hex(sha.as_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(force: bool, src: &str, dst: &str) -> PushSpec {
        PushSpec {
            force,
            src: src.to_owned(),
            dst: dst.to_owned(),
        }
    }

    fn sha(byte: u8) -> String {
        std::iter::repeat(byte)
            .take(20)
            .fold(String::new(), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            })
    }

    #[test]
    fn delete_refspec_produces_delete_change() {
        // `push :refs/heads/old` — empty src, delete the ref.
        let specs = vec![spec(false, "", "refs/heads/old")];
        let edits = build_ref_edits(&specs, &HashMap::new(), &HashMap::new()).expect("build");
        assert_eq!(edits.len(), 1);
        assert!(is_delete(&edits[0]));
        match &edits[0].change {
            Change::Delete { expected, .. } => {
                assert_eq!(*expected, PreviousValue::Any);
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn delete_with_known_prior_tip_uses_match() {
        // Known prior tip tightens the precondition so a concurrent
        // update rejects the delete.
        let specs = vec![spec(false, "", "refs/heads/old")];
        let mut prior = HashMap::new();
        prior.insert("refs/heads/old".to_owned(), sha(0xab));
        let edits = build_ref_edits(&specs, &HashMap::new(), &prior).expect("build");
        match &edits[0].change {
            Change::Delete {
                expected: PreviousValue::MustExistAndMatch(_),
                ..
            } => {}
            other => panic!("expected Delete with MustExistAndMatch, got {other:?}"),
        }
    }

    #[test]
    fn new_ref_uses_must_not_exist() {
        // `push refs/heads/main:refs/heads/main` for a ref the
        // remote has never seen — MustNotExist seals the create
        // against a concurrent push creating the same ref.
        let specs = vec![spec(false, "refs/heads/main", "refs/heads/main")];
        let mut sha_map = HashMap::new();
        sha_map.insert("refs/heads/main".to_owned(), sha(0x11));
        let edits = build_ref_edits(&specs, &sha_map, &HashMap::new()).expect("build");
        match &edits[0].change {
            Change::Update {
                expected: PreviousValue::MustNotExist,
                ..
            } => {}
            other => panic!("expected Update with MustNotExist, got {other:?}"),
        }
    }

    #[test]
    fn existing_ref_uses_must_exist_and_match() {
        let specs = vec![spec(false, "refs/heads/main", "refs/heads/main")];
        let mut sha_map = HashMap::new();
        sha_map.insert("refs/heads/main".to_owned(), sha(0x22));
        let mut prior = HashMap::new();
        prior.insert("refs/heads/main".to_owned(), sha(0x11));
        let edits = build_ref_edits(&specs, &sha_map, &prior).expect("build");
        match &edits[0].change {
            Change::Update {
                expected: PreviousValue::MustExistAndMatch(_),
                ..
            } => {}
            other => panic!("expected Update with MustExistAndMatch, got {other:?}"),
        }
    }

    #[test]
    fn force_push_uses_any_precondition() {
        let specs = vec![spec(true, "refs/heads/main", "refs/heads/main")];
        let mut sha_map = HashMap::new();
        sha_map.insert("refs/heads/main".to_owned(), sha(0x33));
        let mut prior = HashMap::new();
        prior.insert("refs/heads/main".to_owned(), sha(0x11));
        let edits = build_ref_edits(&specs, &sha_map, &prior).expect("build");
        match &edits[0].change {
            Change::Update {
                expected: PreviousValue::Any,
                ..
            } => {}
            other => panic!("expected Update with Any for force push, got {other:?}"),
        }
    }

    #[test]
    fn invalid_ref_name_rejected() {
        // `HEAD..` is not a valid full ref name.
        let specs = vec![spec(false, "refs/heads/main", "invalid..name")];
        let mut sha_map = HashMap::new();
        sha_map.insert("refs/heads/main".to_owned(), sha(0x44));
        let err = build_ref_edits(&specs, &sha_map, &HashMap::new()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid ref name"), "unexpected error: {msg}");
    }

    #[test]
    fn missing_sha_in_map_rejected() {
        let specs = vec![spec(false, "refs/heads/main", "refs/heads/main")];
        let err = build_ref_edits(&specs, &HashMap::new(), &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("missing resolved SHA"));
    }

    #[test]
    fn consolidation_rejects_duplicate_ref() {
        let specs = vec![
            spec(false, "refs/heads/main", "refs/heads/main"),
            spec(true, "refs/heads/main", "refs/heads/main"),
        ];
        let mut sha_map = HashMap::new();
        sha_map.insert("refs/heads/main".to_owned(), sha(0x55));
        let edits = build_ref_edits(&specs, &sha_map, &HashMap::new()).expect("build");
        // Two edits for the same ref — consolidation must reject.
        let err = validate_edit_set(&edits).unwrap_err();
        assert!(err.to_string().contains("conflicting ref edits"));
    }

    #[test]
    fn consolidation_accepts_disjoint_edits() {
        let specs = vec![
            spec(false, "refs/heads/main", "refs/heads/main"),
            spec(false, "", "refs/heads/old"),
        ];
        let mut sha_map = HashMap::new();
        sha_map.insert("refs/heads/main".to_owned(), sha(0x66));
        let edits = build_ref_edits(&specs, &sha_map, &HashMap::new()).expect("build");
        validate_edit_set(&edits).expect("disjoint edits must validate");
    }

    #[test]
    fn dst_name_matches_input() {
        let specs = vec![spec(false, "", "refs/heads/old")];
        let edits = build_ref_edits(&specs, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(dst_name(&edits[0]), "refs/heads/old");
    }
}

/// Lockfile helper for local `.git/refs/` writes.
///
/// Wraps `gix_lock::Marker` with crab's error surface and tracing
/// span. Callers acquire a marker before writing a loose ref under
/// `.git/refs/<name>` and hold it until the write + rename is
/// complete. The marker is dropped automatically, releasing the
/// lockfile. This mirrors git's own refs/<name>.lock convention —
/// signal-safe cleanup is provided by `gix-tempfile` under the
/// marker.
///
/// Note: today crab's fetch path does not write local refs
/// directly — the git client consumes the remote-helper `fetch`
/// response and writes refs itself. This helper exists so any
/// future local-ref-write site in crab (e.g. a fetch-install
/// path that bypasses the helper protocol, or the `install_head`
/// path in `cmd/install.rs` if it ever moves off shellout) has a
/// single, tested entry point and inherits the same atomic
/// semantics.
///
/// The S3-side CAS path keeps using crab's coordination-plane
/// lock (`crab_coordination::PushLock`). `gix-lock` is
/// file-backed and has no S3 equivalent, and the S3 ref plane is
/// not trait-abstracted by gitoxide yet (Req 4 Phase B / spec
/// §Gitoxide Capability Boundary). This module's scope is local
/// refs only.
pub mod local_ref_lock {
    use std::path::Path;
    use std::time::Duration;

    use gix_lock::{Marker, acquire::Fail};

    use crate::core::error::{CrabError, Result};
    use crate::core::tracing::gix_boundary;

    /// Acquire an exclusive marker lockfile for the ref at `ref_path`.
    ///
    /// `ref_path` must point at the loose-ref file itself, e.g.
    /// `<git_dir>/refs/heads/main`. The resulting `<ref_path>.lock`
    /// is created atomically; if another process already holds the
    /// lock the call retries for up to `timeout` (or fails
    /// immediately when `timeout` is zero).
    ///
    /// Drop the returned [`Marker`] to release. The marker intentionally
    /// holds no open file handle — it exists to serialize writers, not
    /// to stage bytes. To atomically replace the ref content, wrap the
    /// same path in `gix_lock::File::acquire_to_update_resource` in the
    /// write site itself; this marker just turns "ref is being
    /// updated" into an observable filesystem signal for any sibling
    /// writer.
    ///
    /// # Errors
    ///
    /// - [`CrabError::Internal`] on any lock-acquisition failure.
    ///   Error messages include the resource path and attempt count
    ///   from `gix-lock`.
    pub fn acquire(ref_path: &Path, timeout: Duration) -> Result<Marker> {
        let _span = gix_boundary!("lock", "acquire_ref_marker").entered();

        let mode = if timeout.is_zero() {
            Fail::Immediately
        } else {
            Fail::AfterDurationWithBackoff(timeout)
        };

        // `boundary_directory = None` — the `.git/refs/` tree is
        // pre-created by `git init`; we don't want `gix-lock` to
        // auto-clean parent dirs on failure because that would
        // remove directories another writer might still need.
        Marker::acquire_to_hold_resource(ref_path, mode, None).map_err(|e| {
            CrabError::Internal(format!("failed to lock ref at {}: {e}", ref_path.display()))
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn acquires_lock_for_fresh_ref_path() {
            let dir = tempfile::tempdir().expect("tempdir");
            let ref_path = dir.path().join("refs").join("heads").join("main");
            std::fs::create_dir_all(ref_path.parent().unwrap()).expect("mkdir");

            let _marker = acquire(&ref_path, Duration::ZERO).expect("first acquire");
            // Hold the marker through the end of the test — drop
            // releases the lock.
        }

        #[test]
        fn second_acquire_fails_when_first_is_held() {
            let dir = tempfile::tempdir().expect("tempdir");
            let ref_path = dir.path().join("refs").join("heads").join("contested");
            std::fs::create_dir_all(ref_path.parent().unwrap()).expect("mkdir");

            let first = acquire(&ref_path, Duration::ZERO).expect("first acquire");
            let err = acquire(&ref_path, Duration::ZERO).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("failed to lock ref"),
                "unexpected error: {msg}"
            );
            // Keep `first` alive through the second attempt so the
            // lock is genuinely contested at the moment of the
            // call, not racing on drop.
            drop(first);
        }

        #[test]
        fn acquire_after_release_succeeds() {
            let dir = tempfile::tempdir().expect("tempdir");
            let ref_path = dir.path().join("refs").join("heads").join("releasable");
            std::fs::create_dir_all(ref_path.parent().unwrap()).expect("mkdir");

            {
                let _marker = acquire(&ref_path, Duration::ZERO).expect("first");
            } // dropped here
            let _reacquired = acquire(&ref_path, Duration::ZERO).expect("second");
        }
    }
}
