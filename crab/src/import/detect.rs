//! Detect stage for `crab import`.
//!
//! Before enumerate or ingest runs, we need to decide whether the
//! source prefix behaves as a *flat* (non-versioned) bucket, a
//! *versioned* bucket, or a *single-snapshot* target addressed by
//! `--at`. Downstream stages branch on this decision:
//!
//! - Flat → one commit reflecting the current state.
//! - Versioned → window-planned history with one commit per bucket.
//! - `SingleSnapshot { at }` → exactly one commit capturing the
//!   live version of each key at the given epoch-second timestamp.
//!
//! Detection rules follow requirement I1a:
//!
//! 1. `--at <ts>` wins unconditionally — the user asked for a
//!    specific instant, so we do not probe versioning. `--at`
//!    works against flat buckets too (the bucket's current state
//!    *is* the state at `at` in that case).
//! 2. `--versions off` forces `SourceMode::Flat` without sampling
//!    — useful when the user knows the bucket is versioned but
//!    only wants the live state.
//! 3. `--versions auto` (the default) samples up to 1 000 keys via
//!    [`VersionedList::sample`] and classifies via
//!    [`VersionSample::is_versioned`] (duplicates-per-key OR any
//!    delete-marker). If the sample itself fails (e.g. the
//!    backend's versioned-listing is unavailable), we fall back
//!    to `Flat` — the bucket is indistinguishable from flat for
//!    import purposes.
//! 4. `--versions on` requires versioning. If the sample shows a
//!    flat bucket we raise [`CrabError::ImportVersioningUnavailable`]
//!    so the user can correct their flags before we start work. A
//!    sample-level error in `on` mode surfaces verbatim rather
//!    than being hidden — it is typically more actionable than a
//!    "flat" fallback when the user explicitly asked for
//!    versioned import.
//!
//! The [`SourceMode`] returned here also converts to and from the
//! journal's [`SourceModeTag`] so the coordinator can record the
//! decision in the resume plan.

use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::cmd::import::VersionsMode;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::import::journal::SourceModeTag;
use crate::import::versions::{VersionedList, VersionedListImpl};

/// How the detect stage classified the source prefix.
///
/// `Debug + Clone + PartialEq + Eq` makes this convenient to
/// assert against in tests and to pass through the coordinator
/// struct without fighting lifetimes. `SingleSnapshot { at }`
/// stores the caller-provided timestamp as epoch seconds so it
/// lines up with [`crate::import::journal::PlanInputs::snapshot_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMode {
    /// Flat (non-versioned) bucket. One commit will reflect the
    /// current state.
    Flat,
    /// Versioned bucket — duplicates or delete markers observed.
    /// Enumerate + assemble produce one commit per time window.
    Versioned,
    /// Single-snapshot import at the given epoch-second timestamp.
    SingleSnapshot { at: i64 },
}

impl SourceMode {
    /// Convert to the stable integer tag used in the resume
    /// journal's `plan.source_mode` column.
    ///
    /// The journal drops the `at` payload in [`SourceModeTag`];
    /// callers that need to persist the timestamp write it to
    /// `PlanInputs::snapshot_at` separately.
    #[must_use]
    pub fn tag(&self) -> SourceModeTag {
        match self {
            Self::Flat => SourceModeTag::Flat,
            Self::Versioned => SourceModeTag::Versioned,
            Self::SingleSnapshot { .. } => SourceModeTag::SingleSnapshot,
        }
    }

    /// Build a [`SourceMode`] from a journal tag and (optional)
    /// snapshot timestamp. Returns an error only if the tag is
    /// `SingleSnapshot` but no timestamp was provided — the two
    /// fields must agree or the plan is malformed.
    pub fn from_tag(tag: SourceModeTag, snapshot_at: Option<i64>) -> Result<Self> {
        match (tag, snapshot_at) {
            (SourceModeTag::Flat, _) => Ok(Self::Flat),
            (SourceModeTag::Versioned, _) => Ok(Self::Versioned),
            (SourceModeTag::SingleSnapshot, Some(at)) => Ok(Self::SingleSnapshot { at }),
            (SourceModeTag::SingleSnapshot, None) => Err(CrabError::Internal(
                "SourceMode::from_tag: SingleSnapshot tag without snapshot_at".into(),
            )),
        }
    }
}

