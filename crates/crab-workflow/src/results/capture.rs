use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{encode, flatten_scalars, invalid, *};
use crate::{CheckpointRecord, Cmd, Dep, EnvSpec, ExperimentMetadata, Lockfile, Result, Workflow};

mod templates;

/// Retains input evidence before execution and observations before worktree cleanup.
pub struct ResultCapture {
    local: LocalResults,
    workdir: PathBuf,
    template_root: PathBuf,
    workflow: Workflow,
    parameters: Vec<Datum>,
    reproduction: Option<ReproductionSpec>,
    attachments: BTreeMap<String, Attachment>,
    issues: Vec<String>,
    captured_bytes: usize,
}

impl ResultCapture {
    fn observations(local: LocalResults, workdir: &Path, mut workflow: Workflow) -> Result<Self> {
        workflow.plot_configs = super::plots::declared_plot_configs(&workflow);
        let workdir = fs::canonicalize(workdir)?;
        Ok(Self {
            local,
            template_root: workdir.clone(),
            workdir,
            workflow,
            parameters: Vec::new(),
            reproduction: None,
            attachments: BTreeMap::new(),
            issues: Vec::new(),
            captured_bytes: 0,
        })
    }

    /// Captures saved workspace observations and declared render assets without claiming execution.
    ///
    /// Templates resolve in the authoring repository; other observations use the owned workspace.
    pub fn saved(
        local: LocalResults,
        workdir: &Path,
        template_root: &Path,
        workflow: Workflow,
        workflow_files: &[PathBuf],
        metadata: &ExperimentMetadata,
    ) -> Result<SealedResult> {
        if metadata.status != "saved" {
            return Err(invalid(
                "manual observations require saved experiment metadata",
            ));
        }
        let mut capture = Self::observations(local, workdir, workflow)?;
        capture.template_root = fs::canonicalize(template_root)?;
        for file in workflow_files {
            let path = file.strip_prefix(workdir).unwrap_or(file);
            capture.capture_observation(path, AttachmentKind::Workflow)?;
        }
        for path in capture.parameter_files() {
            let attachment = capture.capture_observation(&path, AttachmentKind::Input)?;
            capture.record_parameters(&path, &attachment)?;
        }
        capture.finish(metadata, &Lockfile::default(), &[])
    }

