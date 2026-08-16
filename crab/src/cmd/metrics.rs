//! `crab metrics show` / `crab metrics diff` — structured
//! read and diff of metrics files across git refs.
//!
//! Mirror of [`crate::cmd::params`] with one difference: numeric
//! diffs render absolute + percent deltas. The underlying parser,
//! differ, and envelope types are shared; only the renderer
//! invocation differs in `metrics_mode: true`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use clap::{Parser, ValueEnum};
use serde::Serialize;

use crate::cmd::params::{ChangedEntry, Format, ScalarJson};
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::params::{self, Scalar, ScalarMap};
use crate::workflow::stage::PlotConfig;
use crab_workflow::{Workflow, yaml};

const WORKSPACE_REF: &str = "workspace";
const SCHEMA_SHOW: &str = "metrics.show";
const SCHEMA_DIFF: &str = "metrics.diff";
pub const SCHEMA_PLOT_TEMPLATES: &str = "metrics.plot.templates";
const SCHEMA_VERSION: &str = "1.0";

/// `crab metrics show [targets ...]`.
#[derive(Debug, Clone, Parser)]
pub struct ShowArgs {
    /// Metrics file or directory targets. Defaults to workflow-declared metrics.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<PathBuf>,

    #[arg(long = "ref", value_name = "REF", default_value = WORKSPACE_REF)]
    pub git_ref: String,

    /// Compatibility spelling for older Crab scripts. Prefer positional targets.
    #[arg(long = "paths", value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,

    #[arg(long, value_name = "FMT", default_value = "table")]
    pub format: Format,

    /// DVC-compatible alias for `--format md`.
    #[arg(long = "md", default_value_t = false)]
    pub md: bool,

    /// Show metrics from all Git branches plus the workspace.
    #[arg(long = "all-branches", short = 'a', default_value_t = false)]
    pub all_branches: bool,

    /// Show metrics from all Git tags plus the workspace.
    #[arg(long = "all-tags", short = 'T', default_value_t = false)]
    pub all_tags: bool,

    /// Show metrics from every reachable Git commit plus the workspace.
    #[arg(long = "all-commits", short = 'A', default_value_t = false)]
    pub all_commits: bool,

    /// Recursively discover metric files under directory targets.
    #[arg(long = "recursive", short = 'R', default_value_t = false)]
    pub recursive: bool,

    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Whether higher metric values are "better". Drives the
    /// `✓` / `✗` marker selection in `--format=pr-comment`.
    /// Default is `true` (accuracy-style metrics).
    #[arg(long, default_value_t = true)]
    pub higher_is_better: bool,
}

impl ShowArgs {
    fn target_paths(&self) -> Result<Vec<PathBuf>> {
        if !self.targets.is_empty() && !self.paths.is_empty() {
            return Err(CrabError::Configuration {
                key: "metrics show targets".to_owned(),
                origin: "use positional targets or --paths, not both".to_owned(),
            });
        }
        if self.targets.is_empty() {
            Ok(self.paths.clone())
        } else {
            Ok(self.targets.clone())
        }
    }

    fn uses_history(&self) -> bool {
        self.all_branches || self.all_tags || self.all_commits
    }

    fn effective_format(&self) -> Result<Format> {
        if self.md {
            if self.format != Format::Table {
                return Err(CrabError::Configuration {
                    key: "metrics show --md".to_owned(),
                    origin: "--md cannot be combined with --format".to_owned(),
                });
            }
            return Ok(Format::Md);
        }
        Ok(self.format)
    }
}

/// `crab metrics diff [--targets PATH ...] [revisions ...]`.
#[derive(Debug, Clone, Parser)]
pub struct DiffArgs {
    /// Revisions to compare. None means HEAD vs workspace; one means ref vs workspace.
    #[arg(value_name = "REV")]
    pub revisions: Vec<String>,

    /// Metric file paths. If omitted, uses metrics declared in crab.yaml,
    /// then falls back to metrics.json.
    #[arg(long = "paths", alias = "targets", value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,

    #[arg(long, value_name = "FMT", default_value = "table")]
    pub format: Format,

    /// DVC-compatible alias for `--format md`.
    #[arg(long = "md", default_value_t = false)]
    pub md: bool,

    /// Include unchanged metrics in rendered output and JSON.
    #[arg(long = "all", default_value_t = false)]
    pub all: bool,

    /// Recursively discover metric files under directory targets.
    #[arg(long = "recursive", short = 'R', default_value_t = false)]
    pub recursive: bool,

    /// Hide the path column in human-readable output.
    #[arg(long = "no-path", default_value_t = false)]
    pub no_path: bool,

    /// Decimal precision for numeric metrics and deltas.
    #[arg(long, value_name = "N", default_value_t = 5)]
    pub precision: usize,

    #[arg(long, default_value_t = false)]
    pub json: bool,

    #[arg(long, default_value_t = true)]
    pub higher_is_better: bool,
}

impl DiffArgs {
    fn comparison_refs(&self) -> Result<(String, String)> {
        match self.revisions.as_slice() {
            [] => Ok(("HEAD".to_owned(), WORKSPACE_REF.to_owned())),
            [baseline] => Ok((baseline.clone(), WORKSPACE_REF.to_owned())),
            [baseline, target] => Ok((baseline.clone(), target.clone())),
            _ => Err(CrabError::Configuration {
                key: "metrics diff revisions".to_owned(),
                origin: "metrics diff accepts at most two revisions".to_owned(),
            }),
        }
    }

    fn effective_format(&self) -> Result<Format> {
        if self.md {
            if self.format != Format::Table {
                return Err(CrabError::Configuration {
                    key: "metrics diff --md".to_owned(),
                    origin: "--md cannot be combined with --format".to_owned(),
                });
            }
            return Ok(Format::Md);
        }
        Ok(self.format)
    }
}

pub fn exec_show(args: ShowArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_show_in(&args, &cwd)
}

pub fn exec_diff(args: DiffArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_diff_in(&args, &cwd)
}

/// `crab metrics plot [--format table|vega|html] [--output FILE] [targets ...]`.
#[derive(Debug, Clone, Parser)]
pub struct PlotArgs {
    /// Plot target files. If omitted, uses plots declared in crab.yaml.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<PathBuf>,

    /// Write output to a file instead of stdout.
    #[arg(long, short = 'o', alias = "out", value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Baseline git ref for plot comparison. Defaults target to HEAD.
    #[arg(long, value_name = "REF")]
    pub baseline: Option<String>,

    /// Target git ref for plot comparison.
    #[arg(long, value_name = "REF")]
    pub target: Option<String>,

    /// Plot output format.
    #[arg(long, value_enum, default_value_t = PlotFormat::Table)]
    pub format: PlotFormat,

    /// DVC-compatible alias for `--format vega`.
    #[arg(long = "show-vega", default_value_t = false)]
    pub show_vega: bool,

    /// Field to use for the X axis.
    #[arg(long, short = 'x', value_name = "FIELD")]
    pub x: Option<String>,

    /// Field to plot on the Y axis. Repeat to plot multiple fields.
    #[arg(long, short = 'y', value_name = "FIELD")]
    pub y: Vec<String>,

    /// Treat CSV/TSV targets as headerless and address columns by zero-based index.
    #[arg(long = "no-header", default_value_t = false)]
    pub no_header: bool,

    /// Plot title override.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// X-axis label override.
    #[arg(long = "x-label", value_name = "TEXT")]
    pub x_label: Option<String>,

    /// Y-axis label override.
    #[arg(long = "y-label", value_name = "TEXT")]
    pub y_label: Option<String>,

    /// Plot template name. Built-in names affect the Vega mark type.
    #[arg(long, short = 't', value_name = "NAME")]
    pub template: Option<String>,

    /// Custom HTML wrapper template containing a `{plot_divs}` marker.
    #[arg(long = "html-template", value_name = "PATH")]
    pub html_template: Option<PathBuf>,

    /// Open the generated HTML plot in the default browser.
    #[arg(long, default_value_t = false)]
    pub open: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl PlotArgs {
    fn effective_format(&self) -> Result<PlotFormat> {
        if self.show_vega {
            if self.open || self.html_template.is_some() {
                return Err(CrabError::Configuration {
                    key: "plots --show-vega".to_owned(),
                    origin: "--show-vega cannot be combined with HTML output options".to_owned(),
                });
            }
            if self.format != PlotFormat::Table {
                return Err(CrabError::Configuration {
                    key: "plots --show-vega".to_owned(),
                    origin: "--show-vega cannot be combined with --format".to_owned(),
                });
            }
            return Ok(PlotFormat::Vega);
        }
        if self.open || self.html_template.is_some() {
            if self.format == PlotFormat::Vega {
                return Err(CrabError::Configuration {
                    key: "plots html output".to_owned(),
                    origin: "--open and --html-template require HTML output".to_owned(),
                });
            }
            return Ok(PlotFormat::Html);
        }
        Ok(self.format)
    }
}

/// `crab plots diff [--targets PATH ...] [revisions ...]`.
#[derive(Debug, Clone, Parser)]
pub struct PlotDiffArgs {
    /// Plot target files. If omitted, uses plots declared in crab.yaml.
    #[arg(long = "targets", value_name = "PATH", num_args = 1..)]
    pub targets: Vec<PathBuf>,

    /// Revisions to compare. None means HEAD vs workspace; one means ref vs workspace.
    #[arg(value_name = "REV")]
    pub revisions: Vec<String>,

    /// Write output to a file instead of stdout.
    #[arg(long, short = 'o', alias = "out", value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Baseline git ref for plot comparison. Defaults target to workspace.
    #[arg(long, value_name = "REF")]
    pub baseline: Option<String>,

    /// Target git ref for plot comparison.
    #[arg(long, value_name = "REF")]
    pub target: Option<String>,

    /// Plot output format.
    #[arg(long, value_enum, default_value_t = PlotFormat::Table)]
    pub format: PlotFormat,

    /// DVC-compatible alias for `--format vega`.
    #[arg(long = "show-vega", default_value_t = false)]
    pub show_vega: bool,

    /// Field to use for the X axis.
    #[arg(long, short = 'x', value_name = "FIELD")]
    pub x: Option<String>,

    /// Field to plot on the Y axis. Repeat to plot multiple fields.
    #[arg(long, short = 'y', value_name = "FIELD")]
    pub y: Vec<String>,

    /// Treat CSV/TSV targets as headerless and address columns by zero-based index.
    #[arg(long = "no-header", default_value_t = false)]
    pub no_header: bool,

    /// Plot title override.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    /// X-axis label override.
    #[arg(long = "x-label", value_name = "TEXT")]
    pub x_label: Option<String>,

    /// Y-axis label override.
    #[arg(long = "y-label", value_name = "TEXT")]
    pub y_label: Option<String>,

    /// Plot template name. Built-in names affect the Vega mark type.
    #[arg(long, short = 't', value_name = "NAME")]
    pub template: Option<String>,

    /// Custom HTML wrapper template containing a `{plot_divs}` marker.
    #[arg(long = "html-template", value_name = "PATH")]
    pub html_template: Option<PathBuf>,

    /// Open the generated HTML plot in the default browser.
    #[arg(long, default_value_t = false)]
    pub open: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl PlotDiffArgs {
    fn comparison_revisions(&self) -> Result<Vec<String>> {
        if (!self.revisions.is_empty()) && (self.baseline.is_some() || self.target.is_some()) {
            return Err(CrabError::Configuration {
                key: "plots diff revisions".to_owned(),
                origin: "positional revisions cannot be combined with --baseline/--target"
                    .to_owned(),
            });
        }

        if self.baseline.is_some() || self.target.is_some() {
            return Ok(vec![
                self.baseline.clone().unwrap_or_else(|| "HEAD".to_owned()),
                self.target
                    .clone()
                    .unwrap_or_else(|| WORKSPACE_REF.to_owned()),
            ]);
        }

        Ok(match self.revisions.as_slice() {
            [] => vec!["HEAD".to_owned(), WORKSPACE_REF.to_owned()],
            [baseline] => vec![baseline.clone(), WORKSPACE_REF.to_owned()],
            revisions => revisions.to_vec(),
        })
    }

    fn plot_args(&self) -> PlotArgs {
        PlotArgs {
            targets: self.targets.clone(),
            output: self.output.clone(),
            baseline: None,
            target: None,
            format: self.format,
            show_vega: self.show_vega,
            x: self.x.clone(),
            y: self.y.clone(),
            no_header: self.no_header,
            title: self.title.clone(),
            x_label: self.x_label.clone(),
            y_label: self.y_label.clone(),
            template: self.template.clone(),
            html_template: self.html_template.clone(),
            open: self.open,
            json: self.json,
        }
    }
}

/// `crab plots templates [template]`.
#[derive(Debug, Clone, Parser)]
pub struct PlotTemplatesArgs {
    /// Built-in or local template name to print as Vega-Lite JSON.
    pub template: Option<String>,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PlotFormat {
    /// Human-readable table preview.
    Table,
    /// Vega-Lite JSON specification.
    Vega,
    /// Self-contained HTML document embedding Vega-Lite specs.
    Html,
}

impl std::fmt::Display for PlotFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table => f.write_str("table"),
            Self::Vega => f.write_str("vega"),
            Self::Html => f.write_str("html"),
        }
    }
}

pub fn exec_plot(args: PlotArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_plot_in(&args, &cwd)
}

pub fn exec_plot_show(args: PlotArgs) -> Result<()> {
    exec_plot(args)
}

pub fn exec_plot_diff(args: PlotDiffArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_plot_diff_in(&args, &cwd)
}

pub fn exec_plot_templates(args: PlotTemplatesArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_plot_templates_in(&args, &cwd)
}

pub fn run_plot_in(args: &PlotArgs, repo_root: &Path) -> Result<()> {
    run_plot_in_with_opener(args, repo_root, open_plot_file)
}

pub fn run_plot_diff_in(args: &PlotDiffArgs, repo_root: &Path) -> Result<()> {
    run_plot_diff_in_with_opener(args, repo_root, open_plot_file)
}

pub fn run_plot_templates_in(args: &PlotTemplatesArgs, repo_root: &Path) -> Result<()> {
    let mode = OutputMode::from_flags(args.json, false);
    if let Some(template) = args.template.as_deref() {
        let spec = plot_template_spec(repo_root, template)?;
        if mode == OutputMode::Json {
            emit_json(SCHEMA_PLOT_TEMPLATES, SCHEMA_VERSION, &spec);
        } else {
            let json =
                serde_json::to_string_pretty(&spec.spec).map_err(|e| CrabError::Configuration {
                    key: template.to_owned(),
                    origin: format!("plot template serialization error: {e}"),
                })?;
            println!("{json}");
        }
    } else {
        let payload = list_plot_templates(repo_root)?;
        if mode == OutputMode::Json {
            emit_json(SCHEMA_PLOT_TEMPLATES, SCHEMA_VERSION, &payload);
        } else {
            render_plot_template_list(&payload);
        }
    }
    Ok(())
}

fn run_plot_in_with_opener(
    args: &PlotArgs,
    repo_root: &Path,
    opener: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    if args.open && args.json {
        return Err(CrabError::Configuration {
            key: "metrics plot --open".to_owned(),
            origin: "--open cannot be combined with --json".to_owned(),
        });
    }
    let format = args.effective_format()?;
    if args.open && format != PlotFormat::Html {
        return Err(CrabError::Configuration {
            key: "metrics plot --open".to_owned(),
            origin: "--open requires --format html".to_owned(),
        });
    }

    let mode = OutputMode::from_flags(args.json, false);
    let plot_configs = load_plot_configs(repo_root, args)?;
    if plot_configs.is_empty() {
        if mode == OutputMode::Json {
            let payload: Vec<serde_json::Value> = Vec::new();
            emit_json("metrics.plot", "1.0", payload);
        } else {
            println!("No plot configurations found in crab.yaml.");
        }
        return Ok(());
    }

    if mode == OutputMode::Json {
        let payload: Vec<serde_json::Value> = plot_configs
            .iter()
            .map(|pc| plot_config_to_json(pc, repo_root))
            .collect();
        emit_json("metrics.plot", "1.0", payload);
        return Ok(());
    }

    let output = match plot_comparison_refs(args)? {
        Some((baseline, target)) => match format {
            PlotFormat::Table => {
                render_plot_diff_tables(&plot_configs, repo_root, &baseline, &target)?
            }
            PlotFormat::Vega => {
                render_vega_diff_output(&plot_configs, repo_root, &baseline, &target)?
            }
            PlotFormat::Html => render_plot_diff_html(
                &plot_configs,
                repo_root,
                &baseline,
                &target,
                args.html_template.as_deref(),
            )?,
        },
        None => match format {
            PlotFormat::Table => render_plot_tables(&plot_configs, repo_root),
            PlotFormat::Vega => render_vega_output(&plot_configs, repo_root)?,
            PlotFormat::Html => {
                render_plot_html(&plot_configs, repo_root, args.html_template.as_deref())?
            }
        },
    };

    write_plot_output(args, repo_root, &output, opener)?;

    Ok(())
}

fn run_plot_diff_in_with_opener(
    args: &PlotDiffArgs,
    repo_root: &Path,
    opener: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let revisions = args.comparison_revisions()?;
    let plot_args = args.plot_args();
    if plot_args.open && plot_args.json {
        return Err(CrabError::Configuration {
            key: "plots diff --open".to_owned(),
            origin: "--open cannot be combined with --json".to_owned(),
        });
    }
    let format = plot_args.effective_format()?;
    if plot_args.open && format != PlotFormat::Html {
        return Err(CrabError::Configuration {
            key: "plots diff --open".to_owned(),
            origin: "--open requires --format html".to_owned(),
        });
    }

    let mode = OutputMode::from_flags(plot_args.json, false);
    let plot_configs = load_plot_configs(repo_root, &plot_args)?;
    if plot_configs.is_empty() {
        if mode == OutputMode::Json {
            let payload: Vec<serde_json::Value> = Vec::new();
            emit_json("metrics.plot", "1.0", payload);
        } else {
            println!("No plot configurations found in crab.yaml.");
        }
        return Ok(());
    }

    if mode == OutputMode::Json {
        let payload: Vec<serde_json::Value> = plot_configs
            .iter()
            .map(|pc| plot_config_to_json(pc, repo_root))
            .collect();
        emit_json("metrics.plot", "1.0", payload);
        return Ok(());
    }

    let output = match format {
        PlotFormat::Table => render_plot_revision_tables(&plot_configs, repo_root, &revisions)?,
        PlotFormat::Vega => render_vega_revision_output(&plot_configs, repo_root, &revisions)?,
        PlotFormat::Html => render_plot_revision_html(
            &plot_configs,
            repo_root,
            &revisions,
            plot_args.html_template.as_deref(),
        )?,
    };
    write_plot_output(&plot_args, repo_root, &output, opener)
}

