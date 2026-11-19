use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{digest, encode, invalid, *};
use crate::Result;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunQuery {
    pub search: String,
    pub sort: String,
    pub descending: bool,
    pub columns: Option<Vec<String>>,
    pub limit: usize,
    pub cursor: Option<String>,
}

impl Default for RunQuery {
    fn default() -> Self {
        Self {
            search: String::new(),
            sort: "started_at".into(),
            descending: true,
            columns: None,
            limit: 100,
            cursor: None,
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub id: String,
    pub digest: String,
    pub origin: ResultOrigin,
    pub name: Option<String>,
    pub status: String,
    pub base_commit: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub parameters: Vec<Datum>,
    pub metrics: Vec<Datum>,
    pub capture_issues: Vec<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub key: String,
    pub file: String,
    pub pointer: String,
    pub kind: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPage {
    pub generation: String,
    pub total: usize,
    pub columns: Vec<Column>,
    pub display_columns: Vec<String>,
    pub rows: Vec<RunSummary>,
    pub next_cursor: Option<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    generation: String,
    query: String,
    offset: usize,
}

pub fn query(runs: &[SealedResult], request: &RunQuery) -> Result<RunPage> {
    if request.limit == 0
        || request.limit > MAX_PAGE_ROWS
        || request
            .columns
            .as_ref()
            .is_some_and(|columns| columns.len() > MAX_COLUMNS)
        || request.search.len() > 1024
        || request.sort.len() > 8192
    {
        return Err(invalid("query exceeds row, column or text limits"));
    }
    let mut identities = Vec::with_capacity(runs.len());
    let mut unique = BTreeSet::new();
    let mut columns = BTreeSet::new();
    for run in runs {
        run.verify()?;
        let snapshot = &run.snapshot;
        if !unique.insert(&snapshot.experiment_id) {
            return Err(invalid("duplicate run ID in catalog"));
        }
        identities.push((&snapshot.experiment_id, &run.digest));
        for (kind, fields) in [
            ("metric", &snapshot.metrics),
            ("param", &snapshot.parameters),
        ] {
            for field in fields {
                columns.insert(Column {
                    key: format!("{kind}:{}", field.key()),
                    file: field.file.clone(),
                    pointer: field.pointer.clone(),
                    kind: kind.into(),
                });
                if columns.len() > MAX_FIELDS {
                    return Err(invalid("catalog exceeds column limit"));
                }
            }
        }
    }
    identities.sort();
    if !matches!(request.sort.as_str(), "name" | "status" | "started_at")
        && !columns.iter().any(|column| column.key == request.sort)
    {
        return Err(invalid("unknown result sort field"));
    }
    if request
        .columns
        .iter()
        .flatten()
        .any(|key| !columns.iter().any(|column| &column.key == key))
    {
        return Err(invalid("selected result column is not in the catalog"));
    }
    let display_columns = request.columns.clone().unwrap_or_else(|| {
        columns
            .iter()
            .take(MAX_COLUMNS)
            .map(|column| column.key.clone())
            .collect()
    });
    let generation = digest(&encode(&identities)?);
    let mut scoped_request = request.clone();
    scoped_request.cursor = None;
    let query_hash = digest(&encode(&scoped_request)?);
    let offset = match &request.cursor {
        None => 0,
        Some(raw) => {
            if raw.len() > 1024 {
                return Err(invalid("cursor exceeds size limit"));
            }
            let cursor: Cursor = super::decode(raw.as_bytes())?;
            if cursor.generation != generation || cursor.query != query_hash {
                return Err(invalid(
                    "stale cursor: refresh the catalog and restart pagination",
                ));
            }
            cursor.offset
        }
    };
    let needle = request.search.to_lowercase();
    let mut selected = runs
        .iter()
        .filter(|run| {
            let s = &run.snapshot;
            needle.is_empty()
                || [
                    s.experiment_id.as_str(),
                    s.base_commit.as_str(),
                    s.status.as_str(),
                    match s.origin {
                        ResultOrigin::Captured => "captured",
                        ResultOrigin::ExperimentMetadata => "imported experiment metadata",
                    },
                    s.name.as_deref().unwrap_or(""),
                ]
                .iter()
                .any(|value| value.to_lowercase().contains(&needle))
                || s.parameters.iter().chain(&s.metrics).any(|field| {
                    field.file.to_lowercase().contains(&needle)
                        || field.pointer.to_lowercase().contains(&needle)
                        || (field.state == Availability::Present
                            && field.value.to_string().to_lowercase().contains(&needle))
                })
        })
        .collect::<Vec<_>>();
    selected.sort_by(|a, b| {
        let order = sort_value(&a.snapshot, &request.sort)
            .zip(sort_value(&b.snapshot, &request.sort))
            .map(|(a, b)| scalar_order(&a, &b));
        let primary = match order {
            Some(order) if request.descending => order.reverse(),
            Some(order) => order,
            None => match (
                sort_value(&a.snapshot, &request.sort),
                sort_value(&b.snapshot, &request.sort),
            ) {
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                _ => Ordering::Equal,
            },
        };
        primary.then_with(|| a.snapshot.experiment_id.cmp(&b.snapshot.experiment_id))
    });
    if offset > selected.len() {
        return Err(invalid("cursor offset exceeds catalog"));
    }
    let end = offset.saturating_add(request.limit).min(selected.len());
    let rows = selected[offset..end]
        .iter()
        .map(|run| summary(run, &display_columns))
        .collect();
    let next_cursor = (end < selected.len())
        .then(|| {
            encode(&Cursor {
                generation: generation.clone(),
                query: query_hash,
                offset: end,
            })
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|_| invalid("cursor encoding failed"))
            })
        })
        .transpose()?;
    Ok(RunPage {
        generation,
        total: selected.len(),
        columns: columns.into_iter().collect(),
        display_columns,
        rows,
        next_cursor,
    })
}