    pub fn begin(
        local: LocalResults,
        workdir: &Path,
        workflow: Workflow,
        workflow_files: &[PathBuf],
        materialized_inputs: &[PathBuf],
        mut reproduction: ReproductionSpec,
    ) -> Result<Self> {
        let mut capture = Self::observations(local, workdir, workflow)?;
        // Ordinary selected producers regenerate their outputs. Capturing those
        // before execution would misclassify them as missing source bytes.
        let mut inputs = crate::graph::source_paths(&capture.workflow.stages)?;
        // Explicitly restored outputs are initial state even when selected
        // producers own them. Retain that state before execution overwrites it.
        inputs.extend(materialized_inputs.iter().cloned());
        for file in workflow_files {
            if reproduction.files.len() >= MAX_REPRODUCTION_FILES {
                reproduction
                    .unavailable_reasons
                    .push("Additional workflow files exceed the retained input limit".into());
                break;
            }
            let path = file.strip_prefix(workdir).unwrap_or(file);
            capture.capture_input(path, AttachmentKind::Workflow, &mut reproduction)?;
        }
        for stage in capture.workflow.stages.values() {
            for dep in &stage.deps {
                match dep {
                    Dep::Path(_) | Dep::StageOut { .. } => {}
                    _ => reproduction.unavailable_reasons.push(format!(
                        "Stage {} has external inputs whose executable bytes are not retained",
                        stage.name
                    )),
                }
            }
            let reproducible_env = match &stage.env {
                EnvSpec::Empty => true,
                EnvSpec::Allowlist(names) => names.is_empty(),
                EnvSpec::Inherit => false,
            };
            if !reproducible_env {
                reproduction.unavailable_reasons.push(format!(
                    "Stage {} inherits environment values that are not retained",
                    stage.name
                ));
            }
            match &stage.cmd {
                Cmd::Argv(argv) => {
                    if let Some(program) = argv.first() {
                        if !Path::new(program).is_absolute() {
                            reproduction.unavailable_reasons.push(format!(
                                "Stage {} resolves its runtime through PATH; exact reproduction requires an absolute executable", stage.name));
                        }
                        match runtime_witness(program) {
                            Ok(runtime) => reproduction.runtimes.push(runtime),
                            Err(error) => reproduction.unavailable_reasons.push(error.to_string()),
                        }
                    }
                }
                _ => reproduction.unavailable_reasons.push(format!(
                    "Stage {} uses a shell with undeclared runtime dependencies",
                    stage.name
                )),
            }
        }
        for path in capture.parameter_files() {
            if reproduction.files.len() >= MAX_REPRODUCTION_FILES {
                reproduction
                    .unavailable_reasons
                    .push("Additional parameter files exceed the retained input limit".into());
                break;
            }
            let attachment =
                capture.capture_input(&path, AttachmentKind::Input, &mut reproduction)?;
            capture.record_parameters(&path, &attachment)?;
        }
        for path in inputs {
            if retained_input(&reproduction, &path).is_some()
                || reproduction
                    .directories
                    .iter()
                    .any(|p| Path::new(p) == path)
            {
                continue;
            }
            if reproduction.files.len() >= MAX_REPRODUCTION_FILES {
                reproduction
                    .unavailable_reasons
                    .push("Additional inputs exceed the retained input limit".into());
                break;
            }
            capture.capture_input_tree(&path, &mut reproduction)?;
        }
        let local_settings = capture.workdir.join(".crab/local.toml");
        if local_settings.is_file() {
            let bytes = super::local::read_bounded(&local_settings, MAX_DOCUMENT_BYTES)?;
            let settings = scalar_document(&bytes, &local_settings)?;
            // Init creates a comment-only document with no execution policy.
            // Private values stay excluded; only a proven empty table is safe.
            if !settings.as_object().is_some_and(serde_json::Map::is_empty) {
                reproduction.unavailable_reasons.push(
                    "Local execution configuration is not retained; exact reproduction is unavailable"
                        .into(),
                );
            }
        }
        reproduction
            .runtimes
            .sort_by(|a, b| a.program.cmp(&b.program));
        reproduction.runtimes.dedup();
        reproduction.directories.sort();
        reproduction.directories.dedup();
        reproduction.unavailable_reasons.sort();
        reproduction.unavailable_reasons.dedup();
        capture.reproduction = Some(reproduction);
        Ok(capture)
    }

