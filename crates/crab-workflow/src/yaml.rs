//! `crab.yaml` schema and parser.
//!
//! See design §"`crab.yaml` Schema" for the shape. The parser is
//! deliberately strict: unknown keys fail so typos surface at parse
//! time rather than silently becoming no-ops.
//!
//! The grammar is forgiving only where users obviously want it to
//! be — `cmd` accepts a shell string, DVC-style command list, or
//! argv map; `deps` accept either a bare path or one of the
//! structured dep forms; `outs` accept a path string, Crab's
//! explicit `path:` map, or DVC's path-key override form.
//!
//! Stage names are validated at parse time per R17. The returned
//! [`Workflow`] carries already-validated [`Stage`] structs so the
//! executor can trust their invariants.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::{
    ArtifactMetadata, Cmd, Defaults, Dep, EnvSpec, Out, OutKind, ParamRef, PlotConfig, Resources,
    Result, RetryPolicy, Stage, StageCondition, StageName, TemplateContext, Workflow,
    WorkflowError, expand_foreach, expand_matrix, is_external_url_out_path, is_url_dep, substitute,
    substitute_cmd, validate_wdir,
};

/// Parse a `crab.yaml` document from a string.
///
/// Unknown keys at any level fail the parse. Stage names are
/// validated against R17 as soon as each stage is materialized.
/// Returns [`WorkflowError::YamlParse`] for syntax errors (the
/// source error carries line / column via
/// [`serde_yaml::Error::location`]) and
/// [`WorkflowError::StageNameInvalid`] for name violations.
///
/// Template `${...}` expressions are resolved when a non-empty
/// [`TemplateContext`] is supplied via [`parse_with_context`]. This
/// overload uses an empty context so existing callers that don't use
/// templates continue to work unchanged.
pub fn parse(text: &str) -> Result<Workflow> {
    parse_with_context(text, &TemplateContext::empty())
}

/// Parse a `crab.yaml` document with template substitution.
///
/// After deserialization, every string-valued field in each raw stage
/// is walked through the substitution engine. Undefined `${...}`
/// references produce [`WorkflowError::TemplateUndefined`] with
/// the field name and stage name for actionable diagnostics.
pub fn parse_with_context(text: &str, ctx: &TemplateContext) -> Result<Workflow> {
    let raw: RawWorkflow =
        serde_yaml::from_str(text).map_err(|source| WorkflowError::YamlParse {
            path: PathBuf::new(),
            source,
        })?;
    let artifacts = ArtifactMetadata::from_declarations(raw.artifacts);
    // Validate preserved catalog metadata at workflow parse time so malformed
    // artifact declarations cannot reach execution and be silently ignored.
    crate::ArtifactCatalog::from_metadata(&artifacts)?;

    let defaults = raw
        .defaults
        .map(Defaults::try_from)
        .transpose()?
        .unwrap_or_default();

    // Collect all stage entries from both `stages:` and `workflows:`.
    // Track workflow membership for filtering support.
    let mut expanded_entries: Vec<(String, serde_yaml::Value)> = Vec::new();
    let mut workflow_membership: BTreeMap<StageName, String> = BTreeMap::new();

    // Process top-level `stages:` (the "default" workflow).
    for (name_str, stage_value) in &raw.stages {
        let entries = expand_stage_entry(name_str, stage_value, ctx)?;
        for (expanded_name, expanded_value) in entries {
            if !raw.workflows.is_empty() {
                workflow_membership.insert(expanded_name.clone(), String::new());
            }
            expanded_entries.push((expanded_name.as_str().to_owned(), expanded_value));
        }
    }

    // Process named `workflows:` groups.
    for (workflow_name, group) in &raw.workflows {
        for (name_str, stage_value) in &group.stages {
            let entries = expand_stage_entry(name_str, stage_value, ctx)?;
            for (expanded_name, expanded_value) in entries {
                workflow_membership.insert(expanded_name.clone(), workflow_name.clone());
                expanded_entries.push((expanded_name.as_str().to_owned(), expanded_value));
            }
        }
    }

    // Convert all stage values (regular + expanded) into Stage structs.
    let mut stages = BTreeMap::new();
    for (name_str, stage_value) in expanded_entries {
        let name = StageName::parse(&name_str)?;

        // Check for name collisions from foreach expansion.
        if stages.contains_key(&name) {
            return Err(WorkflowError::YamlInvalid {
                key: format!("stage '{name_str}'"),
                origin: "expanded stage name collides with another stage".to_owned(),
            });
        }

        let raw_stage: RawStage =
            serde_yaml::from_value(stage_value).map_err(|source| WorkflowError::YamlParse {
                path: PathBuf::new(),
                source,
            })?;
        let substituted = substitute_raw_stage(raw_stage, ctx, &name)?;
        let stage = stage_from_raw(name.clone(), substituted, &defaults)?;
        stages.insert(name, stage);
    }

    // Parse plots: each entry is either a bare path string or a
    // structured map `{ path: { x, y, title, template } }`.
    let (simple_plots, plot_configs) = parse_raw_plots(&raw.plots)?;

    Ok(Workflow {
        params: raw.params,
        metrics: raw.metrics,
        plots: simple_plots,
        plot_configs,
        artifacts,
        defaults,
        stages,
        workflow_membership,
    })
}

/// Parse with a context built from the workflow's own `vars:` and
/// params files loaded relative to `base_dir`.
///
/// This is the full-featured entry point used by callers that have
/// filesystem access (e.g. `crab run`, `crab status`).
pub fn parse_with_base_dir(text: &str, base_dir: &Path) -> Result<Workflow> {
    // First pass: deserialize to get vars and params declarations.
    let raw: RawWorkflow =
        serde_yaml::from_str(text).map_err(|source| WorkflowError::YamlParse {
            path: PathBuf::new(),
            source,
        })?;
    let vars = TemplateContext::load_vars(&raw.vars, base_dir)?;
    let param_paths = template_param_paths(&raw.params);
    let params = TemplateContext::load_params(&param_paths, base_dir)?;
    let ctx = TemplateContext::new(vars, params, false);

    // Second pass: parse with the built context.
    parse_with_context(text, &ctx)
}

fn template_param_paths(declared: &[PathBuf]) -> Vec<PathBuf> {
    let default = PathBuf::from("params.yaml");
    let mut paths = Vec::with_capacity(declared.len() + 1);
    paths.push(default.clone());
    paths.extend(declared.iter().filter(|path| *path != &default).cloned());
    paths
}

