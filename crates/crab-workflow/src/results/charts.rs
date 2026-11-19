use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::plot_matrices::CountedMatrix;
use super::{invalid, plot_specs::*, plot_templates::*, plots::*, *};
use crate::{PlotConfig, Result, WorkflowError};

#[derive(Debug, thiserror::Error)]
enum ChartFailure {
    #[error("retained plot evidence unavailable ({state:?})")]
    Evidence {
        state: Availability,
        #[source]
        source: Arc<WorkflowError>,
    },
    #[error(transparent)]
    Specification(#[from] WorkflowError),
}

impl ChartFailure {
    fn state(&self) -> Availability {
        match self {
            Self::Evidence { state, .. } => *state,
            Self::Specification(source) => evidence_state(source),
        }
    }
}

impl From<std::io::Error> for ChartFailure {
    fn from(source: std::io::Error) -> Self {
        Self::Specification(WorkflowError::Io(source))
    }
}

/// Describes how retained observations were reduced for display.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartReduction {
    Sampled,
    Counted,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chart {
    pub key: String,
    pub title: String,
    pub state: Availability,
    pub reason: Option<String>,
    pub spec: Option<Value>,
    pub points: usize,
    pub displayed_points: usize,
    pub reduction: Option<ChartReduction>,
    pub interaction: Option<ChartInteraction>,
    pub images: Vec<ChartImage>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartImage {
    pub experiment_id: String,
    pub digest: String,
    pub path: String,
    pub hash: String,
    pub label: String,
}

pub(crate) fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Builds overlays exclusively from verified evidence belonging to the selected snapshots.
pub fn charts<E>(
    runs: &[SealedResult],
    read: impl Fn(&str) -> std::result::Result<Vec<u8>, E>,
) -> Result<Vec<Chart>>
where
    E: Into<Arc<WorkflowError>>,
{
    if runs.len() > MAX_COMPARISON_RUNS {
        return Err(invalid("too many chart runs"));
    }
    let mut configs = BTreeMap::new();
    for run in runs {
        run.verify()?;
        for config in &run.snapshot.plots {
            let children: Vec<_> = run
                .snapshot
                .attachments
                .iter()
                .filter(|a| {
                    matches!(a.kind, AttachmentKind::Plot | AttachmentKind::Image)
                        && Path::new(&a.path).starts_with(&config.path)
                        && Path::new(&a.path) != config.path
                })
                .collect();
            if children.is_empty() {
                let key = (config.id.clone(), config.path.clone());
                configs.entry(key).or_insert_with(|| config.clone());
            } else {
                for child in children {
                    let mut resolved = config.clone();
                    resolved.path = child.path.clone().into();
                    let key = (config.id.clone(), resolved.path.clone());
                    configs.entry(key).or_insert(resolved);
                }
            }
        }
    }
    let mut output = Vec::new();
    let mut sources_per_id = BTreeMap::new();
    for (id, _) in configs.keys() {
        if let Some(id) = id {
            *sources_per_id.entry(id.clone()).or_insert(0) += 1;
        }
    }
    for ((id, path), config) in configs {
        let key = match &id {
            Some(id) => {
                serde_json::to_string(&(id, &path)).map_err(crate::WorkflowError::ResultsJson)?
            }
            None => path.to_string_lossy().into_owned(),
        };
        let title = config.title.clone().unwrap_or_else(|| match id {
            Some(id) if sources_per_id[&id] > 1 => format!("{id} ({})", path.display()),
            Some(id) => id,
            None => path.to_string_lossy().into_owned(),
        });
        let mut chart = Chart {
            key,
            title,
            state: Availability::Present,
            reason: None,
            spec: None,
            points: 0,
            displayed_points: 0,
            reduction: None,
            interaction: None,
            images: Vec::new(),
        };
        if image_mime(&config.path).is_some() {
            for run in runs {
                let path = config.path.to_string_lossy();
                let attachment = run.snapshot.attachments.iter().find(|a| a.path == path);
                if let Some(Attachment {
                    state: Availability::Present,
                    hash: Some(hash),
                    ..
                }) = attachment
                {
                    chart.images.push(ChartImage {
                        experiment_id: run.snapshot.experiment_id.clone(),
                        digest: run.digest.clone(),
                        path: path.into_owned(),
                        hash: hash.clone(),
                        label: revision_label(&run.snapshot, runs),
                    });
                } else {
                    chart.state = Availability::Missing;
                    chart.reason = Some("Some selected runs have no retained image; inspect run evidence for availability".into());
                }
            }
            chart.points = chart.images.len();
            chart.displayed_points = chart.points;
            output.push(chart);
            continue;
        }
        let result = chart_spec(runs, &config, &read, &mut chart);
        match result {
            Ok(spec) => chart.spec = spec,
            Err(error) => {
                chart.state = error.state();
                // Provider errors remain chained in the read cache. Renderer
                // data carries recovery guidance, never provider diagnostics.
                chart.reason = Some(match error {
                    ChartFailure::Evidence { .. } => evidence_notice(chart.state).into(),
                    ChartFailure::Specification(source) => source.to_string(),
                });
            }
        }
        output.push(chart);
    }
    if super::encode(&output)?.len() > 2 * 1024 * 1024 {
        return Err(invalid(
            "Normalized plots exceed 2 MiB; select fewer runs or plots",
        ));
    }
    Ok(output)
}

fn chart_spec<E>(
    runs: &[SealedResult],
    config: &PlotConfig,
    read: &impl Fn(&str) -> std::result::Result<Vec<u8>, E>,
    chart: &mut Chart,
) -> std::result::Result<Option<Value>, ChartFailure>
where
    E: Into<Arc<WorkflowError>>,
{
    let temporary = tempfile::tempdir()?;
    let mut revisions = Vec::new();
    let mut missing = Vec::new();
    let mut bounds = BTreeMap::new();
    let mut categories = std::collections::BTreeSet::new();
    let mut matrix = None;
    let mut admitted: Option<PlotConfig> = None;
    let mut template_bytes = None;
    for run in runs {
        let snapshot = &run.snapshot;
        let own = snapshot
            .plots
            .iter()
            .find(|own| own.id == config.id && config.path.starts_with(&own.path))
            .ok_or_else(|| invalid("selected runs declare different plots"))?;
        let mut own = own.clone();
        own.path = config.path.clone();
        if own.x != config.x
            || own.y != config.y
            || own.x_path != config.x_path
            || own.no_header != config.no_header
            || own.x_label != config.x_label
            || own.y_label != config.y_label
        {
            return Err(invalid("plot declarations differ; inspect runs separately").into());
        }
        // Admit a revision only after all its inputs verify. A bad coordinate
        // or template must not erase verified curves or leave partial observations.
        let prepared: std::result::Result<_, ChartFailure> = (|| {
            let bytes = evidence(snapshot, &own.path, read)?;
            let mut data = parse_plot_data_bytes(&own.path, &bytes, &own)?;
            if let Some(x_path) = &own.x_path {
                let x_bytes = evidence(snapshot, x_path, read)?;
                let mut x_config = own.clone();
                x_config.path = x_path.clone();
                x_config.y.clear();
                let x_data = parse_plot_data_bytes(x_path, &x_bytes, &x_config)?;
                if x_data.rows.len() != data.rows.len() {
                    return Err(invalid("x/y source row counts differ").into());
                }
                for (row, x) in data.rows.iter_mut().zip(x_data.rows) {
                    row.replace_x(x);
                }
            }
            if let Some(template) = &own.template
                && builtin_plot_template_spec(template).is_none()
            {
                super::model::validate_path(template)?;
                let bytes = evidence(snapshot, Path::new(template), read)?;
                if template_bytes
                    .as_ref()
                    .is_some_and(|previous| previous != &bytes)
                {
                    return Err(
                        invalid("plot template bytes differ; inspect runs separately").into(),
                    );
                }
                let path = temporary.path().join(template);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, &bytes)?;
                template_bytes = Some(bytes);
            }
            Ok(data)
        })();
        let mut data = match prepared {
            Ok(data) => data,
            Err(error @ ChartFailure::Evidence { .. }) => {
                let state = error.state();
                chart.state = match (chart.state, state) {
                    (Availability::Corrupt, _) | (_, Availability::Corrupt) => {
                        Availability::Corrupt
                    }
                    (Availability::Denied, _) | (_, Availability::Denied) => Availability::Denied,
                    (Availability::Present, state) => state,
                    (state, _) => state,
                };
                missing.push(format!(
                    "{}: {}",
                    snapshot.experiment_id,
                    evidence_notice(state)
                ));
                continue;
            }
            Err(error) => return Err(error),
        };
        // Failed captures have distinct template paths. Only verified revisions
        // may choose a template; unavailable ones must not suppress valid curves.
        if let Some(config) = &admitted {
            if own.template != config.template {
                return Err(invalid("plot declarations differ; inspect runs separately").into());
            }
        } else {
            // Only bare builtins select exact counting. Authored templates keep
            // their transforms even when their original name matches a builtin.
            matrix = own
                .template
                .as_deref()
                .filter(|template| builtin_matrix(template))
                .map(|_| CountedMatrix::bounded());
            admitted = Some(own.clone());
        }
        let series = data.y_fields.len() * runs.len();
        if series > MAX_COMPARISON_RUNS {
            return Err(
                invalid("Plot exceeds 20 compared series; select fewer runs or y fields").into(),
            );
        }
        if let Some(matrix) = &mut matrix {
            matrix.observe(&revision_label(snapshot, runs), &data)?;
            chart.points = matrix.points();
            chart.displayed_points = matrix.pairs();
            continue;
        }
        for row in &data.rows {
            if !row.x_missing {
                super::interactions::observe(
                    &mut bounds,
                    &coordinate_field(config.x.as_deref().unwrap_or("step"), &data.y_fields),
                    &row.x,
                );
                if categories.len() <= MAX_SERIES_POINTS {
                    categories.insert(row.x.clone());
                }
            }
            for (field, value) in data.y_fields.iter().zip(&row.y_values) {
                super::interactions::observe(&mut bounds, field, value);
                super::interactions::observe(&mut bounds, "value", value);
            }
        }
        chart.points += data.rows.len() * data.y_fields.len();
        let row_limit = (MAX_CHART_POINTS / series.max(1)).min(MAX_SERIES_POINTS);
        if data.rows.len() > row_limit {
            // Sampling before an authored aggregate/transform would change the
            // statistic being plotted. Keep bounded originals or refuse visibly.
            if let Some(template) = own.template.as_deref() {
                let spec = resolve_plot_template(temporary.path(), template)?;
                if spec
                    .as_ref()
                    .is_some_and(|spec| !sampling_preserves_values(&spec.spec))
                {
                    return Err(invalid(
                        "This plot requires all input rows; sampling would change its statistics. Inspect retained source bytes or select a bounded plot",
                    ).into());
                }
            }
            data.rows = sample_rows(&data.rows, data.y_fields.len(), row_limit)?;
        }
        chart.displayed_points += data.rows.len() * data.y_fields.len();
        revisions.push(PlotRevisionData {
            revision: revision_label(snapshot, runs),
            data,
        });
    }
    if !missing.is_empty() {
        chart.reason = Some(missing.join("; "));
    }
    let Some(config) = admitted.as_ref() else {
        return Ok(None);
    };
    let counted = matrix.is_some();
    let mut spec = if let Some(matrix) = matrix {
        let template = config
            .template
            .as_deref()
            .and_then(builtin_plot_template_spec)
            .ok_or_else(|| invalid("owned matrix template is unavailable"))?;
        matrix.spec(template, config, &chart.title)?
    } else {
        build_vega_revision_spec(config, &chart.title, &revisions, temporary.path())?
    };
    if config
        .template
        .as_ref()
        .is_none_or(|template| builtin_plot_template_spec(template).is_some())
    {
        spec["title"] = Value::from(chart.title.clone());
    }
    // External data and executable URLs must never escape the captured evidence boundary.
    validate_inline_spec(&spec)?;
    if spec.get("width").is_none() {
        spec["width"] = Value::from(700);
    }
    if (spec["mark"]["type"] == "line" || spec["mark"] == "line") && spec.get("layer").is_none() {
        if spec["encoding"]["strokeDash"].is_null()
            && spec["encoding"]["color"]["type"] == "nominal"
        {
            let mut dashes = spec["encoding"]["color"].clone();
            let patterns: Vec<_> = std::iter::once(vec![1, 0])
                .chain(
                    (1..MAX_COMPARISON_RUNS)
                        .map(|index| vec![(index % 5 + 1) * 2, (index / 5 + 1) * 2]),
                )
                .collect();
            dashes["scale"] = serde_json::json!({"range":patterns});
            spec["encoding"]["strokeDash"] = dashes;
        }
        chart.interaction = super::interactions::augment(&mut spec, &bounds, &categories)?;
    }
    chart.reduction = if counted {
        Some(ChartReduction::Counted)
    } else if chart.displayed_points < chart.points {
        Some(ChartReduction::Sampled)
    } else {
        None
    };
    Ok(Some(spec))
}

fn evidence_state(error: &WorkflowError) -> Availability {
    match error {
        WorkflowError::ResultsStored { .. }
        | WorkflowError::CorruptObject { .. }
        | WorkflowError::StorageDomain(crab_storage::StorageError::CorruptObject { .. }) => {
            Availability::Corrupt
        }
        WorkflowError::NotFound { .. }
        | WorkflowError::Storage(object_store::Error::NotFound { .. })
        | WorkflowError::StorageDomain(crab_storage::StorageError::NotFound { .. }) => {
            Availability::Missing
        }
        WorkflowError::Io(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Availability::Missing
        }
        WorkflowError::Io(source) if source.kind() == std::io::ErrorKind::PermissionDenied => {
            Availability::Denied
        }
        WorkflowError::Storage(
            object_store::Error::PermissionDenied { .. }
            | object_store::Error::Unauthenticated { .. },
        )
        | WorkflowError::StorageDomain(
            crab_storage::StorageError::Forbidden { .. }
            | crab_storage::StorageError::NoCredentials
            | crab_storage::StorageError::AuthFailed { .. }
            | crab_storage::StorageError::AuthExpired { .. },
        ) => Availability::Denied,
        _ => Availability::Unsupported,
    }
}

fn evidence_notice(state: Availability) -> &'static str {
    match state {
        Availability::Corrupt => {
            "Retained plot failed integrity verification. Restore its verified bytes, then refresh Results."
        }
        Availability::Missing => {
            "Retained plot bytes are missing. Retrieve the run evidence, then refresh Results."
        }
        Availability::Denied => {
            "Access to retained plot bytes was denied. Check repository storage access, then refresh Results."
        }
        Availability::NotCaptured => {
            "Plot evidence was not captured for this run. Correct the source or reduce captured assets, then save or run a new experiment."
        }
        _ => {
            "Retained plot is unavailable. Check captured evidence and repository storage, then refresh Results."
        }
    }
}