    pub fn finish(
        mut self,
        metadata: &ExperimentMetadata,
        lockfile: &Lockfile,
        checkpoints: &[CheckpointRecord],
    ) -> Result<SealedResult> {
        let mut metrics = Vec::new();
        let mut metric_paths = self
            .workflow
            .metrics
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut plot_paths = self.workflow.plots.iter().cloned().collect::<BTreeSet<_>>();
        let mut output_paths = BTreeSet::new();
        for stage in self.workflow.stages.values() {
            let wdir = stage.wdir.as_deref().unwrap_or_else(|| Path::new(""));
            metric_paths.extend(stage.metrics.iter().map(|p| wdir.join(p)));
            plot_paths.extend(stage.plots.iter().map(|p| wdir.join(p)));
            output_paths.extend(
                stage
                    .outs
                    .iter()
                    .filter(|out| !out.is_external_url())
                    .map(|out| wdir.join(&out.path)),
            );
        }
        for config in &self.workflow.plot_configs {
            plot_paths.insert(config.path.clone());
            if let Some(path) = &config.x_path {
                plot_paths.insert(path.clone());
            }
        }
        self.capture_templates()?;
        for path in metric_paths {
            let attachment = self.capture_observation(&path, AttachmentKind::Metric)?;
            if attachment.state == Availability::Present {
                let bytes = self.local.blob(
                    attachment
                        .hash
                        .as_deref()
                        .ok_or_else(|| invalid("metric lacks digest"))?,
                )?;
                match scalar_document(&bytes, &path)
                    .and_then(|v| flatten_scalars(&attachment.path, &v))
                {
                    Ok(fields) => metrics.extend(fields),
                    Err(error) => metrics.push(unavailable_field(
                        &attachment.path,
                        Availability::Invalid,
                        error.to_string(),
                    )),
                }
            } else {
                metrics.push(unavailable_field(
                    &attachment.path,
                    attachment.state,
                    attachment
                        .reason
                        .clone()
                        .unwrap_or_else(|| "Metric was not produced".into()),
                ));
            }
        }
        for path in plot_paths {
            self.capture_observation_tree(&path, AttachmentKind::Plot)?;
        }
        for path in output_paths {
            self.capture_observation_tree(&path, AttachmentKind::Output)?;
        }
        let logs = self.workdir.join(".crab/workflow/runs");
        if self.reproduction.is_some() && logs.is_dir() {
            self.capture_logs(&logs, &logs)?;
        }
        let mut lineages = BTreeMap::<String, crate::CheckpointLineage>::new();
        for record in checkpoints {
            if self.attachments.len() >= MAX_ATTACHMENTS {
                self.issues
                    .push("Additional checkpoints exceed the attachment limit".into());
                break;
            }
            lineages
                .entry(record.stage.clone())
                .or_default()
                .append(record.clone())?;
            let path = format!("checkpoints/{}/{}.json", record.stage, record.sequence);
            let bytes = encode(record)?;
            if self.captured_bytes.saturating_add(bytes.len()) > MAX_CAPTURE_BYTES {
                self.issues
                    .push("Additional checkpoints exceed the retained byte limit".into());
                break;
            }
            self.captured_bytes += bytes.len();
            let hash = self.local.put_blob(&bytes)?;
            self.attachments.insert(
                path.clone(),
                Attachment {
                    path,
                    kind: AttachmentKind::Checkpoint,
                    stage: Some(record.stage.clone()),
                    hash: Some(hash),
                    size: bytes.len() as u64,
                    state: Availability::Present,
                    reason: None,
                    push: true,
                    remote: None,
                },
            );
        }
        self.parameters.sort_by_key(Datum::key);
        self.parameters.dedup_by(|a, b| a.key() == b.key());
        if metrics.len() + self.parameters.len() > MAX_FIELDS {
            self.issues.push("Scalar field limit exceeded; retained metric and parameter files contain remaining values".into());
            metrics.truncate(MAX_FIELDS);
            self.parameters
                .truncate(MAX_FIELDS.saturating_sub(metrics.len()));
        }
        metrics.sort_by_key(Datum::key);
        metrics.dedup_by(|a, b| a.key() == b.key());
        // A manual save observes files, not a stage attempt. Old lockfiles and
        // journals in that workspace cannot establish how those files were made.
        let stages = if self.reproduction.is_some() {
            super::stages::capture_stages(&self.workflow, &self.workdir, lockfile)?
        } else {
            Vec::new()
        };
        let result = ResultSnapshot {
            schema_version: RESULTS_SCHEMA_VERSION,
            origin: ResultOrigin::Captured,
            experiment_id: metadata.exp_id.to_string(),
            source_metadata_hash: metadata.content_hash()?,
            base_commit: metadata.base_commit.clone(),
            name: metadata.name.clone(),
            status: metadata.status.clone(),
            started_at: metadata.started_at.clone(),
            ended_at: metadata.ended_at.clone(),
            parameters: self.parameters,
            metrics,
            stages,
            attachments: self.attachments.into_values().collect(),
            plots: self.workflow.plot_configs,
            reproduction: self.reproduction,
            capture_issues: self.issues,
        }
        .seal()?;
        self.local.install(&result)?;
        Ok(result)
    }

