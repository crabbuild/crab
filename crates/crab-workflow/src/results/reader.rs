use super::*;
use crate::Result;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSelection {
    pub id: String,
    pub digest: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResultsRead {
    List(RunQuery),
    Run(RunSelection),
    Compare {
        runs: Vec<RunSelection>,
        spec: ComparisonSpec,
    },
    Reports,
    Report {
        id: String,
    },
    Evidence {
        run: RunSelection,
        path: String,
        report: Option<String>,
    },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonView {
    pub comparison: Comparison,
    pub charts: Vec<Chart>,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportView {
    pub id: String,
    pub report: Report,
    pub charts: Vec<Chart>,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceView {
    pub attachment: Attachment,
    pub text: Option<String>,
    pub data_url: Option<String>,
    pub reason: Option<String>,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ResultsResponse {
    Page(RunPage),
    Run(Box<SealedResult>),
    Comparison(ComparisonView),
    Reports(Vec<String>),
    Report(ReportView),
    Evidence(EvidenceView),
}

/// Shared interpretation for local, direct-storage and HTTP repository consumers.
pub enum ResultsReader {
    Local(LocalResults),
    Remote(Box<RemoteResults>),
}
impl ResultsReader {
    pub async fn read(&self, request: ResultsRead) -> Result<ResultsResponse> {
        let response = self.read_inner(request).await?;
        if super::encode(&response)?.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid(
                "Results response exceeds 8 MiB; select fewer runs or columns",
            ));
        }
        Ok(response)
    }
    async fn read_inner(&self, request: ResultsRead) -> Result<ResultsResponse> {
        match request {
            ResultsRead::List(query) => {
                let runs = match self {
                    Self::Local(local) => local.list()?,
                    Self::Remote(remote) => remote.list().await?,
                };
                Ok(ResultsResponse::Page(query_results(&runs, &query)?))
            }
            ResultsRead::Run(selection) => {
                Ok(ResultsResponse::Run(Box::new(self.run(&selection).await?)))
            }
            ResultsRead::Compare { runs, spec } => {
                if runs.is_empty() || runs.len() > MAX_COMPARISON_RUNS {
                    return Err(invalid("choose 1–20 comparison runs"));
                }
                let mut selected = Vec::new();
                for selection in runs {
                    selected.push(self.run(&selection).await?);
                }
                let comparison = compare(&selected, &spec)?;
                let mut charts = self.charts(&selected).await?;
                super::interactions::apply_axes(&mut charts, &spec.axes)?;
                Ok(ResultsResponse::Comparison(ComparisonView {
                    comparison,
                    charts,
                }))
            }
            ResultsRead::Reports => Ok(ResultsResponse::Reports(match self {
                Self::Local(local) => local.reports()?,
                Self::Remote(remote) => remote.reports().await?,
            })),
            ResultsRead::Report { id } => {
                let report = self.report(&id).await?;
                let charts = report.charts.clone();
                Ok(ResultsResponse::Report(ReportView { id, report, charts }))
            }
            ResultsRead::Evidence { run, path, report } => {
                let report_only = report.is_some();
                let run = if let Some(id) = report {
                    let report = self.report(&id).await?;
                    let row = report
                        .comparison
                        .rows
                        .into_iter()
                        .find(|row| {
                            row.run.digest == run.digest && row.run.snapshot.experiment_id == run.id
                        })
                        .ok_or_else(|| invalid("run is not part of the frozen report"))?;
                    if !report.comparison.spec.include_logs
                        && row
                            .run
                            .snapshot
                            .attachments
                            .iter()
                            .any(|a| a.path == path && a.kind == AttachmentKind::Log)
                    {
                        return Err(invalid("logs were excluded from this report"));
                    }
                    row.run
                } else {
                    self.run(&run).await?
                };
                let attachment = run
                    .snapshot
                    .attachments
                    .iter()
                    .find(|a| a.path == path)
                    .ok_or_else(|| invalid("attachment is not part of the selected result"))?
                    .clone();
                let mut view = EvidenceView {
                    attachment,
                    text: None,
                    data_url: None,
                    reason: None,
                };
                if report_only && !view.attachment.report_content(true) {
                    view.reason = Some("This report retains an immutable link. Retrieve the run or artifact version for payload bytes.".into());
                } else if view.attachment.state != Availability::Present {
                    view.reason = view.attachment.reason.clone();
                } else if view.attachment.size > 1024 * 1024 {
                    view.reason = Some(
                        "Preview exceeds 1 MiB; retrieve the retained attachment with the CLI"
                            .into(),
                    );
                } else {
                    let hash = view
                        .attachment
                        .hash
                        .as_deref()
                        .ok_or_else(|| invalid("attachment lacks digest"))?;
                    let bytes = self.blob(hash).await?;
                    if bytes.len() as u64 != view.attachment.size {
                        return Err(invalid("attachment size mismatch"));
                    }
                    let mime = super::charts::image_mime(std::path::Path::new(&path));
                    if let Some(mime) = mime {
                        view.data_url = Some(format!(
                            "data:{mime};base64,{}",
                            base64::engine::general_purpose::STANDARD.encode(bytes)
                        ));
                    } else {
                        match String::from_utf8(bytes) {
                            Ok(text) => view.text = Some(text),
                            Err(_) => {
                                view.reason = Some(
                                    "Binary evidence; retrieve the attachment with the CLI".into(),
                                )
                            }
                        }
                    }
                }
                Ok(ResultsResponse::Evidence(view))
            }
        }
    }
    async fn run(&self, selection: &RunSelection) -> Result<SealedResult> {
        match self {
            Self::Local(local) => local.run_at(&selection.id, &selection.digest),
            Self::Remote(remote) => remote.run_at(&selection.id, &selection.digest).await,
        }
    }
    async fn report(&self, id: &str) -> Result<Report> {
        match self {
            Self::Local(local) => local.report(id),
            Self::Remote(remote) => remote.report(id).await,
        }
    }
    async fn blob(&self, hash: &str) -> Result<Vec<u8>> {
        match self {
            Self::Local(local) => local.blob(hash),
            Self::Remote(remote) => remote.blob(hash).await,
        }
    }
    async fn charts(&self, runs: &[SealedResult]) -> Result<Vec<Chart>> {
        let mut objects = BTreeMap::new();
        let mut total = 0;
        for run in runs {
            for attachment in &run.snapshot.attachments {
                if !matches!(
                    attachment.kind,
                    AttachmentKind::Plot | AttachmentKind::Template | AttachmentKind::Image
                ) {
                    continue;
                }
                // Image galleries resolve their verified previews independently;
                // metadata-only comparison must not download large binary images.
                if super::charts::image_mime(std::path::Path::new(&attachment.path)).is_some() {
                    continue;
                }
                if let Some(hash) = &attachment.hash
                    && !objects.contains_key(hash)
                {
                    total += attachment.size;
                    if total > MAX_CAPTURE_BYTES as u64 {
                        return Err(invalid("comparison chart evidence exceeds 256 MiB"));
                    }
                    let bytes = match self.blob(hash).await {
                        // Admission belongs to the whole operation. A partial
                        // chart cannot turn a rejected budget or cancellation
                        // into an acknowledged successful read.
                        Ok(bytes) => Ok(bytes),
                        Err(error) => {
                            // Workflow's transparent I/O variant would skip
                            // io::Error's boxed payload during source traversal.
                            let cause: &(dyn std::error::Error + 'static) = match &error {
                                crate::WorkflowError::Io(source) => source,
                                source => source,
                            };
                            if matches!(
                                &error,
                                crate::WorkflowError::Cancelled
                                    | crate::WorkflowError::StorageDomain(
                                        crab_storage::StorageError::Cancelled
                                    )
                            ) || crab_storage::read_rejection(cause).is_some()
                            {
                                return Err(error);
                            }
                            Err(std::sync::Arc::new(error))
                        }
                    };
                    // Cache failures as well as bytes: one unavailable plot must
                    // not discard unrelated metrics/curves. Keep its source error
                    // alive until the chart owner projects its availability.
                    objects.insert(hash.clone(), bytes);
                }
            }
        }
        charts(runs, |hash| match objects.get(hash) {
            Some(Ok(bytes)) => Ok(bytes.clone()),
            Some(Err(source)) => Err(source.clone()),
            None => Err(std::sync::Arc::new(invalid(
                "chart evidence was not retained",
            ))),
        })
    }
}
fn query_results(runs: &[SealedResult], query: &RunQuery) -> Result<RunPage> {
    super::query(runs, query)
}
