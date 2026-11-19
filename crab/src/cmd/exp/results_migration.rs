use super::{EXP_META_PARENT_REL, ensure_parent_not_symlink, meta_file_path};
use crate::core::error::{CrabError, Result};
use crab_workflow::results::{
    LocalResults, MAX_CAPTURE_BYTES, MAX_CATALOG_RUNS, MAX_DOCUMENT_BYTES, ResultSnapshot,
    RunSelection,
};
use crab_workflow::{ExperimentId, ExperimentMetadata, WorkflowError};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

pub(crate) fn migrate(repo: &Path, requested: &[String], all: bool) -> Result<Vec<RunSelection>> {
    let directory = repo.join(EXP_META_PARENT_REL);
    ensure_parent_not_symlink(&directory, repo, "results migration")?;
    let mut ids = BTreeMap::new();
    for raw in requested {
        let id: ExperimentId = raw.parse()?;
        ids.insert(id.to_string(), id);
    }
    if all {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            let name = entry.file_name();
            let Some(raw) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".meta.json"))
            else {
                continue;
            };
            let id: ExperimentId = raw.parse()?;
            ids.insert(id.to_string(), id);
            if ids.len() > MAX_CATALOG_RUNS {
                return Err(invalid("migration exceeds 10000 experiments"));
            }
        }
    }
    if ids.len() > MAX_CATALOG_RUNS {
        return Err(invalid("migration exceeds 10000 experiments"));
    }
    let mut bytes_read = 0_usize;
    let mut runs = Vec::with_capacity(ids.len());
    for id in ids.into_values() {
        let path = meta_file_path(repo, &id);
        let attributes = fs::symlink_metadata(&path)?;
        if !attributes.is_file() || attributes.len() > MAX_DOCUMENT_BYTES as u64 {
            return Err(invalid(
                "migration requires regular metadata files within the 8 MiB document limit",
            ));
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(MAX_DOCUMENT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid("metadata grew beyond the migration read limit"));
        }
        let metadata = ExperimentMetadata::from_json(&bytes, &id)?;
        let run = ResultSnapshot::try_from(&metadata)?.seal()?;
        let retained_bytes = serde_json::to_vec(&run)
            .map_err(WorkflowError::ResultsJson)?
            .len();
        bytes_read = bytes_read
            .saturating_add(bytes.len())
            .saturating_add(retained_bytes);
        if bytes_read > MAX_CAPTURE_BYTES {
            return Err(invalid(
                "migration exceeds the 256 MiB batch limit; select fewer experiment IDs",
            ));
        }
        runs.push(run);
    }
    // Decode and validate every source before publishing roots. I/O interruptions
    // can be retried; create-only installation never replaces an existing capture.
    let local = LocalResults::new(repo)?;
    runs.iter()
        .map(|run| {
            Ok(RunSelection {
                id: run.snapshot.experiment_id.clone(),
                digest: local.install_if_absent(run)?,
            })
        })
        .collect()
}

fn invalid(reason: &str) -> CrabError {
    CrabError::Configuration {
        key: "results migration".into(),
        origin: reason.into(),
    }
}
