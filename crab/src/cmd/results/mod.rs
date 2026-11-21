//! Native result capture and the CLI's result/report composition.

use std::path::Path;

use crab_workflow::results::{LocalResults, ReproductionSpec, ResultCapture};
use crab_workflow::{Graph, discover};

use crate::core::config::Config;
use crate::core::error::Result;

pub(crate) fn begin_capture(
    repo_root: &Path,
    workdir: &Path,
    args: &crate::cmd::run::RunArgs,
    base_commit: &str,
    source_result: Option<crab_workflow::results::RunSelection>,
    materialized_inputs: &[PathBuf],
) -> Result<ResultCapture> {
    let config = Config::resolve_for_repo(workdir)?;
    let mode = crate::cmd::run::resolve_discover_mode(args, &config);
    let paths = discover::discover(workdir, mode)?;
    let mut workflow = discover::parse_all(workdir, &paths)?;
    let graph = Graph::build(&workflow.stages)?;
    if let Some(selected) = crate::cmd::run::filter_stages(args, &workflow, &graph)? {
        workflow.stages.retain(|name, _| selected.contains(name));
    }
    let spec = ReproductionSpec {
        source_commit: base_commit.into(),
        source_result,
        files: Vec::new(),
        directories: Vec::new(),
        runtimes: Vec::new(),
        targets: args.cmd.clone(),
        recursive: args.recursive,
        single_item: args.single_item,
        downstream: args.downstream,
        pipeline: args.pipeline,
        all_pipelines: args.all_pipelines,
        glob: args.glob,
        unavailable_reasons: Vec::new(),
    };
    ResultCapture::begin(
        LocalResults::new(repo_root)?,
        workdir,
        workflow,
        &paths,
        materialized_inputs,
        spec,
    )
    .map_err(Into::into)
}

use crate::core::output::{emit_json, emit_text};
use clap::{Parser, Subcommand};
use crab_workflow::results::{
    ComparisonSpec, RemoteResults, Report, RunQuery, charts, compare, query,
};
use std::path::PathBuf;

#[derive(Debug, Parser)]
pub struct ResultsArgs {
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: ResultsCommand,
}