fn load_plot_configs(repo_root: &Path, args: &PlotArgs) -> Result<Vec<PlotConfig>> {
    let yaml_path = repo_root.join("crab.yaml");
    let workflow = if yaml_path.exists() {
        let text = std::fs::read_to_string(&yaml_path).map_err(CrabError::Io)?;
        Some(yaml::parse(&text)?)
    } else if args.targets.is_empty() {
        return Err(CrabError::Configuration {
            key: "crab.yaml".to_owned(),
            origin: "no crab.yaml found; pass plot target files explicitly".to_owned(),
        });
    } else {
        None
    };

    Ok(collect_plot_configs(workflow.as_ref(), args))
}

fn write_plot_output(
    args: &PlotArgs,
    repo_root: &Path,
    output: &str,
    mut opener: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let open_output = if args.open {
        Some(plot_output_path_for_open(args, repo_root))
    } else {
        None
    };
    let output_path = args.output.as_ref().or(open_output.as_ref());

    if let Some(out_path) = output_path {
        if let Some(parent) = out_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        std::fs::write(out_path, output).map_err(CrabError::Io)?;
        println!("Plot data written to {}", out_path.display());
        if args.open {
            opener(out_path)?;
            println!("{}", file_url(out_path)?);
        }
    } else {
        print!("{output}");
    }

    Ok(())
}

fn plot_comparison_refs(args: &PlotArgs) -> Result<Option<(String, String)>> {
    match (&args.baseline, &args.target) {
        (Some(baseline), Some(target)) => Ok(Some((baseline.clone(), target.clone()))),
        (Some(baseline), None) => Ok(Some((baseline.clone(), "HEAD".to_owned()))),
        (None, Some(_)) => Err(CrabError::Configuration {
            key: "metrics plot --target".to_owned(),
            origin: "--target requires --baseline".to_owned(),
        }),
        (None, None) => Ok(None),
    }
}

fn collect_plot_configs(workflow: Option<&Workflow>, args: &PlotArgs) -> Vec<PlotConfig> {
    let declared = workflow.map(declared_plot_configs).unwrap_or_default();

    let configs = if args.targets.is_empty() {
        declared
    } else {
        select_plot_configs(&declared, &args.targets)
    };

    configs
        .into_iter()
        .map(|config| apply_plot_arg_overrides(config, args))
        .collect()
}

fn select_plot_configs(declared: &[PlotConfig], targets: &[PathBuf]) -> Vec<PlotConfig> {
    let mut selected = Vec::new();
    for target in targets {
        let mut matches = declared
            .iter()
            .filter(|config| plot_config_matches_target(config, target))
            .peekable();
        if matches.peek().is_none() {
            selected.push(PlotConfig {
                id: None,
                path: target.clone(),
                x: None,
                x_path: None,
                y: Vec::new(),
                no_header: false,
                title: None,
                x_label: None,
                y_label: None,
                template: None,
            });
        } else {
            selected.extend(matches.cloned());
        }
    }
    selected
}

fn plot_config_matches_target(config: &PlotConfig, target: &Path) -> bool {
    config.path == target
        || config
            .id
            .as_deref()
            .is_some_and(|id| Path::new(id) == target)
}

fn declared_plot_configs(workflow: &Workflow) -> Vec<PlotConfig> {
    let mut configs = workflow.plot_configs.clone();
    for path in &workflow.plots {
        push_simple_plot_config(&mut configs, path);
    }
    for stage in workflow.stages.values() {
        for path in &stage.plots {
            push_simple_plot_config(&mut configs, path);
        }
    }
    configs
}

fn push_simple_plot_config(configs: &mut Vec<PlotConfig>, path: &Path) {
    if configs.iter().any(|config| config.path == path) {
        return;
    }
    configs.push(PlotConfig {
        id: None,
        path: path.to_path_buf(),
        x: None,
        x_path: None,
        y: Vec::new(),
        no_header: false,
        title: None,
        x_label: None,
        y_label: None,
        template: None,
    });
}

fn apply_plot_arg_overrides(mut config: PlotConfig, args: &PlotArgs) -> PlotConfig {
    if let Some(x) = &args.x {
        config.x = Some(x.clone());
    }
    if !args.y.is_empty() {
        config.y.clone_from(&args.y);
    }
    if args.no_header {
        config.no_header = true;
    }
    if let Some(title) = &args.title {
        config.title = Some(title.clone());
    }
    if let Some(label) = &args.x_label {
        config.x_label = Some(label.clone());
    }
    if let Some(label) = &args.y_label {
        config.y_label = Some(label.clone());
    }
    if let Some(template) = &args.template {
        config.template = Some(template.clone());
    }
    config
}

fn plot_output_path_for_open(args: &PlotArgs, repo_root: &Path) -> PathBuf {
    args.output
        .clone()
        .unwrap_or_else(|| repo_root.join("crab_plots").join("index.html"))
}

fn open_plot_file(path: &Path) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(path).status()
    }
    .map_err(CrabError::Io)?;

    if status.success() {
        Ok(())
    } else {
        Err(CrabError::Configuration {
            key: "metrics plot --open".to_owned(),
            origin: format!("browser opener exited with status {status}"),
        })
    }
}

fn file_url(path: &Path) -> Result<String> {
    let abs = path.canonicalize().map_err(CrabError::Io)?;
    let raw = abs.to_string_lossy().replace('\\', "/");
    let path = if cfg!(target_os = "windows") && !raw.starts_with('/') {
        format!("/{raw}")
    } else {
        raw
    };
    Ok(format!("file://{}", encode_file_url_path(&path)))
}

fn encode_file_url_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

fn plot_config_to_json(config: &PlotConfig, repo_root: &Path) -> serde_json::Value {
    let sources = collect_working_plot_sources(repo_root, config).unwrap_or_else(|_| {
        vec![PlotSource {
            path: config.path.clone(),
            kind: PlotSourceKind::Data,
        }]
    });
    let mut data_rows = Vec::new();
    let mut data_y_fields = Vec::new();
    let mut image_items = Vec::new();

    for source in &sources {
        match source.kind {
            PlotSourceKind::Data => {
                let source_config = plot_config_for_source(config, &source.path);
                if let Ok(data) = read_working_plot_data(repo_root, &source_config) {
                    if data_y_fields.is_empty() {
                        data_y_fields.clone_from(&data.y_fields);
                    }
                    data_rows.extend(data.rows);
                }
            }
            PlotSourceKind::Image => {
                let path = repo_root.join(&source.path);
                if let Ok(metadata) = fs::metadata(path) {
                    image_items.push(serde_json::json!({
                        "path": source.path.to_string_lossy(),
                        "mime_type": image_mime_type(&source.path),
                        "bytes": metadata.len(),
                    }));
                }
            }
        }
    }

    serde_json::json!({
        "id": config.id,
        "path": config.path.to_string_lossy(),
        "kind": plot_config_kind(&sources),
        "x": config.x,
        "x_path": config.x_path,
        "y": data_y_fields,
        "title": config.title,
        "template": config.template,
        "data_points": data_rows.len(),
        "data": data_rows,
        "images": image_items,
    })
}

fn render_plot_tables(configs: &[PlotConfig], repo_root: &Path) -> String {
    let mut output = String::new();
    for config in configs {
        match render_plot_tables_for_config(config, repo_root) {
            Ok(table) => output.push_str(&table),
            Err(e) => {
                output.push_str(&render_plot_header(config, None));
                let _ = writeln!(output, "  ({e})");
            }
        }
        output.push('\n');
    }
    output
}

fn render_plot_diff_tables(
    configs: &[PlotConfig],
    repo_root: &Path,
    baseline: &str,
    target: &str,
) -> Result<String> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut output = String::new();
    for config in configs {
        output.push_str(&render_plot_diff_tables_for_config(
            config, repo_root, &git_dir, baseline, target,
        )?);
        output.push('\n');
    }
    Ok(output)
}

fn render_plot_revision_tables(
    configs: &[PlotConfig],
    repo_root: &Path,
    revisions: &[String],
) -> Result<String> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut output = String::new();
    for config in configs {
        output.push_str(&render_plot_revision_tables_for_config(
            config, repo_root, &git_dir, revisions,
        )?);
        output.push('\n');
    }
    Ok(output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlotSourceKind {
    Data,
    Image,
}

#[derive(Debug, Clone)]
struct PlotSource {
    path: PathBuf,
    kind: PlotSourceKind,
}

#[derive(Debug, Serialize)]
struct PlotSpecDocument {
    path: String,
    title: String,
    spec: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PlotDocument {
    Vega {
        path: String,
        title: String,
        spec: serde_json::Value,
    },
    Image {
        path: String,
        title: String,
        images: Vec<PlotImageItem>,
    },
}

#[derive(Debug, Clone, Serialize)]
struct PlotImageItem {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    mime_type: String,
    bytes: u64,
    data_url: String,
}

struct PlotRevisionData {
    revision: String,
    data: PlotData,
}

fn render_vega_output(configs: &[PlotConfig], repo_root: &Path) -> Result<String> {
    let specs = build_plot_specs(configs, repo_root)?;
    if specs.len() == 1 {
        return serde_json::to_string_pretty(&specs[0].spec).map_err(|e| {
            CrabError::Configuration {
                key: "plot vega".to_owned(),
                origin: format!("vega serialization error: {e}"),
            }
        });
    }
    serde_json::to_string_pretty(&serde_json::json!({ "plots": specs })).map_err(|e| {
        CrabError::Configuration {
            key: "plot vega".to_owned(),
            origin: format!("vega serialization error: {e}"),
        }
    })
}

fn render_plot_html(
    configs: &[PlotConfig],
    repo_root: &Path,
    html_template: Option<&Path>,
) -> Result<String> {
    let documents = build_plot_documents(configs, repo_root)?;
    render_plot_html_document("Crab plots", &documents, repo_root, html_template)
}

fn render_vega_diff_output(
    configs: &[PlotConfig],
    repo_root: &Path,
    baseline: &str,
    target: &str,
) -> Result<String> {
    let specs = build_plot_diff_specs(configs, repo_root, baseline, target)?;
    if specs.len() == 1 {
        return serde_json::to_string_pretty(&specs[0].spec).map_err(|e| {
            CrabError::Configuration {
                key: "plot vega".to_owned(),
                origin: format!("vega serialization error: {e}"),
            }
        });
    }
    serde_json::to_string_pretty(&serde_json::json!({ "plots": specs })).map_err(|e| {
        CrabError::Configuration {
            key: "plot vega".to_owned(),
            origin: format!("vega serialization error: {e}"),
        }
    })
}

fn render_plot_diff_html(
    configs: &[PlotConfig],
    repo_root: &Path,
    baseline: &str,
    target: &str,
    html_template: Option<&Path>,
) -> Result<String> {
    let documents = build_plot_diff_documents(configs, repo_root, baseline, target)?;
    render_plot_html_document(
        &format!("Crab plot diff: {baseline} vs {target}"),
        &documents,
        repo_root,
        html_template,
    )
}

fn render_vega_revision_output(
    configs: &[PlotConfig],
    repo_root: &Path,
    revisions: &[String],
) -> Result<String> {
    let specs = build_plot_revision_specs(configs, repo_root, revisions)?;
    if specs.len() == 1 {
        return serde_json::to_string_pretty(&specs[0].spec).map_err(|e| {
            CrabError::Configuration {
                key: "plot vega".to_owned(),
                origin: format!("vega serialization error: {e}"),
            }
        });
    }
    serde_json::to_string_pretty(&serde_json::json!({ "plots": specs })).map_err(|e| {
        CrabError::Configuration {
            key: "plot vega".to_owned(),
            origin: format!("vega serialization error: {e}"),
        }
    })
}

fn render_plot_revision_html(
    configs: &[PlotConfig],
    repo_root: &Path,
    revisions: &[String],
    html_template: Option<&Path>,
) -> Result<String> {
    let documents = build_plot_revision_documents(configs, repo_root, revisions)?;
    render_plot_html_document(
        &format!("Crab plot diff: {}", revision_summary(revisions)),
        &documents,
        repo_root,
        html_template,
    )
}

fn render_plot_html_document(
    title: &str,
    documents: &[PlotDocument],
    repo_root: &Path,
    html_template: Option<&Path>,
) -> Result<String> {
    let plot_divs = render_plot_divs(documents)?;
    let template = match html_template {
        Some(path) => read_plot_html_template(repo_root, path)?,
        None => default_plot_html_template(title),
    };
    if !template.contains("{plot_divs}") {
        return Err(CrabError::Configuration {
            key: html_template
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "plot html template".to_owned()),
            origin: "HTML plot template must contain a {plot_divs} marker".to_owned(),
        });
    }
    Ok(template.replace("{plot_divs}", &plot_divs))
}

fn read_plot_html_template(repo_root: &Path, path: &Path) -> Result<String> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    std::fs::read_to_string(&resolved).map_err(CrabError::Io)
}

fn default_plot_html_template(title: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <script src="https://cdn.jsdelivr.net/npm/vega@5"></script>
  <script src="https://cdn.jsdelivr.net/npm/vega-lite@5"></script>
  <script src="https://cdn.jsdelivr.net/npm/vega-embed@6"></script>
  <style>
    body {{ font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 2rem; color: #111827; }}
    main {{ max-width: 960px; margin: 0 auto; }}
    section {{ margin-bottom: 2rem; }}
    h1 {{ font-size: 1.5rem; margin-bottom: 1.5rem; }}
    h2 {{ font-size: 1rem; margin-bottom: .25rem; }}
    .path {{ color: #6b7280; font-size: .875rem; margin-bottom: .75rem; }}
    .image-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 1rem; }}
    figure {{ margin: 0; border: 1px solid #e5e7eb; border-radius: .5rem; padding: .75rem; background: #fff; }}
    figure img {{ display: block; max-width: 100%; height: auto; margin: 0 auto; }}
    figcaption {{ margin-top: .5rem; color: #374151; font-size: .8125rem; overflow-wrap: anywhere; }}
  </style>
</head>
<body>
  <main>
    <h1>{title}</h1>
    {{plot_divs}}
  </main>
</body>
</html>
"#,
        title = escape_html_text(title)
    )
}

fn render_plot_divs(documents: &[PlotDocument]) -> Result<String> {
    let documents_json =
        serde_json::to_string_pretty(&documents).map_err(|e| CrabError::Configuration {
            key: "plot html".to_owned(),
            origin: format!("plot spec serialization error: {e}"),
        })?;
    let documents_json = documents_json.replace("</", "<\\/");
    Ok(format!(
        r#"<div id="plots"></div>
  <script>
    const plots = {documents_json};
    const root = document.getElementById("plots");
    plots.forEach((plot, index) => {{
      const section = document.createElement("section");
      const title = document.createElement("h2");
      title.textContent = plot.title;
      const path = document.createElement("div");
      path.className = "path";
      path.textContent = plot.path;
      const body = document.createElement("div");
      body.id = `plot-${{index}}`;
      section.append(title, path, body);
      root.appendChild(section);
      if (plot.kind === "vega") {{
        vegaEmbed(body, plot.spec, {{ actions: true }});
        return;
      }}
      const grid = document.createElement("div");
      grid.className = "image-grid";
      plot.images.forEach((image) => {{
        const figure = document.createElement("figure");
        const img = document.createElement("img");
        img.src = image.data_url;
        img.alt = image.path;
        const caption = document.createElement("figcaption");
        caption.textContent = image.label ? `${{image.label}}: ${{image.path}}` : image.path;
        figure.append(img, caption);
        grid.appendChild(figure);
      }});
      body.appendChild(grid);
    }});
  </script>
"#
    ))
}

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn build_plot_specs(configs: &[PlotConfig], repo_root: &Path) -> Result<Vec<PlotSpecDocument>> {
    let mut specs = Vec::new();
    for config in configs {
        let sources = collect_working_plot_sources(repo_root, config)?;
        reject_image_sources_for_vega(config, &sources)?;
        for source in data_sources(&sources) {
            let source_config = plot_config_for_source(config, &source.path);
            let data = read_working_plot_data(repo_root, &source_config)?;
            let title = plot_title_for_source(config, &source.path);
            let spec = build_vega_spec(&source_config, &title, &data, repo_root)?;
            specs.push(PlotSpecDocument {
                path: source.path.display().to_string(),
                title,
                spec,
            });
        }
    }
    Ok(specs)
}

fn build_plot_documents(configs: &[PlotConfig], repo_root: &Path) -> Result<Vec<PlotDocument>> {
    let mut documents = Vec::new();
    for config in configs {
        let sources = collect_working_plot_sources(repo_root, config)?;
        for source in data_sources(&sources) {
            let source_config = plot_config_for_source(config, &source.path);
            let data = read_working_plot_data(repo_root, &source_config)?;
            let title = plot_title_for_source(config, &source.path);
            let spec = build_vega_spec(&source_config, &title, &data, repo_root)?;
            documents.push(PlotDocument::Vega {
                path: source.path.display().to_string(),
                title,
                spec,
            });
        }

        let images = read_working_image_items(repo_root, &sources, None)?;
        if !images.is_empty() {
            documents.push(PlotDocument::Image {
                path: config.path.display().to_string(),
                title: plot_title_for_config(config),
                images,
            });
        }
    }
    Ok(documents)
}

fn build_plot_diff_specs(
    configs: &[PlotConfig],
    repo_root: &Path,
    baseline: &str,
    target: &str,
) -> Result<Vec<PlotSpecDocument>> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut specs = Vec::new();
    for config in configs {
        let baseline_sources = collect_plot_sources_at_ref(repo_root, &git_dir, baseline, config)?;
        let target_sources = collect_plot_sources_at_ref(repo_root, &git_dir, target, config)?;
        reject_image_sources_for_vega(config, &baseline_sources)?;
        reject_image_sources_for_vega(config, &target_sources)?;

        for source_path in diff_data_paths(&baseline_sources, &target_sources) {
            let source_config = plot_config_for_source(config, &source_path);
            let baseline_data =
                read_plot_data_at_ref(repo_root, &git_dir, baseline, &source_config)?;
            let target_data = read_plot_data_at_ref(repo_root, &git_dir, target, &source_config)?;
            let title = plot_title_for_source(config, &source_path);
            let spec = build_vega_diff_spec(
                &source_config,
                &title,
                baseline,
                target,
                &baseline_data,
                &target_data,
                repo_root,
            )?;
            specs.push(PlotSpecDocument {
                path: source_path.display().to_string(),
                title,
                spec,
            });
        }
    }
    Ok(specs)
}

fn build_plot_revision_specs(
    configs: &[PlotConfig],
    repo_root: &Path,
    revisions: &[String],
) -> Result<Vec<PlotSpecDocument>> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut specs = Vec::new();
    for config in configs {
        let revision_sources = collect_revision_sources(repo_root, &git_dir, revisions, config)?;
        for sources in &revision_sources {
            reject_image_sources_for_vega(config, sources)?;
        }

        for source_path in data_paths_for_source_sets(&revision_sources) {
            let source_config = plot_config_for_source(config, &source_path);
            let revision_data =
                read_revision_plot_data(repo_root, &git_dir, revisions, &source_config)?;
            let title = plot_title_for_source(config, &source_path);
            let spec = build_vega_revision_spec(&source_config, &title, &revision_data, repo_root)?;
            specs.push(PlotSpecDocument {
                path: source_path.display().to_string(),
                title,
                spec,
            });
        }
    }
    Ok(specs)
}

fn build_plot_diff_documents(
    configs: &[PlotConfig],
    repo_root: &Path,
    baseline: &str,
    target: &str,
) -> Result<Vec<PlotDocument>> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut documents = Vec::new();
    for config in configs {
        let baseline_sources = collect_plot_sources_at_ref(repo_root, &git_dir, baseline, config)?;
        let target_sources = collect_plot_sources_at_ref(repo_root, &git_dir, target, config)?;

        for source_path in diff_data_paths(&baseline_sources, &target_sources) {
            let source_config = plot_config_for_source(config, &source_path);
            let baseline_data =
                read_plot_data_at_ref(repo_root, &git_dir, baseline, &source_config)?;
            let target_data = read_plot_data_at_ref(repo_root, &git_dir, target, &source_config)?;
            let title = plot_title_for_source(config, &source_path);
            let spec = build_vega_diff_spec(
                &source_config,
                &title,
                baseline,
                target,
                &baseline_data,
                &target_data,
                repo_root,
            )?;
            documents.push(PlotDocument::Vega {
                path: source_path.display().to_string(),
                title,
                spec,
            });
        }

        let mut images = read_image_items_at_ref(
            repo_root,
            &git_dir,
            baseline,
            &baseline_sources,
            Some(baseline),
        )?;
        images.extend(read_image_items_at_ref(
            repo_root,
            &git_dir,
            target,
            &target_sources,
            Some(target),
        )?);
        if !images.is_empty() {
            documents.push(PlotDocument::Image {
                path: config.path.display().to_string(),
                title: format!("{}: {baseline} vs {target}", plot_title_for_config(config)),
                images,
            });
        }
    }
    Ok(documents)
}

fn build_plot_revision_documents(
    configs: &[PlotConfig],
    repo_root: &Path,
    revisions: &[String],
) -> Result<Vec<PlotDocument>> {
    let git_dir = params::find_git_dir(repo_root)?;
    let mut documents = Vec::new();
    for config in configs {
        let revision_sources = collect_revision_sources(repo_root, &git_dir, revisions, config)?;

        for source_path in data_paths_for_source_sets(&revision_sources) {
            let source_config = plot_config_for_source(config, &source_path);
            let revision_data =
                read_revision_plot_data(repo_root, &git_dir, revisions, &source_config)?;
            let title = plot_title_for_source(config, &source_path);
            let spec = build_vega_revision_spec(&source_config, &title, &revision_data, repo_root)?;
            documents.push(PlotDocument::Vega {
                path: source_path.display().to_string(),
                title,
                spec,
            });
        }

        let mut images = Vec::new();
        for (revision, sources) in revisions.iter().zip(&revision_sources) {
            images.extend(read_image_items_at_ref(
                repo_root,
                &git_dir,
                revision,
                sources,
                Some(revision),
            )?);
        }
        if !images.is_empty() {
            documents.push(PlotDocument::Image {
                path: config.path.display().to_string(),
                title: format!(
                    "{}: {}",
                    plot_title_for_config(config),
                    revision_summary(revisions)
                ),
                images,
            });
        }
    }
    Ok(documents)
}

fn collect_working_plot_sources(repo_root: &Path, config: &PlotConfig) -> Result<Vec<PlotSource>> {
    let target = repo_root.join(&config.path);
    if target.is_file() {
        let kind = classify_plot_source(&config.path)
            .ok_or_else(|| unsupported_plot_source_error(&config.path))?;
        return Ok(vec![PlotSource {
            path: config.path.clone(),
            kind,
        }]);
    }

    if target.is_dir() {
        let mut sources = Vec::new();
        collect_working_plot_sources_in_dir(repo_root, &target, &mut sources)?;
        sources.sort_by(|a, b| a.path.cmp(&b.path));
        if sources.is_empty() {
            return Err(CrabError::Configuration {
                key: config.path.display().to_string(),
                origin: "plot directory contains no supported plot files".to_owned(),
            });
        }
        return Ok(sources);
    }

    Err(CrabError::Configuration {
        key: config.path.display().to_string(),
        origin: "plot target not found".to_owned(),
    })
}

fn collect_working_plot_sources_in_dir(
    repo_root: &Path,
    dir: &Path,
    sources: &mut Vec<PlotSource>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .map_err(CrabError::Io)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(CrabError::Io)?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;
        if file_type.is_dir() {
            collect_working_plot_sources_in_dir(repo_root, &path, sources)?;
        } else if file_type.is_file() {
            let rel = path.strip_prefix(repo_root).unwrap_or(path.as_path());
            if let Some(kind) = classify_plot_source(rel) {
                sources.push(PlotSource {
                    path: rel.to_path_buf(),
                    kind,
                });
            }
        }
    }
    Ok(())
}

fn collect_plot_sources_at_ref(
    repo_root: &Path,
    git_dir: &Path,
    ref_name: &str,
    config: &PlotConfig,
) -> Result<Vec<PlotSource>> {
    if ref_name == WORKSPACE_REF {
        return collect_working_plot_sources(repo_root, config);
    }

    if let Some(kind) = classify_plot_source(&config.path) {
        if params::read_blob_at_ref(git_dir, ref_name, &config.path)?.is_some()
            || ref_name == "HEAD" && repo_root.join(&config.path).is_file()
        {
            return Ok(vec![PlotSource {
                path: config.path.clone(),
                kind,
            }]);
        }
        return Err(CrabError::StageDepMissing {
            stage: "plots".to_owned(),
            path: config.path.clone(),
        });
    }

    let mut sources = list_plot_sources_at_ref(git_dir, ref_name, &config.path)?;
    if sources.is_empty() && ref_name == "HEAD" && repo_root.join(&config.path).is_dir() {
        sources = collect_working_plot_sources(repo_root, config)?;
    }
    if sources.is_empty() {
        return Err(CrabError::StageDepMissing {
            stage: "plots".to_owned(),
            path: config.path.clone(),
        });
    }
    Ok(sources)
}

fn list_plot_sources_at_ref(
    git_dir: &Path,
    ref_name: &str,
    path: &Path,
) -> Result<Vec<PlotSource>> {
    let work_dir = git_dir.parent().unwrap_or(Path::new("."));
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "-z", ref_name, "--"])
        .arg(path)
        .current_dir(work_dir)
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git ls-tree: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git ls-tree failed for ref {ref_name}: {}",
            stderr.trim()
        )));
    }

    let mut sources = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = PathBuf::from(String::from_utf8_lossy(raw).into_owned());
        if let Some(kind) = classify_plot_source(&rel) {
            sources.push(PlotSource { path: rel, kind });
        }
    }
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(sources)
}