    fn parameter_files(&self) -> BTreeSet<PathBuf> {
        let mut files = self
            .workflow
            .params
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        for stage in self.workflow.stages.values() {
            let wdir = stage.wdir.as_deref().unwrap_or_else(|| Path::new(""));
            for param in &stage.params {
                files.insert(wdir.join(param.file().unwrap_or_else(|| Path::new("params.yaml"))));
            }
        }
        files
    }

    fn record_parameters(&mut self, path: &Path, attachment: &Attachment) -> Result<()> {
        if attachment.state != Availability::Present {
            return Ok(());
        }
        let bytes = self.local.blob(
            attachment
                .hash
                .as_deref()
                .ok_or_else(|| invalid("parameter capture lacks hash"))?,
        )?;
        match scalar_document(&bytes, path).and_then(|v| flatten_scalars(&attachment.path, &v)) {
            Ok(fields) => {
                let remaining = MAX_FIELDS.saturating_sub(self.parameters.len());
                if fields.len() > remaining {
                    self.issues.push("Parameter field limit exceeded; raw retained files contain the remaining values".into());
                }
                self.parameters.extend(fields.into_iter().take(remaining));
            }
            Err(error) => self.issues.push(error.to_string()),
        }
        Ok(())
    }

    fn capture_input(
        &mut self,
        path: &Path,
        kind: AttachmentKind,
        reproduction: &mut ReproductionSpec,
    ) -> Result<Attachment> {
        if let Some(attachment) = retained_input(reproduction, path) {
            return Ok(attachment.clone());
        }
        let attachment = self.capture_file(path, kind)?;
        if attachment.state != Availability::Present {
            reproduction.unavailable_reasons.push(format!(
                "Input {}: {}",
                attachment.path,
                attachment.reason.as_deref().unwrap_or("not captured")
            ));
        }
        reproduction.files.push(attachment.clone());
        Ok(attachment)
    }

    fn capture_observation(&mut self, path: &Path, kind: AttachmentKind) -> Result<Attachment> {
        let key = path.to_string_lossy().replace('\\', "/");
        if let Some(attachment) = self.attachments.get(&key) {
            return Ok(attachment.clone());
        }
        if self.attachments.len() >= MAX_ATTACHMENTS {
            return Ok(Attachment {
                path: relative_path(path).unwrap_or_else(|_| {
                    format!(
                        "external/{}",
                        super::digest(path.to_string_lossy().as_bytes())
                    )
                }),
                kind,
                stage: None,
                hash: None,
                size: 0,
                state: Availability::NotCaptured,
                reason: Some("Attachment limit exceeded".into()),
                push: false,
                remote: None,
            });
        }
        let attachment = self.capture_file(path, kind)?;
        self.attachments
            .insert(attachment.path.clone(), attachment.clone());
        Ok(attachment)
    }

    fn capture_file(&mut self, path: &Path, kind: AttachmentKind) -> Result<Attachment> {
        let relative = match relative_path(path) {
            Ok(path) => path,
            Err(_) => {
                let source = path.to_string_lossy();
                return Ok(Attachment {
                    path: format!("external/{}", super::digest(source.as_bytes())),
                    kind,
                    stage: None,
                    hash: None,
                    size: 0,
                    state: Availability::NotCaptured,
                    reason: Some(format!("{source}: outside retained repository path policy")),
                    push: false,
                    remote: None,
                });
            }
        };
        let (push, remote, stage) = self.output_policy(path);
        let source = self.workdir.join(&relative);
        let mut attachment = Attachment {
            path: relative,
            kind,
            stage,
            hash: None,
            size: 0,
            state: Availability::Missing,
            reason: None,
            push,
            remote,
        };
        let limit = if matches!(kind, AttachmentKind::Input | AttachmentKind::Workflow) {
            MAX_REPRODUCTION_BYTES
        } else {
            MAX_CAPTURE_BYTES
        };
        if self.captured_bytes >= limit {
            attachment.state = Availability::NotCaptured;
            attachment.reason = Some("Retained byte budget exceeded".into());
            return Ok(attachment);
        }
        match super::local::read_bounded(
            &source,
            MAX_ATTACHMENT_BYTES.min(limit - self.captured_bytes),
        ) {
            Ok(bytes) => {
                self.captured_bytes += bytes.len();
                attachment.size = bytes.len() as u64;
                attachment.hash = Some(self.local.put_blob(&bytes)?);
                attachment.state = Availability::Present;
            }
            Err(error) => {
                attachment.state = if source.exists() {
                    Availability::NotCaptured
                } else {
                    Availability::Missing
                };
                attachment.reason = Some(error.to_string());
            }
        }
        Ok(attachment)
    }

