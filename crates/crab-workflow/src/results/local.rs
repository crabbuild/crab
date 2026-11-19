use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{check_digest, decode, digest, encode, invalid, *};
use crate::{ExperimentId, Result, WorkflowError};

#[derive(Debug, Clone)]
pub struct LocalResults {
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunRoot {
    pub schema_version: u16,
    pub experiment_id: String,
    pub digest: String,
}

impl RunRoot {
    pub(crate) fn verify(&self, id: &str) -> Result<()> {
        id.parse::<ExperimentId>()?;
        check_digest(&self.digest)?;
        if self.schema_version != RESULTS_SCHEMA_VERSION || self.experiment_id != id {
            return Err(invalid("result root identity or schema mismatch"));
        }
        Ok(())
    }
}

impl LocalResults {
    pub fn new(repo_root: &Path) -> Result<Self> {
        let root = fs::canonicalize(repo_root)
            .map_err(WorkflowError::Io)?
            .join(".crab/workflow/results");
        crate::atomic::ensure_parent_not_symlink(&root.join("objects"))?;
        Ok(Self { root })
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(invalid("attachment exceeds capture limit"));
        }
        let hash = digest(bytes);
        self.write_immutable(&self.root.join("objects").join(&hash), bytes)?;
        Ok(hash)
    }

    pub fn blob(&self, hash: &str) -> Result<Vec<u8>> {
        check_digest(hash)?;
        let bytes = read_bounded(&self.root.join("objects").join(hash), MAX_ATTACHMENT_BYTES)?;
        if digest(&bytes) != hash {
            return Err(stored(hash, invalid("attachment digest mismatch")));
        }
        Ok(bytes)
    }

    pub fn install(&self, run: &SealedResult) -> Result<()> {
        let root = self.store_snapshot(run)?;
        write_atomic(
            &self
                .root
                .join("refs")
                .join(format!("{}.json", root.experiment_id)),
            &encode(&root)?,
        )
    }

    /// Installs an absent run root and returns its selected digest without replacing existing results.
    pub fn install_if_absent(&self, run: &SealedResult) -> Result<String> {
        run.verify()?;
        let id = &run.snapshot.experiment_id;
        let path = self.root.join("refs").join(format!("{id}.json"));
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(self.run(id)?.digest),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let root = self.store_snapshot(run)?;
        // Migration cannot downgrade a capture installed concurrently. The
        // immutable writer's create-only root either wins or reports a conflict.
        self.write_immutable(&path, &encode(&root)?)?;
        Ok(root.digest)
    }

    fn store_snapshot(&self, run: &SealedResult) -> Result<RunRoot> {
        run.verify()?;
        self.verify_evidence(run.snapshot.evidence())?;
        let id = &run.snapshot.experiment_id;
        let path = self
            .root
            .join("runs")
            .join(id)
            .join(format!("{}.json", run.digest));
        self.write_immutable(&path, &encode(&run.snapshot)?)?;
        Ok(RunRoot {
            schema_version: RESULTS_SCHEMA_VERSION,
            experiment_id: id.clone(),
            digest: run.digest.clone(),
        })
    }

    pub fn run(&self, id: &str) -> Result<SealedResult> {
        id.parse::<ExperimentId>()?;
        let bytes = read_bounded(&self.root.join("refs").join(format!("{id}.json")), 1024)?;
        let root: RunRoot = decode(&bytes)?;
        root.verify(id).map_err(|error| stored(id, error))?;
        self.run_at(id, &root.digest)
    }

    pub fn run_at(&self, id: &str, hash: &str) -> Result<SealedResult> {
        id.parse::<ExperimentId>()?;
        check_digest(hash)?;
        let bytes = read_bounded(
            &self.root.join("runs").join(id).join(format!("{hash}.json")),
            MAX_DOCUMENT_BYTES,
        )?;
        let run = SealedResult {
            digest: hash.into(),
            snapshot: decode(&bytes).map_err(|error| stored(hash, error))?,
        };
        run.verify().map_err(|error| stored(hash, error))?;
        if run.snapshot.experiment_id != id {
            return Err(stored(hash, invalid("requested result ID mismatch")));
        }
        Ok(run)
    }

