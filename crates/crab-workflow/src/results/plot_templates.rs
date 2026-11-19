use super::{
    plot_matrices::CountedMatrix,
    plot_specs::{coordinate_field, field_reference, plot_scalar},
    plots::PlotData,
};
use crate::{PlotConfig, Result, WorkflowError as CrabError};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(super) fn build_template_vega_spec(
    config: &PlotConfig,
    title: &str,
    data: &PlotData,
    repo_root: &Path,
    revisions: &[(&str, &PlotData)],
    comparison: bool,
    x_field: &str,
) -> Result<Option<Value>> {
    let Some(template) = config.template.as_deref() else {
        return Ok(None);
    };
    let Some(resolved) = resolve_plot_template(repo_root, template)? else {
        return Ok(None);
    };
    if resolved.matrix {
        // CLI exports retain the tagged unbounded Vega-spec contract. Results
        // applies its display budget while feeding this same exact-count owner.
        let mut matrix = CountedMatrix::default();
        for (revision, data) in revisions {
            matrix.observe(revision, data)?;
        }
        return matrix.spec(resolved.spec, config, title).map(Some);
    }
    let spec = resolved.spec;
    let y_fields: BTreeSet<_> = revisions
        .iter()
        .flat_map(|(_, data)| &data.y_fields)
        .cloned()
        .collect();
    let multiple = y_fields.len() > 1;
    let x_coordinate = coordinate_field(x_field, &y_fields.iter().cloned().collect::<Vec<_>>());
    let mut occupied = y_fields.clone();
    occupied.extend([x_field.to_owned(), x_coordinate.clone(), "step".to_owned()]);
    // Metadata must never replace authored columns used by template expressions.
    // Prefer DVC's names, allocating private names only when the input owns them.
    let mut available = |name: &str| {
        let mut name = name.to_owned();
        while !occupied.insert(name.clone()) {
            name.push('_');
        }
        name
    };
    let rev = available("rev");
    let field = available("field");
    let filename = available("filename");
    let metric = available("metric");
    let series = available("series");
    let raw_y = data
        .y_fields
        .first()
        .cloned()
        .unwrap_or_else(|| "value".into());
    let y_field = if multiple {
        available("__crab_value")
    } else if raw_y.contains('\\') {
        available("__crab_y")
    } else {
        raw_y.clone()
    };
    let mut values = Vec::new();
    for (revision, data) in revisions {
        for row in &data.rows {
            let mut object = serde_json::Map::new();
            object.insert("step".into(), row.index.into());
            object.insert(x_field.into(), plot_scalar(&row.x, row.x_missing));
            if x_coordinate != x_field {
                object.insert(x_coordinate.clone(), plot_scalar(&row.x, row.x_missing));
            }
            for (index, field) in data.y_fields.iter().enumerate() {
                object.insert(
                    field.clone(),
                    plot_scalar(&row.y_values[index], row.missing_values[index]),
                );
            }
            object.insert(rev.clone(), (*revision).into());
            object.insert(
                filename.clone(),
                config.path.to_string_lossy().into_owned().into(),
            );
            // Long rows retain the original wide columns. Authored calculations
            // can still compare columns while each selected y gets its own path.
            for (index, selected) in data.y_fields.iter().enumerate() {
                let mut item = object.clone();
                item.insert(field.clone(), selected.clone().into());
                item.insert(metric.clone(), selected.clone().into());
                item.insert(series.clone(), format!("{revision}: {selected}").into());
                if multiple || y_field != *selected {
                    item.insert(
                        y_field.clone(),
                        plot_scalar(&row.y_values[index], row.missing_values[index]),
                    );
                }
                values.push(Value::Object(item));
            }
        }
    }
    let x = field_reference(&x_coordinate);
    let y = field_reference(&y_field);
    let legacy_color = if multiple {
        &series
    } else if comparison {
        &rev
    } else {
        &metric
    };
    let group = if multiple {
        vec![rev.clone(), field.clone()]
    } else {
        vec![rev.clone()]
    };
    let mut group_x = group.clone();
    group_x.push(x.clone());
    let mut group_y = group.clone();
    group_y.push(y.clone());
    let revs: Vec<_> = revisions.iter().map(|(rev, _)| *rev).collect();
    let mapping = if multiple {
        json!({"field":field,"type":"nominal","sort":y_fields})
    } else {
        json!({})
    };
    let y_label = config
        .y_label
        .as_deref()
        .unwrap_or(if multiple { "value" } else { &raw_y });
    let mut tooltip = vec![
        json!({"field":rev}),
        json!({"field":x,"title":config.x_label.as_deref().unwrap_or(x_field)}),
        json!({"field":y,"title":y_label}),
    ];
    if multiple {
        tooltip.push(json!({"field":field}));
    }
    let pivot = group
        .iter()
        .map(|key| serde_json::to_string(key).map(|key| format!("datum[{key}]")))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(CrabError::ResultsJson)?
        .join(" + '::' + ");
    let anchors = BTreeMap::from([
        ("DATA", Value::Array(values)),
        ("TITLE", title.into()),
        ("X", x.into()),
        ("Y", y.into()),
        (
            "X_LABEL",
            config.x_label.as_deref().unwrap_or(x_field).into(),
        ),
        ("Y_LABEL", y_label.into()),
        (
            "COLOR",
            json!({"field":rev,"type":"nominal","scale":{"domain":revs}}),
        ),
        ("STROKE_DASH", mapping.clone()),
        ("SHAPE", mapping.clone()),
        ("ROW", mapping.clone()),
        ("COLUMN", mapping),
        ("GROUP_BY", json!(group)),
        ("GROUP_BY_X", json!(group_x)),
        ("GROUP_BY_Y", json!(group_y)),
        ("TOOLTIP", json!(tooltip)),
        ("PIVOT_FIELD", pivot.into()),
        (
            "ZOOM_AND_PAN",
            json!({"name":"grid","select":"interval","bind":"scales"}),
        ),
        ("PLOT_WIDTH", 700.into()),
        ("PLOT_HEIGHT", 300.into()),
    ]);
    fill_anchors(spec, &anchors, legacy_color, "").map(Some)
}