    fn output_policy(&self, path: &Path) -> (bool, Option<String>, Option<String>) {
        self.workflow
            .stages
            .values()
            .find_map(|stage| {
                let wdir = stage.wdir.as_deref().unwrap_or_else(|| Path::new(""));
                stage
                    .outs
                    .iter()
                    .find(|out| path.starts_with(wdir.join(&out.path)))
                    .map(|out| (out.push, out.remote.clone(), Some(stage.name.to_string())))
            })
            .unwrap_or((true, None, None))
    }

    fn capture_input_tree(
        &mut self,
        path: &Path,
        reproduction: &mut ReproductionSpec,
    ) -> Result<()> {
        if retained_input(reproduction, path).is_some()
            || reproduction
                .directories
                .iter()
                .any(|p| Path::new(p) == path)
        {
            return Ok(());
        }
        let full = self.workdir.join(path);
        if relative_path(path).is_err()
            || reproduction.files.len() >= MAX_REPRODUCTION_FILES
            || self.captured_bytes >= MAX_REPRODUCTION_BYTES
        {
            self.capture_input(path, AttachmentKind::Input, reproduction)?;
            return Ok(());
        }
        if fs::symlink_metadata(&full).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            if reproduction.directories.len() >= MAX_REPRODUCTION_FILES {
                reproduction.unavailable_reasons.push(format!(
                    "Input {}: retained directory limit exceeded",
                    path.display()
                ));
                return Ok(());
            }
            reproduction.directories.push(relative_path(path)?);
            let entries = children(&full);
            match entries {
                Ok(entries) if entries.len() <= MAX_ATTACHMENTS => {
                    for child in entries {
                        if reproduction.files.len() >= MAX_REPRODUCTION_FILES
                            || self.captured_bytes >= MAX_REPRODUCTION_BYTES
                        {
                            self.capture_input(path, AttachmentKind::Input, reproduction)?;
                            break;
                        }
                        let child = child
                            .strip_prefix(&self.workdir)
                            .map_err(|_| invalid("input escaped worktree"))?;
                        self.capture_input_tree(child, reproduction)?;
                    }
                }
                _ => {
                    self.capture_input(path, AttachmentKind::Input, reproduction)?;
                }
            }
        } else {
            self.capture_input(path, AttachmentKind::Input, reproduction)?;
        }
        Ok(())
    }

    fn capture_observation_tree(&mut self, path: &Path, kind: AttachmentKind) -> Result<()> {
        if self.attachments.len() >= MAX_ATTACHMENTS {
            if !self
                .issues
                .iter()
                .any(|issue| issue == "Attachment limit exceeded")
            {
                self.issues.push("Attachment limit exceeded".into());
            }
            return Ok(());
        }
        let full = self.workdir.join(path);
        if relative_path(path).is_ok()
            && fs::symlink_metadata(&full).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            match children(&full) {
                Ok(entries) if entries.len() <= MAX_ATTACHMENTS => {
                    for child in entries {
                        let child = child
                            .strip_prefix(&self.workdir)
                            .map_err(|_| invalid("output escaped worktree"))?;
                        self.capture_observation_tree(child, kind)?;
                    }
                }
                _ => {
                    self.capture_observation(path, kind)?;
                }
            }
        } else {
            self.capture_observation(path, kind)?;
        }
        Ok(())
    }

    fn capture_logs(&mut self, root: &Path, directory: &Path) -> Result<()> {
        for child in children(directory)? {
            if self.attachments.len() >= MAX_ATTACHMENTS {
                self.issues
                    .push("Additional logs exceed the attachment limit".into());
                break;
            }
            let metadata = fs::symlink_metadata(&child)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                self.capture_logs(root, &child)?;
            } else if child
                .extension()
                .is_some_and(|ext| ext == "log" || ext == "stdout")
            {
                let relative = child
                    .strip_prefix(root)
                    .map_err(|_| invalid("log escaped run root"))?;
                let path = format!("logs/{}", relative.to_string_lossy().replace('\\', "/"));
                match super::local::read_bounded(&child, MAX_ATTACHMENT_BYTES) {
                    Ok(bytes)
                        if self.captured_bytes.saturating_add(bytes.len()) <= MAX_CAPTURE_BYTES =>
                    {
                        self.captured_bytes += bytes.len();
                        let hash = self.local.put_blob(&bytes)?;
                        self.attachments.insert(
                            path.clone(),
                            Attachment {
                                path,
                                kind: AttachmentKind::Log,
                                stage: None,
                                hash: Some(hash),
                                size: bytes.len() as u64,
                                state: Availability::Present,
                                reason: None,
                                push: true,
                                remote: None,
                            },
                        );
                    }
                    Ok(_) => self
                        .issues
                        .push(format!("Log {path} exceeded the capture byte budget")),
                    Err(error) => self.issues.push(format!("Log {path}: {error}")),
                }
            }
        }
        Ok(())
    }
}