    pub fn list(&self) -> Result<Vec<SealedResult>> {
        let mut runs = Vec::new();
        let mut total = 0_usize;
        for path in entries(&self.root.join("refs"))? {
            let id = path
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("result root has invalid filename"))?;
            let run = self.run(id)?;
            total = total.saturating_add(encode(&run.snapshot)?.len());
            if total > MAX_CAPTURE_BYTES {
                return Err(invalid("Catalog scan exceeds 256 MiB"));
            }
            runs.push(run);
        }
        Ok(runs)
    }

    pub fn save_report(&self, report: &Report) -> Result<String> {
        let id = report.id()?;
        self.verify_evidence(report_evidence(report))?;
        self.write_immutable(
            &self.root.join("reports").join(format!("{id}.json")),
            &encode(report)?,
        )?;
        Ok(id)
    }

    pub fn report(&self, id: &str) -> Result<Report> {
        check_digest(id)?;
        let bytes = read_bounded(
            &self.root.join("reports").join(format!("{id}.json")),
            MAX_DOCUMENT_BYTES,
        )?;
        let report: Report = decode(&bytes).map_err(|error| stored(id, error))?;
        if report.id().map_err(|error| stored(id, error))? != id {
            return Err(stored(id, invalid("report digest mismatch")));
        }
        Ok(report)
    }

    pub fn reports(&self) -> Result<Vec<String>> {
        entries(&self.root.join("reports"))?
            .into_iter()
            .map(|path| {
                let id = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| invalid("invalid report filename"))?;
                check_digest(id)?;
                Ok(id.to_owned())
            })
            .collect()
    }

    pub fn remove_run(&self, id: &str) -> Result<()> {
        id.parse::<ExperimentId>()?;
        let path = self.root.join("refs").join(format!("{id}.json"));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
        // Immutable bodies are independent report evidence. Removing a run's
        // discovery root must never recursively remove the shared object family.
    }

    pub(crate) fn verify_evidence<'a>(
        &self,
        attachments: impl Iterator<Item = &'a Attachment>,
    ) -> Result<()> {
        let mut verified = BTreeSet::new();
        for attachment in attachments.filter(|a| a.state == Availability::Present) {
            let hash = attachment
                .hash
                .as_deref()
                .ok_or_else(|| invalid("present attachment lacks digest"))?;
            if verified.insert(hash) {
                let bytes = self.blob(hash)?;
                if bytes.len() as u64 != attachment.size {
                    return Err(invalid("attachment size mismatch"));
                }
            }
        }
        Ok(())
    }

    fn write_immutable(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        match read_bounded(path, MAX_ATTACHMENT_BYTES) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => {
                return Err(invalid(
                    "immutable result object already has different bytes",
                ));
            }
            Err(WorkflowError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        crate::atomic::ensure_parent_not_symlink(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("object has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(path) {
            Ok(_) => sync_parent(parent),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_bounded(path, MAX_ATTACHMENT_BYTES)? != bytes {
                    return Err(invalid("concurrent object identity conflict"));
                }
                Ok(())
            }
            Err(error) => Err(WorkflowError::Io(error.error)),
        }
    }
}

pub(crate) fn report_evidence(report: &Report) -> impl Iterator<Item = &Attachment> {
    report
        .comparison
        .rows
        .iter()
        .flat_map(|row| &row.run.snapshot.attachments)
        .filter(|a| a.report_content(report.comparison.spec.include_logs))
}

fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    crate::atomic::ensure_parent_not_symlink(&path.join("entry"))?;
    let directory = match fs::read_dir(path) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in directory {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            if !entry.file_type()?.is_file() {
                return Err(invalid("result entry must be a regular file"));
            }
            paths.push(entry.path());
            if paths.len() > MAX_CATALOG_RUNS {
                return Err(invalid(
                    "catalog exceeds 10000 roots; narrow repository retention before listing",
                ));
            }
        }
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    crate::atomic::ensure_parent_not_symlink(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("result file must be regular without symlinks"));
    }
    if metadata.len() > limit as u64 {
        return Err(invalid(format!(
            "result file exceeds the read limit of {limit} bytes"
        )));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid("result file grew beyond the read limit"));
    }
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::atomic::ensure_parent_not_symlink(path)?;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || !m.is_file()) {
        return Err(invalid("result root is not a regular file"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("result root has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    crate::atomic::replace_file(temporary.path(), path)?;
    sync_parent(parent)
}

fn sync_parent(parent: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}