fn sort_value(run: &ResultSnapshot, key: &str) -> Option<Value> {
    match key {
        "name" => run.name.clone().map(Value::String),
        "status" => Some(run.status.clone().into()),
        "started_at" => Some(run.started_at.clone().into()),
        _ => {
            let (kind, field) = key.split_once(':')?;
            let fields = match kind {
                "metric" => &run.metrics,
                "param" => &run.parameters,
                _ => return None,
            };
            fields
                .iter()
                .find(|f| f.key() == field && f.state == Availability::Present)
                .map(|f| f.value.clone())
                .filter(|v| !v.is_null())
        }
    }
}

fn scalar_order(a: &Value, b: &Value) -> Ordering {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

fn summary(run: &SealedResult, columns: &[String]) -> RunSummary {
    let s = &run.snapshot;
    let fields = |kind: &str, values: &[Datum]| {
        values
            .iter()
            .filter(|f| columns.contains(&format!("{kind}:{}", f.key())))
            .cloned()
            .collect()
    };
    RunSummary {
        id: s.experiment_id.clone(),
        digest: run.digest.clone(),
        origin: s.origin,
        name: s.name.clone(),
        status: s.status.clone(),
        base_commit: s.base_commit.clone(),
        started_at: s.started_at.clone(),
        ended_at: s.ended_at.clone(),
        parameters: fields("param", &s.parameters),
        metrics: fields("metric", &s.metrics),
        capture_issues: s.capture_issues.clone(),
    }
}

pub fn compare(runs: &[SealedResult], spec: &ComparisonSpec) -> Result<Comparison> {
    super::interactions::validate_ranges(&spec.axes)?;
    if runs.is_empty()
        || runs.len() > MAX_COMPARISON_RUNS
        || spec
            .columns
            .as_ref()
            .is_some_and(|columns| columns.len() > MAX_COLUMNS)
    {
        return Err(invalid("comparison exceeds run or column limits"));
    }
    let mut ids = BTreeSet::new();
    for run in runs {
        run.verify()?;
        if !ids.insert(&run.snapshot.experiment_id) {
            return Err(invalid("duplicate comparison run"));
        }
    }
    if spec.datasets.len() > MAX_COMPARISON_RUNS || spec.objectives.len() > MAX_COLUMNS {
        return Err(invalid("comparison identity or objective limit exceeded"));
    }
    for (id, identity) in &spec.datasets {
        super::check_digest(&identity.hash)?;
        if identity.split.is_empty()
            || identity.split.len() > 256
            || identity.evaluator.is_empty()
            || identity.evaluator.len() > 256
        {
            return Err(invalid(
                "dataset split and evaluator must contain 1–256 characters",
            ));
        }
        let run = runs
            .iter()
            .find(|run| &run.snapshot.experiment_id == id)
            .ok_or_else(|| invalid("dataset identity refers to an unselected run"))?;
        let witnessed = run
            .snapshot
            .stages
            .iter()
            .filter(|stage| stage.state == "Committed")
            .flat_map(|stage| &stage.inputs)
            .any(|input| input.hash == identity.hash)
            || run
                .snapshot
                .reproduction
                .iter()
                .flat_map(|spec| &spec.files)
                .any(|file| file.hash.as_deref() == Some(&identity.hash));
        if !witnessed {
            return Err(invalid(
                "dataset identity must match a retained input witness",
            ));
        }
    }
    let baseline = runs
        .iter()
        .find(|r| r.snapshot.experiment_id == spec.baseline)
        .ok_or_else(|| invalid("baseline is not in the comparison"))?;
    let mut keys = BTreeSet::new();
    if let Some(columns) = &spec.columns {
        keys.extend(columns.iter().cloned());
    } else {
        for run in runs {
            keys.extend(run.snapshot.metrics.iter().map(Datum::key));
        }
        if keys.len() > MAX_COLUMNS {
            return Err(invalid("choose at most 32 comparison metric columns"));
        }
    }
    let baseline_fields = baseline
        .snapshot
        .metrics
        .iter()
        .map(|f| (f.key(), f))
        .collect::<BTreeMap<_, _>>();
    let baseline_dataset = spec.datasets.get(&spec.baseline);
    let mut rows = Vec::with_capacity(runs.len());
    for run in runs {
        let id = &run.snapshot.experiment_id;
        let comparable = baseline_dataset.is_some() && baseline_dataset == spec.datasets.get(id);
        let metadata_only = run.snapshot.origin == ResultOrigin::ExperimentMetadata
            || baseline.snapshot.origin == ResultOrigin::ExperimentMetadata;
        let completed = !metadata_only
            && run.snapshot.status == "success"
            && baseline.snapshot.status == "success";
        let saved_observations =
            run.snapshot.status == "saved" || baseline.snapshot.status == "saved";
        let fields = run
            .snapshot
            .metrics
            .iter()
            .map(|f| (f.key(), f))
            .collect::<BTreeMap<_, _>>();
        let cells = keys
            .iter()
            .map(|key| {
                let candidate = fields.get(key).copied().cloned();
                let baseline = baseline_fields.get(key).copied().cloned();
                let number = |f: &Option<Datum>| {
                    f.as_ref()
                        .filter(|f| f.state == Availability::Present)
                        .and_then(|f| f.value.as_f64())
                };
                let values = number(&candidate).zip(number(&baseline));
                let delta = values.map(|(a, b)| a - b).filter(|v| v.is_finite());
                let relative = values
                    .filter(|(_, b)| *b != 0.0)
                    .map(|(a, b)| (a - b) / b.abs())
                    .filter(|v| v.is_finite());
                let direction = spec.objectives.get(key);
                let improvement = delta
                    .zip(direction)
                    .filter(|_| comparable && completed)
                    .map(|(d, direction)| match direction {
                        Direction::Minimize => d < 0.0,
                        Direction::Maximize => d > 0.0,
                    });
                let reason = if values.is_none() {
                    Some("Metric is missing or non-numeric")
                } else if metadata_only {
                    Some("Imported experiment metadata does not establish captured execution inputs or completion")
                } else if saved_observations {
                    Some("Saved workspace observations do not record execution completion")
                } else if !completed {
                    Some("A run did not complete; values are partial observations")
                } else if !comparable {
                    Some("Dataset, split or evaluator identity is unknown or differs")
                } else if direction.is_none() {
                    Some("Objective direction is unspecified")
                } else if relative.is_none() {
                    Some("Relative delta is unavailable for a zero baseline")
                } else {
                    None
                };
                ComparisonCell {
                    key: key.clone(),
                    candidate,
                    baseline,
                    absolute_delta: delta,
                    relative_delta: relative,
                    improvement,
                    reason: reason.map(str::to_owned),
                }
            })
            .collect();
        rows.push(ComparisonRow {
            run: run.clone(),
            comparable,
            cells,
        });
    }
    Ok(Comparison {
        spec: spec.clone(),
        rows,
    })
}

pub fn flatten_scalars(file: &str, value: &Value) -> Result<Vec<Datum>> {
    super::model::validate_path(file)?;
    let mut out = Vec::new();
    flatten(file, "", value, 0, &mut out)?;
    Ok(out)
}

fn flatten(
    file: &str,
    pointer: &str,
    value: &Value,
    depth: usize,
    out: &mut Vec<Datum>,
) -> Result<()> {
    if depth > 32 || out.len() >= MAX_FIELDS {
        return Err(invalid("scalar document exceeds depth or field limit"));
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                flatten(file, &format!("{pointer}/{escaped}"), value, depth + 1, out)?;
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                flatten(file, &format!("{pointer}/{index}"), value, depth + 1, out)?;
            }
        }
        value => {
            let unsafe_integer = super::model::unsafe_integer(value);
            out.push(Datum {
                file: file.into(),
                pointer: pointer.into(),
                state: if unsafe_integer {
                    Availability::Unsupported
                } else {
                    Availability::Present
                },
                value: if unsafe_integer {
                    Value::Null
                } else {
                    value.clone()
                },
                reason: unsafe_integer
                    .then(|| "Integer exceeds exact JSON client precision".into()),
            });
        }
    }
    Ok(())
}