/// Parse a `crab.yaml` document anchored at a known path. The
/// path is attached to any resulting parse error so the user sees
/// `path:line:col` in the error message.
pub fn parse_at(path: &std::path::Path, text: &str) -> Result<Workflow> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    parse_with_base_dir(text, base_dir).map_err(|e| match e {
        WorkflowError::YamlParse { source, .. } => WorkflowError::YamlParse {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

// These mirror the YAML shape one-for-one with `deny_unknown_fields`
// turned on. They exist solely as a serde bridge; the public API
// exposes the validated [`Workflow`] / [`Stage`] types above.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    #[serde(default)]
    vars: Vec<serde_yaml::Value>,
    #[serde(default)]
    artifacts: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    params: Vec<PathBuf>,
    #[serde(default)]
    metrics: Vec<PathBuf>,
    #[serde(default)]
    plots: Vec<serde_yaml::Value>,
    #[serde(default)]
    defaults: Option<RawDefaults>,
    /// Stages are deserialized as raw YAML values so we can detect
    /// `foreach:` + `do:` entries before attempting strict `RawStage`
    /// deserialization.
    #[serde(default)]
    stages: BTreeMap<String, serde_yaml::Value>,
    /// Named workflow groups. When present, stages are grouped by
    /// workflow name. The existing `stages:` key continues to work
    /// (backward compatible) and is treated as the "default" workflow.
    #[serde(default)]
    workflows: BTreeMap<String, RawWorkflowGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    #[serde(default)]
    env: Option<RawEnv>,
    #[serde(default)]
    retry: Option<RawRetry>,
}

/// A named workflow group containing its own set of stages.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflowGroup {
    /// Stages within this named workflow, deserialized as raw YAML
    /// values for foreach/matrix detection.
    #[serde(default)]
    stages: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStage {
    cmd: RawCmd,
    #[serde(default)]
    deps: Vec<RawDep>,
    #[serde(default)]
    outs: Vec<RawOut>,
    #[serde(default)]
    params: Vec<serde_yaml::Value>,
    #[serde(default)]
    env: Option<RawEnv>,
    #[serde(default)]
    metrics: Vec<RawMetric>,
    #[serde(default)]
    plots: Vec<serde_yaml::Value>,
    #[serde(default)]
    nondeterministic: bool,
    #[serde(default)]
    always_changed: bool,
    #[serde(default)]
    hermetic: bool,
    #[serde(default)]
    side_effects: bool,
    #[serde(default)]
    on_cache_hit: Option<RawCmd>,
    #[serde(default)]
    retry: Option<RawRetry>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    persist: bool,
    #[serde(default)]
    frozen: bool,
    #[serde(default)]
    resources: Option<RawResources>,
    #[serde(default)]
    wdir: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    meta: Option<serde_yaml::Value>,
    #[serde(default)]
    condition: Option<RawCondition>,
}

/// `cmd:` accepts a shell string, a DVC-style list of shell commands,
/// or `{ argv: [...] }`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCmd {
    Shell(String),
    ShellList(Vec<String>),
    Argv(RawCmdArgv),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCmdArgv {
    argv: Vec<String>,
}

/// `deps:` accepts a bare path string or one of the structured
/// forms. Each map form has exactly one discriminator key so the
/// `untagged` deserialization is unambiguous.
#[expect(
    clippy::large_enum_variant,
    reason = "RawDep only lives during parse; boxing Structured would \
              force a heap allocation per dep and complicate the \
              serde untagged deserialization."
)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDep {
    Path(String),
    Structured(RawDepStructured),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDepStructured {
    // Exactly one of these is expected. We reject `0` and `>1` set
    // fields below in `RawDep::into_dep` so the error message names
    // the offending stage rather than returning a generic
    // "data did not match any variant".
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    crab: Option<RawCrabRef>,
    #[serde(default)]
    git: Option<RawGitRef>,
    #[serde(default)]
    url: Option<RawUrlDep>,
    #[serde(default)]
    oci: Option<RawOciDep>,
    #[serde(default)]
    stage_out: Option<RawStageOutDep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCrabRef {
    repo: String,
    rev: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGitRef {
    url: String,
    rev: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUrlDep {
    url: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOciDep {
    reference: String,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStageOutDep {
    stage: String,
    out: PathBuf,
}

/// `outs:` accepts a bare path, Crab's explicit `path:` map, or
/// DVC's path-key override form.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawOut {
    Path(String),
    Structured(Box<RawOutStructured>),
    DvcPathMap(BTreeMap<String, Option<RawDvcOutSettings>>),
}

/// Stage `metrics:` accepts a bare path or DVC's path-key output
/// settings form. Structured entries also produce an output record so
/// cache policy such as `cache: false` keeps its DVC meaning.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawMetric {
    Path(String),
    Structured(Box<RawOutStructured>),
    DvcPathMap(BTreeMap<String, Option<RawDvcOutSettings>>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutStructured {
    path: PathBuf,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    cache: Option<bool>,
    #[serde(default)]
    push: Option<bool>,
    #[serde(default)]
    persist: Option<bool>,
    #[serde(default)]
    checkpoint: Option<serde_yaml::Value>,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    #[serde(rename = "desc")]
    _desc: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDvcOutSettings {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    cache: Option<bool>,
    #[serde(default)]
    push: Option<bool>,
    #[serde(default)]
    persist: Option<bool>,
    #[serde(default)]
    checkpoint: Option<serde_yaml::Value>,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    #[serde(rename = "desc")]
    _desc: Option<serde_yaml::Value>,
}

/// `env:` accepts `"inherit"` / `"empty"` / `"allowlist"` strings
/// (defaults meaning) or a list of allow-listed var names (shortcut
/// for `allowlist: [...]`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawEnv {
    Named(String),
    Allowlist(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRetry {
    #[serde(default)]
    max_attempts: Option<u32>,
    #[serde(default)]
    initial_backoff: Option<String>,
    #[serde(default)]
    max_backoff: Option<String>,
    #[serde(default)]
    backoff_multiplier: Option<f64>,
    #[serde(default)]
    on_exit_codes: Vec<i32>,
    #[serde(default)]
    on_signals: Vec<i32>,
    #[serde(default)]
    on_timeout: Option<bool>,
}

/// `resources:` block declaring CPU, GPU, and memory requirements.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResources {
    #[serde(default)]
    cpu: Option<u32>,
    #[serde(default)]
    gpu: Option<u32>,
    #[serde(default)]
    memory: Option<String>,
}

/// `condition:` accepts one of `{env: VAR}`, `{file_exists: path}`,
/// or `{expr: "..."}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCondition {
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    file_exists: Option<String>,
    #[serde(default)]
    expr: Option<String>,
}

/// Expand a single stage entry (handling foreach/matrix detection).
/// Returns a list of `(StageName, Value)` pairs — one for regular
/// stages, multiple for foreach/matrix expansions.
fn expand_stage_entry(
    name_str: &str,
    stage_value: &serde_yaml::Value,
    ctx: &TemplateContext,
) -> Result<Vec<(StageName, serde_yaml::Value)>> {
    if is_foreach_stage(stage_value) {
        expand_foreach_stage(name_str, stage_value, ctx)
    } else if is_matrix_stage(stage_value) {
        expand_matrix_stage(name_str, stage_value, ctx)
    } else {
        let name = StageName::parse(name_str)?;
        Ok(vec![(name, stage_value.clone())])
    }
}

/// Check whether a raw stage YAML value represents a `foreach` stage.
///
/// A foreach stage is a mapping that contains both `foreach:` and `do:`
/// keys. Regular stages never have these keys (they have `cmd:` instead).
fn is_foreach_stage(value: &serde_yaml::Value) -> bool {
    if let serde_yaml::Value::Mapping(map) = value {
        let has_foreach = map.contains_key(serde_yaml::Value::String("foreach".to_owned()));
        let has_do = map.contains_key(serde_yaml::Value::String("do".to_owned()));
        has_foreach && has_do
    } else {
        false
    }
}

/// Expand a foreach stage into concrete `(StageName, Value)` pairs.
///
/// The `foreach:` value may itself contain `${...}` expressions that
/// reference params/vars — these are resolved BEFORE expansion. The
/// `do:` block is the stage template passed to `expand_foreach()`.
fn expand_foreach_stage(
    base_name: &str,
    stage_value: &serde_yaml::Value,
    ctx: &TemplateContext,
) -> Result<Vec<(StageName, serde_yaml::Value)>> {
    let map = stage_value
        .as_mapping()
        .ok_or_else(|| WorkflowError::YamlInvalid {
            key: format!("stage '{base_name}'"),
            origin: "foreach stage must be a mapping".to_owned(),
        })?;

    let foreach_raw = map
        .get(serde_yaml::Value::String("foreach".to_owned()))
        .ok_or_else(|| WorkflowError::YamlInvalid {
            key: format!("stage '{base_name}'"),
            origin: "foreach stage missing 'foreach' field".to_owned(),
        })?;

    let do_template = map
        .get(serde_yaml::Value::String("do".to_owned()))
        .ok_or_else(|| WorkflowError::YamlInvalid {
            key: format!("stage '{base_name}'"),
            origin: "foreach stage missing 'do' field".to_owned(),
        })?;

    // Resolve `${...}` expressions in the foreach value itself.
    // This allows `foreach: ${datasets}` where `datasets` is defined
    // in vars or params.
    let foreach_resolved = resolve_foreach_value(foreach_raw, ctx, base_name)?;

    expand_foreach(base_name, &foreach_resolved, do_template, ctx)
}

/// Resolve `${...}` expressions within the foreach iteration source.
///
/// If the foreach value is a plain string like `"${datasets}"`, resolve
/// it and parse the result as YAML. If it's already a list or dict,
/// recursively resolve any string values that contain `${...}`.
fn resolve_foreach_value(
    value: &serde_yaml::Value,
    ctx: &TemplateContext,
    stage_name: &str,
) -> Result<serde_yaml::Value> {
    match value {
        serde_yaml::Value::String(s) => resolve_template_value(s, ctx, stage_name, "foreach"),
        serde_yaml::Value::Sequence(seq) => {
            // Resolve any string items that contain `${...}`.
            let resolved: Result<Vec<serde_yaml::Value>> = seq
                .iter()
                .map(|item| resolve_foreach_value(item, ctx, stage_name))
                .collect();
            Ok(serde_yaml::Value::Sequence(resolved?))
        }
        serde_yaml::Value::Mapping(map) => {
            // Resolve string values in the mapping.
            let mut resolved_map = serde_yaml::Mapping::new();
            for (k, v) in map {
                let resolved_v = resolve_foreach_value(v, ctx, stage_name)?;
                resolved_map.insert(k.clone(), resolved_v);
            }
            Ok(serde_yaml::Value::Mapping(resolved_map))
        }
        // Non-string scalars pass through unchanged.
        other => Ok(other.clone()),
    }
}

fn resolve_template_value(
    input: &str,
    ctx: &TemplateContext,
    stage_name: &str,
    field: &str,
) -> Result<serde_yaml::Value> {
    let Some(expr) = single_template_expr(input) else {
        return Ok(serde_yaml::Value::String(sub_field(
            input, ctx, stage_name, field,
        )?));
    };

    let expr = normalize_template_expr(expr);
    let resolved = ctx.resolve_value(&expr).map_err(|e| match e {
        WorkflowError::TemplateUndefined { key, .. } => WorkflowError::TemplateUndefined {
            key,
            field: field.to_owned(),
            stage: stage_name.to_owned(),
        },
        other => other,
    })?;

    match resolved {
        serde_yaml::Value::String(s) => {
            serde_yaml::from_str(&s).map_err(|source| WorkflowError::YamlParse {
                path: PathBuf::new(),
                source,
            })
        }
        other => Ok(other),
    }
}

fn single_template_expr(input: &str) -> Option<&str> {
    let expr = input.strip_prefix("${")?.strip_suffix('}')?;
    if expr.contains('}') || expr.contains("${") {
        return None;
    }
    Some(expr)
}

fn normalize_template_expr(expr: &str) -> String {
    if !expr.contains('[') {
        return expr.to_owned();
    }

    let mut result = String::with_capacity(expr.len());
    for ch in expr.chars() {
        match ch {
            '[' => result.push('.'),
            ']' => {}
            _ => result.push(ch),
        }
    }
    result
}

/// Check whether a raw stage YAML value represents a `matrix` stage.
///
/// A matrix stage has a `matrix:` key but does NOT have `foreach:` or `do:`
/// keys. The rest of the stage definition (cmd, deps, outs) IS the template.
fn is_matrix_stage(value: &serde_yaml::Value) -> bool {
    if let serde_yaml::Value::Mapping(map) = value {
        let has_matrix = map.contains_key(serde_yaml::Value::String("matrix".to_owned()));
        let has_foreach = map.contains_key(serde_yaml::Value::String("foreach".to_owned()));
        let has_do = map.contains_key(serde_yaml::Value::String("do".to_owned()));
        has_matrix && !has_foreach && !has_do
    } else {
        false
    }
}

/// Expand a matrix stage into concrete `(StageName, Value)` pairs.
///
/// The `matrix:` value is a mapping of variable names to value lists.
/// The stage template is everything in the stage EXCEPT the `matrix:` field.
fn expand_matrix_stage(
    base_name: &str,
    stage_value: &serde_yaml::Value,
    ctx: &TemplateContext,
) -> Result<Vec<(StageName, serde_yaml::Value)>> {
    let map = stage_value
        .as_mapping()
        .ok_or_else(|| WorkflowError::YamlInvalid {
            key: format!("stage '{base_name}'"),
            origin: "matrix stage must be a mapping".to_owned(),
        })?;

    let matrix_value = map
        .get(serde_yaml::Value::String("matrix".to_owned()))
        .ok_or_else(|| WorkflowError::YamlInvalid {
            key: format!("stage '{base_name}'"),
            origin: "matrix stage missing 'matrix' field".to_owned(),
        })?;

    // Build the stage template: everything except the `matrix:` field.
    let mut template_map = serde_yaml::Mapping::new();
    for (k, v) in map {
        if *k != serde_yaml::Value::String("matrix".to_owned()) {
            template_map.insert(k.clone(), v.clone());
        }
    }
    let stage_template = serde_yaml::Value::Mapping(template_map);

    let matrix_value = resolve_matrix_value(matrix_value, ctx, base_name)?;
    expand_matrix(base_name, &matrix_value, &stage_template, ctx)
}

fn resolve_matrix_value(
    value: &serde_yaml::Value,
    ctx: &TemplateContext,
    stage_name: &str,
) -> Result<serde_yaml::Value> {
    match value {
        serde_yaml::Value::String(s) => resolve_template_value(s, ctx, stage_name, "matrix"),
        serde_yaml::Value::Sequence(seq) => {
            let resolved: Result<Vec<serde_yaml::Value>> = seq
                .iter()
                .map(|item| resolve_matrix_value(item, ctx, stage_name))
                .collect();
            Ok(serde_yaml::Value::Sequence(resolved?))
        }
        serde_yaml::Value::Mapping(map) => {
            let mut resolved_map = serde_yaml::Mapping::new();
            for (key, value) in map {
                resolved_map.insert(key.clone(), resolve_matrix_value(value, ctx, stage_name)?);
            }
            Ok(serde_yaml::Value::Mapping(resolved_map))
        }
        other => Ok(other.clone()),
    }
}

/// Apply `${...}` substitution to all string-valued fields in a raw stage.
///
/// Walks `cmd`, `deps` paths, `outs` paths, `params` keys, `timeout`,
/// and `on_cache_hit`. Each undefined reference produces
/// [`WorkflowError::TemplateUndefined`] with the field name and
/// stage name filled in for actionable diagnostics.
fn substitute_raw_stage(
    mut raw: RawStage,
    ctx: &TemplateContext,
    stage_name: &StageName,
) -> Result<RawStage> {
    let stage_str = stage_name.as_str();

    // cmd
    raw.cmd = substitute_raw_cmd(raw.cmd, ctx, stage_str, "cmd")?;

    // deps
    raw.deps = raw
        .deps
        .into_iter()
        .map(|d| substitute_raw_dep(d, ctx, stage_str))
        .collect::<Result<Vec<_>>>()?;

    // outs
    raw.outs = raw
        .outs
        .into_iter()
        .map(|o| substitute_raw_out(o, ctx, stage_str))
        .collect::<Result<Vec<_>>>()?;

    // params (dotted key references or file-scoped maps)
    raw.params = raw
        .params
        .into_iter()
        .map(|p| substitute_raw_param(p, ctx, stage_str))
        .collect::<Result<Vec<_>>>()?;

    // metrics paths/settings
    raw.metrics = raw
        .metrics
        .into_iter()
        .map(|m| substitute_raw_metric(m, ctx, stage_str))
        .collect::<Result<Vec<_>>>()?;

    // plots paths/configs
    raw.plots = raw
        .plots
        .into_iter()
        .map(|p| substitute_plot_value(p, ctx, stage_str))
        .collect::<Result<Vec<_>>>()?;

    // timeout
    if let Some(ref t) = raw.timeout {
        raw.timeout = Some(sub_field(t, ctx, stage_str, "timeout")?);
    }

    // wdir
    if let Some(ref w) = raw.wdir {
        raw.wdir = Some(sub_field(w, ctx, stage_str, "wdir")?);
    }

    // on_cache_hit
    if let Some(cmd) = raw.on_cache_hit {
        raw.on_cache_hit = Some(substitute_raw_cmd(cmd, ctx, stage_str, "on_cache_hit")?);
    }

    // desc (supports template substitution for dynamic descriptions)
    if let Some(ref d) = raw.desc {
        raw.desc = Some(sub_field(d, ctx, stage_str, "desc")?);
    }

    // condition (substitute expr values)
    if let Some(ref mut cond) = raw.condition {
        if let Some(ref e) = cond.expr {
            cond.expr = Some(sub_field(e, ctx, stage_str, "condition.expr")?);
        }
        if let Some(ref p) = cond.file_exists {
            cond.file_exists = Some(sub_field(p, ctx, stage_str, "condition.file_exists")?);
        }
    }

    Ok(raw)
}

fn substitute_raw_cmd(
    cmd: RawCmd,
    ctx: &TemplateContext,
    stage: &str,
    field: &str,
) -> Result<RawCmd> {
    match cmd {
        RawCmd::Shell(s) => Ok(RawCmd::Shell(sub_cmd_field(&s, ctx, stage, field)?)),
        RawCmd::ShellList(commands) => {
            let resolved = commands
                .into_iter()
                .map(|command| sub_cmd_field(&command, ctx, stage, field))
                .collect::<Result<Vec<_>>>()?;
            Ok(RawCmd::ShellList(resolved))
        }
        RawCmd::Argv(RawCmdArgv { argv }) => {
            let resolved = argv
                .into_iter()
                .map(|a| sub_field(&a, ctx, stage, field))
                .collect::<Result<Vec<_>>>()?;
            Ok(RawCmd::Argv(RawCmdArgv { argv: resolved }))
        }
    }
}

fn substitute_raw_dep(dep: RawDep, ctx: &TemplateContext, stage: &str) -> Result<RawDep> {
    match dep {
        RawDep::Path(s) => Ok(RawDep::Path(sub_field(&s, ctx, stage, "deps")?)),
        RawDep::Structured(mut s) => {
            if let Some(ref p) = s.path {
                s.path = Some(PathBuf::from(sub_field(
                    &p.to_string_lossy(),
                    ctx,
                    stage,
                    "deps",
                )?));
            }
            if let Some(ref mut g) = s.crab {
                g.repo = sub_field(&g.repo, ctx, stage, "deps")?;
                g.rev = sub_field(&g.rev, ctx, stage, "deps")?;
                g.path = PathBuf::from(sub_field(&g.path.to_string_lossy(), ctx, stage, "deps")?);
            }
            if let Some(ref mut g) = s.git {
                g.url = sub_field(&g.url, ctx, stage, "deps")?;
                g.rev = sub_field(&g.rev, ctx, stage, "deps")?;
                g.path = PathBuf::from(sub_field(&g.path.to_string_lossy(), ctx, stage, "deps")?);
            }
            if let Some(ref mut u) = s.url {
                u.url = sub_field(&u.url, ctx, stage, "deps")?;
                if let Some(ref d) = u.digest {
                    u.digest = Some(sub_field(d, ctx, stage, "deps")?);
                }
            }
            if let Some(ref mut o) = s.oci {
                o.reference = sub_field(&o.reference, ctx, stage, "deps")?;
                o.digest = sub_field(&o.digest, ctx, stage, "deps")?;
            }
            if let Some(ref mut so) = s.stage_out {
                so.stage = sub_field(&so.stage, ctx, stage, "deps")?;
                so.out = PathBuf::from(sub_field(&so.out.to_string_lossy(), ctx, stage, "deps")?);
            }
            Ok(RawDep::Structured(s))
        }
    }
}

fn substitute_raw_param(
    value: serde_yaml::Value,
    ctx: &TemplateContext,
    stage: &str,
) -> Result<serde_yaml::Value> {
    match value {
        serde_yaml::Value::String(s) => Ok(serde_yaml::Value::String(sub_field(
            &s, ctx, stage, "params",
        )?)),
        serde_yaml::Value::Mapping(map) => {
            let mut out = serde_yaml::Mapping::new();
            for (key, value) in map {
                let key_str = key.as_str().ok_or_else(|| WorkflowError::YamlInvalid {
                    key: format!("stage '{stage}' params"),
                    origin: "params file keys must be strings".to_owned(),
                })?;
                let resolved_key = sub_field(key_str, ctx, stage, "params")?;
                let resolved_value = match value {
                    serde_yaml::Value::Sequence(seq) => {
                        let mut resolved = Vec::with_capacity(seq.len());
                        for item in seq {
                            let item_str =
                                item.as_str().ok_or_else(|| WorkflowError::YamlInvalid {
                                    key: format!("stage '{stage}' params.{resolved_key}"),
                                    origin: "file-scoped params entries must be strings".to_owned(),
                                })?;
                            resolved.push(serde_yaml::Value::String(sub_field(
                                item_str, ctx, stage, "params",
                            )?));
                        }
                        serde_yaml::Value::Sequence(resolved)
                    }
                    serde_yaml::Value::Null => serde_yaml::Value::Null,
                    other => other,
                };
                out.insert(serde_yaml::Value::String(resolved_key), resolved_value);
            }
            Ok(serde_yaml::Value::Mapping(out))
        }
        other => Ok(other),
    }
}

fn substitute_raw_out(out: RawOut, ctx: &TemplateContext, stage: &str) -> Result<RawOut> {
    match out {
        RawOut::Path(s) => Ok(RawOut::Path(sub_field(&s, ctx, stage, "outs")?)),
        RawOut::Structured(mut s) => {
            s.path = PathBuf::from(sub_field(&s.path.to_string_lossy(), ctx, stage, "outs")?);
            Ok(RawOut::Structured(s))
        }
        RawOut::DvcPathMap(entries) => {
            let mut resolved = BTreeMap::new();
            for (path, settings) in entries {
                let path = sub_field(&path, ctx, stage, "outs")?;
                if resolved.insert(path.clone(), settings).is_some() {
                    return Err(WorkflowError::YamlInvalid {
                        key: format!("stage '{stage}' outs"),
                        origin: format!("duplicate output path after substitution: {path}"),
                    });
                }
            }
            Ok(RawOut::DvcPathMap(resolved))
        }
    }
}

fn substitute_raw_metric(
    metric: RawMetric,
    ctx: &TemplateContext,
    stage: &str,
) -> Result<RawMetric> {
    match metric {
        RawMetric::Path(s) => Ok(RawMetric::Path(sub_field(&s, ctx, stage, "metrics")?)),
        RawMetric::Structured(mut s) => {
            s.path = PathBuf::from(sub_field(&s.path.to_string_lossy(), ctx, stage, "metrics")?);
            Ok(RawMetric::Structured(s))
        }
        RawMetric::DvcPathMap(entries) => {
            let mut resolved = BTreeMap::new();
            for (path, settings) in entries {
                let path = sub_field(&path, ctx, stage, "metrics")?;
                if resolved.insert(path.clone(), settings).is_some() {
                    return Err(WorkflowError::YamlInvalid {
                        key: format!("stage '{stage}' metrics"),
                        origin: format!("duplicate metric path after substitution: {path}"),
                    });
                }
            }
            Ok(RawMetric::DvcPathMap(resolved))
        }
    }
}

fn substitute_plot_value(
    value: serde_yaml::Value,
    ctx: &TemplateContext,
    stage: &str,
) -> Result<serde_yaml::Value> {
    match value {
        serde_yaml::Value::String(s) => Ok(serde_yaml::Value::String(sub_field(
            &s, ctx, stage, "plots",
        )?)),
        serde_yaml::Value::Sequence(seq) => {
            let values = seq
                .into_iter()
                .map(|item| substitute_plot_value(item, ctx, stage))
                .collect::<Result<Vec<_>>>()?;
            Ok(serde_yaml::Value::Sequence(values))
        }
        serde_yaml::Value::Mapping(map) => {
            let mut out = serde_yaml::Mapping::new();
            for (key, value) in map {
                let key = substitute_plot_value(key, ctx, stage)?;
                let value = substitute_plot_value(value, ctx, stage)?;
                out.insert(key, value);
            }
            Ok(serde_yaml::Value::Mapping(out))
        }
        serde_yaml::Value::Tagged(tagged) => {
            let mut tagged = tagged;
            tagged.value = substitute_plot_value(tagged.value, ctx, stage)?;
            Ok(serde_yaml::Value::Tagged(tagged))
        }
        other => Ok(other),
    }
}

/// Substitute a single string field, enriching any undefined-key error
/// with the stage name and field name.
fn sub_field(input: &str, ctx: &TemplateContext, stage: &str, field: &str) -> Result<String> {
    substitute(input, ctx).map_err(|e| match e {
        WorkflowError::TemplateUndefined { key, .. } => WorkflowError::TemplateUndefined {
            key,
            field: field.to_owned(),
            stage: stage.to_owned(),
        },
        other => other,
    })
}

fn sub_cmd_field(input: &str, ctx: &TemplateContext, stage: &str, field: &str) -> Result<String> {
    substitute_cmd(input, ctx).map_err(|e| match e {
        WorkflowError::TemplateUndefined { key, .. } => WorkflowError::TemplateUndefined {
            key,
            field: field.to_owned(),
            stage: stage.to_owned(),
        },
        other => other,
    })
}

/// Parse the top-level `plots:` list which accepts both simple path
/// strings and structured DVC-style plot definitions.
fn parse_raw_plots(raw_plots: &[serde_yaml::Value]) -> Result<(Vec<PathBuf>, Vec<PlotConfig>)> {
    let mut simple = Vec::new();
    let mut configs = Vec::new();

    for entry in raw_plots {
        match entry {
            serde_yaml::Value::String(s) => {
                simple.push(PathBuf::from(s));
            }
            serde_yaml::Value::Mapping(map) => {
                // Structured form: `{ "path/to/file.csv": { x: ..., y: ..., ... } }`
                // The map should have exactly one key (the file path).
                if map.len() != 1 {
                    return Err(WorkflowError::YamlInvalid {
                        key: "plots".to_owned(),
                        origin: "structured plot entry must have exactly one key (the file path)"
                            .to_owned(),
                    });
                }
                let Some((key, value)) = map.iter().next() else {
                    return Err(WorkflowError::YamlInvalid {
                        key: "plots".to_owned(),
                        origin: "structured plot entry must have exactly one key (the file path)"
                            .to_owned(),
                    });
                };
                let path_str = key.as_str().ok_or_else(|| WorkflowError::YamlInvalid {
                    key: "plots".to_owned(),
                    origin: "plot path key must be a string".to_owned(),
                })?;

                // Parse the value as plot options.
                let opts = value
                    .as_mapping()
                    .ok_or_else(|| WorkflowError::YamlInvalid {
                        key: "plots".to_owned(),
                        origin: format!("plot options for '{path_str}' must be a mapping"),
                    })?;

                let title = opts
                    .get(serde_yaml::Value::String("title".to_owned()))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);

                let template = opts
                    .get(serde_yaml::Value::String("template".to_owned()))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let no_header = opts
                    .get(serde_yaml::Value::String("no_header".to_owned()))
                    .or_else(|| opts.get(serde_yaml::Value::String("no-header".to_owned())))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let x_label = opts
                    .get(serde_yaml::Value::String("x_label".to_owned()))
                    .or_else(|| opts.get(serde_yaml::Value::String("x-label".to_owned())))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let y_label = opts
                    .get(serde_yaml::Value::String("y_label".to_owned()))
                    .or_else(|| opts.get(serde_yaml::Value::String("y-label".to_owned())))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);

                match opts.get(serde_yaml::Value::String("y".to_owned())) {
                    Some(serde_yaml::Value::Mapping(y_sources)) => {
                        for (source_path, y_value) in y_sources {
                            let source_path =
                                source_path
                                    .as_str()
                                    .ok_or_else(|| WorkflowError::YamlInvalid {
                                        key: "plots".to_owned(),
                                        origin: format!(
                                            "plot y source for '{path_str}' must be a string path"
                                        ),
                                    })?;
                            let path = PathBuf::from(source_path);
                            let x_source = plot_x_source_for_path(opts, source_path);
                            push_unique_path(&mut simple, path.clone());
                            configs.push(PlotConfig {
                                id: Some(path_str.to_owned()),
                                x: x_source.field,
                                x_path: x_source.path,
                                y: plot_fields(y_value),
                                path,
                                title: title.clone(),
                                no_header,
                                x_label: x_label.clone(),
                                y_label: y_label.clone(),
                                template: template.clone(),
                            });
                        }
                    }
                    maybe_y => {
                        let path = PathBuf::from(path_str);
                        push_unique_path(&mut simple, path.clone());
                        configs.push(PlotConfig {
                            id: None,
                            path,
                            x: opts
                                .get(serde_yaml::Value::String("x".to_owned()))
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                            x_path: None,
                            y: maybe_y.map(plot_fields).unwrap_or_default(),
                            no_header,
                            title,
                            x_label,
                            y_label,
                            template,
                        });
                    }
                }
            }
            _ => {
                return Err(WorkflowError::YamlInvalid {
                    key: "plots".to_owned(),
                    origin: "plot entry must be a string or a mapping".to_owned(),
                });
            }
        }
    }

    Ok((simple, configs))
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn plot_fields(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::String(s) => vec![s.clone()],
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

struct PlotXSource {
    path: Option<PathBuf>,
    field: Option<String>,
}

fn plot_x_source_for_path(opts: &serde_yaml::Mapping, path: &str) -> PlotXSource {
    match opts.get(serde_yaml::Value::String("x".to_owned())) {
        Some(serde_yaml::Value::String(s)) => PlotXSource {
            path: None,
            field: Some(s.clone()),
        },
        Some(serde_yaml::Value::Mapping(x_sources)) => {
            if let Some(field) = x_sources
                .get(serde_yaml::Value::String(path.to_owned()))
                .and_then(|value| value.as_str())
            {
                return PlotXSource {
                    path: None,
                    field: Some(field.to_owned()),
                };
            }

            let mut entries = x_sources.iter().filter_map(|(path, value)| {
                Some((PathBuf::from(path.as_str()?), value.as_str()?.to_owned()))
            });
            let Some((x_path, field)) = entries.next() else {
                return PlotXSource {
                    path: None,
                    field: None,
                };
            };
            if entries.next().is_some() {
                return PlotXSource {
                    path: None,
                    field: None,
                };
            }
            PlotXSource {
                path: Some(x_path),
                field: Some(field),
            }
        }
        _ => PlotXSource {
            path: None,
            field: None,
        },
    }
}

fn stage_from_raw(name: StageName, raw: RawStage, defaults: &Defaults) -> Result<Stage> {
    let cmd = raw.cmd.into_cmd(&name)?;

    let mut deps = Vec::with_capacity(raw.deps.len());
    for raw_dep in raw.deps {
        deps.push(raw_dep.into_dep(&name)?);
    }

    let mut outs = Vec::with_capacity(raw.outs.len());
    for raw_out in raw.outs {
        for out in raw_out.into_outs(&name)? {
            out.validate(&name)?;
            outs.push(out);
        }
    }

    // At most one Stdout out per stage.
    let stdout_count = outs.iter().filter(|o| o.kind == OutKind::Stdout).count();
    if stdout_count > 1 {
        return Err(WorkflowError::StageOutMalformed {
            stage: name.as_str().to_owned(),
            path: outs
                .iter()
                .filter(|o| o.kind == OutKind::Stdout)
                .nth(1)
                .map(|o| o.path.clone())
                .unwrap_or_default(),
            reason: "only one stdout out is allowed per stage",
        });
    }

    let mut metrics = Vec::new();
    for raw_metric in raw.metrics {
        for (metric_path, metric_out) in raw_metric.into_metrics(&name)? {
            metrics.push(metric_path);
            if let Some(metric_out) = metric_out {
                metric_out.validate(&name)?;
                push_compatible_out(&mut outs, metric_out, &name)?;
            }
        }
    }

    let mut params = Vec::new();
    for raw_param in raw.params {
        params.extend(parse_stage_param_ref(raw_param, &name)?);
    }

    let env = match raw.env {
        Some(e) => e.into_env_spec(&name)?,
        None => defaults.env.clone().unwrap_or(EnvSpec::Inherit),
    };

    let retry = match raw.retry {
        Some(r) => Some(r.into_policy(&name)?),
        None => defaults.retry.clone(),
    };

    let timeout = match raw.timeout.as_deref() {
        Some(s) => Some(parse_duration(s, &name, "timeout")?),
        None => None,
    };

    let wdir = match raw.wdir {
        Some(ref w) => {
            let path = PathBuf::from(w);
            validate_wdir(&path, &name)?;
            Some(path)
        }
        None => None,
    };

    let on_cache_hit = match raw.on_cache_hit {
        Some(c) => Some(c.into_cmd(&name)?),
        None => None,
    };

    // R15: `on_cache_hit` without `side_effects: true` is
    // almost certainly a mistake — reject rather than silently
    // never fire the hook.
    if on_cache_hit.is_some() && !raw.side_effects {
        return Err(WorkflowError::YamlInvalid {
            key: format!("stage '{name}'"),
            origin: "on_cache_hit requires side_effects: true".to_owned(),
        });
    }

    let resources = match raw.resources {
        Some(r) => r.into_resources(&name)?,
        None => Resources::default(),
    };

    let condition = match raw.condition {
        Some(c) => Some(c.into_condition(&name)?),
        None => None,
    };

    Ok(Stage {
        name,
        cmd,
        deps,
        outs,
        env,
        retry,
        timeout,
        wdir,
        persist: raw.persist,
        nondeterministic: raw.nondeterministic || raw.always_changed,
        hermetic: raw.hermetic,
        params,
        metrics,
        plots: parse_raw_plots(&raw.plots)?.0,
        side_effects: raw.side_effects,
        on_cache_hit,
        resources,
        frozen: raw.frozen,
        desc: raw.desc,
        meta: raw.meta,
        condition,
    })
}

fn parse_stage_param_ref(
    value: serde_yaml::Value,
    stage_name: &StageName,
) -> Result<Vec<ParamRef>> {
    match value {
        serde_yaml::Value::String(s) => Ok(vec![ParamRef::parse(&s)?]),
        serde_yaml::Value::Mapping(map) => {
            if map.len() != 1 {
                return Err(WorkflowError::YamlInvalid {
                    key: format!("stage '{stage_name}' params"),
                    origin: "file-scoped params entry must have exactly one params file key"
                        .to_owned(),
                });
            }
            let Some((file_value, refs_value)) = map.iter().next() else {
                return Err(WorkflowError::YamlInvalid {
                    key: format!("stage '{stage_name}' params"),
                    origin: "file-scoped params entry must have exactly one params file key"
                        .to_owned(),
                });
            };
            let file = file_value
                .as_str()
                .ok_or_else(|| WorkflowError::YamlInvalid {
                    key: format!("stage '{stage_name}' params"),
                    origin: "params file key must be a string".to_owned(),
                })
                .map(PathBuf::from)?;

            match refs_value {
                serde_yaml::Value::Null => Ok(vec![ParamRef::all_in_file(file)?]),
                serde_yaml::Value::Sequence(seq) => {
                    let mut refs = Vec::with_capacity(seq.len());
                    for item in seq {
                        let key = item.as_str().ok_or_else(|| WorkflowError::YamlInvalid {
                            key: format!("stage '{stage_name}' params.{}", file.display()),
                            origin: "file-scoped params entries must be strings".to_owned(),
                        })?;
                        refs.push(ParamRef::parse_in_file(file.clone(), key)?);
                    }
                    Ok(refs)
                }
                _ => Err(WorkflowError::YamlInvalid {
                    key: format!("stage '{stage_name}' params.{}", file.display()),
                    origin: "file-scoped params value must be a list of keys or null".to_owned(),
                }),
            }
        }
        _ => Err(WorkflowError::YamlInvalid {
            key: format!("stage '{stage_name}' params"),
            origin: "params entries must be strings or file-scoped maps".to_owned(),
        }),
    }
}

impl RawCmd {
    fn into_cmd(self, stage: &StageName) -> Result<Cmd> {
        match self {
            RawCmd::Shell(s) => Ok(Cmd::Shell(s)),
            RawCmd::ShellList(commands) => {
                if commands.is_empty() {
                    return Err(WorkflowError::YamlInvalid {
                        key: format!("stage '{stage}' cmd"),
                        origin: "cmd list must contain at least one command".to_owned(),
                    });
                }
                Ok(Cmd::ShellList(commands))
            }
            RawCmd::Argv(RawCmdArgv { argv }) => {
                if argv.is_empty() {
                    return Err(WorkflowError::YamlInvalid {
                        key: format!("stage '{stage}' cmd"),
                        origin: "argv must contain a program".to_owned(),
                    });
                }
                Ok(Cmd::Argv(argv))
            }
        }
    }
}

impl RawDep {
    fn into_dep(self, stage: &StageName) -> Result<Dep> {
        match self {
            RawDep::Path(s) => Ok(string_dep(s)),
            RawDep::Structured(s) => s.into_dep(stage),
        }
    }
}

fn string_dep(value: String) -> Dep {
    if is_url_dep(&value) {
        Dep::Url {
            url: value,
            digest: None,
        }
    } else {
        Dep::Path(PathBuf::from(value))
    }
}

impl RawDepStructured {
    fn into_dep(self, stage: &StageName) -> Result<Dep> {
        // Count which discriminator fields are set. Exactly one
        // must be present; zero or multiple is a config error.
        let fields = [
            self.path.is_some(),
            self.crab.is_some(),
            self.git.is_some(),
            self.url.is_some(),
            self.oci.is_some(),
            self.stage_out.is_some(),
        ];
        let set = fields.iter().filter(|b| **b).count();
        if set != 1 {
            return Err(WorkflowError::YamlInvalid {
                key: format!("stage '{stage}' dep"),
                origin: if set == 0 {
                    "dep map must set exactly one of \
                     path/crab/git/url/oci/stage_out"
                        .to_owned()
                } else {
                    "dep map must set exactly one of \
                     path/crab/git/url/oci/stage_out; found multiple"
                        .to_owned()
                },
            });
        }

        if let Some(path) = self.path {
            return Ok(Dep::Path(path));
        }
        if let Some(r) = self.crab {
            return Ok(Dep::CrabRef {
                repo: r.repo,
                rev: r.rev,
                path: r.path,
            });
        }
        if let Some(r) = self.git {
            return Ok(Dep::GitRef {
                url: r.url,
                rev: r.rev,
                path: r.path,
            });
        }
        if let Some(r) = self.url {
            return Ok(Dep::Url {
                url: r.url,
                digest: r.digest,
            });
        }
        if let Some(r) = self.oci {
            return Ok(Dep::OciImage {
                reference: r.reference,
                digest: r.digest,
            });
        }
        if let Some(r) = self.stage_out {
            let stage_name = StageName::parse(&r.stage)?;
            return Ok(Dep::StageOut {
                stage: stage_name,
                out: r.out,
            });
        }
        // Unreachable: the discriminator count above forces one branch.
        Err(WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' dep"),
            origin: "dep map discriminator check failed".to_owned(),
        })
    }
}

impl RawOut {
    fn into_outs(self, stage: &StageName) -> Result<Vec<Out>> {
        match self {
            RawOut::Path(s) => Ok(vec![string_out(s, stage)?]),
            RawOut::Structured(s) => Ok(vec![s.into_out(stage)?]),
            RawOut::DvcPathMap(entries) => {
                let mut outs = Vec::with_capacity(entries.len());
                for (path, settings) in entries {
                    let out = match settings {
                        Some(settings) => {
                            settings.into_out(output_path_from_string(&path, stage)?, stage)?
                        }
                        None => string_out(path, stage)?,
                    };
                    outs.push(out);
                }
                Ok(outs)
            }
        }
    }
}

fn push_compatible_out(outs: &mut Vec<Out>, out: Out, stage: &StageName) -> Result<()> {
    if let Some(existing) = outs.iter().find(|existing| existing.path == out.path) {
        if existing == &out {
            return Ok(());
        }
        return Err(WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' metrics"),
            origin: format!(
                "metric path '{}' duplicates an output with different settings",
                out.path.display()
            ),
        });
    }

    outs.push(out);
    Ok(())
}

fn string_out(value: String, stage: &StageName) -> Result<Out> {
    let path = output_path_from_string(&value, stage)?;
    let mut out = Out::new(path, OutKind::File);
    if out.path.is_absolute() || out.is_external_url() {
        out.cache = false;
        out.push = false;
    }
    Ok(out)
}

fn output_path_from_string(value: &str, stage: &StageName) -> Result<PathBuf> {
    if !value.to_ascii_lowercase().starts_with("file://") {
        return Ok(PathBuf::from(value));
    }
    let parsed = url::Url::parse(value).map_err(|_| WorkflowError::StageOutMalformed {
        stage: stage.as_str().to_owned(),
        path: PathBuf::from(value),
        reason: "invalid file:// output URL",
    })?;
    parsed
        .to_file_path()
        .map_err(|()| WorkflowError::StageOutMalformed {
            stage: stage.as_str().to_owned(),
            path: PathBuf::from(value),
            reason: "file:// output URL must resolve to a local filesystem path",
        })
}

impl RawMetric {
    fn into_metrics(self, stage: &StageName) -> Result<Vec<(PathBuf, Option<Out>)>> {
        match self {
            RawMetric::Path(s) => Ok(vec![(PathBuf::from(s), None)]),
            RawMetric::Structured(s) => {
                let path = s.path.clone();
                Ok(vec![(path, Some(s.into_out(stage)?))])
            }
            RawMetric::DvcPathMap(entries) => {
                let mut metrics = Vec::with_capacity(entries.len());
                for (path, settings) in entries {
                    let path = PathBuf::from(path);
                    let out = match settings {
                        Some(settings) => settings.into_out(path.clone(), stage)?,
                        None => Out::new(path.clone(), OutKind::File),
                    };
                    metrics.push((path, Some(out)));
                }
                Ok(metrics)
            }
        }
    }
}

impl RawOutStructured {
    fn into_out(self, stage: &StageName) -> Result<Out> {
        let RawOutStructured {
            path,
            kind,
            cache,
            push,
            persist,
            checkpoint,
            max_bytes,
            remote,
            _desc: _,
        } = self;
        let path = output_path_from_string(&path.to_string_lossy(), stage)?;

        let checkpoint = match checkpoint {
            None => false,
            Some(serde_yaml::Value::Bool(value)) => value,
            Some(_) => {
                return Err(WorkflowError::YamlInvalid {
                    key: format!(
                        "stage '{}' output '{}.checkpoint'",
                        stage.as_str(),
                        path.display()
                    ),
                    origin: "checkpoint must be a boolean".to_owned(),
                });
            }
        };

        let kind = match kind.as_deref() {
            None | Some("file") => OutKind::File,
            Some("directory" | "dir") => OutKind::Directory,
            Some("stdout") => OutKind::Stdout,
            Some(_) => {
                return Err(WorkflowError::StageOutMalformed {
                    stage: stage.as_str().to_owned(),
                    path,
                    reason: "out 'kind' must be 'file', 'directory', or 'stdout'",
                });
            }
        };

        let external = path.is_absolute() || is_external_url_out_path(&path);
        Ok(Out {
            path,
            kind,
            cache: cache.unwrap_or(!external),
            push: push.unwrap_or(!external),
            remote,
            persist: persist.unwrap_or(false),
            checkpoint,
            max_bytes,
        })
    }
}

impl RawDvcOutSettings {
    fn into_out(self, path: PathBuf, stage: &StageName) -> Result<Out> {
        let RawDvcOutSettings {
            kind,
            cache,
            push,
            persist,
            checkpoint,
            max_bytes,
            remote,
            _desc: _,
        } = self;
        RawOutStructured {
            path,
            kind,
            cache,
            push,
            persist,
            checkpoint,
            max_bytes,
            remote,
            _desc: None,
        }
        .into_out(stage)
    }
}

impl RawEnv {
    fn into_env_spec(self, stage: &StageName) -> Result<EnvSpec> {
        match self {
            RawEnv::Named(s) => match s.as_str() {
                "inherit" => Ok(EnvSpec::Inherit),
                "empty" => Ok(EnvSpec::Empty),
                "allowlist" => {
                    // `env: allowlist` alone with no list is
                    // ambiguous; treat it as the empty allowlist
                    // which excludes all inherited vars.
                    Ok(EnvSpec::Allowlist(Vec::new()))
                }
                other => Err(WorkflowError::YamlInvalid {
                    key: format!("stage '{stage}' env"),
                    origin: format!("unknown env policy '{other}'"),
                }),
            },
            RawEnv::Allowlist(items) => Ok(EnvSpec::Allowlist(items)),
        }
    }
}

impl RawRetry {
    fn into_policy(self, stage: &StageName) -> Result<RetryPolicy> {
        let default = RetryPolicy::no_retry();
        let initial_backoff = match self.initial_backoff.as_deref() {
            Some(s) => parse_duration(s, stage, "retry.initial_backoff")?,
            None => default.initial_backoff,
        };
        let max_backoff = match self.max_backoff.as_deref() {
            Some(s) => parse_duration(s, stage, "retry.max_backoff")?,
            None => default.max_backoff,
        };
        Ok(RetryPolicy {
            max_attempts: self.max_attempts.unwrap_or(default.max_attempts),
            initial_backoff,
            max_backoff,
            backoff_multiplier: self
                .backoff_multiplier
                .unwrap_or(default.backoff_multiplier),
            on_exit_codes: self.on_exit_codes,
            on_signals: self.on_signals,
            on_timeout: self.on_timeout.unwrap_or(default.on_timeout),
        })
    }
}

impl RawResources {
    fn into_resources(self, stage: &StageName) -> Result<Resources> {
        let memory_bytes = match self.memory.as_deref() {
            Some(s) => parse_memory(s, stage)?,
            None => 0,
        };
        Ok(Resources {
            cpu: self.cpu.unwrap_or(1),
            gpu: self.gpu.unwrap_or(0),
            memory_bytes,
        })
    }
}

impl RawCondition {
    fn into_condition(self, stage: &StageName) -> Result<StageCondition> {
        let fields = [
            self.env.is_some(),
            self.file_exists.is_some(),
            self.expr.is_some(),
        ];
        let set = fields.iter().filter(|b| **b).count();
        if set != 1 {
            return Err(WorkflowError::YamlInvalid {
                key: format!("stage '{stage}' condition"),
                origin: if set == 0 {
                    "condition must set exactly one of env/file_exists/expr".to_owned()
                } else {
                    "condition must set exactly one of env/file_exists/expr; found multiple"
                        .to_owned()
                },
            });
        }

        if let Some(var) = self.env {
            return Ok(StageCondition::Env(var));
        }
        if let Some(path) = self.file_exists {
            return Ok(StageCondition::FileExists(PathBuf::from(path)));
        }
        if let Some(expr) = self.expr {
            return Ok(StageCondition::Expr(expr));
        }
        // Unreachable given the count check above.
        Err(WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' condition"),
            origin: "condition discriminator check failed".to_owned(),
        })
    }
}

impl TryFrom<RawDefaults> for Defaults {
    type Error = WorkflowError;

    fn try_from(raw: RawDefaults) -> Result<Self> {
        // Defaults live outside any stage; use a synthetic name so
        // downstream error messages are still coherent.
        let synthetic = StageName::parse("_defaults")?;
        let env = match raw.env {
            Some(e) => Some(e.into_env_spec(&synthetic)?),
            None => None,
        };
        let retry = match raw.retry {
            Some(r) => Some(r.into_policy(&synthetic)?),
            None => None,
        };
        Ok(Defaults { env, retry })
    }
}

/// Parse a human-readable duration like `30s`, `5m`, `6h`, or
/// `500ms`. Bare integers are treated as seconds. Errors are
/// attributed to `<stage>.<field>` so users can jump to the YAML
/// line that tripped.
fn parse_duration(raw: &str, stage: &StageName, field: &str) -> Result<Duration> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' {field}"),
            origin: "duration must not be empty".to_owned(),
        });
    }

    // Order matters: check `ms` before `s` because both end in 's'.
    let (num_str, unit_ns): (&str, u128) = if let Some(n) = trimmed.strip_suffix("ms") {
        (n, 1_000_000)
    } else if let Some(n) = trimmed.strip_suffix('s') {
        (n, 1_000_000_000)
    } else if let Some(n) = trimmed.strip_suffix('m') {
        (n, 60u128 * 1_000_000_000)
    } else if let Some(n) = trimmed.strip_suffix('h') {
        (n, 3600u128 * 1_000_000_000)
    } else {
        // Bare integer → seconds.
        (trimmed, 1_000_000_000)
    };

    let value: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' {field}"),
            origin: format!("invalid duration '{raw}'"),
        })?;
    let nanos = u128::from(value).saturating_mul(unit_ns);
    let secs = (nanos / 1_000_000_000) as u64;
    let rem_nanos = (nanos % 1_000_000_000) as u32;
    Ok(Duration::new(secs, rem_nanos))
}

