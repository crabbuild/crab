use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::{
    MAX_CHART_POINTS, invalid, model::unsafe_integer, plot_specs::plot_scalar,
    plot_templates::fill_anchors, plots::PlotData,
};
use crate::{PlotConfig, Result, WorkflowError};

#[derive(Default)]
struct Classes {
    identities: BTreeMap<String, usize>,
    labels: Vec<String>,
}

impl Classes {
    fn observe(&mut self, raw: &str, missing: bool) -> Result<usize> {
        let value = plot_scalar(raw, missing);
        if unsafe_integer(&value) {
            return Err(invalid("matrix class integer exceeds exact JSON precision"));
        }
        let identity = if let Some(number) = value.as_f64() {
            // JavaScript groups numerically equal values together, including -0.
            let number = if number == 0.0 { 0.0 } else { number };
            let bits = number.to_bits();
            let ordered = if number.is_sign_negative() {
                !bits
            } else {
                bits ^ (1 << 63)
            };
            format!("1:{ordered:016x}")
        } else if value.is_null() {
            "0:".into()
        } else {
            format!(
                "2:{}",
                serde_json::to_string(&value).map_err(WorkflowError::ResultsJson)?
            )
        };
        if let Some(index) = self.identities.get(&identity) {
            return Ok(*index);
        }
        let index = self.labels.len();
        let label = if value.is_null() {
            "(missing)".into()
        } else {
            serde_json::to_string(&value).map_err(WorkflowError::ResultsJson)?
        };
        self.identities.insert(identity, index);
        self.labels.push(label);
        Ok(index)
    }

    fn order(&self) -> Vec<usize> {
        self.identities.values().copied().collect()
    }
}

struct Series {
    revision: String,
    field: String,
    counts: BTreeMap<(usize, usize), usize>,
}

#[derive(Default)]
pub(super) struct CountedMatrix {
    bounded: bool,
    x: Classes,
    y: Classes,
    series: Vec<Series>,
    points: usize,
}

impl CountedMatrix {
    pub(super) fn bounded() -> Self {
        Self {
            bounded: true,
            ..Self::default()
        }
    }

