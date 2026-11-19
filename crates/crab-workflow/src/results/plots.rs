//! Shared parsing of workflow plot data.

use std::path::Path;

use serde::Serialize;

use crate::{PlotConfig, Result, WorkflowError as CrabError};

#[derive(Debug, Default, Clone, Serialize)]
pub struct PlotData {
    pub y_fields: Vec<String>,
    pub rows: Vec<PlotRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlotRow {
    pub x: String,
    pub y_values: Vec<String>,
    // Tagged CLI JSON exposes the original string rows. Keep absence separate
    // so chart normalization can distinguish nulls from literal category text.
    #[serde(skip)]
    pub(super) x_missing: bool,
    #[serde(skip)]
    pub(super) missing_values: Vec<bool>,
    #[serde(skip)]
    pub(super) index: usize,
}

impl PlotRow {
    /// Replaces the x coordinate and its absence witness from a cross-file source.
    pub fn replace_x(&mut self, source: Self) {
        self.x = source.x;
        self.x_missing = source.x_missing;
    }
}

pub fn parse_plot_data_bytes(path: &Path, bytes: &[u8], config: &PlotConfig) -> Result<PlotData> {
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
            x_missing: !x_col.is_empty() && x_val.is_empty(),
            missing_values: y_indices
                .iter()
                .map(|idx| {
                    idx.and_then(|idx| cols.get(idx))
                        .is_none_or(|value| value.is_empty())
                })
                .collect(),
            index: i,
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
            x_missing: !x_col.is_empty() && obj.get(x_col).is_none_or(serde_json::Value::is_null),
            missing_values: y_fields
                .iter()
                .map(|key| obj.get(key).is_none_or(serde_json::Value::is_null))
                .collect(),
            index: i,
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

pub fn declared_plot_configs(workflow: &crate::Workflow) -> Vec<PlotConfig> {
    let mut configs = workflow.plot_configs.clone();
    for path in &workflow.plots {
        push_simple_plot_config(&mut configs, path);
    }
    for stage in workflow.stages.values() {
        for path in &stage.plots {
            push_simple_plot_config(
                &mut configs,
                &stage
                    .wdir
                    .as_deref()
                    .unwrap_or_else(|| Path::new(""))
                    .join(path),
            );
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
