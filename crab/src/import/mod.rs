//! `crab import` — materialize a Crab-backed git repo from an
//! existing object-storage prefix.
//!
//! Pipeline stages (detect, enumerate, ingest, assemble, publish)
//! land here across multiple PRs. This module re-exports the shared
//! types that the stages and the CLI entry point both need.

pub mod assemble;
pub mod coordinator;
pub mod detect;
pub mod enumerate;
pub mod ingest;
pub mod journal;
pub mod lfs_guard;
pub mod publish;
pub mod summary;
pub mod versions;
pub mod window;

/// Returns true when an object key can be safely materialized as a
/// relative git worktree path by the import pipeline.
pub(crate) fn is_importable_relative_path(key: &str) -> bool {
    if key.is_empty() || key.starts_with('/') || key.ends_with('/') {
        return false;
    }
    if key
        .as_bytes()
        .iter()
        .any(|&b| matches!(b, 0x00 | b'\n' | b'\r' | b'\\'))
    {
        return false;
    }

    let mut components = key.split('/');
    let Some(first) = components.next() else {
        return false;
    };
    if is_collapsing_component(first) || is_reserved_import_component(first) {
        return false;
    }

    for component in components {
        if is_collapsing_component(component) || is_reserved_import_component(component) {
            return false;
        }
    }

    true
}

fn is_collapsing_component(component: &str) -> bool {
    component.is_empty() || matches!(component, "." | "..")
}

fn is_reserved_import_component(component: &str) -> bool {
    component.eq_ignore_ascii_case(".git")
        || component.eq_ignore_ascii_case(".crab")
        || component.eq_ignore_ascii_case(".gitattributes")
}

pub use crate::cmd::import::VersionsMode;
pub use assemble::{
    AssembleEvent, AssembleInputs, AssembleProgressSink, AssembleStats, run_assemble,
};
pub use coordinator::{ImportSummary, run_import_inner, run_import_with_stores};
pub use detect::{DetectArgs, SourceMode, detect_source_mode};
pub use enumerate::{EnumerateEvent, EnumerateStats, ProgressSink, enumerate as run_enumerate};
pub use ingest::{
    IngestInputs, IngestProgressSink, IngestStats, IngestStatsSnapshot, ResolvedStore, run_ingest,
};
pub use journal::{EntryState, ImportEntry, Journal, Plan, PlanInputs, SkipReason, SourceModeTag};
pub use lfs_guard::{LfsDetection, detect_lfs_source};
pub use publish::{PublishInputs, PublishStats, run_publish};
pub use summary::{
    ExtensionBucket, HistoryRange, ImportPlanSummary, SummaryVersioning, build_extension_histogram,
};
pub use versions::{
    AzureVersionedList, GcsVersionedList, LocalVersionedList, S3VersionedList, VersionRecord,
    VersionSample, VersionedList, VersionedListImpl,
};
pub use window::{
    CommitWindow, plan_commit_windows, plan_flat_single_commit, plan_snapshot,
    validate_history_range,
};

#[cfg(test)]
mod tests {
    use super::is_importable_relative_path;

    #[test]
    fn importable_relative_path_allows_plain_dotted_names() {
        assert!(is_importable_relative_path("data/model..v2.bin"));
        assert!(is_importable_relative_path("nested/path/file.tar.gz"));
    }

    #[test]
    fn importable_relative_path_rejects_collapsing_and_control_paths() {
        for path in [
            "",
            "/absolute.bin",
            "data/",
            "data//file.bin",
            "data/./file.bin",
            "data/../file.bin",
            "data\\file.bin",
            ".git/config",
            "nested/.git/config",
            ".crab/staging/index.db",
            "nested/.crab/file.bin",
            ".gitattributes",
            "nested/.gitattributes",
            "bad\npath.bin",
            "bad\rpath.bin",
            "bad\0path.bin",
        ] {
            assert!(!is_importable_relative_path(path), "{path:?}");
        }
    }
}