fn classify_plot_source(path: &Path) -> Option<PlotSourceKind> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)?;
    match ext.as_str() {
        "csv" | "tsv" | "json" | "yaml" | "yml" => Some(PlotSourceKind::Data),
        "jpg" | "jpeg" | "png" | "gif" | "svg" => Some(PlotSourceKind::Image),
        _ => None,
    }
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)?;
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

fn unsupported_plot_source_error(path: &Path) -> CrabError {
    CrabError::Configuration {
        key: path.display().to_string(),
        origin: "unsupported plot extension (expected .csv, .tsv, .json, .yaml, .yml, .jpg, .jpeg, .png, .gif, .svg, or a directory containing those files)".to_owned(),
    }
}

fn data_sources(sources: &[PlotSource]) -> impl Iterator<Item = &PlotSource> {
    sources
        .iter()
        .filter(|source| source.kind == PlotSourceKind::Data)
}

fn image_sources(sources: &[PlotSource]) -> impl Iterator<Item = &PlotSource> {
    sources
        .iter()
        .filter(|source| source.kind == PlotSourceKind::Image)
}

fn read_working_image_items(
    repo_root: &Path,
    sources: &[PlotSource],
    label: Option<&str>,
) -> Result<Vec<PlotImageItem>> {
    image_sources(sources)
        .map(|source| {
            let bytes = fs::read(repo_root.join(&source.path)).map_err(CrabError::Io)?;
            build_image_item(&source.path, label, &bytes)
        })
        .collect()
}

fn read_image_items_at_ref(
    repo_root: &Path,
    git_dir: &Path,
    ref_name: &str,
    sources: &[PlotSource],
    label: Option<&str>,
) -> Result<Vec<PlotImageItem>> {
    if ref_name == WORKSPACE_REF {
        return read_working_image_items(repo_root, sources, label);
    }

    image_sources(sources)
        .map(|source| {
            let bytes = match params::read_blob_at_ref(git_dir, ref_name, &source.path)? {
                Some(bytes) => bytes,
                None if ref_name == "HEAD" && repo_root.join(&source.path).is_file() => {
                    fs::read(repo_root.join(&source.path)).map_err(CrabError::Io)?
                }
                None => {
                    return Err(CrabError::StageDepMissing {
                        stage: "plots".to_owned(),
                        path: source.path.clone(),
                    });
                }
            };
            build_image_item(&source.path, label, &bytes)
        })
        .collect()
}

fn build_image_item(path: &Path, label: Option<&str>, bytes: &[u8]) -> Result<PlotImageItem> {
    let mime_type = image_mime_type(path).ok_or_else(|| unsupported_plot_source_error(path))?;
    Ok(PlotImageItem {
        path: path.display().to_string(),
        label: label.map(str::to_owned),
        mime_type: mime_type.to_owned(),
        bytes: bytes.len() as u64,
        data_url: format!("data:{mime_type};base64,{}", BASE64_STANDARD.encode(bytes)),
    })
}

fn plot_config_kind(sources: &[PlotSource]) -> &'static str {
    let has_data = sources
        .iter()
        .any(|source| source.kind == PlotSourceKind::Data);
    let has_image = sources
        .iter()
        .any(|source| source.kind == PlotSourceKind::Image);
    match (has_data, has_image) {
        (true, true) => "mixed",
        (true, false) => "data",
        (false, true) => "image",
        (false, false) => "empty",
    }
}

fn plot_config_for_source(config: &PlotConfig, source_path: &Path) -> PlotConfig {
    let mut source_config = config.clone();
    source_config.path = source_path.to_path_buf();
    if source_path != config.path.as_path() {
        source_config.title = Some(plot_title_for_source(config, source_path));
    }
    source_config
}

fn plot_title_for_config(config: &PlotConfig) -> String {
    config
        .title
        .clone()
        .or_else(|| config.id.clone())
        .unwrap_or_else(|| config.path.display().to_string())
}

fn plot_title_for_source(config: &PlotConfig, source_path: &Path) -> String {
    match &config.title {
        Some(title) if source_path == config.path.as_path() => title.clone(),
        Some(title) => format!("{title}: {}", source_path.display()),
        None => match &config.id {
            Some(id) if source_path == config.path.as_path() => id.clone(),
            Some(id) => format!("{id}: {}", source_path.display()),
            None => source_path.display().to_string(),
        },
    }
}

fn diff_data_paths(baseline_sources: &[PlotSource], target_sources: &[PlotSource]) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    paths.extend(
        baseline_sources
            .iter()
            .filter(|source| source.kind == PlotSourceKind::Data)
            .map(|source| source.path.clone()),
    );
    paths.extend(
        target_sources
            .iter()
            .filter(|source| source.kind == PlotSourceKind::Data)
            .map(|source| source.path.clone()),
    );
    paths.into_iter().collect()
}

fn data_paths_for_source_sets(source_sets: &[Vec<PlotSource>]) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for sources in source_sets {
        paths.extend(
            sources
                .iter()
                .filter(|source| source.kind == PlotSourceKind::Data)
                .map(|source| source.path.clone()),
        );
    }
    paths.into_iter().collect()
}

fn collect_revision_sources(
    repo_root: &Path,
    git_dir: &Path,
    revisions: &[String],
    config: &PlotConfig,
) -> Result<Vec<Vec<PlotSource>>> {
    revisions
        .iter()
        .map(|revision| collect_plot_sources_at_ref(repo_root, git_dir, revision, config))
        .collect()
}

fn read_revision_plot_data(
    repo_root: &Path,
    git_dir: &Path,
    revisions: &[String],
    config: &PlotConfig,
) -> Result<Vec<PlotRevisionData>> {
    revisions
        .iter()
        .map(|revision| {
            Ok(PlotRevisionData {
                revision: revision.clone(),
                data: read_plot_data_at_ref(repo_root, git_dir, revision, config)?,
            })
        })
        .collect()
}

fn reject_image_sources_for_vega(config: &PlotConfig, sources: &[PlotSource]) -> Result<()> {
    if sources
        .iter()
        .any(|source| source.kind == PlotSourceKind::Image)
    {
        return Err(CrabError::Configuration {
            key: config.path.display().to_string(),
            origin: "image plot targets require --format html or --format table".to_owned(),
        });
    }
    Ok(())
}

fn build_vega_spec(
    config: &PlotConfig,
    title: &str,
    data: &PlotData,
    repo_root: &Path,
) -> Result<serde_json::Value> {
    let x_field = config.x.as_deref().unwrap_or("step");
    if let Some(spec) = build_template_vega_spec(
        config,
        title,
        data,
        repo_root,
        TemplateSeries::Single,
        x_field,
    )? {
        return Ok(spec);
    }

    let values = plot_data_values(x_field, data, None);
    let mark = match config.template.as_deref() {
        Some("scatter" | "point") => "point",
        Some("bar" | "confusion") => "bar",
        _ => "line",
    };
    let x_type = infer_vega_type(&values, x_field);
    let y_type = infer_vega_type(&values, "value");
    let y_field_title = if data.y_fields.len() == 1 {
        data.y_fields[0].as_str()
    } else {
        "value"
    };
    let x_title = plot_x_label(config, x_field);
    let y_title = plot_y_label(config, y_field_title);

    Ok(serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "title": title,
        "data": { "values": values },
        "mark": { "type": mark, "tooltip": true },
        "encoding": {
            "x": { "field": x_field, "type": x_type, "title": x_title },
            "y": { "field": "value", "type": y_type, "title": y_title },
            "color": { "field": "metric", "type": "nominal" }
        }
    }))
}

fn build_vega_diff_spec(
    config: &PlotConfig,
    title: &str,
    baseline: &str,
    target: &str,
    baseline_data: &PlotData,
    target_data: &PlotData,
    repo_root: &Path,
) -> Result<serde_json::Value> {
    let x_field = config.x.as_deref().unwrap_or("step");
    if let Some(spec) = build_template_vega_spec(
        config,
        &format!("{title}: {baseline} vs {target}"),
        baseline_data,
        repo_root,
        TemplateSeries::Diff {
            baseline,
            target,
            target_data,
        },
        x_field,
    )? {
        return Ok(spec);
    }

    let mut values = plot_data_values(x_field, baseline_data, Some(baseline));
    values.extend(plot_data_values(x_field, target_data, Some(target)));
    let mark = match config.template.as_deref() {
        Some("scatter" | "point") => "point",
        Some("bar" | "confusion") => "bar",
        _ => "line",
    };
    let x_type = infer_vega_type(&values, x_field);
    let y_type = infer_vega_type(&values, "value");
    let x_title = plot_x_label(config, x_field);
    let y_title = plot_y_label(config, "value");

    Ok(serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "title": format!("{title}: {baseline} vs {target}"),
        "data": { "values": values },
        "mark": { "type": mark, "tooltip": true },
        "encoding": {
            "x": { "field": x_field, "type": x_type, "title": x_title },
            "y": { "field": "value", "type": y_type, "title": y_title },
            "color": { "field": "series", "type": "nominal", "title": "series" }
        }
    }))
}

fn build_vega_revision_spec(
    config: &PlotConfig,
    title: &str,
    revision_data: &[PlotRevisionData],
    repo_root: &Path,
) -> Result<serde_json::Value> {
    let Some(first) = revision_data.first() else {
        return Err(CrabError::Configuration {
            key: config.path.display().to_string(),
            origin: "plots diff requires at least one revision".to_owned(),
        });
    };

    let summary = revision_data_summary(revision_data);
    let x_field = config.x.as_deref().unwrap_or("step");
    if let Some(spec) = build_template_vega_spec(
        config,
        &format!("{title}: {summary}"),
        &first.data,
        repo_root,
        TemplateSeries::Revisions(revision_data),
        x_field,
    )? {
        return Ok(spec);
    }

    let mut values = Vec::new();
    for item in revision_data {
        values.extend(plot_data_values(x_field, &item.data, Some(&item.revision)));
    }
    let mark = match config.template.as_deref() {
        Some("scatter" | "point") => "point",
        Some("bar" | "confusion") => "bar",
        _ => "line",
    };
    let x_type = infer_vega_type(&values, x_field);
    let y_type = infer_vega_type(&values, "value");
    let x_title = plot_x_label(config, x_field);
    let y_title = plot_y_label(config, "value");

    Ok(serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "title": format!("{title}: {summary}"),
        "data": { "values": values },
        "mark": { "type": mark, "tooltip": true },
        "encoding": {
            "x": { "field": x_field, "type": x_type, "title": x_title },
            "y": { "field": "value", "type": y_type, "title": y_title },
            "color": { "field": "series", "type": "nominal", "title": "series" }
        }
    }))
}

fn plot_x_label(config: &PlotConfig, fallback: &str) -> String {
    config
        .x_label
        .clone()
        .unwrap_or_else(|| fallback.to_owned())
}

fn plot_y_label(config: &PlotConfig, fallback: &str) -> String {
    config
        .y_label
        .clone()
        .unwrap_or_else(|| fallback.to_owned())
}

enum TemplateSeries<'a> {
    Single,
    Diff {
        baseline: &'a str,
        target: &'a str,
        target_data: &'a PlotData,
    },
    Revisions(&'a [PlotRevisionData]),
}

fn build_template_vega_spec(
    config: &PlotConfig,
    title: &str,
    data: &PlotData,
    repo_root: &Path,
    series: TemplateSeries<'_>,
    x_field: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(template) = config.template.as_deref() else {
        return Ok(None);
    };
    let Some(spec) = resolve_plot_template_spec(repo_root, template)? else {
        return Ok(None);
    };
    let y_field = data.y_fields.first().map_or("value", String::as_str);
    let x_label = plot_x_label(config, x_field);
    let y_label = plot_y_label(config, y_field);
    let values = match series {
        TemplateSeries::Single => plot_template_data_values(x_field, data, None),
        TemplateSeries::Diff {
            baseline,
            target,
            target_data,
        } => {
            let mut values = plot_template_data_values(x_field, data, Some(baseline));
            values.extend(plot_template_data_values(
                x_field,
                target_data,
                Some(target),
            ));
            values
        }
        TemplateSeries::Revisions(revision_data) => {
            let mut values = Vec::new();
            for item in revision_data {
                values.extend(plot_template_data_values(
                    x_field,
                    &item.data,
                    Some(&item.revision),
                ));
            }
            values
        }
    };
    let color_field = if values.iter().any(|value| value.get("rev").is_some()) {
        "rev"
    } else {
        "metric"
    };
    Ok(Some(apply_plot_template_anchors(
        spec,
        &PlotTemplateValues {
            data: serde_json::Value::Array(values),
            title,
            x_field,
            y_field,
            x_label: &x_label,
            y_label: &y_label,
            color_field,
        },
    )))
}