/// Parse a human-readable memory size like `"16G"`, `"512M"`,
/// `"1024K"`, or `"1073741824"` (bare bytes). Case-insensitive
/// suffixes: `K`/`KB` = KiB, `M`/`MB` = MiB, `G`/`GB` = GiB,
/// `T`/`TB` = TiB.
fn parse_memory(raw: &str, stage: &StageName) -> Result<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' resources.memory"),
            origin: "memory value must not be empty".to_owned(),
        });
    }

    let upper = trimmed.to_ascii_uppercase();
    let (num_str, multiplier): (&str, u64) = if let Some(n) = upper.strip_suffix("TB") {
        (n, 1024 * 1024 * 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix("GB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix("KB") {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('T') {
        (n, 1024 * 1024 * 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix('G') {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix('M') {
        (n, 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix('K') {
        (n, 1024)
    } else {
        (upper.as_str(), 1)
    };

    // num_str references `upper` which is a local — reborrow from
    // the original trimmed string at the same byte offset.
    let num_len = num_str.len();
    let num_from_raw = &trimmed[..num_len];

    let value: u64 = num_from_raw
        .trim()
        .parse()
        .map_err(|_| WorkflowError::YamlInvalid {
            key: format!("stage '{stage}' resources.memory"),
            origin: format!("invalid memory size '{raw}'"),
        })?;

    Ok(value.saturating_mul(multiplier))
}

/// Run all semantic validation checks on a parsed [`Workflow`],
/// collecting ALL errors rather than stopping at the first.
///
/// Checks performed:
/// - Self-loops: a stage dep that is also one of its own outs.
/// - Timeout/retry value ranges (max_attempts >= 1, backoff > 0).
/// - Duplicate out paths across stages (also caught by Graph::build,
///   but included here for completeness in --validate mode).
pub fn validate_semantics(workflow: &Workflow) -> Vec<WorkflowError> {
    let mut errors = Vec::new();

    for (name, stage) in &workflow.stages {
        // Self-loop check: dep path == own out path.
        let out_paths: std::collections::HashSet<&std::path::Path> =
            stage.outs.iter().map(|o| o.path.as_path()).collect();
        for dep in &stage.deps {
            if let Dep::Path(dep_path) = dep
                && out_paths.contains(dep_path.as_path())
            {
                errors.push(WorkflowError::WorkflowSelfLoop {
                    stage: name.as_str().to_owned(),
                    path: dep_path.clone(),
                });
            }
        }

        // Retry value range checks.
        if let Some(ref retry) = stage.retry {
            if retry.max_attempts == 0 {
                errors.push(WorkflowError::WorkflowValidation {
                    field: format!("stage '{name}' retry.max_attempts"),
                    value: "0".to_owned(),
                    expected: "integer >= 1".to_owned(),
                });
            }
            if retry.backoff_multiplier < 0.0 {
                errors.push(WorkflowError::WorkflowValidation {
                    field: format!("stage '{name}' retry.backoff_multiplier"),
                    value: retry.backoff_multiplier.to_string(),
                    expected: "positive number".to_owned(),
                });
            }
        }

        // Timeout range check: zero timeout is likely a mistake.
        if let Some(ref timeout) = stage.timeout
            && timeout.is_zero()
        {
            errors.push(WorkflowError::WorkflowValidation {
                field: format!("stage '{name}' timeout"),
                value: "0".to_owned(),
                expected: "duration > 0 (e.g. '30s', '5m')".to_owned(),
            });
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_single_stage() {
        let yaml = r#"
stages:
  clean:
    cmd: "python clean.py"
"#;
        let wf = parse(yaml).expect("parse");
        assert_eq!(wf.stages.len(), 1);
        let stage = wf
            .stages
            .get(&StageName::parse("clean").unwrap())
            .expect("clean");
        assert!(matches!(stage.cmd, Cmd::Shell(ref s) if s == "python clean.py"));
        assert!(stage.deps.is_empty());
        assert!(stage.outs.is_empty());
    }

    #[test]
    fn parses_dvc_cmd_list_as_separate_shell_commands() {
        let yaml = r#"
stages:
  build:
    cmd:
      - mkdir -p output
      - python build.py
      - python validate.py
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("build").unwrap())
            .expect("build");
        assert!(matches!(
            stage.cmd,
            Cmd::ShellList(ref commands) if commands == &vec![
                "mkdir -p output".to_owned(),
                "python build.py".to_owned(),
                "python validate.py".to_owned(),
            ]
        ));
    }

    #[test]
    fn rejects_empty_argv_command() {
        let yaml = r#"
stages:
  empty:
    cmd:
      argv: []
"#;
        let error = parse(yaml).unwrap_err();
        assert!(matches!(
            error,
            WorkflowError::YamlInvalid { key, origin }
                if key == "stage 'empty' cmd" && origin == "argv must contain a program"
        ));
    }

    #[test]
    fn accepts_dvc_top_level_artifacts_metadata() {
        let yaml = r#"
artifacts:
  cv-classification:
    path: models/resnet.pt
    type: model
    desc: CV classification model
    labels:
      - resnet50
      - classification
    meta:
      framework: pytorch
stages:
  train:
    cmd: "python train.py"
    outs:
      - models/resnet.pt
"#;
        let wf = parse(yaml).expect("parse DVC artifacts metadata");
        assert_eq!(wf.stages.len(), 1);
        assert!(wf.stages.contains_key(&StageName::parse("train").unwrap()));
        assert_eq!(
            wf.artifacts.schema_version,
            ArtifactMetadata::SCHEMA_VERSION
        );
        assert_eq!(wf.artifacts.declarations.len(), 1);
        assert!(wf.artifacts.declarations.contains_key("cv-classification"));
    }

    #[test]
    fn preserves_checkpoint_output_semantics() {
        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    outs:
      - path: model.pt
        checkpoint: true
"#;
        let workflow = parse(yaml).expect("checkpoint field should remain explicit");
        assert!(workflow.stages[&StageName::parse("train").unwrap()].outs[0].checkpoint);
    }

    #[test]
    fn rejects_empty_dvc_cmd_list() {
        let yaml = r#"
stages:
  build:
    cmd: []
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::YamlInvalid { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn parses_full_schema() {
        let yaml = r#"
params:
  - params.yaml
metrics:
  - metrics/train.json
plots:
  - metrics/roc.csv
defaults:
  env: allowlist
  retry:
    max_attempts: 2
stages:
  clean:
    cmd: "python clean.py"
    deps:
      - data/raw.csv
      - src/clean.py
    outs:
      - data/clean.parquet
  train:
    cmd:
      argv: ["python", "train.py"]
    deps:
      - data/clean.parquet
    params:
      - model.lr
      - model.epochs
    outs:
      - path: models/model.pkl
        kind: file
        cache: true
        max_bytes: 1048576
    metrics:
      - metrics/train.json
    env:
      - CUDA_VISIBLE_DEVICES
    retry:
      max_attempts: 3
      on_signals: [9]
    timeout: "6h"
  notify:
    cmd: "./notify.sh"
    deps:
      - reports/summary.html
    side_effects: true
    on_cache_hit: "./notify.sh --resend"
"#;
        let wf = parse(yaml).expect("parse full schema");
        assert_eq!(wf.params, vec![PathBuf::from("params.yaml")]);
        assert_eq!(wf.metrics, vec![PathBuf::from("metrics/train.json")]);
        assert_eq!(wf.plots, vec![PathBuf::from("metrics/roc.csv")]);

        let train = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(
            matches!(train.cmd, Cmd::Argv(ref v) if v == &vec!["python".to_owned(), "train.py".to_owned()])
        );
        assert_eq!(train.params.len(), 2);
        assert_eq!(train.params[0].as_str(), "model.lr");
        assert_eq!(train.outs.len(), 1);
        assert_eq!(train.outs[0].max_bytes, Some(1_048_576));
        assert!(
            matches!(train.env, EnvSpec::Allowlist(ref v) if v == &vec!["CUDA_VISIBLE_DEVICES".to_owned()])
        );
        let retry = train.retry.as_ref().expect("train retry");
        assert_eq!(retry.max_attempts, 3);
        assert_eq!(retry.on_signals, vec![9]);
        assert_eq!(train.timeout, Some(Duration::from_secs(6 * 3600)));

        let notify = wf
            .stages
            .get(&StageName::parse("notify").unwrap())
            .expect("notify stage");
        assert!(notify.side_effects);
        assert!(
            matches!(notify.on_cache_hit, Some(Cmd::Shell(ref s)) if s == "./notify.sh --resend")
        );
    }

    #[test]
    fn parses_file_scoped_stage_params() {
        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    params:
      - model.lr
      - custom.yaml:
          - epochs
          - model.dropout
      - all.json:
"#;
        let wf = parse(yaml).expect("parse file-scoped params");
        let train = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");

        assert_eq!(train.params.len(), 4);
        assert_eq!(train.params[0].as_str(), "model.lr");
        assert_eq!(train.params[1].file(), Some(Path::new("custom.yaml")));
        assert_eq!(train.params[1].key(), Some("epochs"));
        assert_eq!(train.params[1].lock_key_for("epochs"), "custom.yaml:epochs");
        assert_eq!(train.params[2].file(), Some(Path::new("custom.yaml")));
        assert_eq!(train.params[2].key(), Some("model.dropout"));
        assert_eq!(train.params[3].file(), Some(Path::new("all.json")));
        assert!(train.params[3].tracks_all());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let yaml = r#"
surprise: true
stages:
  a:
    cmd: "true"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::YamlParse { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn rejects_unknown_stage_field() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    whatever: 5
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::YamlParse { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn rejects_unknown_retry_field() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    retry:
      max_attempts: 3
      bogus: 1
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::YamlParse { .. }));
    }

    #[test]
    fn rejects_invalid_stage_name() {
        let yaml = r#"
stages:
  "bad name":
    cmd: "true"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::StageNameInvalid { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn parse_error_display_includes_line_number() {
        // Malformed YAML — unterminated mapping. serde_yaml's
        // Display impl includes the position; we only need to
        // confirm a digit is somewhere in the error text.
        let yaml = "stages:\n  clean:\n    cmd: [\n";
        let err = parse(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.chars().any(|c| c.is_ascii_digit()),
            "no line number in: {msg}"
        );
    }

    #[test]
    fn parse_error_surfaces_source_location() {
        // Confirm `serde_yaml::Error::location()` yields a line
        // position through the `#[source]` chain. This is what
        // the error catalog's `details_json` will report.
        let yaml = "stages:\n  clean:\n    cmd: [unterminated\n";
        let err = parse(yaml).unwrap_err();
        let WorkflowError::YamlParse { source, .. } = &err else {
            panic!("wrong variant: {err}");
        };
        let loc = source.location().expect("serde_yaml reports location");
        assert!(loc.line() >= 1);
    }

    #[test]
    fn parse_at_attaches_path_to_error() {
        let path = std::path::PathBuf::from("workflows/broken.yaml");
        let err = parse_at(&path, "stages: [oops").unwrap_err();
        match err {
            WorkflowError::YamlParse { path: p, .. } => assert_eq!(p, path),
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn parses_all_dep_forms() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    deps:
      - data/raw.csv
      - path: data/clean.parquet
      - crab:
          repo: "bucket/other"
          rev: "main"
          path: "shared/ref.bin"
      - git:
          url: "https://example.com/repo.git"
          rev: "deadbeef"
          path: "src/util.py"
      - url:
          url: "https://example.com/blob.bin"
          digest: "b3:0000"
      - oci:
          reference: "registry/img:tag"
          digest: "sha256:0000"
      - stage_out:
          stage: "upstream"
          out: "out/file.bin"
"#;
        let wf = parse(yaml).expect("parse all dep forms");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.deps.len(), 7);
        assert!(matches!(stage.deps[0], Dep::Path(ref p) if p == &PathBuf::from("data/raw.csv")));
        assert!(
            matches!(stage.deps[1], Dep::Path(ref p) if p == &PathBuf::from("data/clean.parquet"))
        );
        assert!(matches!(stage.deps[2], Dep::CrabRef { .. }));
        assert!(matches!(stage.deps[3], Dep::GitRef { .. }));
        assert!(matches!(stage.deps[4], Dep::Url { .. }));
        assert!(matches!(stage.deps[5], Dep::OciImage { .. }));
        assert!(matches!(stage.deps[6], Dep::StageOut { .. }));
    }

    #[test]
    fn parses_dvc_style_url_string_deps_as_url_deps() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    deps:
      - https://example.com/data.txt
      - s3://bucket/data.txt
      - data/local.txt
"#;
        let wf = parse(yaml).expect("parse url string deps");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert!(
            matches!(stage.deps[0], Dep::Url { ref url, digest: None } if url == "https://example.com/data.txt")
        );
        assert!(
            matches!(stage.deps[1], Dep::Url { ref url, digest: None } if url == "s3://bucket/data.txt")
        );
        assert!(
            matches!(stage.deps[2], Dep::Path(ref path) if path == &PathBuf::from("data/local.txt"))
        );
    }

    #[test]
    fn parses_out_forms() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    outs:
      - data/simple.bin
      - path: data/dir
        kind: directory
        cache: false
        push: false
        persist: true
        max_bytes: 4096
      - model.pkl:
          cache: false
          push: false
          persist: true
          desc: "compat metadata"
      - metrics.json:
"#;
        let wf = parse(yaml).expect("parse outs");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.outs.len(), 4);
        assert_eq!(stage.outs[0].kind, OutKind::File);
        assert!(stage.outs[0].cache);
        assert_eq!(stage.outs[1].kind, OutKind::Directory);
        assert!(!stage.outs[1].cache);
        assert!(!stage.outs[1].push);
        assert!(stage.outs[1].persist);
        assert_eq!(stage.outs[1].max_bytes, Some(4096));
        assert_eq!(stage.outs[2].path, PathBuf::from("model.pkl"));
        assert!(!stage.outs[2].cache);
        assert!(!stage.outs[2].push);
        assert!(stage.outs[2].persist);
        assert_eq!(stage.outs[3].path, PathBuf::from("metrics.json"));
        assert!(stage.outs[3].cache);
    }

    #[test]
    fn parses_file_url_outs_as_uncached_external_local_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = url::Url::from_file_path(tmp.path().join("raw.bin"))
            .unwrap()
            .to_string();
        let mapped = url::Url::from_file_path(tmp.path().join("mapped.bin"))
            .unwrap()
            .to_string();
        let structured = url::Url::from_file_path(tmp.path().join("structured.bin"))
            .unwrap()
            .to_string();
        let yaml = format!(
            "stages:\n  a:\n    cmd: \"true\"\n    outs:\n      - \"{raw}\"\n      - \"{mapped}\":\n          persist: true\n      - path: \"{structured}\"\n"
        );

        let wf = parse(&yaml).expect("parse file URL outs");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.outs.len(), 3);
        for out in &stage.outs {
            assert!(out.path.is_absolute());
            assert!(!out.cache);
            assert!(!out.push);
        }
        assert!(stage.outs[1].persist);
    }

    #[test]
    fn parses_provider_url_outs_as_uncached_external_outputs() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    outs:
      - s3://bucket/model.pkl
      - azure://container/metrics.json:
          kind: directory
          persist: true
      - path: remote://models/checkpoints/latest
"#;

        let wf = parse(yaml).expect("parse provider URL outs");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();

        assert_eq!(stage.outs.len(), 3);
        assert_eq!(stage.outs[0].path, PathBuf::from("s3://bucket/model.pkl"));
        assert_eq!(
            stage.outs[1].path,
            PathBuf::from("azure://container/metrics.json")
        );
        assert_eq!(stage.outs[1].kind, OutKind::Directory);
        assert!(stage.outs[1].persist);
        assert_eq!(
            stage.outs[2].path,
            PathBuf::from("remote://models/checkpoints/latest")
        );
        for out in &stage.outs {
            assert!(out.is_external_url());
            assert!(!out.cache);
            assert!(!out.push);
        }
    }

    #[test]
    fn parses_dvc_path_key_out_with_remote() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    outs:
      - model.pkl:
          remote: cold-storage
"#;
        let wf = parse(yaml).expect("parse out remote");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.outs[0].remote.as_deref(), Some("cold-storage"));
    }

    #[test]
    fn parses_structured_out_with_remote() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    outs:
      - path: model.pkl
        remote: cold-storage
"#;
        let wf = parse(yaml).expect("parse structured out remote");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.outs[0].remote.as_deref(), Some("cold-storage"));
    }

    #[test]
    fn rejects_empty_out_remote_name() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    outs:
      - model.pkl:
          remote: ""
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::StageOutMalformed { .. }));
    }

    #[test]
    fn rejects_out_with_double_dot() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    outs:
      - "outs/../escape"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::StageOutMalformed { .. }));
    }

    #[test]
    fn rejects_on_cache_hit_without_side_effects() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    on_cache_hit: "./replay.sh"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::YamlInvalid { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn rejects_dep_map_with_multiple_discriminators() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    deps:
      - path: "a.bin"
        url:
          url: "https://x"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::YamlInvalid { .. }));
    }

    #[test]
    fn rejects_dep_map_with_no_discriminator() {
        // Empty map — serde will accept it because all fields are
        // Option and default, but `into_dep` must reject it.
        let yaml = r#"
stages:
  a:
    cmd: "true"
    deps:
      - {}
"#;
        let err = parse(yaml).unwrap_err();
        // An empty inline map parses as RawDepStructured with all
        // Nones, which is our error path.
        assert!(
            matches!(
                err,
                WorkflowError::YamlInvalid { .. } | WorkflowError::YamlParse { .. }
            ),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn env_accepts_named_and_list_forms() {
        let yaml_named = r#"
stages:
  a:
    cmd: "true"
    env: empty
"#;
        let wf = parse(yaml_named).expect("named env");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert!(matches!(stage.env, EnvSpec::Empty));

        let yaml_list = r#"
stages:
  b:
    cmd: "true"
    env: ["PATH", "HOME"]
"#;
        let wf = parse(yaml_list).expect("list env");
        let stage = wf.stages.get(&StageName::parse("b").unwrap()).unwrap();
        assert!(
            matches!(stage.env, EnvSpec::Allowlist(ref v) if v == &vec!["PATH".to_owned(), "HOME".to_owned()])
        );
    }

    #[test]
    fn defaults_apply_when_stage_omits_fields() {
        let yaml = r#"
defaults:
  env: empty
  retry:
    max_attempts: 5
stages:
  a:
    cmd: "true"
"#;
        let wf = parse(yaml).expect("defaults");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert!(matches!(stage.env, EnvSpec::Empty));
        let retry = stage.retry.as_ref().expect("inherited retry");
        assert_eq!(retry.max_attempts, 5);
    }

    #[test]
    fn stage_retry_overrides_defaults() {
        let yaml = r#"
defaults:
  retry:
    max_attempts: 5
stages:
  a:
    cmd: "true"
    retry:
      max_attempts: 2
"#;
        let wf = parse(yaml).expect("override");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.retry.as_ref().unwrap().max_attempts, 2);
    }

    #[test]
    fn timeout_parses_hms_suffixes() {
        for (input, expected_secs) in [("90s", 90u64), ("5m", 300), ("2h", 7200), ("500ms", 0)] {
            let yaml = format!("stages:\n  a:\n    cmd: \"true\"\n    timeout: \"{input}\"\n");
            let wf = parse(&yaml).expect("timeout");
            let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
            let got = stage.timeout.expect("timeout set");
            assert_eq!(got.as_secs(), expected_secs, "input {input}");
        }
    }

    #[test]
    fn timeout_rejects_invalid_string() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    timeout: "forever"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::YamlInvalid { .. }));
    }

    #[test]
    fn stage_name_rejects_unicode() {
        let yaml = r#"
stages:
  "café":
    cmd: "true"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));
    }

    #[test]
    fn stage_name_rejects_slash() {
        let yaml = r#"
stages:
  "nested/name":
    cmd: "true"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(matches!(err, WorkflowError::StageNameInvalid { .. }));
    }

    #[test]
    fn roundtrip_yields_equivalent_workflow() {
        // Parse the canonical full-schema example, re-emit each
        // stage via `serde_yaml::to_string` using [`Stage`]'s derive,
        // then re-parse the emitted fragment — the resulting Stage
        // must be field-equal to the original. This guards against
        // silent fields being dropped in the raw → Stage lowering.
        let yaml = r#"
stages:
  a:
    cmd: "echo hi"
    deps: [in.txt]
    outs: [out.txt]
    nondeterministic: true
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        let emitted = serde_yaml::to_string(stage).expect("serialize");
        let reparsed: Stage = serde_yaml::from_str(&emitted).expect("reparse");
        assert_eq!(stage, &reparsed);
    }

    #[test]
    fn dvc_always_changed_sets_nondeterministic_flag() {
        let yaml = r#"
stages:
  pull_latest:
    cmd: "python pull_latest.py"
    deps:
      - pull_latest.py
    outs:
      - latest.csv
    always_changed: true
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("pull_latest").unwrap())
            .unwrap();
        assert!(stage.nondeterministic);
        assert!(stage.always_changed());
    }

    #[test]
    fn retry_durations_parse() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    retry:
      max_attempts: 3
      initial_backoff: "500ms"
      max_backoff: "10s"
      backoff_multiplier: 2.0
      on_exit_codes: [1, 2]
      on_timeout: true
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        let retry = stage.retry.as_ref().expect("retry");
        assert_eq!(retry.max_attempts, 3);
        assert_eq!(retry.initial_backoff, Duration::from_millis(500));
        assert_eq!(retry.max_backoff, Duration::from_secs(10));
        assert!((retry.backoff_multiplier - 2.0).abs() < f64::EPSILON);
        assert_eq!(retry.on_exit_codes, vec![1, 2]);
        assert!(retry.on_timeout);
    }

    #[test]
    fn dvc_plot_id_is_preserved_for_multi_source_plots() {
        let yaml = r#"
plots:
  - train_val_test:
      x: epoch
      y:
        metrics/train.csv: [train_loss, val_loss]
        metrics/test.csv: test_loss
      title: Compare loss
      template: linear
"#;
        let wf = parse(yaml).expect("parse plot id");

        assert_eq!(
            wf.plots,
            vec![
                PathBuf::from("metrics/train.csv"),
                PathBuf::from("metrics/test.csv"),
            ]
        );
        assert_eq!(wf.plot_configs.len(), 2);
        assert!(
            wf.plot_configs
                .iter()
                .all(|config| config.id.as_deref() == Some("train_val_test"))
        );
        assert!(wf.plot_configs.iter().any(|config| config.path.as_path()
            == Path::new("metrics/train.csv")
            && config.y == vec!["train_loss", "val_loss"]));
        assert!(wf.plot_configs.iter().any(|config| config.path.as_path()
            == Path::new("metrics/test.csv")
            && config.y == vec!["test_loss"]));
    }

    #[test]
    fn dvc_plot_id_can_source_x_from_a_different_file() {
        let yaml = r#"
plots:
  - confusion:
      x:
        actual.csv: actual_class
      y:
        preds.csv: predicted_class
      template: confusion
"#;
        let wf = parse(yaml).expect("parse cross-file plot");

        assert_eq!(wf.plots, vec![PathBuf::from("preds.csv")]);
        assert_eq!(wf.plot_configs.len(), 1);
        let config = &wf.plot_configs[0];
        assert_eq!(config.id.as_deref(), Some("confusion"));
        assert_eq!(config.path, PathBuf::from("preds.csv"));
        assert_eq!(config.x_path, Some(PathBuf::from("actual.csv")));
        assert_eq!(config.x.as_deref(), Some("actual_class"));
        assert_eq!(config.y, vec!["predicted_class"]);
        assert_eq!(config.template.as_deref(), Some("confusion"));
    }

    #[test]
    fn parses_resources_block() {
        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    resources:
      gpu: 1
      memory: "16G"
      cpu: 4
"#;
        let wf = parse(yaml).expect("parse resources");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert_eq!(stage.resources.gpu, 1);
        assert_eq!(stage.resources.cpu, 4);
        assert_eq!(stage.resources.memory_bytes, 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn resources_defaults_when_omitted() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.resources.cpu, 1);
        assert_eq!(stage.resources.gpu, 0);
        assert_eq!(stage.resources.memory_bytes, 0);
    }

    #[test]
    fn resources_partial_fields() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    resources:
      gpu: 2