pub(super) fn fill_anchors(
    value: Value,
    anchors: &BTreeMap<&str, Value>,
    color: &str,
    context: &str,
) -> Result<Value> {
    match value {
        Value::String(text) => {
            for (name, value) in anchors {
                let anchor = format!("<DVC_METRIC_{name}>");
                if text == anchor {
                    // Historical DVC templates use COLOR as a field accessor;
                    // current templates insert an entire encoding definition.
                    return Ok(
                        if *name == "COLOR" && matches!(context, "field" | "groupby") {
                            color.into()
                        } else {
                            value.clone()
                        },
                    );
                }
            }
            let mut output = text.clone();
            for (name, value) in anchors {
                let anchor = format!("<DVC_METRIC_{name}>");
                if !text.contains(&anchor) {
                    continue;
                }
                if *name == "COLOR" {
                    output = output.replace(&anchor, color);
                } else if let Some(value) = value.as_str() {
                    output = output.replace(&anchor, value);
                } else {
                    return Err(super::invalid(format!(
                        "{anchor} must occupy a complete JSON value"
                    )));
                }
            }
            // Check the authored text only: captured data and labels containing
            // anchor-looking text are user values, never another template pass.
            for suffix in text.split("<DVC_METRIC_").skip(1) {
                let Some((name, _)) = suffix.split_once('>') else {
                    return Err(super::invalid("unclosed DVC plot anchor"));
                };
                if !anchors.contains_key(name) {
                    return Err(super::invalid(format!("unknown DVC plot anchor {name}")));
                }
            }
            Ok(output.into())
        }
        Value::Array(items) => items
            .into_iter()
            .map(|item| fill_anchors(item, anchors, color, context))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(map) => {
            // Statistical field definitions keep Vega's derived titles, such
            // as "Mean of loss"; plain coordinates should not expose aliases.
            let label = if !["title", "aggregate", "bin", "timeUnit"]
                .iter()
                .any(|key| map.contains_key(*key))
                && matches!(context, "x" | "y" | "tooltip")
            {
                match map.get("field").and_then(Value::as_str) {
                    Some("<DVC_METRIC_X>") => anchors.get("X_LABEL").cloned(),
                    Some("<DVC_METRIC_Y>") => anchors.get("Y_LABEL").cloned(),
                    _ => None,
                }
            } else {
                None
            };
            let mut output = map
                .into_iter()
                .map(|(key, value)| {
                    fill_anchors(value, anchors, color, &key).map(|value| (key, value))
                })
                .collect::<Result<serde_json::Map<_, _>>>()?;
            if let Some(label) = label {
                output.insert("title".into(), label);
            }
            Ok(Value::Object(output))
        }
        other => Ok(other),
    }
}

pub struct BuiltinPlotTemplate {
    pub name: &'static str,
    pub description: &'static str,
}