#[derive(Debug, Serialize)]
pub struct PlotTemplatesPayload {
    pub templates: Vec<PlotTemplateInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlotTemplateInfo {
    pub name: String,
    pub source: PlotTemplateSource,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PlotTemplateSource {
    Builtin,
    Local,
}

#[derive(Debug, Serialize)]
pub struct PlotTemplateSpecPayload {
    pub name: String,
    pub source: PlotTemplateSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub spec: serde_json::Value,
}

struct BuiltinPlotTemplate {
    name: &'static str,
    description: &'static str,
}

const BUILTIN_PLOT_TEMPLATES: &[BuiltinPlotTemplate] = &[
    BuiltinPlotTemplate {
        name: "linear",
        description: "basic linear plot including cursor interactivity",
    },
    BuiltinPlotTemplate {
        name: "simple",
        description: "minimal linear template, useful as a custom template base",
    },
    BuiltinPlotTemplate {
        name: "scatter",
        description: "scatter plot",
    },
    BuiltinPlotTemplate {
        name: "smooth",
        description: "linear plot with LOESS smoothing",
    },
    BuiltinPlotTemplate {
        name: "confusion",
        description: "confusion matrix heatmap",
    },
    BuiltinPlotTemplate {
        name: "confusion_normalized",
        description: "confusion matrix heatmap for normalized values",
    },
    BuiltinPlotTemplate {
        name: "bar_horizontal",
        description: "horizontal bar plot",
    },
    BuiltinPlotTemplate {
        name: "bar_horizontal_sorted",
        description: "horizontal bar plot sorted by value",
    },
];

fn list_plot_templates(repo_root: &Path) -> Result<PlotTemplatesPayload> {
    let mut templates = BUILTIN_PLOT_TEMPLATES
        .iter()
        .map(|template| PlotTemplateInfo {
            name: template.name.to_owned(),
            source: PlotTemplateSource::Builtin,
            description: template.description.to_owned(),
            path: None,
        })
        .collect::<Vec<_>>();
    templates.extend(local_plot_templates(repo_root)?);
    templates.sort_by(|a, b| {
        a.source
            .cmp_key()
            .cmp(&b.source.cmp_key())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(PlotTemplatesPayload { templates })
}

impl PlotTemplateSource {
    fn cmp_key(self) -> u8 {
        match self {
            Self::Builtin => 0,
            Self::Local => 1,
        }
    }
}

fn local_plot_templates(repo_root: &Path) -> Result<Vec<PlotTemplateInfo>> {
    let mut seen = BTreeMap::<String, PathBuf>::new();
    for dir in [
        repo_root.join(".crab").join("plots"),
        repo_root.join(".dvc").join("plots"),
    ] {
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(CrabError::Io)?;
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    seen.entry(name.to_owned()).or_insert(path);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CrabError::Io(e)),
        }
    }
    Ok(seen
        .into_iter()
        .map(|(name, path)| PlotTemplateInfo {
            name,
            source: PlotTemplateSource::Local,
            description: "local Vega-Lite JSON template".to_owned(),
            path: Some(path),
        })
        .collect())
}

fn render_plot_template_list(payload: &PlotTemplatesPayload) {
    let mut current_source = None;
    for template in &payload.templates {
        if current_source != Some(template.source) {
            current_source = Some(template.source);
            match template.source {
                PlotTemplateSource::Builtin => println!("Built-in templates:"),
                PlotTemplateSource::Local => println!("Local templates:"),
            }
        }
        match &template.path {
            Some(path) => println!(
                "  {:<24} {} ({})",
                template.name,
                template.description,
                path.display()
            ),
            None => println!("  {:<24} {}", template.name, template.description),
        }
    }
}

fn plot_template_spec(repo_root: &Path, template: &str) -> Result<PlotTemplateSpecPayload> {
    if let Some(path) = resolve_custom_plot_template_path(repo_root, template) {
        let spec = read_plot_template_path(&path)?;
        return Ok(PlotTemplateSpecPayload {
            name: template_name_from_path(template, &path),
            source: PlotTemplateSource::Local,
            path: Some(path),
            spec,
        });
    }
    if let Some(spec) = builtin_plot_template_spec(template) {
        return Ok(PlotTemplateSpecPayload {
            name: template.to_owned(),
            source: PlotTemplateSource::Builtin,
            path: None,
            spec,
        });
    }
    Err(CrabError::Configuration {
        key: template.to_owned(),
        origin: "plot template not found in built-ins, .crab/plots, or .dvc/plots".to_owned(),
    })
}

fn template_name_from_path(template: &str, path: &Path) -> String {
    Path::new(template)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .or_else(|| path.file_stem().and_then(|stem| stem.to_str()))
        .unwrap_or(template)
        .to_owned()
}

fn resolve_plot_template_spec(
    repo_root: &Path,
    template: &str,
) -> Result<Option<serde_json::Value>> {
    if let Some(path) = resolve_custom_plot_template_path(repo_root, template) {
        return read_plot_template_path(&path).map(Some);
    }
    Ok(builtin_plot_template_spec(template))
}

fn read_plot_template_path(path: &Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(path).map_err(CrabError::Io)?;
    serde_json::from_str(&text).map_err(|e| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("plot template JSON parse error: {e}"),
    })
}

fn resolve_custom_plot_template_path(repo_root: &Path, template: &str) -> Option<PathBuf> {
    let template_path = Path::new(template);
    let mut candidates = Vec::new();
    if template_path.is_absolute() {
        candidates.push(template_path.to_path_buf());
    } else {
        candidates.push(repo_root.join(template_path));
        candidates.push(repo_root.join(".crab").join("plots").join(template_path));
        candidates.push(repo_root.join(".dvc").join("plots").join(template_path));
        if template_path.extension().is_none() {
            let json_name = format!("{template}.json");
            candidates.push(repo_root.join(".crab").join("plots").join(&json_name));
            candidates.push(repo_root.join(".dvc").join("plots").join(json_name));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn builtin_plot_template_spec(template: &str) -> Option<serde_json::Value> {
    let spec = match template {
        "linear" => serde_json::json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "title": "<DVC_METRIC_TITLE>",
            "data": {"values": "<DVC_METRIC_DATA>"},
            "mark": {"type": "line", "point": true, "tooltip": true},
            "encoding": {
                "x": {"field": "<DVC_METRIC_X>", "type": "quantitative", "title": "<DVC_METRIC_X_LABEL>"},
                "y": {"field": "<DVC_METRIC_Y>", "type": "quantitative", "title": "<DVC_METRIC_Y_LABEL>"},
                "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
            }
        }),
        "simple" => serde_json::json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "title": "<DVC_METRIC_TITLE>",
            "data": {"values": "<DVC_METRIC_DATA>"},
            "mark": "line",
            "encoding": {
                "x": {"field": "<DVC_METRIC_X>", "type": "quantitative"},
                "y": {"field": "<DVC_METRIC_Y>", "type": "quantitative"}
            }
        }),
        "scatter" => serde_json::json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "title": "<DVC_METRIC_TITLE>",
            "data": {"values": "<DVC_METRIC_DATA>"},
            "mark": {"type": "point", "tooltip": true},
            "encoding": {
                "x": {"field": "<DVC_METRIC_X>", "type": "quantitative", "title": "<DVC_METRIC_X_LABEL>"},
                "y": {"field": "<DVC_METRIC_Y>", "type": "quantitative", "title": "<DVC_METRIC_Y_LABEL>"},
                "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
            }
        }),
        "smooth" => serde_json::json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "title": "<DVC_METRIC_TITLE>",
            "data": {"values": "<DVC_METRIC_DATA>"},
            "transform": [{"loess": "<DVC_METRIC_Y>", "on": "<DVC_METRIC_X>"}],
            "mark": {"type": "line", "tooltip": true},
            "encoding": {
                "x": {"field": "<DVC_METRIC_X>", "type": "quantitative", "title": "<DVC_METRIC_X_LABEL>"},
                "y": {"field": "<DVC_METRIC_Y>", "type": "quantitative", "title": "<DVC_METRIC_Y_LABEL>"},
                "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
            }
        }),
        "confusion" => confusion_template_spec(false),
        "confusion_normalized" => confusion_template_spec(true),
        "bar_horizontal" => bar_template_spec(false),
        "bar_horizontal_sorted" => bar_template_spec(true),
        _ => return None,
    };
    Some(spec)
}

fn confusion_template_spec(normalized: bool) -> serde_json::Value {
    if normalized {
        return serde_json::json!({
            "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
            "title": "<DVC_METRIC_TITLE> (normalized)",
            "data": {"values": "<DVC_METRIC_DATA>"},
            "transform": [
                {
                    "aggregate": [{"op": "count", "as": "count"}],
                    "groupby": ["<DVC_METRIC_X>", "<DVC_METRIC_Y>"]
                },
                {
                    "joinaggregate": [{"op": "sum", "field": "count", "as": "total"}]
                },
                {
                    "calculate": "datum.count / datum.total",
                    "as": "normalized"
                }
            ],
            "mark": {"type": "rect", "tooltip": true},
            "encoding": {
                "x": {"field": "<DVC_METRIC_X>", "type": "nominal", "title": "<DVC_METRIC_X_LABEL>"},
                "y": {"field": "<DVC_METRIC_Y>", "type": "nominal", "title": "<DVC_METRIC_Y_LABEL>"},
                "color": {"field": "normalized", "type": "quantitative", "title": "normalized"}
            }
        });
    }
    serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "title": "<DVC_METRIC_TITLE>",
        "data": {"values": "<DVC_METRIC_DATA>"},
        "mark": {"type": "rect", "tooltip": true},
        "encoding": {
            "x": {"field": "<DVC_METRIC_X>", "type": "nominal", "title": "<DVC_METRIC_X_LABEL>"},
            "y": {"field": "<DVC_METRIC_Y>", "type": "nominal", "title": "<DVC_METRIC_Y_LABEL>"},
            "color": {"aggregate": "count", "type": "quantitative", "title": "count"}
        }
    })
}

fn bar_template_spec(sorted: bool) -> serde_json::Value {
    let sort = if sorted {
        serde_json::json!("-x")
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "title": "<DVC_METRIC_TITLE>",
        "data": {"values": "<DVC_METRIC_DATA>"},
        "mark": {"type": "bar", "tooltip": true},
        "encoding": {
            "x": {"field": "<DVC_METRIC_Y>", "type": "quantitative", "title": "<DVC_METRIC_Y_LABEL>"},
            "y": {
                "field": "<DVC_METRIC_X>",
                "type": "nominal",
                "title": "<DVC_METRIC_X_LABEL>",
                "sort": sort
            },
            "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
        }
    })
}

struct PlotTemplateValues<'a> {
    data: serde_json::Value,
    title: &'a str,
    x_field: &'a str,
    y_field: &'a str,
    x_label: &'a str,
    y_label: &'a str,
    color_field: &'a str,
}

fn apply_plot_template_anchors(
    value: serde_json::Value,
    replacements: &PlotTemplateValues<'_>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => replace_plot_template_string(&text, replacements),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|item| apply_plot_template_anchors(item, replacements))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, apply_plot_template_anchors(value, replacements)))
                .collect(),
        ),
        other => other,
    }
}

fn replace_plot_template_string(
    text: &str,
    replacements: &PlotTemplateValues<'_>,
) -> serde_json::Value {
    if text == "<DVC_METRIC_DATA>" {
        return replacements.data.clone();
    }
    let replaced = text
        .replace("<DVC_METRIC_TITLE>", replacements.title)
        .replace("<DVC_METRIC_X>", replacements.x_field)
        .replace("<DVC_METRIC_Y>", replacements.y_field)
        .replace("<DVC_METRIC_X_LABEL>", replacements.x_label)
        .replace("<DVC_METRIC_Y_LABEL>", replacements.y_label)
        .replace("<DVC_METRIC_COLOR>", replacements.color_field)
        .replace("<DVC_METRIC_PLOT_HEIGHT>", "300")
        .replace("<DVC_METRIC_PLOT_WIDTH>", "700");
    serde_json::Value::String(replaced)
}

fn plot_data_values(
    x_field: &str,
    data: &PlotData,
    revision: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    for (step, row) in data.rows.iter().enumerate() {
        for (idx, field) in data.y_fields.iter().enumerate() {
            let Some(value) = row.y_values.get(idx) else {
                continue;
            };
            let mut object = serde_json::Map::new();
            if x_field == "step" {
                object.insert("step".to_owned(), plot_scalar(&row.x));
            } else {
                object.insert("step".to_owned(), serde_json::Value::Number(step.into()));
                object.insert(x_field.to_owned(), plot_scalar(&row.x));
            }
            object.insert(
                "metric".to_owned(),
                serde_json::Value::String(field.clone()),
            );
            object.insert("value".to_owned(), plot_scalar(value));
            if let Some(revision) = revision {
                object.insert(
                    "revision".to_owned(),
                    serde_json::Value::String(revision.to_owned()),
                );
                object.insert(
                    "series".to_owned(),
                    serde_json::Value::String(format!("{revision}: {field}")),
                );
            } else {
                object.insert(
                    "series".to_owned(),
                    serde_json::Value::String(field.clone()),
                );
            }
            values.push(serde_json::Value::Object(object));
        }
    }
    values
}

fn plot_template_data_values(
    x_field: &str,
    data: &PlotData,
    revision: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    for (step, row) in data.rows.iter().enumerate() {
        let mut object = serde_json::Map::new();
        object.insert("step".to_owned(), serde_json::Value::Number(step.into()));
        object.insert(x_field.to_owned(), plot_scalar(&row.x));
        for (idx, field) in data.y_fields.iter().enumerate() {
            let Some(value) = row.y_values.get(idx) else {
                continue;
            };
            object.insert(field.clone(), plot_scalar(value));
        }
        if data.y_fields.len() > 1 {
            object.insert(
                "metric".to_owned(),
                serde_json::Value::String(data.y_fields.join(",")),
            );
        }
        if let Some(revision) = revision {
            object.insert(
                "rev".to_owned(),
                serde_json::Value::String(revision.to_owned()),
            );
        }
        values.push(serde_json::Value::Object(object));
    }
    values
}

fn plot_scalar(raw: &str) -> serde_json::Value {
    if let Ok(i) = raw.parse::<i64>() {
        return serde_json::Value::Number(i.into());
    }
    if let Ok(f) = raw.parse::<f64>()
        && f.is_finite()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return serde_json::Value::Number(n);
    }
    serde_json::Value::String(raw.to_owned())
}

fn infer_vega_type(values: &[serde_json::Value], field: &str) -> &'static str {
    if values
        .iter()
        .all(|value| value.get(field).is_some_and(serde_json::Value::is_number))
    {
        "quantitative"
    } else {
        "nominal"
    }
}

fn render_plot_tables_for_config(config: &PlotConfig, repo_root: &Path) -> Result<String> {
    let sources = collect_working_plot_sources(repo_root, config)?;
    let mut out = String::new();
    for source in data_sources(&sources) {
        let source_config = plot_config_for_source(config, &source.path);
        out.push_str(&render_plot_data_table(&source_config, repo_root));
    }

    let images = read_working_image_items(repo_root, &sources, None)?;
    if !images.is_empty() {
        out.push_str(&render_plot_image_table(
            config,
            None,
            &images,
            config.path.as_path(),
        ));
    }
    Ok(out)
}

fn render_plot_diff_tables_for_config(
    config: &PlotConfig,
    repo_root: &Path,
    git_dir: &Path,
    baseline: &str,
    target: &str,
) -> Result<String> {
    let baseline_sources = collect_plot_sources_at_ref(repo_root, git_dir, baseline, config)?;
    let target_sources = collect_plot_sources_at_ref(repo_root, git_dir, target, config)?;
    let mut out = String::new();

    for source_path in diff_data_paths(&baseline_sources, &target_sources) {
        let source_config = plot_config_for_source(config, &source_path);
        let baseline_data = read_plot_data_at_ref(repo_root, git_dir, baseline, &source_config)?;
        let target_data = read_plot_data_at_ref(repo_root, git_dir, target, &source_config)?;
        out.push_str(&render_plot_diff_table(
            &source_config,
            baseline,
            target,
            &baseline_data,
            &target_data,
        ));
    }

    let mut images = read_image_items_at_ref(
        repo_root,
        git_dir,
        baseline,
        &baseline_sources,
        Some(baseline),
    )?;
    images.extend(read_image_items_at_ref(
        repo_root,
        git_dir,
        target,
        &target_sources,
        Some(target),
    )?);
    if !images.is_empty() {
        out.push_str(&render_plot_image_table(
            config,
            Some((baseline, target)),
            &images,
            config.path.as_path(),
        ));
    }

    Ok(out)
}

fn render_plot_revision_tables_for_config(
    config: &PlotConfig,
    repo_root: &Path,
    git_dir: &Path,
    revisions: &[String],
) -> Result<String> {
    let revision_sources = collect_revision_sources(repo_root, git_dir, revisions, config)?;
    let mut out = String::new();

    for source_path in data_paths_for_source_sets(&revision_sources) {
        let source_config = plot_config_for_source(config, &source_path);
        let revision_data = read_revision_plot_data(repo_root, git_dir, revisions, &source_config)?;
        out.push_str(&render_plot_revision_table(&source_config, &revision_data));
    }

    let mut images = Vec::new();
    for (revision, sources) in revisions.iter().zip(&revision_sources) {
        images.extend(read_image_items_at_ref(
            repo_root,
            git_dir,
            revision,
            sources,
            Some(revision),
        )?);
    }
    if !images.is_empty() {
        out.push_str(&render_plot_revision_image_table(
            config,
            revisions,
            &images,
            config.path.as_path(),
        ));
    }

    Ok(out)
}

fn render_plot_header(config: &PlotConfig, comparison: Option<(&str, &str)>) -> String {
    let mut out = String::new();
    let title = config
        .title
        .as_deref()
        .unwrap_or_else(|| config.path.to_str().unwrap_or("plot"));
    if let Some((baseline, target)) = comparison {
        let _ = writeln!(out, "── {title} ({baseline} vs {target}) ──");
    } else {
        let _ = writeln!(out, "── {title} ──");
    }
    let _ = writeln!(out, "  Source: {}", config.path.display());
    if let Some(ref x) = config.x {
        let _ = writeln!(out, "  X-axis: {x}");
    }
    if !config.y.is_empty() {
        let _ = writeln!(out, "  Y-axis: {}", config.y.join(", "));
    }
    if let Some(ref tmpl) = config.template {
        let _ = writeln!(out, "  Template: {tmpl}");
    }
    out
}

fn render_plot_revision_header(config: &PlotConfig, revisions: &[String]) -> String {
    let mut out = String::new();
    let title = config
        .title
        .as_deref()
        .unwrap_or_else(|| config.path.to_str().unwrap_or("plot"));
    let _ = writeln!(out, "── {title} ({}) ──", revision_summary(revisions));
    let _ = writeln!(out, "  Source: {}", config.path.display());
    if let Some(ref x) = config.x {
        let _ = writeln!(out, "  X-axis: {x}");
    }
    if !config.y.is_empty() {
        let _ = writeln!(out, "  Y-axis: {}", config.y.join(", "));
    }
    if let Some(ref tmpl) = config.template {
        let _ = writeln!(out, "  Template: {tmpl}");
    }
    out
}

