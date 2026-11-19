use super::plot_templates::build_template_vega_spec;
use super::plots::PlotData;
use crate::{PlotConfig, Result, WorkflowError as CrabError};
use std::path::Path;

pub(super) fn field_reference(field: &str) -> String {
    field
        .chars()
        .flat_map(|character| {
            let escape = matches!(character, '\\' | '.' | '[' | ']');
            escape
                .then_some('\\')
                .into_iter()
                .chain(std::iter::once(character))
        })
        .collect()
}

pub(super) fn literal_field(reference: &str) -> Option<String> {
    let mut characters = reference.chars();
    let mut field = String::new();
    while let Some(character) = characters.next() {
        match character {
            '\\' => field.push(characters.next()?),
            '.' | '[' | ']' => return None,
            other => field.push(other),
        }
    }
    Some(field)
}

pub(super) fn coordinate_field(field: &str, y_fields: &[String]) -> String {
    if !field.contains('\\') {
        return field.into();
    }
    // Vega-Lite 5 drops literal backslashes when flattening field accessors.
    // Keep the authored key for expressions and use an alias for encodings.
    let mut alias = "__crab_x".to_owned();
    while y_fields.contains(&alias) || alias == field {
        alias.push('_');
    }
    alias
}

pub struct PlotRevisionData {
    pub revision: String,
    pub data: PlotData,
}

pub fn revision_data_summary(revision_data: &[PlotRevisionData]) -> String {
    revision_data
        .iter()
        .map(|item| item.revision.as_str())
        .collect::<Vec<_>>()
        .join(" vs ")
}

pub fn build_vega_spec(
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
        &[("workspace", data)],
        false,
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
            "x": { "field": field_reference(&coordinate_field(x_field, &data.y_fields)), "type": x_type, "title": x_title },
            "y": { "field": "value", "type": y_type, "title": y_title },
            "color": { "field": "metric", "type": "nominal" }
        }
    }))
}

pub fn build_vega_diff_spec(
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
        &[(baseline, baseline_data), (target, target_data)],
        true,
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
            "x": { "field": field_reference(&coordinate_field(x_field, &baseline_data.y_fields)), "type": x_type, "title": x_title },
            "y": { "field": "value", "type": y_type, "title": y_title },
            "color": { "field": "series", "type": "nominal", "title": "series" }
        }
    }))
}

pub fn build_vega_revision_spec(
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
        &revision_data
            .iter()
            .map(|item| (item.revision.as_str(), &item.data))
            .collect::<Vec<_>>(),
        true,
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
            "x": { "field": field_reference(&coordinate_field(x_field, &first.data.y_fields)), "type": x_type, "title": x_title },
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

pub fn plot_data_values(
    x_field: &str,
    data: &PlotData,
    revision: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let coordinate = coordinate_field(x_field, &data.y_fields);
    for row in &data.rows {
        for (idx, field) in data.y_fields.iter().enumerate() {
            let Some(value) = row.y_values.get(idx) else {
                continue;
            };
            let mut object = serde_json::Map::new();
            if x_field == "step" {
                object.insert("step".to_owned(), plot_scalar(&row.x, row.x_missing));
            } else {
                object.insert(
                    "step".to_owned(),
                    serde_json::Value::Number(row.index.into()),
                );
                object.insert(x_field.to_owned(), plot_scalar(&row.x, row.x_missing));
                if coordinate != x_field {
                    object.insert(coordinate.clone(), plot_scalar(&row.x, row.x_missing));
                }
            }
            object.insert(
                "metric".to_owned(),
                serde_json::Value::String(field.clone()),
            );
            object.insert(
                "value".to_owned(),
                plot_scalar(value, row.missing_values[idx]),
            );
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

pub(super) fn plot_scalar(raw: &str, missing: bool) -> serde_json::Value {
    if missing {
        return serde_json::Value::Null;
    }
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

pub fn infer_vega_type(values: &[serde_json::Value], field: &str) -> &'static str {
    if values.iter().all(|value| {
        value
            .get(field)
            .is_none_or(|value| value.is_null() || value.is_number())
    }) {
        "quantitative"
    } else {
        "nominal"
    }
}