"#;
        let wf = parse(yaml).expect("parse partial resources");
        let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
        assert_eq!(stage.resources.gpu, 2);
        assert_eq!(stage.resources.cpu, 1);
        assert_eq!(stage.resources.memory_bytes, 0);
    }

    #[test]
    fn resources_memory_suffixes() {
        for (input, expected) in [
            ("1K", 1024u64),
            ("1KB", 1024),
            ("512M", 512 * 1024 * 1024),
            ("512MB", 512 * 1024 * 1024),
            ("2G", 2 * 1024 * 1024 * 1024),
            ("2GB", 2 * 1024 * 1024 * 1024),
            ("1T", 1024u64 * 1024 * 1024 * 1024),
            ("1073741824", 1073741824),
        ] {
            let yaml = format!(
                "stages:\n  a:\n    cmd: \"true\"\n    resources:\n      memory: \"{input}\"\n"
            );
            let wf = parse(&yaml).unwrap_or_else(|e| panic!("failed for {input}: {e}"));
            let stage = wf.stages.get(&StageName::parse("a").unwrap()).unwrap();
            assert_eq!(
                stage.resources.memory_bytes, expected,
                "memory mismatch for input '{input}'"
            );
        }
    }

    #[test]
    fn resources_rejects_unknown_field() {
        let yaml = r#"
stages:
  a:
    cmd: "true"
    resources:
      gpu: 1
      tpu: 2
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::YamlParse { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn template_substitution_in_cmd() {
        let vars = serde_yaml::from_str("codedir: src").unwrap();
        let params = serde_yaml::from_str("model:\n  lr: 0.001").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd: "python ${codedir}/train.py --lr ${model.lr}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse with context");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python src/train.py --lr 0.001"
        ));
    }

    #[test]
    fn parse_with_base_dir_loads_default_params_yaml_for_templates() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.yaml"),
            "codedir: src\nmodel:\n  lr: 0.001\n  filename: model.pkl\n",
        )
        .unwrap();

        let yaml = r#"