fn render_plot_data_table(config: &PlotConfig, repo_root: &Path) -> String {
    let mut out = render_plot_header(config, None);
    let data = match read_working_plot_data(repo_root, config) {
        Ok(data) if !data.rows.is_empty() => data,
        Ok(_) => {
            out.push_str("  (no data)\n");
            return out;
        }
        Err(e) => {
            let _ = writeln!(out, "  ({e})");
            return out;
        }
    };

    let x_col = config.x.as_deref().unwrap_or("index");
    append_plot_data_table(&mut out, x_col, &data);

    out
}

fn render_plot_revision_table(config: &PlotConfig, revision_data: &[PlotRevisionData]) -> String {
    let mut out = String::new();
    let title = config
        .title
        .as_deref()
        .unwrap_or_else(|| config.path.to_str().unwrap_or("plot"));
    let _ = writeln!(
        out,
        "── {title} ({}) ──",
        revision_data_summary(revision_data)
    );
    let _ = writeln!(out, "  Source: {}", config.path.display());
    if let Some(ref x) = config.x {
        let _ = writeln!(out, "  X-axis: {x}");
    }
    let x_col = config.x.as_deref().unwrap_or("index");
    for item in revision_data {
        let _ = writeln!(out, "  {}", item.revision);
        append_plot_data_table(&mut out, x_col, &item.data);
    }
    out
}

fn render_plot_diff_table(
    config: &PlotConfig,
    baseline: &str,
    target: &str,
    baseline_data: &PlotData,
    target_data: &PlotData,
) -> String {
    let mut out = String::new();
    let title = config
        .title
        .as_deref()
        .unwrap_or_else(|| config.path.to_str().unwrap_or("plot"));
    let _ = writeln!(out, "── {title} ({baseline} vs {target}) ──");
    let _ = writeln!(out, "  Source: {}", config.path.display());
    if let Some(ref x) = config.x {
        let _ = writeln!(out, "  X-axis: {x}");
    }
    let x_col = config.x.as_deref().unwrap_or("index");
    let _ = writeln!(out, "  {baseline}");
    append_plot_data_table(&mut out, x_col, baseline_data);
    let _ = writeln!(out, "  {target}");
    append_plot_data_table(&mut out, x_col, target_data);
    out
}

fn render_plot_revision_image_table(
    config: &PlotConfig,
    revisions: &[String],
    images: &[PlotImageItem],
    source_path: &Path,
) -> String {
    let mut out = render_plot_revision_header(config, revisions);
    if source_path != config.path.as_path() {
        let _ = writeln!(out, "  Image source: {}", source_path.display());
    }
    if images.is_empty() {
        out.push_str("  (no images)\n");
        return out;
    }
    for image in images {
        let prefix = image
            .label
            .as_deref()
            .map(|label| format!("{label}: "))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  Image: {prefix}{} ({}, {} bytes)",
            image.path, image.mime_type, image.bytes
        );
    }
    out
}

fn render_plot_image_table(
    config: &PlotConfig,
    comparison: Option<(&str, &str)>,
    images: &[PlotImageItem],
    source_path: &Path,
) -> String {
    let mut out = render_plot_header(config, comparison);
    if source_path != config.path.as_path() {
        let _ = writeln!(out, "  Image source: {}", source_path.display());
    }
    if images.is_empty() {
        out.push_str("  (no images)\n");
        return out;
    }
    for image in images {
        let prefix = image
            .label
            .as_deref()
            .map(|label| format!("{label}: "))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  Image: {prefix}{} ({}, {} bytes)",
            image.path, image.mime_type, image.bytes
        );
    }
    out
}

fn revision_summary(revisions: &[String]) -> String {
    revisions
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" vs ")
}

fn revision_data_summary(revision_data: &[PlotRevisionData]) -> String {
    revision_data
        .iter()
        .map(|item| item.revision.as_str())
        .collect::<Vec<_>>()
        .join(" vs ")
}

fn append_plot_data_table(out: &mut String, x_col: &str, data: &PlotData) {
    if data.rows.is_empty() {
        out.push_str("  (no data)\n");
        return;
    }
    let mut header = format!("  {x_col:<12}");
    for y in &data.y_fields {
        let _ = write!(header, "{y:<12}");
    }
    out.push_str(&header);
    out.push('\n');

    for row in data.rows.iter().take(20) {
        let mut line = format!("  {:<12}", row.x);
        for val in &row.y_values {
            let _ = write!(line, "{val:<12}");
        }
        out.push_str(&line);
        out.push('\n');
    }
    if data.rows.len() > 20 {
        let _ = writeln!(out, "  ... ({} more rows)", data.rows.len() - 20);
    }
}

#[derive(Debug, Default, Clone, Serialize)]
struct PlotData {
    y_fields: Vec<String>,
    rows: Vec<PlotRow>,
}

#[derive(Debug, Clone, Serialize)]
struct PlotRow {
    x: String,
    y_values: Vec<String>,
}

fn read_plot_data(path: &Path, config: &PlotConfig) -> Result<PlotData> {
    if !path.exists() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "plot data file not found".to_owned(),
        });
    }
    let bytes = std::fs::read(path).map_err(CrabError::Io)?;
    parse_plot_data_bytes(path, &bytes, config)
}

fn read_working_plot_data(repo_root: &Path, config: &PlotConfig) -> Result<PlotData> {
    let data = read_plot_data(&repo_root.join(&config.path), config)?;
    overlay_working_x_data(repo_root, config, data)
}

fn read_plot_data_at_ref(
    repo_root: &Path,
    git_dir: &Path,
    ref_name: &str,
    config: &PlotConfig,
) -> Result<PlotData> {
    let bytes = read_plot_bytes_at_ref(repo_root, git_dir, ref_name, &config.path)?;
    let data = parse_plot_data_bytes(&config.path, &bytes, config)?;
    overlay_x_data_at_ref(repo_root, git_dir, ref_name, config, data)
}

fn read_plot_bytes_at_ref(
    repo_root: &Path,
    git_dir: &Path,
    ref_name: &str,
    path: &Path,
) -> Result<Vec<u8>> {
    if ref_name == WORKSPACE_REF {
        let working = repo_root.join(path);
        if working.is_file() {
            return std::fs::read(working).map_err(CrabError::Io);
        }
        return Err(CrabError::StageDepMissing {
            stage: "plots".to_owned(),
            path: path.to_path_buf(),
        });
    }

    Ok(match params::read_blob_at_ref(git_dir, ref_name, path)? {
        Some(bytes) => bytes,
        None if ref_name == "HEAD" => {
            let working = repo_root.join(path);
            if working.is_file() {
                std::fs::read(&working).map_err(CrabError::Io)?
            } else {
                return Err(CrabError::StageDepMissing {
                    stage: "plots".to_owned(),
                    path: path.to_path_buf(),
                });
            }
        }
        None => {
            return Err(CrabError::StageDepMissing {
                stage: "plots".to_owned(),
                path: path.to_path_buf(),
            });
        }
    })
}

fn overlay_working_x_data(
    repo_root: &Path,
    config: &PlotConfig,
    data: PlotData,
) -> Result<PlotData> {
    let Some(x_path) = &config.x_path else {
        return Ok(data);
    };
    let x_config = x_source_config(config, x_path);
    let x_data = read_plot_data(&repo_root.join(x_path), &x_config)?;
    overlay_x_rows(config, data, x_data)
}

fn overlay_x_data_at_ref(
    repo_root: &Path,
    git_dir: &Path,
    ref_name: &str,
    config: &PlotConfig,
    data: PlotData,
) -> Result<PlotData> {
    let Some(x_path) = &config.x_path else {
        return Ok(data);
    };
    let x_config = x_source_config(config, x_path);
    let bytes = read_plot_bytes_at_ref(repo_root, git_dir, ref_name, x_path)?;
    let x_data = parse_plot_data_bytes(x_path, &bytes, &x_config)?;
    overlay_x_rows(config, data, x_data)
}

fn x_source_config(config: &PlotConfig, x_path: &Path) -> PlotConfig {
    let mut x_config = config.clone();
    x_config.path = x_path.to_path_buf();
    x_config.x_path = None;
    x_config.y.clear();
    x_config
}

fn overlay_x_rows(config: &PlotConfig, mut data: PlotData, x_data: PlotData) -> Result<PlotData> {
    if data.rows.len() != x_data.rows.len() {
        return Err(CrabError::Configuration {
            key: config.path.display().to_string(),
            origin: format!(
                "plot x source {} has {} rows but y source has {} rows",
                config
                    .x_path
                    .as_deref()
                    .unwrap_or_else(|| Path::new("<none>"))
                    .display(),
                x_data.rows.len(),
                data.rows.len()
            ),
        });
    }
    for (row, x_row) in data.rows.iter_mut().zip(x_data.rows) {
        row.x = x_row.x;
    }
    Ok(data)
}

fn parse_plot_data_bytes(path: &Path, bytes: &[u8], config: &PlotConfig) -> Result<PlotData> {
    let text = std::str::from_utf8(bytes).map_err(|e| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("plot data file is not valid UTF-8: {e}"),
    })?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "csv" => Ok(read_delimited_plot_data(text, config, ',')),
        "tsv" => Ok(read_delimited_plot_data(text, config, '\t')),
        "json" => read_json_plot_data(path, text, config),
        "yaml" | "yml" => read_yaml_plot_data(path, text, config),
        other => Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!(
                "unsupported plot data extension {:?} (expected .csv, .tsv, .json, .yaml, or .yml)",
                if other.is_empty() { "<none>" } else { other }
            ),
        }),
    }
}

fn read_delimited_plot_data(text: &str, config: &PlotConfig, delimiter: char) -> PlotData {
    let raw_lines = text.lines().collect::<Vec<_>>();
    let Some(first_line) = raw_lines.first() else {
        return PlotData::default();
    };
    let first_cols = split_delimited_row(first_line, delimiter);
    let headers: Vec<String> = if config.no_header {
        (0..first_cols.len()).map(|idx| idx.to_string()).collect()
    } else {
        first_cols.iter().map(|col| (*col).to_owned()).collect()
    };

    let y_fields = effective_y_fields(&headers, config);

    let x_col = config.x.as_deref().unwrap_or("");
    let x_idx = headers.iter().position(|h| h == x_col);
    let y_indices: Vec<Option<usize>> = config
        .y
        .iter()
        .map(|y| headers.iter().position(|h| h == y))
        .collect();
    let y_indices = if y_indices.is_empty() {
        y_fields
            .iter()
            .map(|y| headers.iter().position(|h| h == y))
            .collect()
    } else {
        y_indices
    };

    let mut rows = Vec::new();
    let data_lines: &[&str] = if config.no_header {
        &raw_lines
    } else {
        &raw_lines[1..]
    };
    for (i, line) in data_lines.iter().enumerate() {
        let cols = split_delimited_row(line, delimiter);
        let x_val = x_idx.and_then(|idx| cols.get(idx).copied()).unwrap_or("");
        let x_display = if x_val.is_empty() {
            i.to_string()
        } else {
            x_val.to_owned()
        };
        let y_vals: Vec<String> = y_indices
            .iter()
            .map(|opt_idx| {
                opt_idx
                    .and_then(|idx| cols.get(idx).copied())
                    .unwrap_or("-")
                    .to_owned()
            })
            .collect();
        rows.push(PlotRow {
            x: x_display,
            y_values: y_vals,
        });
    }
    PlotData { y_fields, rows }
}

fn split_delimited_row(line: &str, delimiter: char) -> Vec<&str> {
    line.split(delimiter).map(str::trim).collect()
}

fn plot_json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn read_json_plot_data(path: &Path, text: &str, config: &PlotConfig) -> Result<PlotData> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("json plot parse error: {e}"),
        })?;
    read_object_array_plot_data(path, config, &parsed)
}

fn read_yaml_plot_data(path: &Path, text: &str, config: &PlotConfig) -> Result<PlotData> {
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| CrabError::Configuration {
            key: path.display().to_string(),
            origin: format!("yaml plot parse error: {e}"),
        })?;
    let json = serde_json::to_value(parsed).map_err(|e| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("yaml plot normalization error: {e}"),
    })?;
    read_object_array_plot_data(path, config, &json)
}

fn read_object_array_plot_data(
    path: &Path,
    config: &PlotConfig,
    parsed: &serde_json::Value,
) -> Result<PlotData> {
    let arr = find_first_object_array(parsed).ok_or_else(|| CrabError::Configuration {
        key: path.display().to_string(),
        origin: "plot JSON/YAML data must contain an array of objects".to_owned(),
    })?;

    let headers = arr
        .iter()
        .find_map(|obj| obj.as_object())
        .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let y_fields = effective_y_fields(&headers, config);
    let x_col = config.x.as_deref().unwrap_or("");
    let mut rows = Vec::new();
    for (i, obj) in arr.iter().enumerate() {
        let x_val = if x_col.is_empty() {
            i.to_string()
        } else {
            obj.get(x_col)
                .map_or_else(|| i.to_string(), plot_json_value_to_string)
        };
        let y_vals: Vec<String> = y_fields
            .iter()
            .map(|y_key| {
                obj.get(y_key.as_str())
                    .map_or_else(|| "-".to_owned(), plot_json_value_to_string)
            })
            .collect();
        rows.push(PlotRow {
            x: x_val,
            y_values: y_vals,
        });
    }
    Ok(PlotData { y_fields, rows })
}

fn find_first_object_array(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    match value {
        serde_json::Value::Array(items) => {
            if items.iter().any(serde_json::Value::is_object) {
                return Some(items);
            }
            items.iter().find_map(find_first_object_array)
        }
        serde_json::Value::Object(map) => map.values().find_map(find_first_object_array),
        _ => None,
    }
}

fn effective_y_fields(headers: &[String], config: &PlotConfig) -> Vec<String> {
    if !config.y.is_empty() {
        return config.y.clone();
    }
    let x = config.x.as_deref();
    headers
        .iter()
        .rev()
        .find(|field| Some(field.as_str()) != x)
        .cloned()
        .into_iter()
        .collect()
}

/// Pick default metrics paths when the caller didn't pass any.
fn default_paths(repo_root: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths.to_vec());
    }

    let yaml_path = repo_root.join("crab.yaml");
    if yaml_path.is_file() {
        let text = std::fs::read_to_string(&yaml_path).map_err(CrabError::Io)?;
        let workflow = yaml::parse_at(&yaml_path, &text)?;
        let mut seen = BTreeSet::new();
        let mut declared = Vec::new();
        for path in workflow.metrics {
            if seen.insert(path.clone()) {
                declared.push(path);
            }
        }
        for stage in workflow.stages.values() {
            for metric in &stage.metrics {
                let path = stage_scoped_metric_path(stage.wdir.as_deref(), metric);
                if seen.insert(path.clone()) {
                    declared.push(path);
                }
            }
        }
        if !declared.is_empty() {
            return Ok(declared);
        }
    }

    // `metrics.json` is the conventional metrics file. Users with
    // multiple metrics files pass `--paths` explicitly.
    Ok(vec![PathBuf::from("metrics.json")])
}

fn metric_paths_for_diff(
    repo_root: &Path,
    ref_a: &str,
    ref_b: &str,
    paths: &[PathBuf],
    recursive: bool,
) -> Result<Vec<PathBuf>> {
    let candidates = default_paths(repo_root, paths)?;
    if !recursive {
        return Ok(candidates);
    }

    let git_dir = params::find_git_dir(repo_root)?;
    let mut expanded = BTreeSet::new();
    for target in candidates {
        if is_supported_metric_file(&target) {
            expanded.insert(target.clone());
        }
        collect_working_metric_paths(repo_root, &target, &mut expanded)?;
        for ref_name in [ref_a, ref_b] {
            if ref_name != WORKSPACE_REF {
                collect_metric_paths_at_ref(&git_dir, ref_name, &target, &mut expanded)?;
            }
        }
    }
    Ok(expanded.into_iter().collect())
}

fn show_revisions(repo_root: &Path, args: &ShowArgs) -> Result<Vec<String>> {
    if !args.uses_history() {
        return Ok(vec![args.git_ref.clone()]);
    }
    if args.git_ref != WORKSPACE_REF {
        return Err(CrabError::Configuration {
            key: "metrics show --ref".to_owned(),
            origin: "--ref cannot be combined with --all-branches, --all-tags, or --all-commits"
                .to_owned(),
        });
    }

    let mut seen = BTreeSet::new();
    let mut refs = Vec::new();
    push_unique_ref(&mut refs, &mut seen, WORKSPACE_REF.to_owned());
    if args.all_branches {
        for ref_name in collect_git_ref_names(repo_root, "refs/heads")? {
            push_unique_ref(&mut refs, &mut seen, ref_name);
        }
    }
    if args.all_tags {
        for ref_name in collect_git_ref_names(repo_root, "refs/tags")? {
            push_unique_ref(&mut refs, &mut seen, ref_name);
        }
    }
    if args.all_commits {
        for ref_name in collect_git_commit_ids(repo_root)? {
            push_unique_ref(&mut refs, &mut seen, ref_name);
        }
    }
    Ok(refs)
}

fn push_unique_ref(refs: &mut Vec<String>, seen: &mut BTreeSet<String>, ref_name: String) {
    if seen.insert(ref_name.clone()) {
        refs.push(ref_name);
    }
}

fn collect_git_ref_names(repo_root: &Path, namespace: &str) -> Result<Vec<String>> {
    git_lines(
        repo_root,
        &["for-each-ref", "--format=%(refname:short)", namespace],
        "git for-each-ref",
    )
}

fn collect_git_commit_ids(repo_root: &Path) -> Result<Vec<String>> {
    git_lines(repo_root, &["rev-list", "--all"], "git rev-list")
}

fn git_lines(repo_root: &Path, args: &[&str], label: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn {label}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "{label} failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn metric_paths_for_show(
    repo_root: &Path,
    refs: &[String],
    paths: &[PathBuf],
    recursive: bool,
) -> Result<Vec<PathBuf>> {
    let candidates = default_paths(repo_root, paths)?;
    if !recursive {
        return Ok(candidates);
    }

    let needs_git = refs.iter().any(|ref_name| ref_name != WORKSPACE_REF);
    let git_dir = if needs_git {
        Some(params::find_git_dir(repo_root)?)
    } else {
        None
    };
    let mut expanded = BTreeSet::new();
    for target in candidates {
        if is_supported_metric_file(&target) {
            expanded.insert(target.clone());
        }
        collect_working_metric_paths(repo_root, &target, &mut expanded)?;
        if let Some(git_dir) = git_dir.as_deref() {
            for ref_name in refs {
                if ref_name != WORKSPACE_REF {
                    collect_metric_paths_at_ref(git_dir, ref_name, &target, &mut expanded)?;
                }
            }
        }
    }
    Ok(expanded.into_iter().collect())
}

