//! Workflow plot configuration contracts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Structured plot configuration for workflow metrics consumers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlotConfig {
    /// Optional DVC plot ID used to target a named plot whose data lives in `path`.
    #[serde(default)]
    pub id: Option<String>,
    /// Path to the metrics data file.
    pub path: PathBuf,
    /// Column or field name for the x-axis.
    #[serde(default)]
    pub x: Option<String>,
    /// Optional source file for x-axis values when DVC maps x and y to different files.
    #[serde(default)]
    pub x_path: Option<PathBuf>,
    /// Column or field names for the y-axis.
    #[serde(default)]
    pub y: Vec<String>,
    /// CSV/TSV inputs do not contain a header row; columns are addressed by zero-based index.
    #[serde(default)]
    pub no_header: bool,
    /// Human-readable plot title.
    #[serde(default)]
    pub title: Option<String>,
    /// Human-readable x-axis label.
    #[serde(default)]
    pub x_label: Option<String>,
    /// Human-readable y-axis label.
    #[serde(default)]
    pub y_label: Option<String>,
    /// Plot template.
    #[serde(default)]
    pub template: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plot_config_serde_defaults_optional_fields() {
        let raw = r#"{"path":"metrics.json"}"#;
        let config: PlotConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config.path, PathBuf::from("metrics.json"));
        assert_eq!(config.id, None);
        assert_eq!(config.x, None);
        assert!(config.y.is_empty());
        assert!(!config.no_header);
    }
}