stages:
  train:
    cmd: "python ${codedir}/train.py --lr ${model.lr}"
    outs:
      - ${model.filename}
"#;
        let wf = parse_with_base_dir(yaml, tmp.path()).expect("parse with default params.yaml");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python src/train.py --lr 0.001"
        ));
        assert_eq!(stage.outs[0].path, PathBuf::from("model.pkl"));
    }

    #[test]
    fn vars_python_params_file_drives_template_substitution() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.py"),
            "codedir = 'src'\nclass Train:\n    lr = 0.01\n",
        )
        .unwrap();

        let yaml = r#"
vars:
  - params.py
stages:
  train:
    cmd: "python ${codedir}/train.py --lr ${Train.lr}"
"#;
        let wf = parse_with_base_dir(yaml, tmp.path()).expect("parse with Python vars file");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python src/train.py --lr 0.01"
        ));
    }

    #[test]
    fn vars_file_selector_drives_template_substitution() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.yaml"),
            "clean:\n  filename: data/clean.csv\nfeats:\n  dirname: features\ntrain:\n  epochs: 5\n",
        )
        .unwrap();

        let yaml = r#"
vars:
  - params.yaml:
      - clean
      - feats
stages:
  featurize:
    cmd: "python featurize.py ${clean.filename} ${feats.dirname}"
