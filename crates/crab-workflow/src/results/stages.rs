use std::collections::BTreeSet;
use std::path::Path;

use super::{InputWitness, MAX_ATTACHMENTS, MAX_FIELDS, OutputWitness, StageResult, invalid};
use crate::{Lockfile, Result, StageState, Workflow, journal::Journal};

pub(super) fn capture_stages(
    workflow: &Workflow,
    workdir: &Path,
    lockfile: &Lockfile,
) -> Result<Vec<StageResult>> {
    let selected: BTreeSet<_> = workflow.stages.keys().map(|name| name.as_str()).collect();
    let records = lockfile
        .stages
        .iter()
        .filter(|(name, _)| selected.contains(name.as_str()));
    let mut stages = Vec::new();
    let mut witnesses = 0usize;
    for (name, stage) in records {
        witnesses = witnesses.saturating_add(stage.outs.len());
        if stages.len() == MAX_ATTACHMENTS || witnesses > MAX_FIELDS {
            return Err(invalid("too many stage output witnesses"));
        }
        stages.push(StageResult {
            name: name.to_string(),
            hash: Some(stage.stage_hash.as_hex()),
            state: "lockfile_record".into(),
            duration_ms: Some(stage.duration_ms),
            attempts: stage.attempts,
            source: stage.source.clone(),
            // Lockfiles are output/cache records. Only the admitted journal
            // inputs can describe directories, produced data and external sources.
            inputs: Vec::new(),
            outputs: stage
                .outs
                .iter()
                .map(|out| OutputWitness {
                    path: out.path.to_string_lossy().into_owned(),
                    hash: blake3::Hash::from(out.hash).to_hex().to_string(),
                    size: out.size,
                })
                .collect(),
        });
    }
    let root = workdir.join(".crab/workflow/runs");
    if !root.is_dir() {
        return Ok(stages);
    }
    for directory in super::capture::children(&root)? {
        let Some(id) = directory
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        else {
            continue;
        };
        let path = directory.join("journal.db");
        if !path.is_file() {
            continue;
        }
        let journal = Journal::open(&path)?;
        let mut seen = BTreeSet::new();
        // Rows are ordered by stage and ascending attempt. A stage summary
        // uses its final attempt; earlier attempts remain available in logs.
        for row in journal
            .all_stage_rows_with_limit(id, MAX_ATTACHMENTS)?
            .into_iter()
            .rev()
        {
            if !selected.contains(row.stage_name.as_str()) || !seen.insert(row.stage_name.clone()) {
                continue;
            }
            let index = match stages.iter().position(|stage| stage.name == row.stage_name) {
                Some(index) => index,
                None => {
                    if stages.len() == MAX_ATTACHMENTS {
                        return Err(invalid("too many captured stages"));
                    }
                    stages.push(StageResult {
                        name: row.stage_name.clone(),
                        hash: None,
                        state: String::new(),
                        duration_ms: None,
                        attempts: row.attempt,
                        source: "Unknown".into(),
                        inputs: Vec::new(),
                        outputs: Vec::new(),
                    });
                    stages.len() - 1
                }
            };
            let stage = &mut stages[index];
            witnesses -= stage.inputs.len() + stage.outputs.len();
            let hash = row
                .stage_hash
                .map(|hash| blake3::Hash::from(hash).to_hex().to_string());
            // A failed attempt must not inherit old lockfile outputs. Partial
            // bytes are retained separately as attachments with explicit state.
            if row.state != StageState::Committed || stage.hash != hash {
                stage.outputs.clear();
            }
            let available = MAX_FIELDS.saturating_sub(witnesses + stage.outputs.len());
            stage.inputs = journal
                .admitted_inputs(id, &row.stage_name, row.attempt, available)?
                .into_iter()
                .map(|(path, hash)| InputWitness {
                    path,
                    hash: blake3::Hash::from(hash).to_hex().to_string(),
                })
                .collect();
            witnesses += stage.inputs.len() + stage.outputs.len();
            stage.hash = hash;
            stage.state = row.state.to_string();
            stage.attempts = row.attempt;
            let payload: serde_json::Value = serde_json::from_str(&row.payload_json)
                .map_err(crate::WorkflowError::ResultsJson)?;
            stage.source = payload["source"]
                .as_str()
                .unwrap_or(if row.state == StageState::Committed {
                    "Execution"
                } else {
                    "Unknown"
                })
                .into();
            stage.duration_ms = payload
                .get("duration_ms")
                .map(|value| {
                    value
                        .as_u64()
                        .ok_or_else(|| invalid("invalid observed stage duration"))
                })
                .transpose()?;
        }
    }
    stages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(stages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CachedCmd, LockedOut, LockedStage, OutKind, StageName};
    use crab_types::workflow::StageHash;

    fn fixture() -> (tempfile::TempDir, Workflow, Lockfile, Journal, uuid::Uuid) {
        let directory = tempfile::tempdir().unwrap();
        let workflow = crate::yaml::parse("stages:\n  train:\n    cmd: echo training\n").unwrap();
        let mut lockfile = Lockfile::new();
        lockfile.stages.insert(
            StageName::parse("train").unwrap(),
            LockedStage {
                stage_hash: StageHash([1; 32]),
                cmd: CachedCmd::Shell {
                    shell: "echo training".into(),
                },
                deps: Vec::new(),
                params: Default::default(),
                env: Default::default(),
                outs: vec![LockedOut {
                    path: "model.bin".into(),
                    kind: OutKind::File,
                    hash: [3; 32],
                    size: 17,
                    mode: 0o644,
                }],
                metrics: Vec::new(),
                plots: Vec::new(),
                executed_at: "2026-11-18T00:00:00Z".into(),
                duration_ms: 123,
                host_fingerprint: "test".into(),
                attempts: 1,
                source: "Execution".into(),
            },
        );
        let id = uuid::Uuid::now_v7();
        let journal = Journal::open(
            &directory
                .path()
                .join(".crab/workflow/runs")
                .join(id.to_string())
                .join("journal.db"),
        )
        .unwrap();
        journal.insert_run_start(id, "test", "test").unwrap();
        journal.insert_stage_start(id, "train").unwrap();
        admit(&journal, id, 1, [1; 32]);
        (directory, workflow, lockfile, journal, id)
    }

    fn admit(journal: &Journal, id: uuid::Uuid, attempt: u32, hash: [u8; 32]) {
        for state in [StageState::Resolved, StageState::CacheChecked] {
            journal
                .transition(id, "train", attempt, state, "{}")
                .unwrap();
        }
        journal
            .record_admission(
                id,
                "train",
                attempt,
                &hash,
                [(Some("dataset".into()), hash)].into_iter(),
            )
            .unwrap();
    }

    #[test]
    fn cache_summary_keeps_admitted_identity_without_inheriting_execution_time() {
        let (directory, workflow, lockfile, journal, id) = fixture();
        for state in [
            StageState::Produced,
            StageState::Hashed,
            StageState::Staged,
            StageState::EntryWritten,
            StageState::RefPublished,
            StageState::LockfileUpdated,
            StageState::Committed,
        ] {
            journal
                .transition(id, "train", 1, state, r#"{"source":"Cache"}"#)
                .unwrap();
        }
        let captured = capture_stages(&workflow, directory.path(), &lockfile).unwrap();
        assert_eq!(
            captured,
            vec![StageResult {
                name: "train".into(),
                hash: Some(blake3::Hash::from([1; 32]).to_hex().to_string()),
                state: "Committed".into(),
                duration_ms: None,
                attempts: 1,
                source: "Cache".into(),
                inputs: vec![InputWitness {
                    path: Some("dataset".into()),
                    hash: blake3::Hash::from([1; 32]).to_hex().to_string(),
                }],
                outputs: vec![OutputWitness {
                    path: "model.bin".into(),
                    hash: blake3::Hash::from([3; 32]).to_hex().to_string(),
                    size: 17,
                }],
            }]
        );
    }

    #[test]
    fn final_failed_attempt_does_not_inherit_success_outputs_or_duration() {
        let (directory, workflow, lockfile, journal, id) = fixture();
        journal
            .transition(id, "train", 1, StageState::Failed, "{}")
            .unwrap();
        journal.insert_stage_retry(id, "train", 2).unwrap();
        admit(&journal, id, 2, [2; 32]);
        journal
            .transition(id, "train", 2, StageState::Failed, "{}")
            .unwrap();
        let captured = capture_stages(&workflow, directory.path(), &lockfile).unwrap();
        let stage = &captured[0];
        assert_eq!(
            (
                stage.attempts,
                stage.state.as_str(),
                stage.duration_ms,
                stage.outputs.len(),
                stage.inputs[0].hash.as_str()
            ),
            (
                2,
                "Failed",
                None,
                0,
                blake3::Hash::from([2; 32]).to_hex().as_str()
            )
        );
    }
}