pub const BUILTIN_PLOT_TEMPLATES: &[BuiltinPlotTemplate] = &[
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

pub(super) struct ResolvedPlotTemplate {
    pub(super) spec: Value,
    pub(super) matrix: bool,
}

pub(super) fn builtin_matrix(template: &str) -> bool {
    matches!(template, "confusion" | "confusion_normalized")
}

pub(super) fn resolve_plot_template(
    repo_root: &Path,
    template: &str,
) -> Result<Option<ResolvedPlotTemplate>> {
    if let Some(path) = resolve_custom_plot_template_path(repo_root, template) {
        return read_plot_template_path(&path).map(|spec| {
            Some(ResolvedPlotTemplate {
                spec,
                matrix: false,
            })
        });
    }
    Ok(
        builtin_plot_template_spec(template).map(|spec| ResolvedPlotTemplate {
            spec,
            matrix: builtin_matrix(template),
        }),
    )
}

pub fn read_plot_template_path(path: &Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(path).map_err(CrabError::Io)?;
    serde_json::from_str(&text).map_err(|e| CrabError::Configuration {
        key: path.display().to_string(),
        origin: format!("plot template JSON parse error: {e}"),
    })
}

pub fn resolve_custom_plot_template_path(repo_root: &Path, template: &str) -> Option<PathBuf> {
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

pub fn builtin_plot_template_spec(template: &str) -> Option<serde_json::Value> {
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
                "x": {"field": "<DVC_METRIC_X>", "type": "quantitative", "title":"<DVC_METRIC_X_LABEL>"},
                "y": {"field": "<DVC_METRIC_Y>", "type": "quantitative", "title":"<DVC_METRIC_Y_LABEL>"},
                "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
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
            "transform": [{"loess": "<DVC_METRIC_Y>", "on": "<DVC_METRIC_X>", "groupby":["<DVC_METRIC_COLOR>"]}],
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
    // Aggregate before faceting, retaining the revision in every grouping.
    // Normalization is within each y class of each run, never across runs.
    let mut transforms = vec![
        serde_json::json!({"aggregate":[{"op":"count","as":"count"}],
            "groupby":["<DVC_METRIC_X>","<DVC_METRIC_Y>","<DVC_METRIC_COLOR>"]}),
        serde_json::json!({"impute":"count", "key":"<DVC_METRIC_X>",
            "groupby":["<DVC_METRIC_Y>","<DVC_METRIC_COLOR>"], "value":0}),
        serde_json::json!({"impute":"count", "key":"<DVC_METRIC_Y>",
            "groupby":["<DVC_METRIC_X>","<DVC_METRIC_COLOR>"], "value":0}),
    ];
    if normalized {
        transforms.extend([
            serde_json::json!({"joinaggregate":[{"op":"sum","field":"count","as":"total"}],
                "groupby":["<DVC_METRIC_Y>","<DVC_METRIC_COLOR>"]}),
            serde_json::json!({"calculate":"datum.total ? datum.count / datum.total : 0","as":"normalized"}),
        ]);
    }
    serde_json::json!({
        "$schema": "https://vega.github.io/schema/vega-lite/v5.json",
        "title": "<DVC_METRIC_TITLE>",
        "data": {"values": "<DVC_METRIC_DATA>"},
        "transform": transforms,
        "width": 280,
        "height": 280,
        "mark": {"type": "rect", "tooltip": true},
        "encoding": {
            "x": {"field": "<DVC_METRIC_X>", "type": "nominal", "title": "<DVC_METRIC_X_LABEL>"},
            "y": {"field": "<DVC_METRIC_Y>", "type": "nominal", "title": "<DVC_METRIC_Y_LABEL>"},
            "color": if normalized {
                serde_json::json!({"field":"normalized","type":"quantitative","scale":{"domain":[0,1]}})
            } else {
                serde_json::json!({"field":"count","type":"quantitative"})
            },
            "column":{"field":"<DVC_METRIC_COLOR>","type":"nominal","title":"series"}
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
            "x": {"field": "<DVC_METRIC_X>", "type": "quantitative", "title": "<DVC_METRIC_X_LABEL>"},
            "y": {
                "field": "<DVC_METRIC_Y>",
                "type": "nominal",
                "title": "<DVC_METRIC_Y_LABEL>",
                "sort": sort
            },
            "yOffset": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"},
            "color": {"field": "<DVC_METRIC_COLOR>", "type": "nominal"}
        }
    })
}