    pub(super) fn observe(&mut self, revision: &str, data: &PlotData) -> Result<()> {
        let start = self.series.len();
        self.series.extend(data.y_fields.iter().map(|field| Series {
            revision: revision.into(),
            field: field.clone(),
            counts: BTreeMap::new(),
        }));
        self.check_cells()?;
        for row in &data.rows {
            if row.y_values.len() != data.y_fields.len()
                || row.missing_values.len() != data.y_fields.len()
            {
                return Err(invalid("matrix row does not match selected columns"));
            }
            let x = self.x.observe(&row.x, row.x_missing)?;
            for ((value, missing), series) in row
                .y_values
                .iter()
                .zip(&row.missing_values)
                .zip(&mut self.series[start..])
            {
                let y = self.y.observe(value, *missing)?;
                let count = series.counts.entry((x, y)).or_default();
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| invalid("matrix count overflow"))?;
            }
            self.points = self
                .points
                .checked_add(data.y_fields.len())
                .ok_or_else(|| invalid("matrix observation count overflow"))?;
            self.check_cells()?;
        }
        Ok(())
    }

    fn check_cells(&self) -> Result<()> {
        let cells = self
            .x
            .labels
            .len()
            .checked_mul(self.y.labels.len())
            .and_then(|cells| cells.checked_mul(self.series.len()))
            .ok_or_else(|| invalid("matrix grid exceeds addressable size"))?;
        if self.bounded && cells > MAX_CHART_POINTS {
            return Err(invalid(
                "Confusion matrix exceeds 2,000 display cells; select fewer runs or inspect retained source bytes",
            ));
        }
        Ok(())
    }

    pub(super) fn points(&self) -> usize {
        self.points
    }

    pub(super) fn pairs(&self) -> usize {
        self.series.iter().map(|series| series.counts.len()).sum()
    }

    pub(super) fn spec(&self, spec: Value, config: &PlotConfig, title: &str) -> Result<Value> {
        let mut values = Vec::new();
        for (index, series) in self.series.iter().enumerate() {
            values.extend(series.counts.iter().map(|((x, y), count)| {
                json!({
                    "__crab_x":x,"__crab_y":y,"__crab_series":index,"__crab_weight":count,
                })
            }));
        }
        let fields = self
            .series
            .iter()
            .map(|series| &series.field)
            .collect::<BTreeSet<_>>();
        let x_label = config
            .x_label
            .as_deref()
            .unwrap_or(config.x.as_deref().unwrap_or("step"));
        let y_label = config.y_label.as_deref().unwrap_or_else(|| {
            if fields.len() == 1 {
                fields.first().map_or("value", |field| field.as_str())
            } else {
                "value"
            }
        });
        let series_labels = self
            .series
            .iter()
            .map(|series| {
                if fields.len() > 1 {
                    format!("{}: {}", series.revision, series.field)
                } else {
                    series.revision.clone()
                }
            })
            .collect::<Vec<_>>();
        let anchors = BTreeMap::from([
            ("DATA", json!(values)),
            ("TITLE", title.into()),
            ("X", "__crab_x".into()),
            ("Y", "__crab_y".into()),
            ("X_LABEL", x_label.into()),
            ("Y_LABEL", y_label.into()),
            ("COLOR", json!({"field":"__crab_series","type":"nominal"})),
        ]);
        let mut spec = fill_anchors(spec, &anchors, "__crab_series", "")?;
        // Vega-Lite rewrites datum.value/label textually in label expressions.
        // Keep user labels in parameter values so literal text is never rewritten.
        spec["params"] = json!([
            {"name":"crab_matrix_x_labels","value":self.x.labels},
            {"name":"crab_matrix_y_labels","value":self.y.labels},
            {"name":"crab_matrix_series_labels","value":series_labels},
        ]);
        let label_expression = |parameter: &str, field: &str| format!("{parameter}[{field}]");
        spec["encoding"]["x"]["axis"] =
            json!({"labelExpr":label_expression("crab_matrix_x_labels","datum.value")});
        spec["encoding"]["x"]["sort"] = json!(self.x.order());
        spec["encoding"]["y"]["axis"] =
            json!({"labelExpr":label_expression("crab_matrix_y_labels","datum.value")});
        spec["encoding"]["y"]["sort"] = json!(self.y.order());
        spec["encoding"]["column"]["header"] =
            json!({"labelExpr":label_expression("crab_matrix_series_labels","datum.value")});
        let dimension_title = |role: &str, label: &str| {
            if label.is_empty() {
                role.to_owned()
            } else {
                format!("{role} ({label})")
            }
        };
        let x_tooltip = dimension_title("X", x_label);
        let y_tooltip = dimension_title("Y", y_label);
        spec["encoding"]["tooltip"] = json!([
            {"field":"__crab_actual_label","type":"nominal","title":x_tooltip},
            {"field":"__crab_predicted_label","type":"nominal","title":y_tooltip},
            {"field":"__crab_series_label","type":"nominal","title":"series"},
            {"field":"count","type":"quantitative"},
        ]);
        let normalized = spec["encoding"]["color"]["field"] == "normalized";
        if normalized {
            let tooltip = spec["encoding"]["tooltip"]
                .as_array_mut()
                .ok_or_else(|| invalid("owned matrix tooltip is unavailable"))?;
            tooltip.push(json!({"field":"normalized","type":"quantitative"}));
        }
        spec["encoding"]["description"] = json!({"field":"__crab_description","type":"nominal"});
        let transforms = spec
            .get_mut("transform")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid("owned matrix template lacks its transforms"))?;
        let first = transforms
            .first_mut()
            .ok_or_else(|| invalid("owned matrix template lacks its count"))?;
        // Owned matrices count before rendering. Summing weights preserves the
        // raw-data template contract while keeping all Vega keys numeric: Vega's
        // string keys otherwise merge null/"null" and delimiter-bearing classes.
        first["aggregate"] = json!([{"op":"sum","field":"__crab_weight","as":"count"}]);
        for (parameter, field, alias) in [
            (
                "crab_matrix_x_labels",
                "datum.__crab_x",
                "__crab_actual_label",
            ),
            (
                "crab_matrix_y_labels",
                "datum.__crab_y",
                "__crab_predicted_label",
            ),
            (
                "crab_matrix_series_labels",
                "datum.__crab_series",
                "__crab_series_label",
            ),
        ] {
            transforms.push(json!({"calculate":label_expression(parameter,field),"as":alias}));
        }
        let x_title = serde_json::to_string(&x_tooltip).map_err(WorkflowError::ResultsJson)?;
        let y_title = serde_json::to_string(&y_tooltip).map_err(WorkflowError::ResultsJson)?;
        let mut description = format!(
            "datum.__crab_series_label + '; ' + {x_title} + ': ' + datum.__crab_actual_label + '; ' + {y_title} + ': ' + datum.__crab_predicted_label"
        );
        if normalized {
            description.push_str(" + '; normalized: ' + datum.normalized");
        }
        description.push_str(" + '; count: ' + datum.count");
        transforms.push(json!({"calculate":description,"as":"__crab_description"}));
        Ok(spec)
    }
}