"#;
        let wf = parse_with_base_dir(yaml, tmp.path()).expect("parse with vars selector");
        let stage = wf
            .stages
            .get(&StageName::parse("featurize").unwrap())
            .expect("featurize stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python featurize.py data/clean.csv features"
        ));
    }

    #[test]
    fn command_template_unpacks_dictionary_params() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params = serde_yaml::from_str(
            "mydict:\n  foo: foo\n  bar: 1\n  bool: true\n  nested:\n    baz: bar\n  list: [2, 3, 'qux']\n",
        )
        .unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd: "R train.r ${mydict}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse with context");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s)
                if s == "R train.r --foo 'foo' --bar 1 --bool --nested.baz 'bar' --list 2 3 'qux'"
        ));
    }

    #[test]
    fn template_substitution_in_deps_and_outs() {
        let vars = serde_yaml::from_str("datadir: data/processed").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  clean:
    cmd: "python clean.py"
    deps:
      - ${datadir}/raw.csv
    outs:
      - ${datadir}/clean.parquet
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("clean").unwrap())
            .expect("clean stage");
        assert!(matches!(
            stage.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("data/processed/raw.csv")
        ));
        assert_eq!(
            stage.outs[0].path,
            PathBuf::from("data/processed/clean.parquet")
        );
    }

    #[test]
    fn template_substitution_in_dvc_path_key_out() {
        let vars = serde_yaml::from_str("model:\n  path: models/us.pkl").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    outs:
      - ${model.path}:
          cache: false
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse with context");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert_eq!(stage.outs[0].path, PathBuf::from("models/us.pkl"));
        assert!(!stage.outs[0].cache);
    }

    #[test]
    fn template_substitution_in_argv_cmd() {
        let vars = serde_yaml::from_str("script: train.py").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd:
      argv: ["python", "${script}"]
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Argv(ref v) if v == &vec!["python".to_owned(), "train.py".to_owned()]
        ));
    }

    #[test]
    fn template_substitution_in_dvc_cmd_list() {
        let vars = serde_yaml::from_str("codedir: src").unwrap();
        let params = serde_yaml::from_str("model:\n  lr: 0.001").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd:
      - mkdir -p models
      - python ${codedir}/train.py --lr ${model.lr}
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse with context");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::ShellList(ref commands) if commands == &vec![
                "mkdir -p models".to_owned(),
                "python src/train.py --lr 0.001".to_owned(),
            ]
        ));
    }

    #[test]
    fn template_undefined_key_includes_stage_and_field() {
        let ctx = TemplateContext::empty();
        let yaml = r#"
stages:
  train:
    cmd: "python --lr ${model.lr}"
"#;
        let err = parse_with_context(yaml, &ctx).unwrap_err();
        match err {
            WorkflowError::TemplateUndefined { key, field, stage } => {
                assert_eq!(key, "model.lr");
                assert_eq!(field, "cmd");
                assert_eq!(stage, "train");
            }
            other => panic!("wrong error variant: {other}"),
        }
    }

    #[test]
    fn no_templates_passes_through_unchanged() {
        let ctx = TemplateContext::empty();
        let yaml = r#"
stages:
  clean:
    cmd: "python clean.py"
    deps:
      - data/raw.csv
    outs:
      - data/clean.parquet
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("clean").unwrap())
            .expect("clean stage");
        assert!(matches!(stage.cmd, Cmd::Shell(ref s) if s == "python clean.py"));
        assert!(matches!(stage.deps[0], Dep::Path(ref p) if p == &PathBuf::from("data/raw.csv")));
        assert_eq!(stage.outs[0].path, PathBuf::from("data/clean.parquet"));
    }

    #[test]
    fn template_substitution_in_structured_out() {
        let vars = serde_yaml::from_str("outdir: models").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    outs:
      - path: ${outdir}/model.pkl
        kind: file
        cache: true
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert_eq!(stage.outs[0].path, PathBuf::from("models/model.pkl"));
    }

    #[test]
    fn vars_field_accepted_in_yaml() {
        let yaml = r#"
vars:
  - codedir: src
  - datadir: data
stages:
  clean:
    cmd: "python clean.py"
"#;
        let wf = parse(yaml).expect("parse with vars field");
        assert_eq!(wf.stages.len(), 1);
    }

    #[test]
    fn foreach_list_expands_into_multiple_stages() {
        let yaml = r#"
stages:
  preprocess:
    foreach: [raw_a, raw_b, raw_c]
    do:
      cmd: "python clean.py ${item}"
      deps:
        - "${item}.csv"
      outs:
        - "${item}_clean.csv"
"#;
        let wf = parse(yaml).expect("parse foreach list");
        assert_eq!(wf.stages.len(), 3);
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@raw_a").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@raw_b").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@raw_c").unwrap())
        );

        let stage_a = wf
            .stages
            .get(&StageName::parse("preprocess@raw_a").unwrap())
            .unwrap();
        assert!(matches!(stage_a.cmd, Cmd::Shell(ref s) if s == "python clean.py raw_a"));
        assert!(matches!(stage_a.deps[0], Dep::Path(ref p) if p == &PathBuf::from("raw_a.csv")));
        assert_eq!(stage_a.outs[0].path, PathBuf::from("raw_a_clean.csv"));
    }

    #[test]
    fn foreach_command_template_unpacks_dictionary_params() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params = serde_yaml::from_str("args:\n  lr: 0.01\n  model: resnet\n").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    foreach: [a]
    do:
      cmd: "python train.py ${args} --fold ${item}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse foreach dictionary command");
        let stage = wf
            .stages
            .get(&StageName::parse("train@a").unwrap())
            .expect("expanded train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python train.py --lr 0.01 --model 'resnet' --fold a"
        ));
    }

    #[test]
    fn foreach_dict_expands_with_key_suffix() {
        let yaml = r#"
stages:
  build:
    foreach:
      uk:
        region: eu-west-1
        bucket: data-uk
      us:
        region: us-east-1
        bucket: data-us
    do:
      cmd: "python sync.py --region ${item.region} --bucket ${item.bucket}"
"#;
        let wf = parse(yaml).expect("parse foreach dict");
        assert_eq!(wf.stages.len(), 2);
        assert!(
            wf.stages
                .contains_key(&StageName::parse("build@uk").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("build@us").unwrap())
        );

        let stage_uk = wf
            .stages
            .get(&StageName::parse("build@uk").unwrap())
            .unwrap();
        assert!(matches!(
            stage_uk.cmd,
            Cmd::Shell(ref s) if s.contains("eu-west-1") && s.contains("data-uk")
        ));
    }

    #[test]
    fn foreach_coexists_with_regular_stages() {
        let yaml = r#"
stages:
  setup:
    cmd: "echo setup"
  preprocess:
    foreach: [a, b]
    do:
      cmd: "python clean.py ${item}"
  finalize:
    cmd: "echo done"
"#;
        let wf = parse(yaml).expect("parse mixed stages");
        // 1 regular (setup) + 2 expanded (preprocess@a, preprocess@b) + 1 regular (finalize)
        assert_eq!(wf.stages.len(), 4);
        assert!(wf.stages.contains_key(&StageName::parse("setup").unwrap()));
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@a").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@b").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("finalize").unwrap())
        );
    }

    #[test]
    fn foreach_with_global_context_resolves_vars() {
        let vars = serde_yaml::from_str("codedir: src").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  clean:
    foreach: [a, b]
    do:
      cmd: "python ${codedir}/clean.py ${item}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage_a = wf
            .stages
            .get(&StageName::parse("clean@a").unwrap())
            .unwrap();
        assert!(matches!(
            stage_a.cmd,
            Cmd::Shell(ref s) if s == "python src/clean.py a"
        ));
    }

    #[test]
    fn foreach_with_param_referenced_list() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params = serde_yaml::from_str("datasets: \"[x, y, z]\"").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  process:
    foreach: "${datasets}"
    do:
      cmd: "python process.py ${item}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        assert_eq!(wf.stages.len(), 3);
        assert!(
            wf.stages
                .contains_key(&StageName::parse("process@x").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("process@y").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("process@z").unwrap())
        );
    }

    #[test]
    fn foreach_with_param_referenced_mapping_value() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params = serde_yaml::from_str(
            "myobject:\n  us:\n    prop1: alpha\n    prop2: model-us.pkl\n  eu:\n    prop1: beta\n    prop2: model-eu.pkl\n",
        )
        .unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  mystages:
    foreach: ${myobject}
    do:
      cmd: "./script.py ${key} ${item.prop1}"
      outs:
        - ${item.prop2}
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse foreach from param mapping");
        assert_eq!(wf.stages.len(), 2);

        let us = wf
            .stages
            .get(&StageName::parse("mystages@us").unwrap())
            .expect("mystages@us");
        assert!(matches!(us.cmd, Cmd::Shell(ref s) if s == "./script.py us alpha"));
        assert_eq!(us.outs[0].path, PathBuf::from("model-us.pkl"));

        let eu = wf
            .stages
            .get(&StageName::parse("mystages@eu").unwrap())
            .expect("mystages@eu");
        assert!(matches!(eu.cmd, Cmd::Shell(ref s) if s == "./script.py eu beta"));
        assert_eq!(eu.outs[0].path, PathBuf::from("model-eu.pkl"));
    }

    #[test]
    fn foreach_empty_list_returns_error() {
        let yaml = r#"
stages:
  process:
    foreach: []
    do:
      cmd: "echo ${item}"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::ForeachEmpty { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn foreach_name_collision_with_regular_stage_returns_error() {
        // A foreach expansion that produces a name colliding with
        // another stage should fail.
        let yaml = r#"
stages:
  build@uk:
    cmd: "echo existing"
  build:
    foreach:
      uk:
        region: eu-west-1
    do:
      cmd: "echo ${item.region}"
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::YamlInvalid { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn foreach_list_of_dicts_uses_index_suffix() {
        let yaml = r#"
stages:
  train:
    foreach:
      - name: alpha
        lr: "0.01"
      - name: beta
        lr: "0.001"
    do:
      cmd: "python train.py --name ${item.name} --lr ${item.lr}"
"#;
        let wf = parse(yaml).expect("parse foreach list of dicts");
        assert_eq!(wf.stages.len(), 2);
        assert!(
            wf.stages
                .contains_key(&StageName::parse("train@0").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("train@1").unwrap())
        );

        let stage0 = wf
            .stages
            .get(&StageName::parse("train@0").unwrap())
            .unwrap();
        assert!(matches!(
            stage0.cmd,
            Cmd::Shell(ref s) if s == "python train.py --name alpha --lr 0.01"
        ));
    }

    #[test]
    fn undefined_var_in_deps_reports_stage_and_field() {
        let ctx = TemplateContext::empty();
        let yaml = r#"
stages:
  preprocess:
    cmd: "python clean.py"
    deps:
      - ${missing_var}/input.csv
"#;
        let err = parse_with_context(yaml, &ctx).unwrap_err();
        match err {
            WorkflowError::TemplateUndefined { key, field, stage } => {
                assert_eq!(key, "missing_var");
                assert_eq!(field, "deps");
                assert_eq!(stage, "preprocess");
            }
            other => panic!("expected WorkflowTemplateUndefined, got: {other}"),
        }
    }

    #[test]
    fn undefined_var_in_outs_reports_stage_and_field() {
        let ctx = TemplateContext::empty();
        let yaml = r#"
stages:
  export:
    cmd: "python export.py"
    outs:
      - ${outdir}/result.bin
"#;
        let err = parse_with_context(yaml, &ctx).unwrap_err();
        match err {
            WorkflowError::TemplateUndefined { key, field, stage } => {
                assert_eq!(key, "outdir");
                assert_eq!(field, "outs");
                assert_eq!(stage, "export");
            }
            other => panic!("expected WorkflowTemplateUndefined, got: {other}"),
        }
    }

    #[test]
    fn nested_dotted_path_resolves_through_pipeline() {
        let params = serde_yaml::from_str("model:\n  lr: 0.001\n  arch: resnet").unwrap();
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd: "python train.py --lr ${model.lr} --arch ${model.arch}"
    deps:
      - src/train.py
    outs:
      - models/model.pkl
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python train.py --lr 0.001 --arch resnet"
        ));
    }

    #[test]
    fn array_index_access_resolves_through_pipeline() {
        let params = serde_yaml::from_str("widths:\n  - 64\n  - 128\n  - 256").unwrap();
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  resize:
    cmd: "python resize.py --width ${widths[0]} --max ${widths[2]}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("resize").unwrap())
            .expect("resize stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python resize.py --width 64 --max 256"
        ));
    }

    #[test]
    fn vars_block_values_available_for_substitution() {
        // Use parse_with_context with a context built from vars,
        // simulating what parse_with_base_dir does.
        let vars = serde_yaml::from_str("codedir: src/ml\ndatadir: data/raw").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
vars:
  - codedir: src/ml
  - datadir: data/raw
stages:
  train:
    cmd: "python ${codedir}/train.py"
    deps:
      - ${datadir}/features.csv
    outs:
      - ${datadir}/output.parquet
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python src/ml/train.py"
        ));
        assert!(matches!(
            stage.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("data/raw/features.csv")
        ));
        assert_eq!(stage.outs[0].path, PathBuf::from("data/raw/output.parquet"));
    }

    #[test]
    fn params_override_vars_on_key_conflict() {
        let vars = serde_yaml::from_str("lr: 0.1\ncodedir: src").unwrap();
        let params = serde_yaml::from_str("lr: 0.001").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    cmd: "python train.py --lr ${lr} --dir ${codedir}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train stage");
        // lr should come from params (0.001), not vars (0.1)
        // codedir should come from vars since params doesn't have it
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python train.py --lr 0.001 --dir src"
        ));
    }

    #[test]
    fn escaped_expression_passes_through_as_literal() {
        let vars = serde_yaml::from_str("name: world").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        // Use a raw string with single-quoted YAML value to avoid YAML
        // interpreting `\$` as an escape. In single-quoted YAML strings,
        // backslashes are literal.
        let yaml = "stages:\n  echo:\n    cmd: 'echo \\${HOME} and ${name}'\n";
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("echo").unwrap())
            .expect("echo stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "echo ${HOME} and world"
        ));
    }

    #[test]
    fn multiple_expressions_in_single_field() {
        let vars = serde_yaml::from_str("src: code\ndata: datasets\nversion: v2").unwrap();
        let params = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  process:
    cmd: "python ${src}/process.py --input ${data}/raw --version ${version}"
    deps:
      - ${src}/process.py
      - ${data}/raw
    outs:
      - ${data}/processed-${version}
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("process").unwrap())
            .expect("process stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python code/process.py --input datasets/raw --version v2"
        ));
        assert!(matches!(
            stage.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("code/process.py")
        ));
        assert!(matches!(
            stage.deps[1],
            Dep::Path(ref p) if p == &PathBuf::from("datasets/raw")
        ));
        assert_eq!(stage.outs[0].path, PathBuf::from("datasets/processed-v2"));
    }

    #[test]
    fn template_resolution_in_all_string_fields() {
        let vars = serde_yaml::from_str("base: project\nscript: run").unwrap();
        let params = serde_yaml::from_str("timeout_val: 30").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  full:
    cmd: "python ${base}/${script}.py"
    deps:
      - ${base}/input.csv
    outs:
      - ${base}/output.bin
    timeout: "${timeout_val}s"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("full").unwrap())
            .expect("full stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python project/run.py"
        ));
        assert!(matches!(
            stage.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("project/input.csv")
        ));
        assert_eq!(stage.outs[0].path, PathBuf::from("project/output.bin"));
        assert_eq!(stage.timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn parses_wdir_field() {
        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    wdir: training/
    deps:
      - data.csv
    outs:
      - model.pkl
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train");
        assert_eq!(stage.wdir, Some(PathBuf::from("training/")));
    }

    #[test]
    fn wdir_none_when_absent() {
        let yaml = r#"
stages:
  clean:
    cmd: "python clean.py"
"#;
        let wf = parse(yaml).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("clean").unwrap())
            .expect("clean");
        assert_eq!(stage.wdir, None);
    }

    #[test]
    fn wdir_rejects_absolute_path() {
        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    wdir: /absolute/path
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::WdirInvalid { reason, .. } if reason.contains("relative")),
            "wrong error: {err}"
        );
    }

    #[test]
    fn wdir_rejects_parent_traversal() {
        let yaml = r#"
stages:
  train:
    cmd: "python train.py"
    wdir: sub/../escape
"#;
        let err = parse(yaml).unwrap_err();
        assert!(
            matches!(err, WorkflowError::WdirInvalid { reason, .. } if reason.contains("..")),
            "wrong error: {err}"
        );
    }

    #[test]
    fn wdir_supports_template_substitution() {
        let yaml = r#"
vars:
  - project_dir: training
stages:
  train:
    cmd: "python train.py"
    wdir: "${project_dir}"
"#;
        let wf = parse_with_base_dir(yaml, Path::new(".")).expect("parse");
        let stage = wf
            .stages
            .get(&StageName::parse("train").unwrap())
            .expect("train");
        assert_eq!(stage.wdir, Some(PathBuf::from("training")));
    }

    #[test]
    fn matrix_2x3_produces_six_expanded_stages() {
        let yaml = r#"
stages:
  train:
    matrix:
      model: [resnet, vgg]
      dataset: [imagenet, cifar10, coco]
    cmd: "python train.py --model ${item.model} --data ${item.dataset}"
    deps:
      - "data/${item.dataset}/"
    outs:
      - "models/${item.model}-${item.dataset}.pkl"
"#;
        let wf = parse(yaml).expect("parse matrix 2x3");
        assert_eq!(wf.stages.len(), 6);

        // Verify all expected stage names are present with @val1-val2 pattern.
        let expected_names = [
            "train@resnet-imagenet",
            "train@resnet-cifar10",
            "train@resnet-coco",
            "train@vgg-imagenet",
            "train@vgg-cifar10",
            "train@vgg-coco",
        ];
        for name in &expected_names {
            assert!(
                wf.stages.contains_key(&StageName::parse(name).unwrap()),
                "missing expanded stage: {name}"
            );
        }
    }

    #[test]
    fn matrix_command_template_unpacks_dictionary_params() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params = serde_yaml::from_str("args:\n  lr: 0.01\n  model: resnet\n").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  train:
    matrix:
      dataset: [cifar10]
    cmd: "python train.py ${args} --data ${item.dataset}"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse matrix dictionary command");
        let stage = wf
            .stages
            .get(&StageName::parse("train@cifar10").unwrap())
            .expect("expanded train stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python train.py --lr 0.01 --model 'resnet' --data cifar10"
        ));
    }

    #[test]
    fn matrix_substitution_in_cmd_deps_outs() {
        let yaml = r#"
stages:
  train:
    matrix:
      model: [resnet, vgg]
      dataset: [imagenet, cifar10, coco]
    cmd: "python train.py --model ${item.model} --data ${item.dataset}"
    deps:
      - "data/${item.dataset}/"
      - "src/train.py"
    outs:
      - "models/${item.model}-${item.dataset}.pkl"
"#;
        let wf = parse(yaml).expect("parse matrix");

        // Check cmd substitution.
        let stage = wf
            .stages
            .get(&StageName::parse("train@resnet-imagenet").unwrap())
            .expect("train@resnet-imagenet");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python train.py --model resnet --data imagenet"
        ));

        // Check deps substitution.
        assert!(matches!(
            stage.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("data/imagenet/")
        ));
        // Static dep preserved.
        assert!(matches!(
            stage.deps[1],
            Dep::Path(ref p) if p == &PathBuf::from("src/train.py")
        ));

        // Check outs substitution.
        assert_eq!(
            stage.outs[0].path,
            PathBuf::from("models/resnet-imagenet.pkl")
        );

        // Verify a different combination.
        let stage_vgg_coco = wf
            .stages
            .get(&StageName::parse("train@vgg-coco").unwrap())
            .expect("train@vgg-coco");
        assert!(matches!(
            stage_vgg_coco.cmd,
            Cmd::Shell(ref s) if s == "python train.py --model vgg --data coco"
        ));
        assert!(matches!(
            stage_vgg_coco.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("data/coco/")
        ));
        assert_eq!(
            stage_vgg_coco.outs[0].path,
            PathBuf::from("models/vgg-coco.pkl")
        );
    }

    #[test]
    fn matrix_complex_values_use_index_names_and_nested_item_paths() {
        let yaml = r#"
stages:
  train:
    matrix:
      labels:
        - [label1, label2, label3]
      config:
        - n_estimators: 150
          max_depth: 20
    cmd: "python train.py --trees ${item.config.n_estimators} --label ${item.labels[1]}"
    outs:
      - "${key}.pkl"
"#;
        let wf = parse(yaml).expect("parse matrix with complex values");
        let stage = wf
            .stages
            .get(&StageName::parse("train@labels0-config0").unwrap())
            .expect("expanded stage");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python train.py --trees 150 --label label2"
        ));
        assert_eq!(stage.outs[0].path, PathBuf::from("labels0-config0.pkl"));
    }

    #[test]
    fn matrix_with_param_sourced_values() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params = serde_yaml::from_str("base_dir: experiments\nscript: train.py").unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  sweep:
    matrix:
      lr: ["0.001", "0.01"]
      arch: [resnet, vgg, efficientnet]
    cmd: "python ${base_dir}/${script} --lr ${item.lr} --arch ${item.arch}"
    deps:
      - "${base_dir}/${script}"
    outs:
      - "${base_dir}/output/${item.arch}-${item.lr}.pkl"
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse matrix with params");
        assert_eq!(wf.stages.len(), 6);

        // Verify param substitution combined with matrix item substitution.
        let stage = wf
            .stages
            .get(&StageName::parse("sweep@0_001-resnet").unwrap())
            .expect("sweep@0_001-resnet");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "python experiments/train.py --lr 0.001 --arch resnet"
        ));
        assert!(matches!(
            stage.deps[0],
            Dep::Path(ref p) if p == &PathBuf::from("experiments/train.py")
        ));
        assert_eq!(
            stage.outs[0].path,
            PathBuf::from("experiments/output/resnet-0.001.pkl")
        );

        // Verify another combination to confirm Cartesian product.
        let stage2 = wf
            .stages
            .get(&StageName::parse("sweep@0_01-efficientnet").unwrap())
            .expect("sweep@0_01-efficientnet");
        assert!(matches!(
            stage2.cmd,
            Cmd::Shell(ref s) if s == "python experiments/train.py --lr 0.01 --arch efficientnet"
        ));
        assert_eq!(
            stage2.outs[0].path,
            PathBuf::from("experiments/output/efficientnet-0.01.pkl")
        );
    }

    #[test]
    fn matrix_values_can_reference_param_sequences() {
        let vars = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let params =
            serde_yaml::from_str("datasets: [dataset1, dataset2]\nprocessors: [cpu, gpu]\n")
                .unwrap();
        let ctx = TemplateContext::new(vars, params, false);

        let yaml = r#"
stages:
  preprocess:
    matrix:
      dataset: ${datasets}
      processor: ${processors}
    cmd: "./preprocess.py ${item.dataset} ${item.processor}"
    deps:
      - ${item.dataset}
    outs:
      - ${key}.json
"#;
        let wf = parse_with_context(yaml, &ctx).expect("parse matrix from param sequences");
        assert_eq!(wf.stages.len(), 4);

        let stage = wf
            .stages
            .get(&StageName::parse("preprocess@dataset1-cpu").unwrap())
            .expect("preprocess@dataset1-cpu");
        assert!(matches!(
            stage.cmd,
            Cmd::Shell(ref s) if s == "./preprocess.py dataset1 cpu"
        ));
        assert!(matches!(stage.deps[0], Dep::Path(ref p) if p == &PathBuf::from("dataset1")));
        assert_eq!(stage.outs[0].path, PathBuf::from("dataset1-cpu.json"));
    }

    #[test]
    fn matrix_coexists_with_regular_and_foreach_stages() {
        let yaml = r#"
stages:
  setup:
    cmd: "echo setup"
  preprocess:
    foreach: [a, b]
    do:
      cmd: "python clean.py ${item}"
  train:
    matrix:
      model: [resnet, vgg]
      dataset: [imagenet, cifar10]
    cmd: "python train.py --model ${item.model} --data ${item.dataset}"
  finalize:
    cmd: "echo done"
"#;
        let wf = parse(yaml).expect("parse mixed stages with matrix");
        // 1 (setup) + 2 (foreach) + 4 (matrix 2×2) + 1 (finalize) = 8
        assert_eq!(wf.stages.len(), 8);
        assert!(wf.stages.contains_key(&StageName::parse("setup").unwrap()));
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@a").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("preprocess@b").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("train@resnet-imagenet").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("train@resnet-cifar10").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("train@vgg-imagenet").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("train@vgg-cifar10").unwrap())
        );
        assert!(
            wf.stages
                .contains_key(&StageName::parse("finalize").unwrap())
        );
    }
}