#[derive(Debug, Subcommand)]
pub enum ResultsCommand {
    /// Explicitly migrate recorded v1 experiment metrics without inferring missing execution evidence.
    Migrate {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Search, sort and page captured experiment results.
    List {
        #[arg(long, default_value = "")]
        search: String,
        #[arg(long, default_value = "started_at")]
        sort: String,
        #[arg(long)]
        ascending: bool,
        #[arg(long)]
        column: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Inspect an immutable captured result.
    Show { id: String },
    /// Preview workspace paths or the checkpoint and current HEAD used by resume.
    Preview {
        id: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        resume: bool,
    },
    /// Compare captured metrics; an optional spec declares objectives and dataset identities.
    Compare {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        spec: Option<PathBuf>,
    },
    /// Export one retained attachment after verifying its captured identity.
    Attachment {
        id: String,
        path: String,
        #[arg(long)]
        digest: Option<String>,
        #[arg(long)]
        report: Option<String>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Promote an immutable artifact version whose bytes match this run's output.
    Promote {
        id: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        stage: String,
        #[arg(long)]
        expected: Option<String>,
    },
    /// Build retained curve overlays.
    Charts {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Publish captured evidence and a run discovery root.
    Push {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Retrieve and verify remote results, including all retained evidence.
    Pull {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Create, inspect, publish or retrieve immutable comparison reports.
    #[command(subcommand)]
    Report(ReportCommand),
}

#[derive(Debug, Subcommand)]
pub enum ReportCommand {
    List,
    Create {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        spec: Option<PathBuf>,
    },
    Show {
        id: String,
    },
    Push {
        id: String,
    },
    Pull {
        id: String,
    },
    Export {
        id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

/// Structured command payloads emitted by `crab results --json`.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum ResultsPayload {
    Migration {
        runs: Vec<crab_workflow::results::RunSelection>,
    },
    Page(crab_workflow::results::RunPage),
    Run(Box<crab_workflow::results::SealedResult>),
    Preview(crab_workflow::results::ActionPreview),
    Promotion(crate::cmd::artifacts::ArtifactPromotionPayload),
    Comparison(crab_workflow::results::Comparison),
    Charts(Vec<crab_workflow::results::Chart>),
    Report(Report),
    Reports(Vec<String>),
    SavedReport {
        id: String,
        report: Report,
    },
    ReportId {
        id: String,
    },
    Export {
        id: String,
        output: PathBuf,
    },
    Transfer {
        ids: Vec<String>,
    },
    Attachment {
        path: String,
        hash: String,
        bytes: usize,
        output: PathBuf,
    },
}

pub async fn exec(args: ResultsArgs) -> Result<()> {
    let root = std::env::current_dir()?;
    let value = run_in(&args, &root).await?;
    if args.json {
        emit_json("workflow.results", "1.0", &value)?;
    } else {
        let text = serde_json::to_string_pretty(&value)
            .map_err(crab_workflow::WorkflowError::ResultsJson)?;
        emit_text(&text);
    }
    Ok(())
}

pub async fn run_in(args: &ResultsArgs, root: &Path) -> Result<ResultsPayload> {
    use ResultsCommand::*;
    let local = LocalResults::new(root)?;
    let value = match &args.command {
        Migrate { ids, all } => ResultsPayload::Migration {
            runs: crate::cmd::exp::results_migration::migrate(root, ids, *all)?,
        },
        List {
            search,
            sort,
            ascending,
            column,
            limit,
            cursor,
        } => ResultsPayload::Page(query(
            &local.list()?,
            &RunQuery {
                search: search.clone(),
                sort: sort.clone(),
                descending: !ascending,
                columns: (!column.is_empty()).then(|| column.clone()),
                limit: *limit,
                cursor: cursor.clone(),
            },
        )?),
        Show { id } => ResultsPayload::Run(Box::new(local.run(id)?)),
        Preview { id, digest, resume } => ResultsPayload::Preview(
            crate::cmd::exp::preview_result_action(root, id, digest, *resume)?,
        ),
        Promote {
            id,
            digest,
            name,
            version,
            stage,
            expected,
        } => {
            let run = local.run_at(id, digest)?;
            let promotion = crate::cmd::artifacts::ArtifactPromoteArgs {
                name: name.clone(),
                version: version.clone(),
                stage: stage.clone(),
                expected: expected.clone(),
                json: true,
                jsonl: false,
            };
            let root = root.to_path_buf();
            let value = tokio::task::spawn_blocking(move || {
                crate::cmd::artifacts::promote_result(&root, &promotion, &run)
            })
            .await
            .map_err(|source| crate::core::error::CrabError::Io(std::io::Error::other(source)))??;
            ResultsPayload::Promotion(value)
        }
        Compare { ids, spec } => {
            let runs = ids
                .iter()
                .map(|id| local.run(id))
                .collect::<crab_workflow::Result<Vec<_>>>()?;
            let spec = comparison_spec(spec.as_deref(), &runs)?;
            ResultsPayload::Comparison(compare(&runs, &spec)?)
        }
        Attachment {
            id,
            path,
            digest,
            report,
            output,
        } => {
            let report_only = report.is_some();
            let run = if let Some(report_id) = report {
                let saved = local.report(report_id)?;
                let run = saved
                    .comparison
                    .rows
                    .iter()
                    .find(|row| row.run.snapshot.experiment_id == *id)
                    .ok_or_else(|| crate::core::error::CrabError::Configuration {
                        key: "results attachment".into(),
                        origin: "run is not part of the report".into(),
                    })?
                    .run
                    .clone();
                if !saved.comparison.spec.include_logs
                    && run.snapshot.attachments.iter().any(|a| {
                        a.path == *path && a.kind == crab_workflow::results::AttachmentKind::Log
                    })
                {
                    return Err(crate::core::error::CrabError::Configuration {
                        key: "results attachment".into(),
                        origin: "logs were excluded from this report".into(),
                    });
                }
                run
            } else if let Some(digest) = digest {
                local.run_at(id, digest)?
            } else {
                local.run(id)?
            };
            let attachment = run
                .snapshot
                .attachments
                .iter()
                .find(|a| a.path == *path)
                .ok_or_else(|| crate::core::error::CrabError::NotFound { path: path.clone() })?;
            if report_only && !attachment.report_content(true) {
                return Err(crate::core::error::CrabError::Configuration {
                    key: "results attachment".into(),
                    origin: "This report retains an immutable link. Retrieve the run or artifact version for payload bytes.".into(),
                });
            }
            let hash = attachment.hash.as_deref().ok_or_else(|| {
                crate::core::error::CrabError::Configuration {
                    key: "results attachment".into(),
                    origin: attachment
                        .reason
                        .clone()
                        .unwrap_or_else(|| "attachment was not retained".into()),
                }
            })?;
            let bytes = local.blob(hash)?;
            if bytes.len() as u64 != attachment.size {
                return Err(crate::core::error::CrabError::CorruptObject {
                    path: path.clone(),
                    reason: "attachment size mismatch".into(),
                });
            }
            std::fs::write(output, &bytes)?;
            ResultsPayload::Attachment {
                path: path.clone(),
                hash: hash.into(),
                bytes: bytes.len(),
                output: output.clone(),
            }
        }
        Charts { ids } => {
            let runs = ids
                .iter()
                .map(|id| local.run(id))
                .collect::<crab_workflow::Result<Vec<_>>>()?;
            ResultsPayload::Charts(charts(&runs, |hash| local.blob(hash))?)
        }
        Push { ids } | Pull { ids } => {
            let remote = remote(root).await?;
            for id in ids {
                if matches!(&args.command, Push { .. }) {
                    remote.push(&local, &local.run(id)?).await?;
                } else {
                    remote.pull(&local, id).await?;
                }
            }
            ResultsPayload::Transfer { ids: ids.clone() }
        }
        Report(command) => return report(command, root, &local).await,
    };
    Ok(value)
}

fn comparison_spec(
    path: Option<&Path>,
    runs: &[crab_workflow::results::SealedResult],
) -> Result<ComparisonSpec> {
    if let Some(path) = path {
        let bytes = std::fs::read(path)?;
        if bytes.len() > crab_workflow::results::MAX_DOCUMENT_BYTES {
            return Err(crate::core::error::CrabError::Configuration {
                key: "results spec".into(),
                origin: "spec exceeds size limit".into(),
            });
        }
        return serde_json::from_slice(&bytes)
            .map_err(crab_workflow::WorkflowError::ResultsJson)
            .map_err(Into::into);
    }
    Ok(ComparisonSpec {
        baseline: runs
            .first()
            .map(|run| run.snapshot.experiment_id.clone())
            .unwrap_or_default(),
        ..ComparisonSpec::default()
    })
}

async fn report(
    command: &ReportCommand,
    root: &Path,
    local: &LocalResults,
) -> Result<ResultsPayload> {
    let value = match command {
        ReportCommand::List => ResultsPayload::Reports(local.reports()?),
        ReportCommand::Create { ids, title, spec } => {
            let runs = ids
                .iter()
                .map(|id| local.run(id))
                .collect::<crab_workflow::Result<Vec<_>>>()?;
            let spec = comparison_spec(spec.as_deref(), &runs)?;
            let comparison = compare(&runs, &spec)?;
            let plots = charts(&runs, |hash| local.blob(hash))?;
            let report = Report::new(title.clone(), comparison, plots)?;
            let id = local.save_report(&report)?;
            ResultsPayload::SavedReport { id, report }
        }
        ReportCommand::Show { id } => ResultsPayload::Report(local.report(id)?),
        ReportCommand::Push { id } => {
            remote(root)
                .await?
                .push_report(local, &local.report(id)?)
                .await?;
            ResultsPayload::ReportId { id: id.clone() }
        }
        ReportCommand::Pull { id } => {
            ResultsPayload::Report(remote(root).await?.pull_report(local, id).await?)
        }
        ReportCommand::Export { id, output } => {
            let report = local.report(id)?;
            let html = crab_workflow::results::report_html(&report, |hash| local.blob(hash))?;
            std::fs::write(output, html)?;
            ResultsPayload::Export {
                id: id.clone(),
                output: output.clone(),
            }
        }
    };
    Ok(value)
}

async fn remote(root: &Path) -> Result<RemoteResults> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let config = Config::resolve_for_repo(root)?;
    let (store, prefix) =
        crate::cmd::workflow::build_remote_store_for(root, &config, None, &cancel).await?;
    Ok(RemoteResults::new(store, &prefix, cancel))
}