/// Subset of `ImportArgs` that the detect stage actually reads.
///
/// Carrying a focused struct keeps the signature testable — call
/// sites assemble one of these from their full `ImportArgs` (or
/// their test fixture) and pass it in. `source_url` is used only
/// for the [`CrabError::ImportVersioningUnavailable`] message;
/// keep it in sync with whatever the user typed on the CLI so the
/// hint points at the right bucket.
#[derive(Debug, Clone)]
pub struct DetectArgs<'a> {
    /// `--versions on|off|auto` — drives the detection branch.
    pub versions: VersionsMode,
    /// `--at <rfc3339>` parsed to epoch seconds. When set, wins
    /// over `versions` and sampling.
    pub at: Option<i64>,
    /// User-facing source URL for error messages.
    pub source_url: &'a str,
}

/// Size of the detect-stage sample. Matches requirement I1a.
const SAMPLE_LIMIT: usize = 1_000;

/// Classify the source prefix as flat, versioned, or single-
/// snapshot.
///
/// Runs before enumerate + ingest so the pipeline can pick the
/// right listing path and commit plan up front. The `cancel` check
/// happens before the (potentially slow) sample call so a user
/// Ctrl+C between stages never drags us into a cloud round-trip we
/// are about to throw away.
///
/// # Errors
///
/// - [`CrabError::Cancelled`] when `cancel` has already fired.
/// - [`CrabError::ImportVersioningUnavailable`] when
///   `--versions on` was requested but the sample reports a flat
///   bucket.
/// - Any error bubbled from [`VersionedList::sample`] when the
///   user passed `--versions on` and the sample itself failed.
///   In `--versions auto` we swallow the sample error and fall
///   back to flat — see module-level docs.
pub async fn detect_source_mode(
    source: &dyn VersionedList,
    args: &DetectArgs<'_>,
    cancel: &CancellationToken,
) -> Result<SourceMode> {
    check_cancelled(cancel)?;

    // `--at` wins unconditionally. The caller may have passed
    // `--versions off` *and* `--at`; per requirement I1a single-
    // snapshot mode works against both flat and versioned
    // buckets, so `--at` selects `SingleSnapshot` regardless of
    // the versions flag. No probing needed — the at-timestamp
    // pass runs against whatever listing the source exposes.
    if let Some(at) = args.at {
        debug!(at, "detect: --at set, selecting SingleSnapshot");
        return Ok(SourceMode::SingleSnapshot { at });
    }

    match args.versions {
        VersionsMode::Off => {
            // User asserts the bucket is flat (or wants to treat
            // it as flat). Skip sampling — honoring the assertion
            // also saves a potentially slow list round-trip.
            debug!("detect: --versions off, selecting Flat without sampling");
            Ok(SourceMode::Flat)
        }
        VersionsMode::Auto => {
            // Probe the sample. Any error is treated as "cannot
            // confirm versioning" and we fall back to flat, since
            // a bucket we can't enumerate versions on is
            // indistinguishable from a flat bucket for import
            // purposes. Surface the underlying reason at debug
            // level so operators chasing "why didn't it detect
            // versioning?" have a breadcrumb.
            match source.sample(SAMPLE_LIMIT).await {
                Ok(sample) => {
                    let versioned = sample.is_versioned();
                    info!(
                        records = sample.total_versions,
                        unique_keys = sample.unique_keys,
                        delete_markers = sample.has_delete_markers,
                        versioned,
                        "detect: sample classified source"
                    );
                    if versioned {
                        Ok(SourceMode::Versioned)
                    } else {
                        Ok(SourceMode::Flat)
                    }
                }
                Err(err) => {
                    debug!(
                        ?err,
                        "detect: sample failed under --versions auto; falling back to Flat"
                    );
                    Ok(SourceMode::Flat)
                }
            }
        }
        VersionsMode::On => {
            // Strict mode: the user insists on a versioned
            // import. Sampling errors surface verbatim (they are
            // typically "this backend doesn't implement versioned
            // listing yet", which is more useful than a vague
            // "bucket is flat" fallback). A sample that comes
            // back non-versioned is a hard
            // `ImportVersioningUnavailable`.
            let sample = source.sample(SAMPLE_LIMIT).await?;
            if sample.is_versioned() {
                info!(
                    records = sample.total_versions,
                    unique_keys = sample.unique_keys,
                    delete_markers = sample.has_delete_markers,
                    "detect: --versions on confirmed versioned source"
                );
                Ok(SourceMode::Versioned)
            } else {
                Err(CrabError::ImportVersioningUnavailable {
                    url: args.source_url.to_string(),
                })
            }
        }
    }
}