fn evidence<E>(
    snapshot: &ResultSnapshot,
    path: &Path,
    read: &impl Fn(&str) -> std::result::Result<Vec<u8>, E>,
) -> std::result::Result<Vec<u8>, ChartFailure>
where
    E: Into<Arc<WorkflowError>>,
{
    let path = path.to_string_lossy().replace('\\', "/");
    let attachment = snapshot
        .attachments
        .iter()
        .find(|a| a.path == path)
        .ok_or_else(|| ChartFailure::Evidence {
            state: Availability::NotCaptured,
            source: Arc::new(invalid(format!("{path} was not captured"))),
        })?;
    if attachment.state != Availability::Present {
        return Err(ChartFailure::Evidence {
            state: attachment.state,
            source: Arc::new(invalid(format!(
                "{path}: {}",
                attachment.reason.as_deref().unwrap_or("unavailable")
            ))),
        });
    }
    let hash = attachment
        .hash
        .as_deref()
        .ok_or_else(|| invalid("evidence lacks digest"))?;
    let bytes = read(hash).map_err(|source| {
        let source = source.into();
        ChartFailure::Evidence {
            state: evidence_state(&source),
            source,
        }
    })?;
    if bytes.len() as u64 != attachment.size || super::digest(&bytes) != hash {
        return Err(ChartFailure::Evidence {
            state: Availability::Corrupt,
            source: Arc::new(super::stored(
                hash,
                invalid("plot evidence digest or size mismatch"),
            )),
        });
    }
    Ok(bytes)
}

