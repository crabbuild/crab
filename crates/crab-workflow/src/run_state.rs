//! In-memory accumulator of completed stages for a single DAG run.
//!
//! Downstream stages whose dependencies reference a producer's output consult
//! this state first. Freshly-produced hashes from the current run are more
//! authoritative than anything on disk.

use std::collections::BTreeMap;

use crate::{StageCacheEntry, StageName};

/// Accumulates every stage that has committed in the current run.
#[derive(Debug, Default)]
pub struct RunState {
    entries: BTreeMap<StageName, StageCacheEntry>,
}

impl RunState {
    /// Start with an empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a stage's cache entry.
    ///
    /// A later call with the same name replaces the earlier one; retry loops use
    /// the same name across attempts, and the last successful commit is the one
    /// other stages must see.
    pub fn insert(&mut self, name: StageName, entry: StageCacheEntry) {
        self.entries.insert(name, entry);
    }

    /// Looks up a previously committed stage's cache entry.
    pub fn get(&self, name: &StageName) -> Option<&StageCacheEntry> {
        self.entries.get(name)
    }

    /// Number of committed stages.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no stages have committed yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CachedCmd, CachedOut, ENTRY_SCHEMA_VERSION, OutKind};
    use crab_types::workflow::StageHash;
    use std::path::PathBuf;

    fn sample_entry(stage: &str) -> StageCacheEntry {
        StageCacheEntry {
            schema_version: ENTRY_SCHEMA_VERSION,
            stage_hash: StageHash([0u8; 32]),
            stage_name: stage.to_owned(),
            cmd: CachedCmd::Shell {
                shell: "true".into(),
            },
            outs: vec![CachedOut {
                path: PathBuf::from("out.txt"),
                kind: OutKind::File,
                push: true,
                remote: None,
                file_hash: format!("b3:{}", "11".repeat(32)),
                size: 2,
                mode: 0o644,
                tree_manifest: None,
            }],
            metrics: Vec::new(),
            plots: Vec::new(),
            executed_at: "1970-01-01T00:00:00.000Z".into(),
            duration_ms: 0,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "test".into(),
        }
    }

    #[test]
    fn new_state_is_empty() {
        let s = RunState::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn insert_then_get_roundtrips() {
        let mut s = RunState::new();
        let name = StageName::parse("build").unwrap();
        s.insert(name.clone(), sample_entry("build"));
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(&name).unwrap().stage_name, "build");
    }

    #[test]
    fn insert_replaces_earlier_entry_for_same_name() {
        let mut s = RunState::new();
        let name = StageName::parse("build").unwrap();
        let mut first = sample_entry("build");
        first.attempts = 1;
        s.insert(name.clone(), first);

        let mut second = sample_entry("build");
        second.attempts = 2;
        s.insert(name.clone(), second);

        assert_eq!(s.get(&name).unwrap().attempts, 2);
        assert_eq!(s.len(), 1, "replacement must not grow the map");
    }

    #[test]
    fn get_misses_for_unknown_name() {
        let s = RunState::new();
        let name = StageName::parse("other").unwrap();
        assert!(s.get(&name).is_none());
    }
}