/// Convenience: build [`DetectArgs`] from the coordinator's
/// [`VersionedListImpl`] enum and the import CLI state.
///
/// Kept as a module-level helper so tests can assemble a
/// `DetectArgs` without needing to thread an `ImportArgs` through.
#[allow(dead_code)] // wired up by the coordinator in Task 14
pub async fn detect_for_impl(
    source: &VersionedListImpl,
    args: &DetectArgs<'_>,
    cancel: &CancellationToken,
) -> Result<SourceMode> {
    detect_source_mode(source, args, cancel).await
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::import::versions::{VersionRecord, VersionSample};

    /// Minimal mock `VersionedList` for driving detect without
    /// touching the filesystem or any cloud SDK.
    ///
    /// Records `sample` call counts so tests can assert
    /// `--versions off` and `--at` truly short-circuit sampling.
    struct MockVersionedList {
        sample_response: Result<VersionSample>,
        sample_calls: Arc<AtomicUsize>,
    }

    impl MockVersionedList {
        fn flat() -> Self {
            Self {
                sample_response: Ok(VersionSample {
                    total_versions: 5,
                    unique_keys: 5,
                    has_delete_markers: false,
                    records: Vec::new(),
                }),
                sample_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn versioned_duplicates() -> Self {
            Self {
                sample_response: Ok(VersionSample {
                    total_versions: 10,
                    unique_keys: 4,
                    has_delete_markers: false,
                    records: Vec::new(),
                }),
                sample_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn versioned_delete_markers() -> Self {
            Self {
                sample_response: Ok(VersionSample {
                    total_versions: 5,
                    unique_keys: 5,
                    has_delete_markers: true,
                    records: Vec::new(),
                }),
                sample_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn sample_error() -> Self {
            Self {
                sample_response: Err(CrabError::Internal("mock sample failure".into())),
                sample_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn sample_call_count(&self) -> usize {
            self.sample_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl VersionedList for MockVersionedList {
        async fn sample(&self, _limit: usize) -> Result<VersionSample> {
            self.sample_calls.fetch_add(1, Ordering::SeqCst);
            match &self.sample_response {
                Ok(s) => Ok(s.clone()),
                // Clone the specific error variants we use in tests.
                // `CrabError` does not implement `Clone`; rebuild
                // the single variant we care about.
                Err(CrabError::Internal(msg)) => Err(CrabError::Internal(msg.clone())),
                Err(other) => Err(CrabError::Internal(format!(
                    "unhandled mock error: {other}"
                ))),
            }
        }

        async fn enumerate(
            &self,
            _since: Option<i64>,
            _until: Option<i64>,
            _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            unreachable!("detect does not call enumerate")
        }

        async fn enumerate_at(
            &self,
            _at: i64,
            _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            unreachable!("detect does not call enumerate_at")
        }
    }

    fn args(versions: VersionsMode, at: Option<i64>) -> DetectArgs<'static> {
        DetectArgs {
            versions,
            at,
            source_url: "s3://example-bucket/prefix",
        }
    }

    // --- --versions auto sampling ---

    #[tokio::test]
    async fn auto_mode_classifies_versioned_on_duplicates() {
        // Duplicates-per-key alone flip the bit — the sample's
        // `is_versioned()` rule is the only source of truth.
        let mock = MockVersionedList::versioned_duplicates();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::Auto, None), &cancel)
            .await
            .unwrap();

        assert_eq!(mode, SourceMode::Versioned);
        assert_eq!(mock.sample_call_count(), 1, "auto mode must sample once");
    }

    #[tokio::test]
    async fn auto_mode_classifies_versioned_on_delete_markers() {
        // Delete markers alone also flip the bit, even when the
        // unique-key / total-version counts match.
        let mock = MockVersionedList::versioned_delete_markers();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::Auto, None), &cancel)
            .await
            .unwrap();

        assert_eq!(mode, SourceMode::Versioned);
    }

    #[tokio::test]
    async fn auto_mode_classifies_flat_when_sample_is_non_versioned() {
        // One version per key, no delete markers — classic flat.
        let mock = MockVersionedList::flat();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::Auto, None), &cancel)
            .await
            .unwrap();

        assert_eq!(mode, SourceMode::Flat);
        assert_eq!(mock.sample_call_count(), 1);
    }

    #[tokio::test]
    async fn auto_mode_falls_back_to_flat_on_sample_error() {
        // A backend that can't sample (stub cloud listers, for
        // example) is indistinguishable from a flat bucket — pick
        // Flat rather than surfacing an opaque error. The
        // docstring on the cloud stubs in `versions.rs` relies on
        // this behavior.
        let mock = MockVersionedList::sample_error();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::Auto, None), &cancel)
            .await
            .unwrap();

        assert_eq!(mode, SourceMode::Flat);
    }

    // --- --versions on (strict) ---

    #[tokio::test]
    async fn versions_on_flat_bucket_errors_with_unavailable() {
        // Strict mode against a flat bucket must surface
        // `ImportVersioningUnavailable` with the source URL so
        // the user can fix their flags.
        let mock = MockVersionedList::flat();
        let cancel = CancellationToken::new();

        let err = detect_source_mode(&mock, &args(VersionsMode::On, None), &cancel)
            .await
            .expect_err("strict mode must error on flat");

        match err {
            CrabError::ImportVersioningUnavailable { url } => {
                assert_eq!(url, "s3://example-bucket/prefix");
            }
            other => panic!("expected ImportVersioningUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn versions_on_versioned_bucket_returns_versioned() {
        // Happy path for `--versions on` — sample confirms and we
        // return Versioned.
        let mock = MockVersionedList::versioned_duplicates();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::On, None), &cancel)
            .await
            .unwrap();
        assert_eq!(mode, SourceMode::Versioned);
    }

    #[tokio::test]
    async fn versions_on_bubbles_sample_errors() {
        // Strict mode must not hide sample errors the way auto
        // does — the caller asked for versioning and has to know
        // if we couldn't confirm.
        let mock = MockVersionedList::sample_error();
        let cancel = CancellationToken::new();

        let err = detect_source_mode(&mock, &args(VersionsMode::On, None), &cancel)
            .await
            .expect_err("strict mode must surface sample errors");
        assert!(matches!(err, CrabError::Internal(_)));
    }

    // --- --versions off ---

    #[tokio::test]
    async fn versions_off_returns_flat_without_sampling() {
        // `--versions off` asserts the bucket is flat; we skip
        // the sample round-trip entirely. The mock counter proves
        // it.
        let mock = MockVersionedList::versioned_duplicates();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::Off, None), &cancel)
            .await
            .unwrap();

        assert_eq!(mode, SourceMode::Flat);
        assert_eq!(
            mock.sample_call_count(),
            0,
            "--versions off must not sample"
        );
    }

    // --- --at short-circuit ---

    #[tokio::test]
    async fn at_selects_single_snapshot_and_does_not_sample() {
        // `--at` carries the user's explicit snapshot intent;
        // detect must not probe versions. The timestamp survives
        // verbatim into the SourceMode payload.
        let mock = MockVersionedList::versioned_duplicates();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(
            &mock,
            &args(VersionsMode::Auto, Some(1_700_000_000)),
            &cancel,
        )
        .await
        .unwrap();

        assert_eq!(mode, SourceMode::SingleSnapshot { at: 1_700_000_000 });
        assert_eq!(
            mock.sample_call_count(),
            0,
            "--at must short-circuit sampling"
        );
    }

    #[tokio::test]
    async fn at_wins_over_versions_on() {
        // Even when the user passed `--versions on`, an explicit
        // `--at` selects SingleSnapshot per I1a. The at-timestamp
        // path works regardless of versioning mode.
        let mock = MockVersionedList::flat();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::On, Some(42)), &cancel)
            .await
            .unwrap();
        assert_eq!(mode, SourceMode::SingleSnapshot { at: 42 });
        assert_eq!(mock.sample_call_count(), 0);
    }

    #[tokio::test]
    async fn at_wins_over_versions_off() {
        // `--at` against `--versions off` still selects
        // SingleSnapshot — the user asked for a point-in-time
        // import. Documented explicitly in task 6.4.
        let mock = MockVersionedList::flat();
        let cancel = CancellationToken::new();

        let mode = detect_source_mode(&mock, &args(VersionsMode::Off, Some(99)), &cancel)
            .await
            .unwrap();
        assert_eq!(mode, SourceMode::SingleSnapshot { at: 99 });
    }

    // --- Cancellation ---

    #[tokio::test]
    async fn pre_cancelled_token_returns_cancelled() {
        // Cancel check must land before sampling so an already-
        // cancelled token never drags us into a cloud round-trip.
        let mock = MockVersionedList::flat();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = detect_source_mode(&mock, &args(VersionsMode::Auto, None), &cancel)
            .await
            .expect_err("pre-cancelled token must surface Cancelled");

        assert!(matches!(err, CrabError::Cancelled));
        assert_eq!(
            mock.sample_call_count(),
            0,
            "cancellation must short-circuit before sampling"
        );
    }

    // --- SourceMode tag round-trip ---

    #[test]
    fn source_mode_tag_round_trip() {
        // Tag conversion must preserve every mode the coordinator
        // persists to the journal. Tests here guard the journal's
        // source_mode column against a silent semantic drift.
        assert_eq!(SourceMode::Flat.tag(), SourceModeTag::Flat);
        assert_eq!(SourceMode::Versioned.tag(), SourceModeTag::Versioned);
        assert_eq!(
            SourceMode::SingleSnapshot { at: 7 }.tag(),
            SourceModeTag::SingleSnapshot
        );

        assert_eq!(
            SourceMode::from_tag(SourceModeTag::Flat, None).unwrap(),
            SourceMode::Flat
        );
        assert_eq!(
            SourceMode::from_tag(SourceModeTag::Versioned, None).unwrap(),
            SourceMode::Versioned
        );
        assert_eq!(
            SourceMode::from_tag(SourceModeTag::SingleSnapshot, Some(12)).unwrap(),
            SourceMode::SingleSnapshot { at: 12 }
        );
    }

    #[test]
    fn source_mode_from_tag_rejects_snapshot_without_timestamp() {
        // A SingleSnapshot tag with no snapshot_at is a malformed
        // plan row — surfacing Internal is the right thing
        // because the journal shouldn't be able to produce it in
        // the first place.
        let err = SourceMode::from_tag(SourceModeTag::SingleSnapshot, None)
            .expect_err("missing timestamp must error");
        assert!(matches!(err, CrabError::Internal(_)));
    }

    // --- detect_for_impl dispatch ---

    #[tokio::test]
    async fn detect_for_impl_dispatches_through_versioned_list_impl() {
        // The coordinator will hand us a `VersionedListImpl`;
        // `detect_for_impl` must forward to the same code path.
        // We use the Local variant (a real implementation) so the
        // dispatch is exercised end-to-end.
        use crate::import::versions::LocalVersionedList;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();

        let local = LocalVersionedList::new(tmp.path().to_path_buf());
        let imp = VersionedListImpl::Local(local);
        let cancel = CancellationToken::new();

        let mode = detect_for_impl(&imp, &args(VersionsMode::Auto, None), &cancel)
            .await
            .unwrap();

        // LocalVersionedList always reports non-versioned.
        assert_eq!(mode, SourceMode::Flat);
    }
}