fn collect_working_metric_paths(
    repo_root: &Path,
    target: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let path = repo_root.join(target);
    if path.is_file() {
        if is_supported_metric_file(target) {
            out.insert(target.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }

    let mut entries = fs::read_dir(&path)
        .map_err(CrabError::Io)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(CrabError::Io)?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let entry_path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;
        if file_type.is_dir() {
            let rel = entry_path.strip_prefix(repo_root).unwrap_or(&entry_path);
            collect_working_metric_paths(repo_root, rel, out)?;
        } else if file_type.is_file() {
            let rel = entry_path.strip_prefix(repo_root).unwrap_or(&entry_path);
            if is_supported_metric_file(rel) {
                out.insert(rel.to_path_buf());
            }
        }
    }
    Ok(())
}

fn collect_metric_paths_at_ref(
    git_dir: &Path,
    ref_name: &str,
    target: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let work_dir = git_dir.parent().unwrap_or(Path::new("."));
    let output = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "-z", ref_name, "--"])
        .arg(target)
        .current_dir(work_dir)
        .env("GIT_DIR", git_dir)
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git ls-tree: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git ls-tree failed for ref {ref_name}: {}",
            stderr.trim()
        )));
    }

    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = PathBuf::from(String::from_utf8_lossy(raw).into_owned());
        if is_supported_metric_file(&rel) {
            out.insert(rel);
        }
    }
    Ok(())
}

fn is_supported_metric_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("json" | "yaml" | "yml" | "toml" | "py")
    )
}