fn retained_input<'a>(reproduction: &'a ReproductionSpec, path: &Path) -> Option<&'a Attachment> {
    let relative = relative_path(path).ok()?;
    reproduction
        .files
        .iter()
        .find(|attachment| attachment.path == relative)
}

pub fn scalar_document(bytes: &[u8], path: &Path) -> Result<Value> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("json") => serde_json::from_slice(bytes).map_err(crate::WorkflowError::ResultsJson),
        Some("yaml" | "yml") => {
            let value: serde_yaml::Value = serde_yaml::from_slice(bytes).map_err(|source| {
                crate::WorkflowError::YamlParse {
                    path: path.into(),
                    source,
                }
            })?;
            serde_json::to_value(value).map_err(crate::WorkflowError::ResultsJson)
        }
        Some("toml") => {
            let text = std::str::from_utf8(bytes).map_err(crate::WorkflowError::ResultsUtf8)?;
            let value: toml::Value =
                toml::from_str(text).map_err(crate::WorkflowError::ResultsToml)?;
            serde_json::to_value(value).map_err(crate::WorkflowError::ResultsJson)
        }
        _ => serde_json::to_value(crate::params::parse(bytes, path)?)
            .map_err(crate::WorkflowError::ResultsJson),
    }
}

pub fn runtime_witness(program: &str) -> Result<RuntimeWitness> {
    use std::io::Read;
    let path = if Path::new(program).is_absolute() {
        PathBuf::from(program)
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|path| path.join(program))
            .find(|path| path.is_file())
            .ok_or_else(|| invalid(format!("Runtime {program} cannot be resolved")))?
    };
    let mut file = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(RuntimeWitness {
        program: program.into(),
        hash: hasher.finalize().to_hex().to_string(),
    })
}

pub(super) fn children(path: &Path) -> Result<Vec<PathBuf>> {
    crate::atomic::ensure_parent_not_symlink(&path.join("entry"))?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        paths.push(entry?.path());
        if paths.len() > MAX_ATTACHMENTS {
            break;
        }
    }
    paths.sort();
    Ok(paths)
}

fn relative_path(path: &Path) -> Result<String> {
    let parts = path
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    let relative = parts.join("/");
    super::model::validate_path(&relative)?;
    Ok(relative)
}

fn unavailable_field(file: &str, state: Availability, reason: String) -> Datum {
    Datum {
        file: file.into(),
        pointer: String::new(),
        state,
        value: Value::Null,
        reason: Some(reason),
    }
}