pub fn validate_inline_spec(value: &Value) -> Result<()> {
    if super::model::unsafe_integer(value) {
        return Err(invalid("plot integer exceeds exact JSON client precision"));
    }
    match value {
        Value::Object(object) => {
            if object.contains_key("url") || object.contains_key("href") {
                return Err(invalid(
                    "external plot URLs are unsupported in retained results",
                ));
            }
            for value in object.values() {
                validate_inline_spec(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_inline_spec(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn sampling_preserves_values(spec: &Value) -> bool {
    let mark = spec["mark"]
        .as_str()
        .or_else(|| spec["mark"]["type"].as_str());
    if !matches!(mark, Some("line" | "point")) {
        return false;
    }
    fn transforms(value: &Value) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(key, value)| {
                matches!(
                    key.as_str(),
                    "transform" | "aggregate" | "bin" | "timeUnit" | "stack"
                ) || transforms(value)
            }),
            Value::Array(values) => values.iter().any(transforms),
            _ => false,
        }
    }
    !transforms(spec)
}

fn sample_rows(rows: &[PlotRow], fields: usize, limit: usize) -> Result<Vec<PlotRow>> {
    use std::collections::BTreeSet;
    let fields = fields.max(1);
    let numeric = |value: &str| value.parse::<f64>().is_ok_and(|value| value.is_finite());
    if rows.iter().any(|row| {
        (!row.x_missing && !numeric(&row.x))
            || row
                .y_values
                .iter()
                .zip(&row.missing_values)
                .any(|(value, missing)| !missing && !numeric(value))
    }) {
        return Err(invalid(
            "sampling requires numeric coordinates; inspect retained source bytes",
        ));
    }
    let mut selected = BTreeSet::from([0, rows.len() - 1]);
    // Keep both sides of every absence transition. Dropping these witnesses
    // would turn separated curve segments into an invented continuous result.
    for (index, pair) in rows.windows(2).enumerate() {
        if pair[0].x_missing != pair[1].x_missing
            || pair[0].missing_values != pair[1].missing_values
        {
            selected.extend([index, index + 1]);
        }
    }
    for field in 0..fields {
        let numbers = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if row.x_missing || row.missing_values[field] {
                    return None;
                }
                row.y_values
                    .get(field)?
                    .parse::<f64>()
                    .ok()
                    .map(|value| (index, value))
            })
            .collect::<Vec<_>>();
        if let Some((index, _)) = numbers.iter().min_by(|a, b| a.1.total_cmp(&b.1)) {
            selected.insert(*index);
        }
        if let Some((index, _)) = numbers.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
            selected.insert(*index);
        }
    }
    if selected.len() > limit {
        return Err(invalid(
            "Too many missing intervals for bounded sampling; inspect retained source bytes",
        ));
    }
    let buckets = (limit - selected.len()) / (2 * fields);
    for bucket in 0..buckets {
        let start = bucket * rows.len() / buckets;
        let end = (bucket + 1) * rows.len() / buckets;
        let before = selected.len();
        for field in 0..fields {
            let numbers = (start..end)
                .filter_map(|index| {
                    if rows[index].x_missing || rows[index].missing_values[field] {
                        return None;
                    }
                    rows[index]
                        .y_values
                        .get(field)?
                        .parse::<f64>()
                        .ok()
                        .filter(|value| value.is_finite())
                        .map(|value| (index, value))
                })
                .collect::<Vec<_>>();
            if let Some((index, _)) = numbers.iter().min_by(|a, b| a.1.total_cmp(&b.1)) {
                selected.insert(*index);
            }
            if let Some((index, _)) = numbers.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
                selected.insert(*index);
            }
        }
        if selected.len() == before {
            selected.insert(start);
        }
    }
    Ok(selected
        .into_iter()
        .map(|index| rows[index].clone())
        .collect())
}

fn revision_label(snapshot: &ResultSnapshot, runs: &[SealedResult]) -> String {
    let id = &snapshot.experiment_id;
    let suffix: String = id.chars().skip(id.len().saturating_sub(8)).collect();
    // Compact labels must still identify separate series if suffixes collide.
    // UUIDv7 time prefixes alone cannot distinguish neighboring runs.
    let label = if runs
        .iter()
        .filter(|run| run.snapshot.experiment_id.ends_with(&suffix))
        .count()
        > 1
    {
        id
    } else {
        &suffix
    };
    snapshot
        .name
        .as_ref()
        .map(|name| format!("{name:.24} · {label}"))
        .unwrap_or_else(|| label.clone())
}