fn stage_scoped_metric_path(wdir: Option<&Path>, metric: &Path) -> PathBuf {
    if metric.is_absolute() {
        return metric.to_path_buf();
    }
    match wdir {
        Some(wdir) => wdir.join(metric),
        None => metric.to_path_buf(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricKey {
    path: PathBuf,
    metric: String,
}

type MetricMap = BTreeMap<MetricKey, Scalar>;

#[derive(Debug, Clone, PartialEq)]
struct MetricDiff {
    added: MetricMap,
    removed: MetricMap,
    changed: BTreeMap<MetricKey, (Scalar, Scalar)>,
    unchanged: MetricMap,
}

#[derive(Debug, Clone, Copy)]
struct MetricRenderOptions {
    no_path: bool,
    precision: usize,
    include_unchanged: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct MetricSnapshot {
    revision: String,
    values: MetricMap,
}

#[derive(Debug, Serialize)]
struct MetricsShow {
    targets: Vec<PathBuf>,
    revisions: Vec<MetricsShowRevision>,
}

#[derive(Debug, Serialize)]
struct MetricsShowRevision {
    revision: String,
    values: BTreeMap<String, BTreeMap<String, ScalarJson>>,
}

#[derive(Debug, Serialize)]
struct MetricsDiff {
    ref_a: String,
    ref_b: String,
    added: BTreeMap<String, BTreeMap<String, ScalarJson>>,
    removed: BTreeMap<String, BTreeMap<String, ScalarJson>>,
    changed: BTreeMap<String, BTreeMap<String, ChangedEntry>>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    unchanged: BTreeMap<String, BTreeMap<String, ScalarJson>>,
}

fn read_metric_map_at_ref(
    repo_root: &Path,
    ref_name: &str,
    paths: &[PathBuf],
) -> Result<MetricMap> {
    let git_dir = if ref_name == WORKSPACE_REF {
        None
    } else {
        Some(params::find_git_dir(repo_root)?)
    };
    let mut merged = MetricMap::new();
    for path in paths {
        let Some(values) = read_metric_file_at_ref(repo_root, git_dir.as_deref(), ref_name, path)?
        else {
            continue;
        };
        for (metric, value) in values {
            merged.insert(
                MetricKey {
                    path: path.clone(),
                    metric,
                },
                value,
            );
        }
    }
    Ok(merged)
}

fn read_metric_file_at_ref(
    repo_root: &Path,
    git_dir: Option<&Path>,
    ref_name: &str,
    path: &Path,
) -> Result<Option<ScalarMap>> {
    let bytes = if ref_name == WORKSPACE_REF {
        let working = repo_root.join(path);
        if working.is_file() {
            Some(std::fs::read(&working).map_err(CrabError::Io)?)
        } else {
            None
        }
    } else {
        let git_dir = git_dir.ok_or_else(|| {
            CrabError::Internal("missing git directory for metrics diff".to_owned())
        })?;
        match params::read_blob_at_ref(git_dir, ref_name, path)? {
            Some(bytes) => Some(bytes),
            None if ref_name == "HEAD" && repo_root.join(path).is_file() => {
                Some(std::fs::read(repo_root.join(path)).map_err(CrabError::Io)?)
            }
            None => None,
        }
    };
    bytes
        .map(|bytes| params::parse(&bytes, path))
        .transpose()
        .map_err(Into::into)
}

fn diff_metric_maps(a: &MetricMap, b: &MetricMap) -> MetricDiff {
    let mut added = MetricMap::new();
    let mut removed = MetricMap::new();
    let mut changed = BTreeMap::new();
    let mut unchanged = MetricMap::new();

    for (key, old) in a {
        match b.get(key) {
            Some(new) if old == new => {
                unchanged.insert(key.clone(), old.clone());
            }
            Some(new) => {
                changed.insert(key.clone(), (old.clone(), new.clone()));
            }
            None => {
                removed.insert(key.clone(), old.clone());
            }
        }
    }
    for (key, new) in b {
        if !a.contains_key(key) {
            added.insert(key.clone(), new.clone());
        }
    }

    MetricDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

fn metric_values_to_json(map: &MetricMap) -> BTreeMap<String, BTreeMap<String, ScalarJson>> {
    let mut out = BTreeMap::<String, BTreeMap<String, ScalarJson>>::new();
    for (key, value) in map {
        out.entry(key.path.display().to_string())
            .or_default()
            .insert(key.metric.clone(), value.into());
    }
    out
}

fn metric_snapshots_to_json(snapshots: &[MetricSnapshot]) -> Vec<MetricsShowRevision> {
    snapshots
        .iter()
        .map(|snapshot| MetricsShowRevision {
            revision: snapshot.revision.clone(),
            values: metric_values_to_json(&snapshot.values),
        })
        .collect()
}

fn metric_changes_to_json(
    map: &BTreeMap<MetricKey, (Scalar, Scalar)>,
) -> BTreeMap<String, BTreeMap<String, ChangedEntry>> {
    let mut out = BTreeMap::<String, BTreeMap<String, ChangedEntry>>::new();
    for (key, (old, new)) in map {
        out.entry(key.path.display().to_string())
            .or_default()
            .insert(
                key.metric.clone(),
                ChangedEntry {
                    old: old.into(),
                    new: new.into(),
                },
            );
    }
    out
}

pub fn run_show_in(args: &ShowArgs, repo_root: &Path) -> Result<()> {
    let target_paths = args.target_paths()?;
    let refs = show_revisions(repo_root, args)?;
    let paths = metric_paths_for_show(repo_root, &refs, &target_paths, args.recursive)?;
    let mut snapshots = Vec::new();
    for revision in refs {
        let values = read_metric_map_at_ref(repo_root, &revision, &paths)?;
        snapshots.push(MetricSnapshot { revision, values });
    }
    let mode = OutputMode::from_flags(args.json, false);
    render_show(args, &paths, &snapshots, mode)?;
    Ok(())
}

pub fn run_diff_in(args: &DiffArgs, repo_root: &Path) -> Result<()> {
    let (ref_a, ref_b) = args.comparison_refs()?;
    let paths = metric_paths_for_diff(repo_root, &ref_a, &ref_b, &args.paths, args.recursive)?;
    let a = read_metric_map_at_ref(repo_root, &ref_a, &paths)?;
    let b = read_metric_map_at_ref(repo_root, &ref_b, &paths)?;
    let d = diff_metric_maps(&a, &b);
    let mode = OutputMode::from_flags(args.json, false);
    render_diff(args, &ref_a, &ref_b, &d, mode)?;
    Ok(())
}

fn render_show(
    args: &ShowArgs,
    paths: &[PathBuf],
    snapshots: &[MetricSnapshot],
    mode: OutputMode,
) -> Result<()> {
    let format = args.effective_format()?;
    if mode == OutputMode::Json {
        let payload = MetricsShow {
            targets: paths.to_vec(),
            revisions: metric_snapshots_to_json(snapshots),
        };
        emit_json(SCHEMA_SHOW, SCHEMA_VERSION, payload);
        return Ok(());
    }

    let include_revision = args.uses_history() || snapshots.len() > 1;
    match format {
        Format::Table => print!("{}", render_metric_show_table(snapshots, include_revision)),
        Format::Json => {
            let payload = MetricsShow {
                targets: paths.to_vec(),
                revisions: metric_snapshots_to_json(snapshots),
            };
            emit_json(SCHEMA_SHOW, SCHEMA_VERSION, payload);
        }
        Format::Md => print!(
            "{}",
            render_metric_show_markdown(snapshots, include_revision)
        ),
        Format::PrComment => print!("{}", render_metric_show_pr_comment(snapshots)),
    }
    Ok(())
}

fn render_metric_show_table(snapshots: &[MetricSnapshot], include_revision: bool) -> String {
    let columns = metric_show_columns(snapshots);
    if columns.is_empty() {
        return "_no metrics_\n".to_owned();
    }

    let rows = metric_show_rows(snapshots, &columns, include_revision);
    if rows.is_empty() {
        return "_no metrics_\n".to_owned();
    }

    let mut headers = Vec::with_capacity(columns.len() + 2);
    if include_revision {
        headers.push("revision");
    }
    headers.push("path");
    headers.extend(columns.iter().map(String::as_str));

    let mut out = String::new();
    render_metric_ascii_rows(&headers, &rows, &mut out);
    out
}

fn render_metric_show_markdown(snapshots: &[MetricSnapshot], include_revision: bool) -> String {
    let columns = metric_show_columns(snapshots);
    if columns.is_empty() {
        return "_no metrics_\n".to_owned();
    }
    let rows = metric_show_rows(snapshots, &columns, include_revision);
    if rows.is_empty() {
        return "_no metrics_\n".to_owned();
    }

    let mut headers = Vec::with_capacity(columns.len() + 2);
    if include_revision {
        headers.push("revision".to_owned());
    }
    headers.push("path".to_owned());
    headers.extend(columns.iter().cloned());

    let mut out = String::new();
    let _ = writeln!(
        out,
        "| {} |\n| {} |",
        headers.join(" | "),
        headers
            .iter()
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for row in rows {
        let _ = writeln!(
            out,
            "| {} |",
            row.into_iter()
                .map(|cell| format!("`{cell}`"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    out
}

fn render_metric_show_pr_comment(snapshots: &[MetricSnapshot]) -> String {
    render_metric_show_markdown(snapshots, snapshots.len() > 1)
}

fn metric_show_columns(snapshots: &[MetricSnapshot]) -> Vec<String> {
    let mut columns = BTreeSet::new();
    for snapshot in snapshots {
        for key in snapshot.values.keys() {
            columns.insert(key.metric.clone());
        }
    }
    columns.into_iter().collect()
}

fn metric_show_rows(
    snapshots: &[MetricSnapshot],
    columns: &[String],
    include_revision: bool,
) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for snapshot in snapshots {
        let mut by_path = BTreeMap::<PathBuf, BTreeMap<String, Scalar>>::new();
        for (key, value) in &snapshot.values {
            by_path
                .entry(key.path.clone())
                .or_default()
                .insert(key.metric.clone(), value.clone());
        }
        for (path, values) in by_path {
            let mut row = Vec::with_capacity(columns.len() + 2);
            if include_revision {
                row.push(snapshot.revision.clone());
            }
            row.push(path.display().to_string());
            for column in columns {
                row.push(
                    values
                        .get(column)
                        .map(|value| format_metric_scalar(value, 5))
                        .unwrap_or_else(|| "-".to_owned()),
                );
            }
            rows.push(row);
        }
    }
    rows
}

fn render_diff(
    args: &DiffArgs,
    ref_a: &str,
    ref_b: &str,
    diff: &MetricDiff,
    mode: OutputMode,
) -> Result<()> {
    let format = args.effective_format()?;
    if mode == OutputMode::Json {
        emit_diff_envelope(args, ref_a, ref_b, diff);
        return Ok(());
    }
    let opts = MetricRenderOptions {
        no_path: args.no_path,
        precision: args.precision,
        include_unchanged: args.all,
    };
    match format {
        Format::Table => print!("{}", render_metric_table(diff, ref_a, ref_b, opts)),
        Format::Json => {
            // `--format=json` emits the same envelope as `--json`
            // so callers asking for JSON explicitly get the stable
            // shape.
            emit_diff_envelope(args, ref_a, ref_b, diff);
        }
        Format::Md => print!("{}", render_metric_markdown(diff, ref_a, ref_b, opts)),
        Format::PrComment => print!(
            "{}",
            render_metric_pr_comment(diff, ref_a, ref_b, opts, args.higher_is_better)
        ),
    }
    Ok(())
}

fn emit_diff_envelope(args: &DiffArgs, ref_a: &str, ref_b: &str, diff: &MetricDiff) {
    let payload = MetricsDiff {
        ref_a: ref_a.to_owned(),
        ref_b: ref_b.to_owned(),
        added: metric_values_to_json(&diff.added),
        removed: metric_values_to_json(&diff.removed),
        changed: metric_changes_to_json(&diff.changed),
        unchanged: if args.all {
            metric_values_to_json(&diff.unchanged)
        } else {
            BTreeMap::new()
        },
    };
    emit_json(SCHEMA_DIFF, SCHEMA_VERSION, payload);
}

fn render_metric_table(
    diff: &MetricDiff,
    ref_a: &str,
    ref_b: &str,
    opts: MetricRenderOptions,
) -> String {
    let mut out = String::new();
    out.push_str("=== Added ===\n");
    render_metric_value_table(&diff.added, "value", opts, &mut out);
    out.push('\n');

    out.push_str("=== Removed ===\n");
    render_metric_value_table(&diff.removed, "value", opts, &mut out);
    out.push('\n');

    out.push_str("=== Changed ===\n");
    if diff.changed.is_empty() {
        out.push_str("(none)\n");
    } else {
        let rows = diff
            .changed
            .iter()
            .map(|(key, (old, new))| {
                let (abs_delta, pct_delta) = metric_delta(old, new, opts.precision);
                if opts.no_path {
                    vec![
                        key.metric.clone(),
                        format_metric_scalar(old, opts.precision),
                        format_metric_scalar(new, opts.precision),
                        abs_delta,
                        pct_delta,
                    ]
                } else {
                    vec![
                        key.path.display().to_string(),
                        key.metric.clone(),
                        format_metric_scalar(old, opts.precision),
                        format_metric_scalar(new, opts.precision),
                        abs_delta,
                        pct_delta,
                    ]
                }
            })
            .collect::<Vec<_>>();
        if opts.no_path {
            render_metric_ascii_rows(&["metric", ref_a, ref_b, "abs Δ", "% Δ"], &rows, &mut out);
        } else {
            render_metric_ascii_rows(
                &["path", "metric", ref_a, ref_b, "abs Δ", "% Δ"],
                &rows,
                &mut out,
            );
        }
    }

    if opts.include_unchanged {
        out.push('\n');
        out.push_str("=== Unchanged ===\n");
        render_metric_value_table(&diff.unchanged, "value", opts, &mut out);
    }
    out
}

fn render_metric_value_table(
    values: &MetricMap,
    value_header: &str,
    opts: MetricRenderOptions,
    out: &mut String,
) {
    if values.is_empty() {
        out.push_str("(none)\n");
        return;
    }
    let rows = values
        .iter()
        .map(|(key, value)| {
            if opts.no_path {
                vec![
                    key.metric.clone(),
                    format_metric_scalar(value, opts.precision),
                ]
            } else {
                vec![
                    key.path.display().to_string(),
                    key.metric.clone(),
                    format_metric_scalar(value, opts.precision),
                ]
            }
        })
        .collect::<Vec<_>>();
    if opts.no_path {
        render_metric_ascii_rows(&["metric", value_header], &rows, out);
    } else {
        render_metric_ascii_rows(&["path", "metric", value_header], &rows, out);
    }
}

fn render_metric_markdown(
    diff: &MetricDiff,
    ref_a: &str,
    ref_b: &str,
    opts: MetricRenderOptions,
) -> String {
    let mut out = String::new();
    render_metric_value_markdown("Added", &diff.added, opts, &mut out);
    render_metric_value_markdown("Removed", &diff.removed, opts, &mut out);

    out.push_str("### Changed\n\n");
    if diff.changed.is_empty() {
        out.push_str("_none_\n");
    } else if opts.no_path {
        let _ = writeln!(
            out,
            "| metric | {ref_a} | {ref_b} | Δ | % |\n| --- | --- | --- | --- | --- |"
        );
        for (key, (old, new)) in &diff.changed {
            let (abs_delta, pct_delta) = metric_delta(old, new, opts.precision);
            let _ = writeln!(
                out,
                "| `{}` | `{}` | `{}` | {abs_delta} | {pct_delta} |",
                key.metric,
                format_metric_scalar(old, opts.precision),
                format_metric_scalar(new, opts.precision),
            );
        }
    } else {
        let _ = writeln!(
            out,
            "| path | metric | {ref_a} | {ref_b} | Δ | % |\n| --- | --- | --- | --- | --- | --- |"
        );
        for (key, (old, new)) in &diff.changed {
            let (abs_delta, pct_delta) = metric_delta(old, new, opts.precision);
            let _ = writeln!(
                out,
                "| `{}` | `{}` | `{}` | `{}` | {abs_delta} | {pct_delta} |",
                key.path.display(),
                key.metric,
                format_metric_scalar(old, opts.precision),
                format_metric_scalar(new, opts.precision),
            );
        }
    }
    out.push('\n');

    if opts.include_unchanged {
        render_metric_value_markdown("Unchanged", &diff.unchanged, opts, &mut out);
    }
    out
}

fn render_metric_value_markdown(
    title: &str,
    values: &MetricMap,
    opts: MetricRenderOptions,
    out: &mut String,
) {
    let _ = writeln!(out, "### {title}\n");
    if values.is_empty() {
        out.push_str("_none_\n\n");
        return;
    }
    if opts.no_path {
        out.push_str("| metric | value |\n| --- | --- |\n");
        for (key, value) in values {
            let _ = writeln!(
                out,
                "| `{}` | `{}` |",
                key.metric,
                format_metric_scalar(value, opts.precision)
            );
        }
    } else {
        out.push_str("| path | metric | value |\n| --- | --- | --- |\n");
        for (key, value) in values {
            let _ = writeln!(
                out,
                "| `{}` | `{}` | `{}` |",
                key.path.display(),
                key.metric,
                format_metric_scalar(value, opts.precision)
            );
        }
    }
    out.push('\n');
}

fn render_metric_pr_comment(
    diff: &MetricDiff,
    ref_a: &str,
    ref_b: &str,
    opts: MetricRenderOptions,
    higher_is_better: bool,
) -> String {
    if diff.added.is_empty()
        && diff.removed.is_empty()
        && diff.changed.is_empty()
        && !(opts.include_unchanged && !diff.unchanged.is_empty())
    {
        return "_no changes_\n".to_owned();
    }

    let mut out = String::new();
    if !diff.added.is_empty() {
        out.push_str("**Added**\n\n");
        for (key, value) in &diff.added {
            let _ = writeln!(
                out,
                "- + `{}` = `{}`",
                metric_label(key, opts.no_path),
                format_metric_scalar(value, opts.precision)
            );
        }
        out.push('\n');
    }

    if !diff.removed.is_empty() {
        out.push_str("**Removed**\n\n");
        for (key, value) in &diff.removed {
            let _ = writeln!(
                out,
                "- - `{}` (was `{}`)",
                metric_label(key, opts.no_path),
                format_metric_scalar(value, opts.precision)
            );
        }
        out.push('\n');
    }

    if !diff.changed.is_empty() {
        out.push_str("**Changed**\n\n");
        for (key, (old, new)) in &diff.changed {
            let marker = match (old.as_f64(), new.as_f64()) {
                (Some(a), Some(b)) => {
                    let improved = (b > a) == higher_is_better;
                    if (a - b).abs() < f64::EPSILON {
                        "."
                    } else if improved {
                        "+"
                    } else {
                        "-"
                    }
                }
                _ => ".",
            };
            let (abs_delta, pct_delta) = metric_delta(old, new, opts.precision);
            let _ = writeln!(
                out,
                "- {marker} `{}`: `{}` ({ref_a}) -> `{}` ({ref_b}) ({abs_delta} / {pct_delta})",
                metric_label(key, opts.no_path),
                format_metric_scalar(old, opts.precision),
                format_metric_scalar(new, opts.precision),
            );
        }
    }

    if opts.include_unchanged && !diff.unchanged.is_empty() {
        out.push_str("\n**Unchanged**\n\n");
        for (key, value) in &diff.unchanged {
            let _ = writeln!(
                out,
                "- `{}` = `{}`",
                metric_label(key, opts.no_path),
                format_metric_scalar(value, opts.precision)
            );
        }
    }
    out
}

fn render_metric_ascii_rows(headers: &[&str], rows: &[Vec<String>], out: &mut String) {
    let mut widths = headers.iter().map(|h| h.len()).collect::<Vec<_>>();
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.len());
        }
    }
    for (idx, header) in headers.iter().enumerate() {
        let _ = write!(out, "{header:<width$}", width = widths[idx]);
        if idx + 1 < headers.len() {
            out.push_str("  ");
        }
    }
    out.push('\n');
    for (idx, width) in widths.iter().enumerate() {
        out.push_str(&"-".repeat(*width));
        if idx + 1 < widths.len() {
            out.push_str("  ");
        }
    }
    out.push('\n');
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            let _ = write!(out, "{cell:<width$}", width = widths[idx]);
            if idx + 1 < row.len() {
                out.push_str("  ");
            }
        }
        out.push('\n');
    }
}

fn metric_label(key: &MetricKey, no_path: bool) -> String {
    if no_path {
        return key.metric.clone();
    }
    format!("{}:{}", key.path.display(), key.metric)
}

fn format_metric_scalar(value: &Scalar, precision: usize) -> String {
    match value {
        Scalar::Float(value) => format_metric_float(*value, precision),
        Scalar::Int(value) => value.to_string(),
        _ => value.display(),
    }
}

fn metric_delta(old: &Scalar, new: &Scalar, precision: usize) -> (String, String) {
    match (old.as_f64(), new.as_f64()) {
        (Some(old), Some(new)) => {
            let abs = new - old;
            let sign = if abs >= 0.0 { "+" } else { "" };
            let pct = if old.abs() < f64::EPSILON {
                if abs.abs() < f64::EPSILON {
                    "+0%".to_owned()
                } else if abs > 0.0 {
                    "+inf%".to_owned()
                } else {
                    "-inf%".to_owned()
                }
            } else {
                let pct = abs / old * 100.0;
                let pct_sign = if pct >= 0.0 { "+" } else { "" };
                format!("{pct_sign}{}%", format_metric_float(pct, precision))
            };
            (
                format!("{sign}{}", format_metric_float(abs, precision)),
                pct,
            )
        }
        _ => (String::new(), String::new()),
    }
}

fn format_metric_float(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let mut text = format!("{value:.precision$}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    if text == "-0" { "0".to_owned() } else { text }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use clap::Parser;
    use std::process::Command;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn git_init(repo: &Path) {
        git(repo, &["init", "--initial-branch=main"]);
        git(repo, &["config", "user.email", "t@test.com"]);
        git(repo, &["config", "user.name", "Test"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn plot_args(format: PlotFormat, output: Option<PathBuf>) -> PlotArgs {
        PlotArgs {
            targets: Vec::new(),
            output,
            baseline: None,
            target: None,
            format,
            show_vega: false,
            x: None,
            y: Vec::new(),
            no_header: false,
            title: None,
            x_label: None,
            y_label: None,
            template: None,
            html_template: None,
            open: false,
            json: false,
        }
    }

    fn plot_diff_args(format: PlotFormat, output: Option<PathBuf>) -> PlotDiffArgs {
        PlotDiffArgs {
            targets: Vec::new(),
            revisions: Vec::new(),
            output,
            baseline: None,
            target: None,
            format,
            show_vega: false,
            x: None,
            y: Vec::new(),
            no_header: false,
            title: None,
            x_label: None,
            y_label: None,
            template: None,
            html_template: None,
            open: false,
            json: false,
        }
    }

    #[test]
    fn plot_args_show_vega_selects_vega_format() {
        let mut args = plot_args(PlotFormat::Table, None);
        args.show_vega = true;
        assert_eq!(args.effective_format().unwrap(), PlotFormat::Vega);

        args.html_template = Some(PathBuf::from(".dvc/plots/page.html"));
        let err = args
            .effective_format()
            .expect_err("show-vega conflicts with HTML template");
        assert!(matches!(err, CrabError::Configuration { key, .. } if key == "plots --show-vega"));
        args.html_template = None;

        args.format = PlotFormat::Html;
        let err = args
            .effective_format()
            .expect_err("show-vega conflicts with explicit format");
        assert!(matches!(err, CrabError::Configuration { key, .. } if key == "plots --show-vega"));
    }

    #[test]
    fn plot_args_html_options_select_html_format() {
        let mut args = plot_args(PlotFormat::Table, None);
        args.open = true;
        assert_eq!(args.effective_format().unwrap(), PlotFormat::Html);

        args.open = false;
        args.html_template = Some(PathBuf::from(".dvc/plots/page.html"));
        assert_eq!(args.effective_format().unwrap(), PlotFormat::Html);
    }

    #[test]
    fn plot_args_parse_dvc_option_aliases() {
        let args = PlotArgs::try_parse_from([
            "plot",
            "-t",
            "smooth",
            "-x",
            "epoch",
            "-y",
            "loss",
            "--no-header",
            "--x-label",
            "Epoch",
            "--y-label",
            "Loss",
            "--show-vega",
            "--out",
            "plot.json",
            "metrics/loss.csv",
        ])
        .unwrap();

        assert_eq!(args.template.as_deref(), Some("smooth"));
        assert_eq!(args.x.as_deref(), Some("epoch"));
        assert_eq!(args.y, vec!["loss"]);
        assert!(args.no_header);
        assert_eq!(args.x_label.as_deref(), Some("Epoch"));
        assert_eq!(args.y_label.as_deref(), Some("Loss"));
        assert!(args.show_vega);
        assert_eq!(args.output, Some(PathBuf::from("plot.json")));
        assert_eq!(args.targets, vec![PathBuf::from("metrics/loss.csv")]);
    }

    #[test]
    fn plot_args_parse_dvc_html_template_option() {
        let args = PlotArgs::try_parse_from([
            "plot",
            "--html-template",
            ".dvc/plots/mypage.html",
            "--out",
            "plot.html",
            "metrics/loss.csv",
        ])
        .unwrap();

        assert_eq!(
            args.html_template,
            Some(PathBuf::from(".dvc/plots/mypage.html"))
        );
        assert_eq!(args.output, Some(PathBuf::from("plot.html")));
        assert_eq!(args.targets, vec![PathBuf::from("metrics/loss.csv")]);
    }

    #[test]
    fn metrics_show_args_parse_dvc_targets_history_and_markdown() {
        let args = ShowArgs::try_parse_from([
            "show",
            "-a",
            "-T",
            "-A",
            "-R",
            "--md",
            "metrics",
            "eval.json",
        ])
        .unwrap();

        assert_eq!(
            args.targets,
            vec![PathBuf::from("metrics"), PathBuf::from("eval.json")]
        );
        assert!(args.all_branches);
        assert!(args.all_tags);
        assert!(args.all_commits);
        assert!(args.recursive);
        assert_eq!(args.effective_format().unwrap(), Format::Md);
    }

    #[test]
    fn metrics_show_table_keeps_same_metric_names_scoped_by_path() {
        let mut values = MetricMap::new();
        values.insert(
            MetricKey {
                path: PathBuf::from("metrics/train.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.91),
        );
        values.insert(
            MetricKey {
                path: PathBuf::from("metrics/eval.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.83),
        );
        let snapshot = MetricSnapshot {
            revision: WORKSPACE_REF.to_owned(),
            values,
        };

        let table = render_metric_show_table(&[snapshot], false);
        assert!(table.contains("path"));
        assert!(table.contains("accuracy"));
        assert!(table.contains("metrics/train.json"));
        assert!(table.contains("metrics/eval.json"));
        assert!(table.contains("0.91"));
        assert!(table.contains("0.83"));
    }

    #[test]
    fn metrics_show_history_refs_include_workspace_branches_tags_and_commits() {
        if !git_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::write(repo.join("metrics.json"), br#"{"accuracy": 0.80}"#).unwrap();
        git(repo, &["add", "metrics.json"]);
        git(repo, &["commit", "-m", "main metrics"]);
        git(repo, &["tag", "v1"]);
        git(repo, &["checkout", "-b", "candidate"]);
        std::fs::write(repo.join("metrics.json"), br#"{"accuracy": 0.85}"#).unwrap();
        git(repo, &["commit", "-am", "candidate metrics"]);

        let args = ShowArgs {
            targets: Vec::new(),
            git_ref: WORKSPACE_REF.to_owned(),
            paths: Vec::new(),
            format: Format::Table,
            md: false,
            all_branches: true,
            all_tags: true,
            all_commits: true,
            recursive: false,
            json: false,
            higher_is_better: true,
        };
        let refs = show_revisions(repo, &args).unwrap();

        assert_eq!(refs.first().map(String::as_str), Some(WORKSPACE_REF));
        assert!(refs.iter().any(|ref_name| ref_name == "main"));
        assert!(refs.iter().any(|ref_name| ref_name == "candidate"));
        assert!(refs.iter().any(|ref_name| ref_name == "v1"));
        assert!(refs.iter().any(|ref_name| ref_name.len() == 40));
    }

    #[test]
    fn metrics_diff_args_parse_dvc_targets_and_revisions() {
        let args = DiffArgs::try_parse_from([
            "diff",
            "-R",
            "--targets",
            "metrics/train.json",
            "metrics/eval.json",
            "--",
            "main",
            "candidate",
        ])
        .unwrap();

        assert_eq!(
            args.paths,
            vec![
                PathBuf::from("metrics/train.json"),
                PathBuf::from("metrics/eval.json"),
            ]
        );
        assert!(args.recursive);
        assert_eq!(args.revisions, vec!["main", "candidate"]);
        assert_eq!(
            args.comparison_refs().unwrap(),
            ("main".to_owned(), "candidate".to_owned())
        );
    }

    #[test]
    fn metrics_diff_args_default_to_head_vs_workspace() {
        let args = DiffArgs::try_parse_from(["diff"]).unwrap();
        assert_eq!(
            args.comparison_refs().unwrap(),
            ("HEAD".to_owned(), WORKSPACE_REF.to_owned())
        );

        let args = DiffArgs::try_parse_from(["diff", "main"]).unwrap();
        assert_eq!(
            args.comparison_refs().unwrap(),
            ("main".to_owned(), WORKSPACE_REF.to_owned())
        );
    }

    #[test]
    fn metrics_default_paths_use_declared_workflow_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(
            repo.join("crab.yaml"),
            "metrics:\n  - metrics/top.json\nstages:\n  train:\n    cmd: \"true\"\n    wdir: models\n    metrics:\n      - scores.json\n  eval:\n    cmd: \"true\"\n    metrics:\n      - metrics/top.json\n      - metrics/eval.json\n",
        )
        .unwrap();

        assert_eq!(
            default_paths(repo, &[]).unwrap(),
            vec![
                PathBuf::from("metrics/top.json"),
                PathBuf::from("metrics/eval.json"),
                PathBuf::from("models/scores.json"),
            ]
        );
    }

    #[test]
    fn metrics_recursive_paths_expand_directory_targets() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::create_dir_all(repo.join("metrics/nested")).unwrap();
        std::fs::write(repo.join("metrics/train.json"), br#"{"accuracy": 0.8}"#).unwrap();
        std::fs::write(repo.join("metrics/nested/eval.yaml"), b"accuracy: 0.7\n").unwrap();
        std::fs::write(repo.join("metrics/notes.txt"), b"ignore me\n").unwrap();
        git(repo, &["add", "metrics"]);
        git(repo, &["commit", "-m", "metrics"]);

        let paths = metric_paths_for_diff(
            repo,
            "HEAD",
            WORKSPACE_REF,
            &[PathBuf::from("metrics")],
            true,
        )
        .unwrap();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("metrics/nested/eval.yaml"),
                PathBuf::from("metrics/train.json"),
            ]
        );
    }

    #[test]
    fn metrics_recursive_paths_include_files_removed_from_workspace() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(repo.join("metrics/old.json"), br#"{"accuracy": 0.8}"#).unwrap();
        git(repo, &["add", "metrics/old.json"]);
        git(repo, &["commit", "-m", "base"]);
        std::fs::remove_file(repo.join("metrics/old.json")).unwrap();

        let paths = metric_paths_for_diff(
            repo,
            "HEAD",
            WORKSPACE_REF,
            &[PathBuf::from("metrics")],
            true,
        )
        .unwrap();

        assert_eq!(paths, vec![PathBuf::from("metrics/old.json")]);
    }

    #[test]
    fn metrics_diff_keeps_same_metric_names_scoped_by_path() {
        let mut old = MetricMap::new();
        old.insert(
            MetricKey {
                path: PathBuf::from("metrics/train.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.80),
        );
        old.insert(
            MetricKey {
                path: PathBuf::from("metrics/eval.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.70),
        );

        let mut new = MetricMap::new();
        new.insert(
            MetricKey {
                path: PathBuf::from("metrics/train.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.85),
        );
        new.insert(
            MetricKey {
                path: PathBuf::from("metrics/eval.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.75),
        );

        let diff = diff_metric_maps(&old, &new);
        assert_eq!(diff.changed.len(), 2);
        assert!(diff.changed.contains_key(&MetricKey {
            path: PathBuf::from("metrics/train.json"),
            metric: "accuracy".to_owned(),
        }));
        assert!(diff.changed.contains_key(&MetricKey {
            path: PathBuf::from("metrics/eval.json"),
            metric: "accuracy".to_owned(),
        }));
    }

    #[test]
    fn metrics_diff_table_hides_path_only_when_requested() {
        let mut old = MetricMap::new();
        old.insert(
            MetricKey {
                path: PathBuf::from("metrics/train.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.80),
        );
        let mut new = MetricMap::new();
        new.insert(
            MetricKey {
                path: PathBuf::from("metrics/train.json"),
                metric: "accuracy".to_owned(),
            },
            Scalar::Float(0.85555),
        );

        let diff = diff_metric_maps(&old, &new);
        let with_path = render_metric_table(
            &diff,
            "main",
            "workspace",
            MetricRenderOptions {
                no_path: false,
                precision: 3,
                include_unchanged: false,
            },
        );
        assert!(with_path.contains("metrics/train.json"));
        assert!(with_path.contains("0.856"));

        let without_path = render_metric_table(
            &diff,
            "main",
            "workspace",
            MetricRenderOptions {
                no_path: true,
                precision: 3,
                include_unchanged: false,
            },
        );
        assert!(!without_path.contains("metrics/train.json"));
        assert!(without_path.contains("accuracy"));
    }

    #[test]
    fn plot_diff_args_parse_dvc_targets_and_revisions() {
        let args = PlotDiffArgs::try_parse_from([
            "diff",
            "--targets",
            "metrics/loss.csv",
            "metrics/acc.csv",
            "--",
            "main",
            "candidate",
            "experiment",
        ])
        .unwrap();

        assert_eq!(
            args.targets,
            vec![
                PathBuf::from("metrics/loss.csv"),
                PathBuf::from("metrics/acc.csv"),
            ]
        );
        assert_eq!(args.revisions, vec!["main", "candidate", "experiment"]);
        assert_eq!(
            args.comparison_revisions().unwrap(),
            vec!["main", "candidate", "experiment"]
        );
    }

    #[test]
    fn plot_diff_args_accepts_multiple_revisions() {
        let args = PlotDiffArgs::try_parse_from(["diff", "a", "b", "c"]).unwrap();
        assert_eq!(args.comparison_revisions().unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn run_plot_diff_defaults_to_head_vs_workspace() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n").unwrap();
        git(repo, &["add", "metrics/loss.csv"]);
        git(repo, &["commit", "-m", "baseline"]);

        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.5\n").unwrap();

        let output = repo.join("plot-diff-workspace.json");
        let mut args = plot_diff_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        run_plot_diff_in(&args, repo).unwrap();

        let spec = std::fs::read_to_string(output).unwrap();
        assert!(spec.contains("\"series\""));
        assert!(spec.contains("HEAD: loss"));
        assert!(spec.contains("workspace: loss"));
    }

    #[test]
    fn run_plot_diff_overlays_multiple_revisions() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::create_dir_all(repo.join("metrics")).unwrap();

        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n").unwrap();
        git(repo, &["add", "metrics/loss.csv"]);
        git(repo, &["commit", "-m", "base"]);
        git(repo, &["tag", "base"]);

        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.7\n").unwrap();
        git(repo, &["add", "metrics/loss.csv"]);
        git(repo, &["commit", "-m", "mid"]);
        git(repo, &["tag", "mid"]);

        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.5\n").unwrap();
        git(repo, &["add", "metrics/loss.csv"]);
        git(repo, &["commit", "-m", "final"]);
        git(repo, &["tag", "final"]);

        let output = repo.join("plot-diff-multi.json");
        let mut args = plot_diff_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.revisions = vec!["base".to_owned(), "mid".to_owned(), "final".to_owned()];
        run_plot_diff_in(&args, repo).unwrap();

        let spec = std::fs::read_to_string(output).unwrap();
        assert!(spec.contains("base: loss"));
        assert!(spec.contains("mid: loss"));
        assert!(spec.contains("final: loss"));
    }

    #[test]
    fn run_plot_writes_vega_for_simple_plot_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(
            repo.join("crab.yaml"),
            "plots:\n  - metrics/loss.csv\nstages:\n  train:\n    cmd: \"true\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("metrics/loss.csv"),
            "epoch,train_loss\n1,0.9\n2,0.5\n",
        )
        .unwrap();

        let output = repo.join("plot.json");
        let args = plot_args(PlotFormat::Vega, Some(output.clone()));
        run_plot_in(&args, repo).unwrap();

        let spec = std::fs::read_to_string(output).unwrap();
        assert!(spec.contains("\"$schema\""));
        assert!(spec.contains("train_loss"));
        assert!(spec.contains("metrics/loss.csv"));
    }

    #[test]
    fn run_plot_uses_stage_level_plot_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("plots")).unwrap();
        std::fs::write(
            repo.join("crab.yaml"),
            "stages:\n  train:\n    cmd: \"true\"\n    plots:\n      - plots/loss.csv\n",
        )
        .unwrap();
        std::fs::write(repo.join("plots/loss.csv"), "step,loss\n1,0.9\n2,0.5\n").unwrap();

        let output = repo.join("plot.json");
        let args = plot_args(PlotFormat::Vega, Some(output.clone()));
        run_plot_in(&args, repo).unwrap();

        let spec = std::fs::read_to_string(output).unwrap();
        assert!(spec.contains("\"$schema\""));
        assert!(spec.contains("loss"));
        assert!(spec.contains("plots/loss.csv"));
    }

    #[test]
    fn run_plot_writes_vega_overlay_between_refs() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(
            repo.join("crab.yaml"),
            "plots:\n  - metrics/loss.csv:\n      x: epoch\n      y: [loss]\n      title: Loss\nstages:\n  train:\n    cmd: \"true\"\n",
        )
        .unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n").unwrap();
        git(repo, &["add", "crab.yaml", "metrics/loss.csv"]);
        git(repo, &["commit", "-m", "baseline"]);

        git(repo, &["checkout", "-b", "candidate"]);
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.5\n").unwrap();
        git(repo, &["commit", "-am", "candidate"]);

        let output = repo.join("plot-diff.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.baseline = Some("main".to_owned());
        args.target = Some("candidate".to_owned());
        run_plot_in(&args, repo).unwrap();

        let spec = std::fs::read_to_string(output).unwrap();
        assert!(spec.contains("\"series\""));
        assert!(spec.contains("main: loss"));
        assert!(spec.contains("candidate: loss"));
        assert!(spec.contains("0.9"));
        assert!(spec.contains("0.5"));
    }

    #[test]
    fn plot_data_infers_y_from_tsv_and_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let tsv = tmp.path().join("loss.tsv");
        std::fs::write(&tsv, "epoch\tloss\n1\t0.9\n2\t0.5\n").unwrap();
        let yaml = tmp.path().join("loss.yaml");
        std::fs::write(&yaml, "- epoch: 1\n  loss: 0.9\n- epoch: 2\n  loss: 0.5\n").unwrap();
        let json = tmp.path().join("train.json");
        std::fs::write(
            &json,
            r#"{"train":[{"accuracy":0.96,"loss":0.10},{"accuracy":0.98,"loss":0.07}]}"#,
        )
        .unwrap();
        let config = PlotConfig {
            id: None,
            path: PathBuf::from("loss.tsv"),
            x: Some("epoch".to_owned()),
            x_path: None,
            y: Vec::new(),
            no_header: false,
            title: None,
            x_label: None,
            y_label: None,
            template: None,
        };

        let tsv_data = read_plot_data(&tsv, &config).unwrap();
        let yaml_data = read_plot_data(&yaml, &config).unwrap();
        let json_data = read_plot_data(
            &json,
            &PlotConfig {
                id: None,
                path: PathBuf::from("train.json"),
                x: None,
                x_path: None,
                y: Vec::new(),
                no_header: false,
                title: None,
                x_label: None,
                y_label: None,
                template: None,
            },
        )
        .unwrap();

        assert_eq!(tsv_data.y_fields, vec!["loss"]);
        assert_eq!(yaml_data.y_fields, vec!["loss"]);
        assert_eq!(json_data.y_fields, vec!["loss"]);
        assert_eq!(tsv_data.rows[1].y_values, vec!["0.5"]);
        assert_eq!(yaml_data.rows[1].y_values, vec!["0.5"]);
        assert_eq!(json_data.rows[1].x, "1");
        assert_eq!(json_data.rows[1].y_values, vec!["0.07"]);
    }

    #[test]
    fn plot_data_accepts_headerless_delimited_columns() {
        let data = read_delimited_plot_data(
            "1,0.2,0.8\n2,0.1,0.9\n",
            &PlotConfig {
                id: None,
                path: PathBuf::from("metrics.csv"),
                x: Some("0".to_owned()),
                x_path: None,
                y: vec!["2".to_owned()],
                no_header: true,
                title: None,
                x_label: Some("epoch".to_owned()),
                y_label: Some("accuracy".to_owned()),
                template: None,
            },
            ',',
        );

        assert_eq!(data.y_fields, vec!["2"]);
        assert_eq!(data.rows.len(), 2);
        assert_eq!(data.rows[0].x, "1");
        assert_eq!(data.rows[0].y_values, vec!["0.8"]);
    }

    #[test]
    fn run_plot_accepts_target_without_crab_yaml_and_cli_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n2,0.5\n").unwrap();

        let output = repo.join("plot.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.x = Some("epoch".to_owned());
        args.y = vec!["loss".to_owned()];
        args.title = Some("Loss target".to_owned());
        args.template = Some("scatter".to_owned());

        run_plot_in(&args, repo).unwrap();

        let spec = std::fs::read_to_string(output).unwrap();
        assert!(spec.contains("Loss target"));
        assert!(spec.contains("\"point\""));
        assert!(spec.contains("\"field\": \"epoch\""));
        assert!(spec.contains("\"loss\""));
    }

    #[test]
    fn run_plot_writes_headerless_vega_with_axis_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(
            repo.join("metrics/headerless.csv"),
            "1,0.2,0.8\n2,0.1,0.9\n",
        )
        .unwrap();

        let output = repo.join("plot.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/headerless.csv")];
        args.x = Some("0".to_owned());
        args.y = vec!["2".to_owned()];
        args.no_header = true;
        args.x_label = Some("epoch".to_owned());
        args.y_label = Some("accuracy".to_owned());

        run_plot_in(&args, repo).unwrap();

        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(spec["encoding"]["x"]["field"].as_str(), Some("0"));
        assert_eq!(spec["encoding"]["x"]["title"].as_str(), Some("epoch"));
        assert_eq!(spec["encoding"]["y"]["field"].as_str(), Some("value"));
        assert_eq!(spec["encoding"]["y"]["title"].as_str(), Some("accuracy"));
        let values = spec["data"]["values"].as_array().unwrap();
        assert!(values.iter().any(|value| {
            value["0"].as_i64() == Some(1) && value["value"].as_f64() == Some(0.8)
        }));
        assert!(values.iter().any(|value| {
            value["0"].as_i64() == Some(2) && value["value"].as_f64() == Some(0.9)
        }));
    }

    #[test]
    fn run_plot_accepts_declared_dvc_plot_id_target() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(
            repo.join("metrics/train.csv"),
            "epoch,train_loss,val_loss\n1,0.9,0.8\n2,0.5,0.4\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("metrics/test.csv"),
            "epoch,test_loss\n1,0.7\n2,0.3\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("crab.yaml"),
            r#"
plots:
  - train_val_test:
      x: epoch
      y:
        metrics/train.csv: [train_loss, val_loss]
        metrics/test.csv: test_loss
"#,
        )
        .unwrap();

        let output = repo.join("plot-id.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("train_val_test")];

        run_plot_in(&args, repo).unwrap();

        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        let plots = spec["plots"].as_array().unwrap();
        assert_eq!(plots.len(), 2);
        assert!(plots.iter().any(|plot| {
            plot["path"].as_str() == Some("metrics/train.csv")
                && plot["title"].as_str() == Some("train_val_test")
                && plot["spec"]["data"]["values"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value["metric"].as_str() == Some("val_loss"))
        }));
        assert!(plots.iter().any(|plot| {
            plot["path"].as_str() == Some("metrics/test.csv")
                && plot["title"].as_str() == Some("train_val_test")
                && plot["spec"]["data"]["values"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value["metric"].as_str() == Some("test_loss"))
        }));
    }

    #[test]
    fn run_plot_uses_different_file_for_declared_x_source() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(repo.join("actual.csv"), "actual_class\ndog\ncat\nbird\n").unwrap();
        std::fs::write(repo.join("preds.csv"), "predicted_class\ndog\ndog\nbird\n").unwrap();
        std::fs::write(
            repo.join("crab.yaml"),
            r#"
plots:
  - confusion:
      x:
        actual.csv: actual_class
      y:
        preds.csv: predicted_class
"#,
        )
        .unwrap();

        let output = repo.join("confusion.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("confusion")];

        run_plot_in(&args, repo).unwrap();

        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(
            spec["encoding"]["x"]["field"].as_str(),
            Some("actual_class")
        );
        assert!(
            spec["data"]["values"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value["actual_class"].as_str() == Some("cat")
                    && value["value"].as_str() == Some("dog"))
        );
    }

    #[test]
    fn run_plot_applies_dvc_template_anchors_from_named_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::create_dir_all(repo.join(".dvc/plots")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n2,0.5\n").unwrap();
        std::fs::write(
            repo.join(".dvc/plots/custom.json"),
            r#"{
  "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
  "title": "<DVC_METRIC_TITLE>",
  "data": {"values": "<DVC_METRIC_DATA>"},
  "mark": "area",
  "encoding": {
    "x": {"field": "<DVC_METRIC_X>", "type": "quantitative"},
    "y": {"field": "<DVC_METRIC_Y>", "type": "quantitative"},
    "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
  }
}"#,
        )
        .unwrap();

        let output = repo.join("plot.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.x = Some("epoch".to_owned());
        args.y = vec!["loss".to_owned()];
        args.title = Some("Templated loss".to_owned());
        args.template = Some("custom".to_owned());

        run_plot_in(&args, repo).unwrap();

        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(spec["title"], "Templated loss");
        assert_eq!(spec["mark"], "area");
        assert_eq!(spec["encoding"]["x"]["field"], "epoch");
        assert_eq!(spec["encoding"]["y"]["field"], "loss");
        assert_eq!(spec["data"]["values"][0]["loss"], 0.9);
    }

    #[test]
    fn plot_templates_list_builtins_and_local_templates() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join(".crab/plots")).unwrap();
        std::fs::create_dir_all(repo.join(".dvc/plots")).unwrap();
        std::fs::write(
            repo.join(".crab/plots/custom.json"),
            r#"{"$schema":"https://vega.github.io/schema/vega-lite/v5.json"}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(".dvc/plots/dvc_custom.json"),
            r#"{"$schema":"https://vega.github.io/schema/vega-lite/v5.json"}"#,
        )
        .unwrap();

        let payload = list_plot_templates(repo).unwrap();

        assert!(payload.templates.iter().any(|template| {
            template.name == "linear" && template.source == PlotTemplateSource::Builtin
        }));
        assert!(payload.templates.iter().any(|template| {
            template.name == "custom" && template.source == PlotTemplateSource::Local
        }));
        assert!(payload.templates.iter().any(|template| {
            template.name == "dvc_custom" && template.source == PlotTemplateSource::Local
        }));
    }

    #[test]
    fn plot_templates_dump_builtin_spec_with_dvc_anchors() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let payload = plot_template_spec(repo, "linear").unwrap();

        assert_eq!(payload.name, "linear");
        assert_eq!(payload.source, PlotTemplateSource::Builtin);
        assert_eq!(payload.spec["title"], "<DVC_METRIC_TITLE>");
        assert_eq!(payload.spec["data"]["values"], "<DVC_METRIC_DATA>");
        assert_eq!(payload.spec["encoding"]["x"]["field"], "<DVC_METRIC_X>");
    }

    #[test]
    fn run_plot_applies_builtin_template_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n2,0.5\n").unwrap();

        let output = repo.join("smooth.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.x = Some("epoch".to_owned());
        args.y = vec!["loss".to_owned()];
        args.title = Some("Smoothed loss".to_owned());
        args.template = Some("smooth".to_owned());

        run_plot_in(&args, repo).unwrap();

        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(spec["title"], "Smoothed loss");
        assert_eq!(spec["transform"][0]["loess"], "loss");
        assert_eq!(spec["transform"][0]["on"], "epoch");
        assert_eq!(spec["data"]["values"][0]["loss"], 0.9);
    }

    #[test]
    fn run_plot_open_writes_default_html_and_uses_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n2,0.5\n").unwrap();

        let mut args = plot_args(PlotFormat::Table, None);
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.open = true;

        let mut opened = None;
        run_plot_in_with_opener(&args, repo, |path| {
            opened = Some(path.to_path_buf());
            Ok(())
        })
        .unwrap();

        let output = repo.join("crab_plots").join("index.html");
        assert_eq!(opened, Some(output.clone()));
        let html = std::fs::read_to_string(output).unwrap();
        assert!(html.contains("vegaEmbed"));
        assert!(html.contains("metrics/loss.csv"));
    }

    #[test]
    fn run_plot_uses_custom_html_template() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::create_dir_all(repo.join(".dvc/plots")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n2,0.5\n").unwrap();
        std::fs::write(
            repo.join(".dvc/plots/mypage.html"),
            r#"<!doctype html>
<html>
  <head>
    <script src="local-vega.js"></script>
    <script src="local-vega-lite.js"></script>
    <script src="local-vega-embed.js"></script>
  </head>
  <body>
    <div id="custom-shell">{plot_divs}</div>
  </body>
</html>"#,
        )
        .unwrap();

        let output = repo.join("plot.html");
        let mut args = plot_args(PlotFormat::Table, Some(output.clone()));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.html_template = Some(PathBuf::from(".dvc/plots/mypage.html"));

        run_plot_in(&args, repo).unwrap();

        let html = std::fs::read_to_string(output).unwrap();
        assert!(html.contains(r#"<div id="custom-shell"><div id="plots"></div>"#));
        assert!(html.contains("local-vega-embed.js"));
        assert!(html.contains("metrics/loss.csv"));
        assert!(html.contains("vegaEmbed"));
        assert!(!html.contains("{plot_divs}"));
        assert!(!html.contains("cdn.jsdelivr.net/npm/vega@5"));
    }

    #[test]
    fn run_plot_rejects_html_template_without_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("metrics")).unwrap();
        std::fs::create_dir_all(repo.join(".dvc/plots")).unwrap();
        std::fs::write(repo.join("metrics/loss.csv"), "epoch,loss\n1,0.9\n2,0.5\n").unwrap();
        std::fs::write(repo.join(".dvc/plots/missing.html"), "<html></html>").unwrap();

        let output = repo.join("plot.html");
        let mut args = plot_args(PlotFormat::Table, Some(output));
        args.targets = vec![PathBuf::from("metrics/loss.csv")];
        args.html_template = Some(PathBuf::from(".dvc/plots/missing.html"));

        let err = run_plot_in(&args, repo).unwrap_err();
        assert!(
            err.to_string().contains("{plot_divs}"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn run_plot_embeds_image_target_in_html() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("plots")).unwrap();
        std::fs::write(repo.join("plots/example.svg"), tiny_svg("current")).unwrap();

        let output = repo.join("image.html");
        let mut args = plot_args(PlotFormat::Html, Some(output.clone()));
        args.targets = vec![PathBuf::from("plots/example.svg")];

        run_plot_in(&args, repo).unwrap();

        let html = std::fs::read_to_string(output).unwrap();
        assert!(html.contains(r#""kind": "image""#));
        assert!(html.contains("data:image/svg+xml;base64,"));
        assert!(html.contains("plots/example.svg"));
    }

    #[test]
    fn run_plot_embeds_image_directory_in_html() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("plots/nested")).unwrap();
        std::fs::write(repo.join("plots/a.svg"), tiny_svg("a")).unwrap();
        std::fs::write(repo.join("plots/nested/b.svg"), tiny_svg("b")).unwrap();

        let output = repo.join("images.html");
        let mut args = plot_args(PlotFormat::Html, Some(output.clone()));
        args.targets = vec![PathBuf::from("plots")];

        run_plot_in(&args, repo).unwrap();

        let html = std::fs::read_to_string(output).unwrap();
        assert!(html.contains("plots/a.svg"));
        assert!(html.contains("plots/nested/b.svg"));
        assert!(html.contains("data:image/svg+xml;base64,"));
    }

    #[test]
    fn run_plot_rejects_image_target_for_vega_output() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("plots")).unwrap();
        std::fs::write(repo.join("plots/example.svg"), tiny_svg("current")).unwrap();

        let output = repo.join("image.json");
        let mut args = plot_args(PlotFormat::Vega, Some(output));
        args.targets = vec![PathBuf::from("plots/example.svg")];

        let err = run_plot_in(&args, repo).unwrap_err();
        assert!(
            err.to_string().contains("image plot targets require"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn run_plot_diff_embeds_image_refs_in_html() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::create_dir_all(repo.join("plots")).unwrap();
        std::fs::write(repo.join("plots/confusion.svg"), tiny_svg("main")).unwrap();
        git(repo, &["add", "plots/confusion.svg"]);
        git(repo, &["commit", "-m", "baseline image"]);

        git(repo, &["checkout", "-b", "candidate"]);
        std::fs::write(repo.join("plots/confusion.svg"), tiny_svg("candidate")).unwrap();
        git(repo, &["commit", "-am", "candidate image"]);

        let output = repo.join("image-diff.html");
        let mut args = plot_args(PlotFormat::Html, Some(output.clone()));
        args.targets = vec![PathBuf::from("plots/confusion.svg")];
        args.baseline = Some("main".to_owned());
        args.target = Some("candidate".to_owned());

        run_plot_in(&args, repo).unwrap();

        let html = std::fs::read_to_string(output).unwrap();
        assert!(html.contains(r#""label": "main""#));
        assert!(html.contains(r#""label": "candidate""#));
        assert!(html.contains("plots/confusion.svg"));
        assert!(html.contains("data:image/svg+xml;base64,"));
    }

    #[test]
    fn run_diff_between_branches_metrics() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::write(
            repo.join("metrics.json"),
            br#"{"accuracy": 0.80, "loss": 0.50}"#,
        )
        .unwrap();
        git(repo, &["add", "metrics.json"]);
        git(repo, &["commit", "-m", "a"]);
        git(repo, &["checkout", "-b", "b"]);
        std::fs::write(
            repo.join("metrics.json"),
            br#"{"accuracy": 0.85, "loss": 0.40}"#,
        )
        .unwrap();
        git(repo, &["commit", "-am", "b"]);

        let args = DiffArgs {
            revisions: vec!["main".into(), "b".into()],
            paths: vec![],
            format: Format::Table,
            md: false,
            all: false,
            recursive: false,
            no_path: false,
            precision: 5,
            json: false,
            higher_is_better: true,
        };
        run_diff_in(&args, repo).unwrap();
    }

    fn tiny_svg(label: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><text x="1" y="12">{label}</text></svg>"#
        )
    }
}
